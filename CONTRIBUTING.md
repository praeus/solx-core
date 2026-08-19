# Contributing to solx

Thanks for taking a look. solx is early and pre-1.0 — interfaces still shift,
and a conversation before a large change will save you rework.

## Before you start

- **Small fixes** (bugs, docs, tests): just open a PR.
- **Anything that changes a trait in `solx-surface`, adds an action to the
  `/builtin` catalogue, or touches the security posture**: open an issue
  first. Those ripple across every surface — CLI, HTTP, MCP, the JS bindings,
  and the web UI — and the right shape is usually worth discussing before
  code.
- **Security issues**: don't open a public issue. See [SECURITY.md](SECURITY.md).

## Build and test

Rust stable, edition 2021. No pinned toolchain.

```sh
cargo build
cargo test --workspace          # 323 tests; all should pass
cargo fmt --all
cargo clippy --workspace --all-targets
```

There's an end-to-end CLI suite that exercises each action kind against a
throwaway appdata directory:

```sh
bash solx-cli/examples/run-all.sh
```

It builds the CLI once, points `SOLX_APPDATA_DIR` at a temp dir, and runs
every numbered script. Adding a numbered script here is the best way to cover
a new action kind or dispatch path.

WASM work needs the component target:

```sh
rustup target add wasm32-wasip2
```

## Where things live

`solx-surface` is the seam. It holds the entity DTOs, the error type, the
wire types, and the four manager traits (`TypeManager`, `FileStore`,
`DocManager`, `ActionManager`) — and it stays dependency-light. Runtime
dependencies (tokio, libsql, tantivy, reqwest) belong in implementation
crates, never in `solx-surface`.

Everything else is either an implementation of those traits (`solx-types`,
`solx-files`, `solx-docs`, `solx-actions`, `solx-client`) or a consumer of
them (`solx-cli`, `solx-server`, `solx-mcp`, `solx-manager`). If you're
adding a capability, work out which side of that line it's on first.

The README's crate table is the short version;
[docs/design-and-progress.md](docs/design-and-progress.md) is the long one.

## Conventions

**Match the surrounding code.** This codebase leans on module-level doc
comments that explain *why* a thing is shaped the way it is, often citing the
alternative that was rejected. That's deliberate — keep it up. A comment
explaining what a line does is usually noise; one explaining why it isn't the
obvious thing is usually load-bearing.

**Keep the four traits in sync across surfaces.** A method added to
`ActionManager` needs the local impl, the `solx-client` remote impl, the
`solx-server` route, and — if it's user-facing — the CLI. The compiler will
find the first three.

**Prefer an action over a special case.** New capability usually belongs in
the `/builtin` catalogue as an action, not as a new tool type or a new HTTP
route. That's what makes it reachable from the CLI, HTTP, MCP, scripts, and
WASM guests at once. Adding a row to `solx-actions/src/seed.rs` is often the
whole change.

**Workspace dependencies go in the root `Cargo.toml`.** Crates reference them
with `workspace = true` so versions stay aligned.

## Tests

Aim for a test at the level the behavior lives at. Most of the suite is unit
tests colocated in `src/`; integration tests that need a real server or a
real MCP client live in `tests/`.

Tests must not touch the real appdata directory — use `tempfile::tempdir()`
and `App::build_in(path)`. Anything that shells out, hits the network, or
needs an external binary should either be avoided or gated so a plain
`cargo test --workspace` stays green on a clean machine.

If you're fixing a bug, a test that fails before your change is the most
useful thing in the PR.

## Pull requests

- One logical change per PR.
- Say what problem it solves, not just what it does.
- Note anything you couldn't verify, or any assumption you made. That's more
  useful than a confident summary.
- If it changes behavior that's documented in `README.md`, `SECURITY.md`, or
  `docs/`, update those in the same PR. Stale docs are the failure mode this
  project is most prone to.

## License

By contributing you agree that your contributions are licensed under
MIT OR Apache-2.0, the same as the project.
