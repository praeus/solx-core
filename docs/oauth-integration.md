# OAuth Integration in Sol

> Status: implementation present (webhook `auth` block + `oauth-loopback` action)
> Owner: `sol-manager` (dispatcher), `sol-server` (HTTP listener), `sol-browser` (system-browser launcher)
> Audience: action authors adding OAuth flows, package authors shipping login scripts, security reviewers

This document describes how the Sol dispatcher integrates with the four
common OAuth 2.0 grant types, the secrets model that backs them, the
allowlist that gates outbound calls, and the lifecycle of the loopback
HTTP listener that captures authorization-code redirects. It is
deliberately written so an action author can answer *"what does my
`action_config.auth` block do and what state does it leave in the
process?"* without reading the Rust source.

---

## 1. Overview

Sol supports four authentication strategies for outbound HTTP calls,
all expressed declaratively on the Webhook action's
[`action_config.auth`](../sol-manager/src/webhook_auth.rs) block. There
is also a fifth mechanism, `oauth-loopback`, that drives the
authorization-code dance by binding a per-state localhost HTTP listener
and feeding the captured `code` back to the caller through the
dispatcher.

| Strategy | Where it lives | Auth type string |
|---|---|---|
| Static bearer token | [webhook_auth.rs](../sol-manager/src/webhook_auth.rs) | `"bearer"` |
| Refresh-token grant (RFC 6749 §6) | webhook_auth.rs | `"oauth_refresh"` |
| Service-account JWT-bearer (RFC 7521 §5.1) | webhook_auth.rs | `"oauth_service_account"` |
| Authorization-code (loopback + token exchange) | [loopback_control.rs](../sol-manager/src/loopback_control.rs) + [webhooks.rs](../sol-manager/src/dispatcher/webhooks.rs) | `"oauth_authorization_code"` (sentinel; the actual orchestration is split across the `oauth-loopback` action and a Webhook with the same sentinel) |

All outbound webhook URLs are gated by `validate_webhook_url` against
the union of the **global** `allowed_webhook_base_urls` allowlist
(declared in `sol-config.json`) and the action's per-action `ActionLink`
entries. Path-template substitution happens **before** the allowlist
check so a substituted URL cannot be used to evade the gate.

---

## 2. The `action_config.auth` block

`action_config` is the JSON document stored on the `Action` entity. Its
`auth` sub-object is the only place the dispatcher looks for credentials.
There is no implicit credential lookup; an action with no `auth` block
makes an anonymous request.

### 2.1 Schema (informal)

```json
{
  "secrets": {                // decryption keys for this action's *_secret pointers
    "GOOGLE_OAUTH_CLIENT_ID": "<32-byte-b64-key>",
    "GOOGLE_OAUTH_CLIENT_SECRET": "<32-byte-b64-key>"
  },
  "auth": {
    "type": "bearer | oauth_refresh | oauth_service_account | oauth_authorization_code",
    "token": "...",            // bearer only — string OR keyring ref
    "ttl_secs": 3600,          // bearer / oauth_service_account; default 3600
    "client_id": "...",        // oauth_refresh, oauth_authorization_code
    "client_secret": "...",    // oauth_refresh, oauth_authorization_code — string OR keyring ref
    "refresh_token": "...",    // oauth_refresh — string OR keyring ref
    "token_uri": "https://...",// oauth_refresh, oauth_service_account
    "scope": "...",            // oauth_service_account (JWT claim), oauth_authorization_code (form)
    "client_email": "...",     // oauth_service_account
    "private_key": "...",      // oauth_service_account — string OR keyring ref OR secret-store pointer
    "client_id_secret": "NAME",
    "client_secret_secret": "NAME",
    "refresh_token_secret": "NAME",
    "scope_secret": "NAME",
    "token_secret": "NAME",     // bearer only
    "private_key_secret": "NAME"
  }
}
```

**Secret sources.** Every secret field in the `auth` block accepts
**one of three** shapes at call time (highest precedence first):

1. **Inline value** — a JSON string in `action_config.auth` itself,
   e.g. `"token": "abc123"`. *Recommended for development fixtures;
   discouraged for production secrets.*
2. **Keyring reference** — `{"keyring_service": "sol", "keyring_account": "google-svc-pk"}`.
   The dispatcher resolves this through the OS credential manager
   (DPAPI on Windows, Keychain on macOS, libsecret on Linux) via the
   [`keyring`](https://crates.io/crates/keyring) crate directly. Not
   scoped to the calling action — any action holding this exact
   `action_config.auth` shape can read it. *Suitable for a single
   action's own long-lived secret.*
3. **Scoped secret-store pointer** — give the secret's *name* in the
   matching `*_secret` field, e.g.
   `"client_secret_secret": "GOOGLE_OAUTH_CLIENT_SECRET"`. The value is
   encrypted with AES-256-GCM and stored in the OS credential manager
   via [`crate::secrets::get_secret`/`set_secret`](../sol-manager/src/secrets.rs).
   The decryption **key** is not looked up from the keyring — it must be
   declared in this action's own `action_config.secrets` map (sibling of
   `auth`). This is what makes the store scoped: an action can only
   decrypt secrets it was explicitly configured with a key for, even if
   it knows the secret's name. Actions that need to share one secret
   (e.g. several actions against the same OAuth provider) share the same
   key across their `action_config.secrets` maps — see the `sol-google`
   package's `install.solx` for a worked example (`solx random` generates
   the shared key once at install time).

