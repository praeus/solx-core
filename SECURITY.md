# Security Policy

## Reporting a vulnerability

Please report security issues privately through GitHub's **Report a
vulnerability** button on this repository's Security tab, rather than opening
a public issue.

Include what you did, what happened, and what you expected. A proof of
concept helps but isn't required. Expect an initial response within a week.

solx is early-stage and pre-1.0, maintained by one person. There is no
embargo process, no CVE pipeline, and no backport policy — fixes land on
`main`. Only `main` is supported.

## Threat model

solx exists to execute things on request: shell commands, remote HTTP calls,
and WASM components, driven by a CLI, an HTTP server, or an LLM through MCP.
That makes the boundaries worth stating precisely.

**Assume a solx instance runs with your full user privileges.** It is not a
sandbox for untrusted operators, and it is not a multi-tenant system. The
controls below limit what *content* — a model, a package, a WASM guest — can
reach. They do not limit what a person holding your `server_token` or your
shell can do.

### What is enforced

**Command actions cannot hold an executable string.** A `command` action's
`fn_name` is a key into `command_actions` in `solx-config.json`, not a
command line. The lookup hard-errors if the key isn't registered — including
when the allowlist is entirely unset. The actions database can therefore only
ever hold a *pointer* to an out-of-band, human-approved definition. Where a
registered definition specifies `cwd`, that wins over the invoking action's
own config: the allowlist author decides where an approved command runs.

**Webhook actions are prefix-allowlisted.** A `webhook` action's resolved URL
must start with an entry in `allowed_webhook_base_urls`. Denied attempts are
rejected before the console's start log is written, so a refused call leaves
no trace beyond the error.

Both allowlists are **deny-by-default**. An empty or absent allowlist denies
everything rather than allowing everything.

**Guests cannot grant themselves execution.** `entity_save_action` and
`entity_delete_action` refuse to create, modify, or delete `command` and
`webhook` actions. That built-in is the only route to action creation
available to an MCP client, a WASM guest, or a `.solx` script, since MCP has
no separate CRUD layer. The check reads the *stored* row, not just the
incoming payload, because `save` is a merge-upsert — otherwise a payload
carrying only `fn_name` could silently repoint an existing command action.

The CLI reaches `ActionManager::save` directly and is unaffected; package
installs depend on that. The HTTP `/actions/save` route is also unrestricted,
deliberately: anyone holding `server_token` can already execute command
actions, so gating it there would buy nothing and would break the CLI in
remote mode.

**Packages declare their grants.** A package's `package.json` lists the
`command_actions` and `allowed_webhook_base_urls` it needs. Install grants
exactly those and records them on the installed-package row; uninstall
revokes exactly those, unless another installed package also declares the
same key or prefix. A collision between packages is a logged warning, not a
silent overwrite.

**Secrets are scoped to the calling action.** `get_secret`/`set_secret`
resolve only against the *calling* action's own `action_config.secrets` map.
The caller is threaded host-side and never serialized, so a client cannot
spoof it.

**Secrets are masked on read.** `ActionManager::get` and `list` redact
`action_config.secrets` and `action_config.auth`, so keys never cross the
HTTP boundary or reach an MCP client. Execution reads through an unmasked
path and still sees real values. `save` runs the inverse merge, restoring
anything echoed back as `"***"` from the stored row — so a fetch → edit →
save round trip cannot silently destroy a key.

**WASM guests are sandboxed by construction.** The `custom-action` world
imports only action dispatch, artifact reads, and logging.
There is no direct database, filesystem, network, or system access; every
sensitive operation must go through a named action, where the restrictions
above apply.

**The file store rejects traversal.** Absolute paths and `../` escapes out of
the files root are refused.

**The server binds loopback only.** `solx-server` listens on `127.0.0.1` and
gates every route behind a bearer token generated on first start.

### What is not enforced

These are known gaps, not oversights. They're accepted for now because
everything running through solx today is trusted by construction.

- **Anything saved to the actions database is executable.** There is no
  approval or confirmation step at execution time.
- **Callers are undifferentiated.** solx tracks exactly one kind of caller
  identity — which action invoked another — and uses it only to scope
  secrets. CLI, MCP, and HTTP callers are all simply "not an action." There
  is no permission system mapping a caller to the entities it may reach.
- **No package signing or verification.** `install-package` will run the
  `install.solx` of any local directory you point it at, and grant whatever
  its manifest declares.
- **Artifact reads are unrestricted.** A WASM guest can read any path in the
  file store; there is no per-artifact permission check.
- **Loopback capabilities are bearer-style.** OAuth loopbacks, action
  consoles, and HTTP streams are reachable by anyone who knows the
  unguessable id, without a further caller check.
- **`solx-server` has no rate limiting, audit log, or token rotation.**

See [docs/future-security-enhancements.md](docs/future-security-enhancements.md)
for the reasoning behind each and the intended direction.

## Running solx safely

- **Keep the allowlists narrow.** Every entry in `command_actions` is a shell
  command something can trigger. Register the specific commands you need, not
  wrappers that take arbitrary arguments.
- **Read a package's manifest before installing it.** The `command_actions`
  and `allowed_webhook_base_urls` blocks are a complete list of the shell
  access and outbound hosts it's asking for.
- **Scope MCP catalogues.** `SOLX_MCP_PATH_PREFIX` limits an MCP instance to
  one path subtree. Give a model the smallest catalogue that does its job
  rather than every action you own.
- **Treat `server_token` as a credential.** It is full access to the
  instance, including command execution. Don't commit it, and don't put it in
  a shared config file.
- **Don't expose `solx-server` beyond loopback.** If you tunnel or proxy it,
  you have made a shell-execution endpoint reachable from wherever the tunnel
  terminates. Put real authentication in front of it.
- **Prefer WASM for third-party logic.** The `custom-action` world is the
  only genuinely sandboxed dispatch kind.