The precedence is: **inline wins** when present (string or keyring
ref); on inline miss, the **secret-store pointer** is consulted; on
miss, the call fails with a message that names the field. See
[`crate::secrets`](../sol-manager/src/secrets.rs) for the resolver and
[`crate::webhook_auth::secret_field`](../sol-manager/src/webhook_auth.rs)
for the per-field dispatch.

> **Note** — the inline and keyring-reference forms remain supported
> for actions that don't need per-action key scoping (e.g. a single
> action holding its own unshared secret). New actions that share
> secrets across multiple actions, or that want secrets encrypted with
> a key the action config itself controls, should use the `*_secret`
> pointer form instead.

### 2.2 Cache behaviour

Access tokens are cached in-process per `action_name` for the lifetime
of the manager. The cache is keyed by the **action entity name**, not
the URL, so two Webhook actions targeting the same provider share a
token. `bearer` tokens are cached for `ttl_secs` (default 1 h).
`oauth_refresh` tokens are cached using the server-reported `expires_in`
minus a 60-second safety window. `oauth_service_account` tokens are
cached the same way. `oauth_authorization_code` does **not** write to
the cache — the dispatcher intercepts that path and returns the raw
token JSON to the caller without injecting a `Bearer` header.

To force a refresh (e.g. after rotating the underlying secret), call
[`webhook_auth::clear_auth_cache(action_name)`](../sol-manager/src/webhook_auth.rs)
or restart the manager.

### 2.3 Refresh-token rotation

`oauth_refresh` performs RFC 6749 §6 and yields three things on the
2xx path:

* `access_token` — the new access token, written to the cache.
* `expires_in` — TTL used by the cache (default 3600s when omitted).
* `refresh_token` — the provider MAY return a new value here even if
  it did not explicitly rotate.

When the response carries a `refresh_token` that differs from the one
we sent, the dispatcher **persists the new value back to**:

* the `action_config.auth.refresh_token` field, if the action is
  using the inline shape; or
* the keyring account identified by
  `action_config.auth.refresh_token.keyring_account`, if the action
  is using the keyring shape.

See [`persist_rotated_refresh_token`](../sol-manager/src/webhook_auth.rs)
and [`persist_refresh_token_for_action`](../sol-manager/src/dispatcher/webhooks.rs).
Persistence is best-effort and logged at `warn` on failure; the access
token is still returned to the caller so the current request can
succeed even if rotation persistence does not.

For the `oauth_authorization_code` path, the `refresh_token` (if
returned by the provider) is persisted onto a *sibling* action whose
name is `{auth-code action name}--refresh` by default; callers can
override via `params.persist_to` in the action call.

> **Security note (2026-06-29)** — refresh-token persistence is
> opt-out, not opt-in. If your action's `auth.refresh_token` is
> **inline** and you do not want the manager to rewrite the value on
> disk, set `auth.persist_rotated_refresh: false`. Persistent storage
> of rotated tokens is the default because most providers rotate. The
> flag currently applies only to the `oauth_refresh` code path; the
> `oauth_authorization_code` path always persists when a new
> `refresh_token` is returned (callers that need to opt out should
> explicitly avoid storing the response field).

---

## 3. Auth-type semantics

### 3.1 `bearer`

The simplest path. The token is wrapped in an `Authorization: Bearer
<token>` header on every request. No refresh logic, no provider
round-trip. Recommended for first-party API keys with a long lifetime.

### 3.2 `oauth_refresh`

Implements RFC 6749 §6 — exchange a `refresh_token` for a fresh
`access_token` against the provider's `token_uri` using a form-encoded
`POST`. The response is JSON of the shape `{access_token, expires_in?,
token_type, …}`. The cache stores the `access_token` keyed by
`action_name`, refreshing 60 s before the server-reported expiry.

This is the steady-state auth used after a user completes the
authorization-code dance: the action's `auth` block is rewritten to
`oauth_refresh` with the long-lived `refresh_token` from the initial
token response.

### 3.3 `oauth_service_account`

Implements RFC 7521 §5.1 — JWT-bearer grant against the provider's
`token_uri`. The manager signs an RS256 JWT with the action's
`private_key` and the claims `{iss: client_email, scope, aud:
token_uri, iat, exp}` (1-hour validity), then POSTs
`grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer&assertion=…`
form-encoded to `token_uri`. Used for server-to-server providers
(Google service accounts, Workload Identity Federation, etc.) where no
user interaction is required.

> **Note** — the `private_key` is currently read from
> `action_config.auth.private_key`. The TODO markers in `webhook_auth.rs`
> flag a phase-2 migration to the `keyring` crate so that long-lived
> PEMs stop living on disk in JSON. See *Future work* below.

### 3.4 `oauth_authorization_code`

A sentinel — the dispatcher short-circuits
[`dispatch_webhook`](../sol-manager/src/dispatcher/webhooks.rs) and
delegates to [`dispatch_oauth_token_exchange`](../sol-manager/src/dispatcher/webhooks.rs),
which performs the form-encoded `POST` to the action's `fn_name`
(treating the URL as the token endpoint). The action's `params`
become per-call values (`code`, `redirect_uri`); `action_config.auth`
supplies the static fields (`client_id`, `client_secret`, `scope`).
The response JSON is returned untouched as the action result, so the
caller can read `access_token`, `refresh_token`, `expires_in`, etc.
directly and persist them onto the next action's `action_config.auth`.

The `webhook_auth::resolve_auth` resolver returns `Ok(None)` for this
type so no `Authorization: Bearer …` header is injected — the body of
the POST carries the credentials.

---

## 4. The `oauth-loopback` action (authorization-code dance)

The handshake for any redirect-based provider (Google "installed app",
GitHub OAuth Apps, Microsoft identity platform, RFC 8252 §7.3 "native
apps", …) is driven by the [`oauth-loopback`](../sol-manager/src/loopback_control.rs)
action. It is provider-agnostic; provider-specific knowledge lives in
the calling script's `model_instructions` and in the `fn_name` of the
final token-exchange webhook.

### 4.1 The three modes

| Mode | Inputs | Returns |
|---|---|---|
| `start` | `{ port? }` | `{ started, port, redirect_uri, state_value, started_at }` |
| `await_callback` | `{ state_value, timeout_secs? }` | `{ code, state, succeeded }` or `{ error, error_description, succeeded: false }` |
| `stop` | `{ state_value }` | `{ stopped, state_value }` |

`start` binds `127.0.0.1:{port}` (default `8765`), generates a 32-char
hex `state_value`, and registers a `oneshot::Receiver` in the in-process
inbox. The HTTP listener is implemented by
[`sol_server::oauth_loopback`](../sol-server/src/oauth_loopback.rs);
the dispatcher hands it the listener address and a shutdown channel.

`await_callback` removes the receiver from the inbox (so each callback
fires exactly once) and waits for the `GET /callback?code=…&state=…`
hit. On success it returns `{code, state, succeeded: true}`. On a
provider-denied redirect it returns `{error, error_description,
succeeded: false}` with `success: false` on the `ActionExecResult`.

`stop` signals the axum server to shut down gracefully via
`with_graceful_shutdown`, drops the inbox receiver, and removes the
registry entry. Best-effort: missing state values return
`success: false` rather than erroring so cleanup code paths are
idempotent.

### 4.2 `state_value` semantics

`state_value` is the OAuth CSRF token *and* the registry key for the
oneshot. It is generated by [`generate_state_value`](../sol-manager/src/loopback_control.rs)
as 32 hex chars from two `DefaultHasher` calls on `SystemTime::now()`
and `std::process::id()`. This is sufficient for a localhost CSRF
token but not used as a security boundary — the actual security
property is that the redirect is bound to a `state` the action
generated.

> **Open question** — `generate_state_value` currently uses
> `DefaultHasher` (SipHash by default). The function would benefit
> from a CSPRNG so the state is unguessable even to a peer on the
> same host. Consider the `rand` crate in the next iteration.

### 4.3 Sequence diagram (provider-generic)

```
Script (Actions action)
 │
 │ 1. await dispatch.execute("oauth-loopback", {"mode":"start"})
 │◀─── {port, redirect_uri, state_value}
 │
 │ 2. navigate system browser to
 │       <provider-auth-url>?client_id=…&redirect_uri=<redirect_uri>
 │      &state=<state_value>&scope=…
 │
 │ provider user authenticates → provider redirects to redirect_uri
 │
 │ 3. await dispatch.execute("oauth-loopback", {"mode":"await_callback",
 │                                              "state_value":…})
 │◀─── {code, state, succeeded:true}
 │
 │ 4. await dispatch.execute(<token-endpoint-webhook>,
 │                          {code,…,"redirect_uri":…})
 │◀─── {access_token, refresh_token, expires_in, …}
 │
 │ 5. persist refresh_token on the next webhooks' action_config.auth
```

Step 4 is performed by the Webhook whose `auth.type` is set to
`oauth_authorization_code` — its `fn_name` is the provider's
`https://.../token` endpoint. Step 5 is application logic; the
dispatcher makes the persistable token round-trip but does not itself
rewrite the `Action` entity.

### 4.4 Concurrency

The loopback registry is a `OnceLock<Mutex<HashMap<state_value,
LoopbackHandle>>>`, keyed by `state_value`. Multiple OAuth flows can
be in flight at once (e.g. one per provider). The inbox is a separate
`OnceLock<Mutex<HashMap<state_value, oneshot::Receiver<LoopbackResult>>>>`.
Both are process-local — no IPC, no Tauri events — which keeps the
implementation testable and decoupled from the frontend event bus.

---

## 5. URL allowlist (what *can* be called)

Every webhook request — including those with an `auth` block — is
validated by [`validate_webhook_url`](../sol-manager/src/lib.rs)
**after** path-template substitution. The check is additive:

1. If `sol-config.json` declares `allowed_webhook_base_urls`, the URL
   must `starts_with` one of those bases; otherwise the request is
   rejected with a config-driven error message that names the missing
   base.
2. If the action has `ActionLink` rows, the URL must `starts_with`
   one of those bases; otherwise rejected with a similarly verbose
   message.
3. If both are populated, the URL must match **either**. This is the
   common case: packages ship `ActionLink` rows granting the action
   permission to its provider's hosts; admins who need to further
   restrict the action can tighten the global list.
4. If neither is populated, the check is intentionally permissive —
   no allowlist configured means fall-open, matching the pre-allowlist
   Sol behaviour.

> **Substitution-before-validation** — `validate_webhook_url` is
> called against the URL **after** the `path_params` substitution in
> `dispatch_webhook` runs. If the user-controlled substitution could
> produce a URL outside the allowlist, the allowlist wins. Action
> authors should not assume that `path_params` is a hidden back door
> onto unrelated hosts.

---

## 6. The scoped secret store (where secrets live at runtime)

OAuth secrets (`client_id`, `client_secret`, `refresh_token`, `scope`)
are read through the scoped secret store —
[`crate::secrets::get_secret`/`set_secret`](../sol-manager/src/secrets.rs) —
via the `*_secret` fields on `action_config.auth` (§2.1), or for WASM
guests, the `get secret`/`set secret` built-in actions (backed by the
`secrets` WIT interface in
[`sol-actions/wit/sol-actions.wit`](../sol-actions/wit/sol-actions.wit)).

Each secret is:

1. Encrypted with AES-256-GCM using a 32-byte key, then
2. Stored in the OS credential manager (DPAPI / Keychain / Secret
   Service) under the secret's name, via the `keyring` crate.

The encryption key is **not** looked up automatically — the caller
(the currently-executing action) must have that key declared in its
own `action_config.secrets` map, keyed by the secret's name. This is
what makes the store *scoped*: unlike a shared global map, an action
that doesn't have a key configured for a given secret name gets
`None`/an error, even if it knows the name and can otherwise reach the
`get secret` action. Actions that legitimately need to share a secret
(e.g. several actions calling the same OAuth provider) share the same
key across their `action_config.secrets` maps, generated once via
`solx random` at install time — see the `sol-google` package's
`install.solx` for a worked example.

There used to be a single flat `wasm_env` store (`sol-config.json`'s
`wasm_env` block, read via `wasm_host::get_wasm_env_value`) that every
trusted WASM action could read and write with no per-action scoping at
all — the `secrets::` key prefix it used was a naming convention only,
not an access-control mechanism. That store still exists for
non-secret config (feature flags, non-sensitive IDs via `get
environment variable`/`set environment variable`), but secrets must no
longer be placed there; the scoped secret store above is the only
supported home for them.

### 6.1 OS credential manager (the keyring path) — for unshared, single-action secrets

This shape is unscoped — any action holding the same
`{keyring_service, keyring_account}` pair can read it, since there is no
action-config-declared key gating access (unlike the `*_secret` pointer
form in §6). Prefer it for a secret that belongs to exactly one action
and is never shared; prefer the `*_secret` scoped store (§6) when
multiple actions need the same secret, or when you want the
action-config-declared-key access control:

```json
{
  "type": "oauth_service_account",
  "client_email": "sa@project.iam.gserviceaccount.com",
  "private_key": {
    "keyring_service": "sol",
    "keyring_account": "google-svc-pk"
  },
  "token_uri": "https://oauth2.googleapis.com/token",
  "scope": "https://www.googleapis.com/auth/cloud-platform"
}
```

The dispatcher resolves the value at call time via
[`keyring::Entry::new(service, account).get_password()`](https://docs.rs/keyring).
Keys are **read** on every call — there is no in-memory cache for
secrets, only for the access tokens they unlock.

#### Setup commands

| OS | First-time setup | Lookup |
|---|---|---|
| Windows | `cmdkey /generic:sol\<account> /user:ignored /pass:"<value>"` | Credential Manager → Windows Credentials |
| macOS | `security add-generic-password -s sol -a <account> -w "<value>"` | Keychain Access |
| Linux | `secret-tool store service sol account <account>` (libsecret) | GNOME Secrets (`gnome-secret-tool lookup`) |

To script rotation, use the corresponding write commands:
* Windows: `cmdkey /generic:sol\<account> /user:ignored /pass:"<new-value>"`
* macOS: `security add-generic-password -s sol -a <account> -w "<new-value>" -U`
* Linux: `secret-tool store service sol account <account>`

Sol detects the platform at build time via the `keyring` crate's
`windows-native`, `apple-native`, and `sync-secret-service` features
(see [sol-manager/Cargo.toml](../sol-manager/Cargo.toml)).

#### Refresh-token rotation through the keyring

When `oauth_refresh` returns a new `refresh_token`, the
[`persist_rotated_refresh_token`](../sol-manager/src/webhook_auth.rs)
function writes the new value through `keyring::Entry::set_password`,
so the rotation is durable and benefits from the OS credential manager
encrypting the new value at rest.

### 6.2 What we don't (yet) persist securely

* `action_config.auth.refresh_token` and `private_key` can still be
  authored *inline* in JSON; the resolver accepts inline, keyring, and
  scoped secret-store shapes side-by-side. Migrating an existing
  action's inline secret to the scoped store is: generate a key
  (`solx random`), add it under `action_config.secrets.<NAME>`, change
  `"refresh_token": "abc"` to `"refresh_token_secret": "<NAME>"`, and
  call `set secret` (or the `solx set-secret`-adjacent flow) once with
  the value.
* The scoped secret store's access control lives entirely in
  `action_config.secrets` — if that JSON blob itself becomes readable
  to an unintended party (e.g. via an over-broad `entity-get` on the
  `Action` entity), the key leaks along with it. The store's threat
  model is "prevents an unrelated action from decrypting a secret it
  wasn't configured for," not "prevents someone who can already read
  arbitrary `action_config` blobs from the DB."

These are the fields where plaintext-on-disk is still *permitted*
(though not *recommended*) for backward compatibility with actions
authored before the scoped store existed. New actions should prefer
the `*_secret` pointer form (§2.1, §6).

---

## 7. Error handling and observability

### 7.1 Error surfaces

| Failure | Where it surfaces | Shape |
|---|---|---|
| Token endpoint non-2xx | action result `success: false` + `result.error` | provider response body |
| Missing required `auth` field | action result `success: false` + `message` | human-readable string |
| Secret not set / no key configured | `Err("secret '…' is not set")` / `Err("… has no key configured for it")` | propagates to caller |
| Loopback port already in use | `start` returns `success: false` + `result.error = "addr-in-use"` etc. | direct axum error |
| Loopback timeout | `await_callback` returns `success: false` + message | `oauth callback timed out after Ns` |
| Loopback stopped before callback fires | `await_callback` returns `Err("loopback for state '…' was stopped before the callback arrived")` | sender dropped |
| URL not in allowlist | `validate_webhook_url` returns `Err` with the missing base | propagated to caller |

All HTTP errors include the status code and the (truncated) response
body in the `message` field of `ActionExecResult`. Token endpoints are
not excluded from this convention — debug-grade token-response text is
returned to the caller.

### 7.2 Logging

The dispatcher logs every webhook dispatch at `INFO` level with the
method, URL host (not path), action name, and status code. Token
endpoint responses are logged at `INFO` with the `token_uri` host and
status but the body is **not** logged (it carries secrets). Bearer /
refresh-token values are never logged. This is a property of the
existing `webhook` log lines in [`commands.rs`-adjacent code](../sol-manager/src/dispatcher/commands.rs);
see the dispatcher entry point if this contract needs to change.

---

## 8. Code review findings (2026-06-29)

Walk-through of the current implementation, with the issues that
warrant follow-up. Severity is the author's judgment.

### 8.1 `state_value` is not from a CSPRNG — *low* (security hardening) — **FIXED 2026-06-29**

[`generate_state_value`](../sol-manager/src/loopback_control.rs)
previously used `DefaultHasher`, which on most Rust targets is
SipHash. SipHash is fast and good for hash tables; it is not a CSPRNG,
and the resulting output was reconstructible by an attacker who could
guess the inputs. The implementation now uses `rand::thread_rng()`
(a CSPRNG on all supported platforms) and emits 16 random bytes as 32
lower-hex characters. Test [`generate_state_value_is_unique_and_hex`]
still passes.

### 8.2 Cache is per-action-name; clearing it is manual — *low*

[`AUTH_CACHE`](../sol-manager/src/webhook_auth.rs#L26) is keyed by
`action_name`, not by `auth` block content. Rotating the bearer or
refresh-token requires calling [`clear_auth_cache`](../sol-manager/src/webhook_auth.rs#L36)
or restarting the manager. There is no automatic invalidation on
config change. A reasonable enhancement is to hash the relevant
`auth` fields and use the hash as part of the cache key, so a config
edit forces a refresh automatically.

### 8.3 RSA private key persisted in JSON — *medium* — **FIXED 2026-06-29**

`webhook_auth.rs` now reads `private_key` through the
[`secrets::resolve`](../sol-manager/src/secrets.rs) pipeline, which
accepts either an inline string **or** a keyring reference
(`{"keyring_service": "...", "keyring_account": "..."}`). The
keyring shape routes the PEM through the OS credential manager
(DPAPI / Keychain / libsecret). Action authors can now configure
the PEM outside of `action_config.auth` entirely. The same change
also covered `client_secret` and `refresh_token` for the OAuth
grant types. New module: [`crate::secrets`](../sol-manager/src/secrets.rs).

### 8.4 Token endpoint body is surfaced in errors — *low*

`dispatch_webhook` returns the raw provider response on non-2xx, and
`oauth_refresh` / `oauth_service_account` do the same. OAuth error
bodies can carry partial credentials (especially when the
`client_secret` is wrong and the provider echoes it back) or PII
scopes. A surgical fix is to log the body at DEBUG, return only the
status + a short error message at INFO/surface.

### 8.5 Cache expiry is read-once, not re-checked — *low*

`cache_get` checks `expires_at > Instant::now()` but
[`resolve_auth`](../sol-manager/src/webhook_auth.rs#L96) only reaches
the cache fast path when `cache_get` returns a value. Once expired, the
cache miss is fine. However, the racing pattern where one caller is
mid-refresh while another reads a slightly-expired entry could result
in two token endpoints calls in quick succession. A `tokio::sync::Mutex`
guarding the refresh would dedupe this. Impact is small.

### 8.6 `oauth_authorization_code` form-body order — *informational*

In [`dispatch_oauth_token_exchange`](../sol-manager/src/dispatcher/webhooks.rs#L228),
the form body is assembled in this order: `grant_type` → `auth.*`
(static fields, skipping `type` and `*_secret`) → secret-store
fallbacks → `params` (per-call). RFC 6749 §4.1.3 only requires the
field names, not their order, but some providers (and some proxies in
front of them) have been observed to be picky about duplicates. The
current code correctly deduplicates via the `already_present` check
around the secret-store fallback loop. **Verified correct**, but worth
re-checking when adding a
new provider.

### 8.7 `path_params` substitution is intentionally not URL-encoded — *informational*

In [webhooks.rs:53-56](../sol-manager/src/dispatcher/webhooks.rs#L53-L56)
the comment explicitly says "We don't URL-encode here — the action
author chose the template." This is the right trade-off (the action
author generally wants to encode differently for path vs query vs
header), but it shifts responsibility to the action author. New
actions should be reviewed for accidental double-encoding.

### 8.8 `validate_webhook_url` reads `sol-config.json` from disk on
every call — *low* (perf) — **FIXED 2026-06-29**

The function previously read `bootstrap_config_path()`
synchronously on every webhook dispatch. The mtime-keyed
[`read_allowlist_cached`](../sol-manager/src/lib.rs) wrapper now
caches the parsed `SolConfig` and only re-reads when the file
changes. Tests that mutate the on-disk file should call
[`invalidate_webhook_allowlist_cache`](../sol-manager/src/lib.rs)
to force a re-read.

---

## 9. Future work

1. **Keyring migration for `private_key` and `refresh_token`.** — **DONE 2026-06-29**
   Accept both inline string and keyring reference; the new
   `crate::secrets` resolver centralises the lookup. Existing actions
   do not need to change.
2. **CSPRNG `state_value`.** — **DONE 2026-06-29**
   `generate_state_value` now uses `rand::thread_rng()`. Removes §8.1.
3. **Refresh-token rotation detection.** — **DONE 2026-06-29**
   Both `oauth_refresh` (per-action) and `oauth_authorization_code`
   (sibling action) persist any rotated `refresh_token` back to disk or
   keyring, honouring the storage shape that was originally configured.
   See §2.3.
4. **Allowlist config caching.** — **DONE 2026-06-29**
   `read_allowlist_cached` memoises the parsed `sol-config.json` and
   only re-reads on mtime change.
5. **Token-endpoint response scrubbing.** Redact the body of OAuth error
   responses before they propagate to the caller. Implement
   `OAuthErrorSanitizer` in `webhook_auth.rs`.
6. **Refresh-token rotation logging at INFO.** The persistence path
   logs at `warn`; success should also log at `info` so operators can
   see rotation events in the action trace.
7. **Single-flight refresh.** Two concurrent callers can race through
   the refresh-token endpoint. A `tokio::sync::Mutex` per
   `action_name` would dedupe.

---

## 10. File map

| File | Symbol | Role |
|---|---|---|
| [sol-manager/src/secrets.rs](../sol-manager/src/secrets.rs) | `resolve`, `resolve_in_field`, `load_keyring_with_test_override` | inline/keyring precedence for secret-bearing fields |
| [sol-manager/src/secrets.rs](../sol-manager/src/secrets.rs) | `get_secret`, `set_secret`, `delete_secret` | AES-256-GCM-encrypted, keyring-backed scoped secret store (`*_secret` indirection) |
| [sol-manager/src/webhook_auth.rs](../sol-manager/src/webhook_auth.rs) | `resolve_auth` | bearer + refresh + service-account resolver |
| [sol-manager/src/webhook_auth.rs](../sol-manager/src/webhook_auth.rs) | `clear_auth_cache` | force token refresh |
| [sol-manager/src/webhook_auth.rs](../sol-manager/src/webhook_auth.rs) | `persist_rotated_refresh_token` | write a new `refresh_token` back to `action_config.auth` (inline) or the OS credential manager (keyring) |
| [sol-manager/src/loopback_control.rs](../sol-manager/src/loopback_control.rs) | `try_execute` | `oauth-loopback` action dispatcher (3 modes) |
| [sol-manager/src/loopback_control.rs](../sol-manager/src/loopback_control.rs) | `await_callback` | inbox lookup by `state_value` |
| [sol-manager/src/loopback_control.rs](../sol-manager/src/loopback_control.rs) | `generate_state_value` | CSPRNG-derived CSRF state (32 hex chars) |
| [sol-manager/src/dispatcher/webhooks.rs](../sol-manager/src/dispatcher/webhooks.rs) | `dispatch_webhook` | regular webhook + path-template substitution |
| [sol-manager/src/dispatcher/webhooks.rs](../sol-manager/src/dispatcher/webhooks.rs) | `dispatch_oauth_token_exchange` | `authorization_code` form-encoded POST |
| [sol-manager/src/dispatcher/webhooks.rs](../sol-manager/src/dispatcher/webhooks.rs) | `persist_refresh_token_for_action` | sibling-action persistence after an auth-code exchange |
| [sol-manager/src/lib.rs](../sol-manager/src/lib.rs) | `validate_webhook_url`, `read_allowlist_cached` | URL allowlist (global ∪ per-action), mtime-cached |
| [sol-server/src/oauth_loopback.rs](../sol-server/src/oauth_loopback.rs) | `serve_loopback_with_shutdown` | axum listener (RFC 6749 §4.1, RFC 8252 §7.3) |
| [sol-manager/src/wasm_host.rs](../sol-manager/src/wasm_host.rs) | `get_wasm_env_value` / `set_wasm_env` | non-secret env-var store (shared with WASM guests) |
| [sol-manager/src/wasm_host.rs](../sol-manager/src/wasm_host.rs) | `sol::actions::secrets::Host` impl (`get_secret`/`set_secret`) | per-action-scoped secret access for WASM guests |
| [sol-manager/src/browser_actions.rs](../sol-manager/src/browser_actions.rs) | `open-system-browser` action | launches the user-visible OAuth browser |

---

## 11. Quick recipes

### 11.1 "I want to add OAuth to a Webhook action"

1. Capture the redirect URI — set `port` to an unused high port, or
   accept the default `8765`. Plan to call
   `oauth-loopback{mode: "start"}` first.
2. Compose the provider auth URL with `client_id`, `redirect_uri`,
   `state`, and `scope` as documented by the provider, then open it in
   the system browser via the `open-system-browser` action.
3. After the user authenticates, call
   `oauth-loopback{mode: "await_callback", state_value: …}` to capture
   `code` + `state`.
4. Call a webhook action with `auth.type: "oauth_authorization_code"`,
   `fn_name: "<provider token URI>"`, and `params: {code, redirect_uri,
   state}` to fetch `access_token` + `refresh_token`.
5. Persist the `refresh_token` onto a new webhook action's
   `auth.refresh_token` (inline/keyring) or `auth.refresh_token_secret`
   (scoped secret store — add the matching key to that action's
   `action_config.secrets` map first) and switch its `auth.type` to
   `oauth_refresh` for steady-state calls.

### 11.2 "I want to add OAuth to a WASM action"

Generate a key once (`solx random`), add it under
`action_config.secrets.<NAME>` on every action that needs the secret,
then call `set secret`/`get secret` (or the `secrets` WIT import
directly from a trusted WASM guest) using that name. The WASM action
itself performs the HTTP call using whichever Rust HTTP client it
bundles. See the `sol-google` package's `install.solx` for a worked
example of sharing one key across several actions.

### 11.3 "I want to test an OAuth flow without a real provider"

Install a fake secret backend with
[`secrets::set_secret_backend_for_tests`](../sol-manager/src/secrets.rs)
(covers `get_secret`/`set_secret`/`delete_secret` without touching the
real OS credential manager), or seed `wasm_host::set_wasm_env` for
non-secret config values. The `oauth-loopback` `start` / `stop` modes
are unit-tested in
[`loopback_control.rs`](../sol-manager/src/loopback_control.rs) with a
manual `oneshot` injection that bypasses the real axum server — extend
that pattern for new assertions. The
[`secrets::set_secret_loader_for_tests`](../sol-manager/src/secrets.rs)
seam lets the inline/keyring-ref resolver (`resolve`) be exercised
without an OS credential manager.

---

## 12. Documented issues tracker

A single-source-of-truth for the current state of the
OAuth-related findings from §8, the rotation behaviour from §2.3,
and the future-work list from §9. The intent is to keep this
section in sync whenever the code changes — anyone touching
`action_config.auth` or the dispatch pipeline should re-read this
list to make sure nothing has been silently marked FIXED or DROPPED.

### 12.1 Findings — fixed (2026-06-29)

| # | Finding | Resolution | Tests |
|---|---|---|---|
| 8.1 | `state_value` was SipHash-derived (not a CSPRNG) | `generate_state_value` now uses `rand::thread_rng()` | `loopback_control::tests::generate_state_value_is_unique_and_hex` (10 pass) |
| 8.3 | Long-lived secrets (`private_key`, `client_secret`, `refresh_token`) lived in plaintext JSON | New `crate::secrets` resolver; keyring shape routes through the OS credential manager | `secrets::tests` (8 pass), `webhook_auth::tests::keyring_shape_resolves_via_test_loader` |
| 8.8 | `validate_webhook_url` re-read `sol-config.json` from disk on every call | `read_allowlist_cached` memoises by file mtime | covered by `webhooks.rs` allowlist tests; `invalidate_webhook_allowlist_cache` exposed for tests |
| §9.1 | Keyring migration for `private_key` / `refresh_token` | DONE — see §6.1 setup commands; keyring ref shape accepted in every secret-bearing field | `secrets::tests` + 3 new keyring/auth tests |
| §9.2 | CSPRNG `state_value` | DONE — see 8.1 | — |
| §9.3 | Refresh-token rotation detection | DONE — both `oauth_refresh` (per-action) and `oauth_authorization_code` (sibling) persist rotated tokens, honouring the originally-configured storage shape. **Opt-out**: `auth.persist_rotated_refresh: false` | `webhook_auth::tests::persist_rotated_refresh_flag_defaults_true` |
| §9.4 | Allowlist config caching | DONE — see 8.8 | — |

### 12.2 Findings — open (2026-06-29)

The following items from §8 / §9 remain open.  Severity is the
original assessment; cost-to-fix is the author's estimate.

| # | Finding | Severity | Cost | Notes |
|---|---|---|---|---|
| 8.2 | Cache key is `action_name`, not the `auth` block content — secret rotation requires manual `clear_auth_cache` | low | medium | Hashing the relevant `auth` fields into the cache key would solve it but also slightly complicate the cache-hit fast path.  Acceptable as-is; consider if "rotate the API key without restart" becomes a real use case. |
| 8.4 | Token-endpoint error body is surfaced to callers verbatim | low | small | Errors can echo partial credentials back. A minimal `OAuthErrorSanitizer` in `webhook_auth.rs` would return a short message at INFO and log the body at DEBUG. |
| 8.5 | Concurrent refresh callers can race through the token endpoint | low | small | A `tokio::sync::Mutex` per `action_name` would dedupe.  Most OAuth providers handle this gracefully (idempotent POST). |
| 8.6 | `oauth_authorization_code` form-body order | informational | — | Verified correct via `already_present` dedup loop.  Re-verify when adding a new provider. |
| 8.7 | `path_params` substitution is intentionally not URL-encoded | informational | — | Action authors own encoding.  Review during code review for new actions. |
| §9.5 | Token-endpoint error body scrubbing | low | small | (Same as 8.4.) |
| §9.6 | Refresh-token rotation logging at INFO | low | trivial | Persistence path logs at `warn` on failure; success should also log at `info`. |
| §9.7 | Single-flight refresh | low | small | (Same as 8.5.) |

### 12.3 Behaviour tracker — `action_config.auth` secrets

A compact reference for the three resolution shapes supported per
field:

| Field | Inline string | Keyring ref | `*_secret` pointer (scoped store) |
|---|---|---|---|
| `bearer.token` | ✅ | ✅ | ✅ (`token_secret`) |
| `bearer.client_id` / `client_secret` etc. | n/a | n/a | n/a |
| `oauth_refresh.client_id` | ✅ | n/a | ✅ (`client_id_secret`) |
| `oauth_refresh.client_secret` | ✅ | ✅ | ✅ (`client_secret_secret`) |
| `oauth_refresh.refresh_token` | ✅ | ✅ | ✅ (`refresh_token_secret`) |
| `oauth_service_account.client_email` | ✅ (required) | n/a | n/a |
| `oauth_service_account.private_key` | ✅ | ✅ | ✅ (`private_key_secret`) |
| `oauth_authorization_code.client_id` | ✅ | n/a | ✅ (`client_id_secret`) |
| `oauth_authorization_code.client_secret` | ✅ | ✅ | ✅ (`client_secret_secret`) |
| `oauth_authorization_code.scope` | ✅ | n/a | ✅ (`scope_secret`) |

(`✅` = supported by the resolver; `n/a` = the field does not normally
live in that slot — e.g. `client_id` is a public identifier, not a
secret, and keyring/secret-store storage is not warranted. Every
`*_secret` pointer requires the matching name to be declared in that
action's own `action_config.secrets` map.)

### 12.4 How to keep this section accurate

When you fix any item above:

1. Move the row from §12.2 to §12.1 with the date and a one-line
   description of the resolution.
2. Update §2.3 / §6.1 / §8 / §9 in this document to match.
3. Add a test under the `tests` mod of the relevant file
   ([secrets.rs](../sol-manager/src/secrets.rs),
   [webhook_auth.rs](../sol-manager/src/webhook_auth.rs), or
   [loopback_control.rs](../sol-manager/src/loopback_control.rs))
   that fails before the fix and passes after.
