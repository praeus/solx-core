//! `solx` — the command-line interface over the solx core crates.
//!
//! Verbs: `save` (upsert), `get`, `delete`, `exec` (actions), `list`, `search`,
//! plus `script` and package management. Entities: `doc`, `action`, `type`,
//! `file`. The CLI constructs the local manager impls and codes against the
//! `solx-surface` traits, so a future client/server can slot in unchanged.

use std::io::Read;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use base64::Engine as _;
use clap::{Parser, Subcommand, ValueEnum};
use serde_json::{Map, Value};

use solx_manager::App;
use solx_scripts::{execute_script, CommandRunner};
use solx_surface::entities::{ActionInput, DocumentInput, TypeInput};
use solx_surface::error::SolxError;
use solx_surface::managers::Solx;
use solx_surface::path::{full_ref, split_ref};
use solx_surface::query::{ListOptions, SearchQuery};

#[derive(Parser)]
#[command(name = "solx", about = "Structured documents and extensible actions via CLI", version)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum Entity {
    Doc,
    Action,
    Type,
    File,
}

#[derive(Subcommand)]
enum Commands {
    /// Create or update an entity (upsert).
    Save {
        entity: Entity,
        /// Full reference `/path/name` (for files: relative path under the files root).
        reference: String,
        /// JSON body (overrides piped/stdin input).
        #[arg(long, short = 'j')]
        json: Option<String>,
        /// For documents: the type reference (`/types/.../Name`).
        #[arg(long = "type")]
        type_ref: Option<String>,
        /// For files: path to the local file whose bytes to store.
        #[arg(long)]
        file: Option<String>,
    },
    /// Fetch an entity.
    Get { entity: Entity, reference: String },
    /// Delete an entity.
    Delete {
        entity: Entity,
        reference: String,
        /// Treat a missing entity as success instead of an error. Teardown
        /// scripts (a package's `uninstall.solx`) run statement-by-statement
        /// and abort on the first failure, so without this a single entity an
        /// older package version never registered leaves the package
        /// half-torn-down.
        #[arg(long = "if-exists")]
        if_exists: bool,
    },
    /// Execute an action.
    Exec {
        /// Action reference `/path/name`.
        reference: String,
        #[arg(long, short = 'j')]
        json: Option<String>,
        /// Don't render the action's console live to stderr while it runs.
        /// stdout's JSON result is unaffected either way — rendering never
        /// touches stdout, so `solx exec ... | jq` works identically with
        /// or without this flag.
        #[arg(long)]
        no_console: bool,
    },
    /// List entities with pagination and an optional path facet.
    List {
        entity: Entity,
        #[arg(long)]
        path: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        offset: Option<usize>,
    },
    /// Full-text + faceted search over documents.
    Search {
        query: String,
        #[arg(long)]
        path: Option<String>,
        #[arg(long = "type")]
        type_ref: Option<String>,
        /// Restrict to documents that link to this target (its full
        /// reference, e.g. as returned by `solx get doc`).
        #[arg(long = "linked-to")]
        linked_to: Option<String>,
        #[arg(long)]
        limit: Option<usize>,
        #[arg(long)]
        offset: Option<usize>,
    },
    /// Run a solx script (`;`/`|`/`$var`).
    Script {
        #[arg(short = 'e', long)]
        expr: Option<String>,
        #[arg(short = 'f', long)]
        file: Option<String>,
    },
    /// Install a package directory (solx-package.json + install.solx).
    InstallPackage { path: String },
    /// Uninstall a package by name.
    UninstallPackage { name: String },
    /// List installed packages.
    ListPackages,
    /// Emit a JSON value (handy as a pipeline source in scripts).
    Json { value: String },
    /// Emit a raw string literal (no JSON parsing) — the multi-line string
    /// primitive the language otherwise lacks.
    Str { value: String },
    /// JSON-encode a string value (surrounding quotes + escapes) so it can
    /// be substituted into a JSON payload.
    Escape { value: String },
    /// Generate a base64-encoded random key (for secrets encryption).
    Random {
        /// Number of random bytes (default 32, for AES-256).
        #[arg(default_value = "32")]
        bytes: usize,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    // Writer is stderr, not the default stdout — stdout is the single JSON
    // result `solx exec ... | jq` depends on, same reasoning as solx-mcp
    // (whose stdout is its JSON-RPC channel). Without this, every
    // `tracing::*` call in solx-core was silently dropped under the CLI —
    // the primary development surface had no logging at all.
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .with_writer(std::io::stderr)
        .with_ansi(false)
        .init();

    let cli = Cli::parse();
    let app = build_app().await?;
    let result = run_command(&app, cli.command, None).await?;
    match result {
        Value::String(s) => println!("{s}"),
        other => println!("{}", serde_json::to_string_pretty(&other)?),
    }
    Ok(())
}

fn to_anyhow(e: SolxError) -> anyhow::Error {
    anyhow!(e.to_string())
}

/// Build the `App`, auto-detecting a locally-reachable `solx-server` before
/// falling back to opening local storage directly.
///
/// Without this, every CLI invocation opens the local libsql storage
/// itself (`App::build()`'s default when no `server_url` is configured),
/// racing a `solx-server` already running against the same appdata dir for
/// the same database files. And with no server running at all, any spawned Command action
/// that talks to `solx-server` over HTTP for the file store (e.g.
/// `solx-quickjs`'s `build-javascript-action`, via
/// `solx-package-lib::ServerConfig`) has nothing to reach.
///
/// So: an explicit `server_url` (env or config) is honored unchanged. With
/// none set, probe the default local port — if something's already
/// listening there, wire remote against it instead of touching local
/// storage. If nothing's listening, build local as today and additionally
/// serve this same `App` over HTTP for the rest of this process's
/// lifetime, so a spawned child still has a server to reach.
async fn build_app() -> Result<Arc<App>> {
    let config = solx_config::ConfigService::open().context("open config")?;
    let snap = config.snapshot();

    let explicit = std::env::var("SOLX_SERVER_URL")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| snap.server_url.clone().filter(|s| !s.trim().is_empty()));
    if explicit.is_some() {
        return App::build().await;
    }

    let port = snap.server_port.unwrap_or(solx_config::DEFAULT_SERVER_PORT);

    if health_check(port).await {
        let token = config.ensure_server_token().context("ensure server token")?;
        std::env::set_var("SOLX_SERVER_URL", format!("http://127.0.0.1:{port}"));
        std::env::set_var("SOLX_SERVER_TOKEN", &token);
        return App::build().await;
    }

    let app = App::build_local().await?;
    match app.config.ensure_server_token() {
        Ok(token) => {
            let state = solx_server::state::AppState {
                app: app.clone(),
                token: Arc::from(token.as_str()),
            };
            if let Err(e) = solx_server::spawn_embedded(state, port).await {
                tracing::warn!(
                    "could not start embedded solx-server on 127.0.0.1:{port} ({e}); \
                     actions that call back into solx-server over HTTP may fail"
                );
            }
        }
        Err(e) => tracing::warn!("could not prepare embedded solx-server token: {e}"),
    }
    Ok(app)
}

async fn health_check(port: u16) -> bool {
    reqwest::Client::new()
        .get(format!("http://127.0.0.1:{port}/health"))
        .timeout(std::time::Duration::from_millis(300))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

async fn run_command(app: &Arc<App>, command: Commands, piped: Option<Value>) -> Result<Value> {
    match command {
        Commands::Save {
            entity,
            reference,
            json,
            type_ref,
            file,
        } => handle_save(app, entity, reference, json, type_ref, file, piped).await,
        Commands::Get { entity, reference } => handle_get(app, entity, reference).await,
        Commands::Delete { entity, reference, if_exists } => {
            handle_delete(app, entity, reference, if_exists).await
        }
        Commands::Exec { reference, json, no_console } => {
            handle_exec(app, reference, json, piped, no_console).await
        }
        Commands::List {
            entity,
            path,
            limit,
            offset,
        } => handle_list(app, entity, path, limit, offset).await,
        Commands::Search {
            query,
            path,
            type_ref,
            linked_to,
            limit,
            offset,
        } => handle_search(app, query, path, type_ref, linked_to, limit, offset).await,
        Commands::Script { expr, file } => handle_script(app, expr, file).await,
        Commands::InstallPackage { path } => {
            let runner = AppRunner { app: app.clone() };
            let pkg = solx_packages::install_package(&runner, &app.config, &PathBuf::from(path))
                .await
                .map_err(to_anyhow)?;
            Ok(serde_json::to_value(pkg)?)
        }
        Commands::UninstallPackage { name } => {
            let runner = AppRunner { app: app.clone() };
            solx_packages::uninstall_package(&runner, &app.config, &name)
                .await
                .map_err(to_anyhow)?;
            Ok(serde_json::json!({ "message": format!("uninstalled '{name}'") }))
        }
        Commands::ListPackages => Ok(serde_json::to_value(solx_packages::list_packages(
            &app.config,
        ))?),
        Commands::Json { value } => serde_json::from_str(&value).context("parse json value"),
        Commands::Str { value } => Ok(Value::String(value)),
        Commands::Escape { value } => Ok(Value::String(
            serde_json::to_string(&Value::String(value)).context("encode string")?,
        )),
        Commands::Random { bytes } => {
            use rand::RngCore;
            let mut buf = vec![0u8; bytes];
            rand::thread_rng().fill_bytes(&mut buf);
            Ok(serde_json::json!(base64::engine::general_purpose::STANDARD.encode(&buf)))
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_save(
    app: &Arc<App>,
    entity: Entity,
    reference: String,
    json: Option<String>,
    type_ref: Option<String>,
    file: Option<String>,
    piped: Option<Value>,
) -> Result<Value> {
    if let Entity::File = entity {
        let local = file.ok_or_else(|| anyhow!("save file requires --file <local path>"))?;
        let bytes = std::fs::read(&local).with_context(|| format!("read {local}"))?;
        let stored = app.files().put(&reference, bytes).await.map_err(to_anyhow)?;
        return Ok(serde_json::json!({ "relPath": stored }));
    }

    let mut body = build_body(json, piped)?;
    if let Some(t) = type_ref {
        body.insert("typeRef".to_string(), Value::String(t));
    }
    let body = Value::Object(body);
    let (path, name) = split_ref(&reference).map_err(to_anyhow)?;

    match entity {
        Entity::Doc => {
            let input: DocumentInput =
                serde_json::from_value(body).context("parse document input")?;
            Ok(serde_json::to_value(
                app.docs().save(&path, &name, input).await.map_err(to_anyhow)?,
            )?)
        }
        Entity::Action => {
            let input: ActionInput = serde_json::from_value(body).context("parse action input")?;
            Ok(serde_json::to_value(
                app.actions().save(&path, &name, input).await.map_err(to_anyhow)?,
            )?)
        }
        Entity::Type => {
            let input: TypeInput = serde_json::from_value(body).context("parse type input")?;
            Ok(serde_json::to_value(
                app.types().save(&path, &name, input).await.map_err(to_anyhow)?,
            )?)
        }
        Entity::File => unreachable!(),
    }
}

async fn handle_get(app: &Arc<App>, entity: Entity, reference: String) -> Result<Value> {
    if let Entity::File = entity {
        let bytes = app.files().get(&reference).await.map_err(to_anyhow)?;
        let (content, encoding) = match String::from_utf8(bytes.clone()) {
            Ok(s) => (s, "utf8"),
            Err(_) => (
                base64::engine::general_purpose::STANDARD.encode(&bytes),
                "base64",
            ),
        };
        return Ok(serde_json::json!({
            "relPath": reference,
            "encoding": encoding,
            "content": content,
        }));
    }
    let (path, name) = split_ref(&reference).map_err(to_anyhow)?;
    match entity {
        Entity::Doc => Ok(serde_json::to_value(
            app.docs().get(&path, &name).await.map_err(to_anyhow)?,
        )?),
        Entity::Action => Ok(serde_json::to_value(
            app.actions().get(&path, &name).await.map_err(to_anyhow)?,
        )?),
        Entity::Type => Ok(serde_json::to_value(
            app.types().get(&path, &name).await.map_err(to_anyhow)?,
        )?),
        Entity::File => unreachable!(),
    }
}

async fn handle_delete(
    app: &Arc<App>,
    entity: Entity,
    reference: String,
    if_exists: bool,
) -> Result<Value> {
    let outcome = if let Entity::File = entity {
        app.files().delete(&reference).await
    } else {
        let (path, name) = split_ref(&reference).map_err(to_anyhow)?;
        match entity {
            Entity::Doc => app.docs().delete(&path, &name).await,
            Entity::Action => app.actions().delete(&path, &name).await,
            Entity::Type => app.types().delete(&path, &name).await,
            Entity::File => unreachable!(),
        }
    };
    match outcome {
        Ok(()) => Ok(serde_json::json!({
            "deleted": true,
            "message": format!("deleted '{reference}'"),
        })),
        // `--if-exists`: a missing entity is not a failure, so a teardown
        // script keeps going instead of aborting half-done.
        Err(SolxError::NotFound(_)) if if_exists => Ok(serde_json::json!({
            "deleted": false,
            "message": format!("'{reference}' not found; skipped"),
        })),
        Err(e) => Err(to_anyhow(e)),
    }
}

async fn handle_exec(
    app: &Arc<App>,
    reference: String,
    json: Option<String>,
    piped: Option<Value>,
    no_console: bool,
) -> Result<Value> {
    let params = match json {
        Some(j) => serde_json::from_str(&j).context("parse --json params")?,
        None => piped.unwrap_or(Value::Object(Map::new())),
    };
    let (path, name) = split_ref(&reference).map_err(to_anyhow)?;

    if no_console {
        return Ok(serde_json::to_value(
            app.actions().exec(&path, &name, params).await.map_err(to_anyhow)?,
        )?);
    }

    let action_ref = full_ref(&path, &name).map_err(to_anyhow)?;
    let tail_handle = spawn_console_tail(app.clone(), action_ref).await;

    let result = app.actions().exec(&path, &name, params).await;
    tail_handle.stop_and_drain().await;

    Ok(serde_json::to_value(result.map_err(to_anyhow)?)?)
}

/// Renders an action's console to stderr while it runs, so `solx exec`
/// shows progress live without disturbing stdout's single JSON result
/// (which is what makes `--no-console` unnecessary for scripted callers —
/// `solx exec ... | jq` behaves identically either way).
struct ConsoleTailHandle {
    stop: Arc<tokio::sync::Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl ConsoleTailHandle {
    async fn stop_and_drain(self) {
        self.stop.notify_one();
        let _ = self.task.await;
    }
}

/// Long-poll interval passed to `console/tail` between renders. Short
/// enough that a human sees output promptly; the loop yields entirely
/// (no busy-wait) between polls via the long-poll itself.
const TAIL_WAIT_SECS: i64 = 2;

async fn spawn_console_tail(app: Arc<App>, action_ref: String) -> ConsoleTailHandle {
    // Start from the console's current tip, not seq 0 — otherwise every
    // invocation of a frequently-run action would replay its entire prior
    // history to the terminal. A console that has never been written to
    // has no row yet, which `find` treats the same as "starts at 0".
    let start_cursor = current_tip(&app, &action_ref).await.unwrap_or(0);

    let stop = Arc::new(tokio::sync::Notify::new());
    let task_stop = stop.clone();
    let task = tokio::spawn(async move {
        let actions = app.actions();
        let mut cursor = start_cursor;
        loop {
            let tail = actions.exec(
                "/builtin/console",
                "tail",
                serde_json::json!({ "action_ref": action_ref, "cursor": cursor, "wait_secs": TAIL_WAIT_SECS }),
            );
            tokio::select! {
                res = tail => {
                    cursor = render_console_result(res, cursor);
                }
                _ = task_stop.notified() => {
                    // One last non-blocking read so nothing printed by the
                    // action right before it returned is lost to a race
                    // against this task's own poll cadence.
                    let res = actions.exec(
                        "/builtin/console",
                        "read",
                        serde_json::json!({ "action_ref": action_ref, "from_seq": cursor }),
                    ).await;
                    render_console_result(res, cursor);
                    return;
                }
            }
        }
    });

    ConsoleTailHandle { stop, task }
}

/// Print any entries in a `console/tail` or `console/read` result to
/// stderr, and return the cursor to continue from. Failures are swallowed —
/// rendering is best-effort and must never fail the exec it's watching.
fn render_console_result(
    res: std::result::Result<solx_surface::entities::ActionExecResult, SolxError>,
    fallback_cursor: i64,
) -> i64 {
    let Ok(res) = res else { return fallback_cursor };
    if let Some(entries) = res.result.get("entries").and_then(Value::as_array) {
        for entry in entries {
            let level = entry.get("level").and_then(Value::as_str).unwrap_or("info");
            let message = entry.get("message").and_then(Value::as_str).unwrap_or("");
            eprintln!("[{}] {message}", level.to_ascii_uppercase());
        }
    }
    res.result
        .get("next_cursor")
        .and_then(Value::as_i64)
        .unwrap_or(fallback_cursor)
}

/// The seq that will be assigned to this console's next write, or `None` if
/// it has never been written to. One `console/list` lookup by exact
/// `action_ref` — O(1) against the `consoles` table, not a history scan.
async fn current_tip(app: &Arc<App>, action_ref: &str) -> Option<i64> {
    let actions = app.actions();
    let res = actions
        .exec(
            "/builtin/console",
            "list",
            serde_json::json!({ "prefix": action_ref, "limit": 50 }),
        )
        .await
        .ok()?;
    res.result
        .get("consoles")
        .and_then(Value::as_array)?
        .iter()
        .find(|c| c.get("action_ref").and_then(Value::as_str) == Some(action_ref))
        .and_then(|c| c.get("next_seq"))
        .and_then(Value::as_i64)
}

async fn handle_list(
    app: &Arc<App>,
    entity: Entity,
    path: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
) -> Result<Value> {
    let opts = ListOptions {
        path_prefix: path,
        limit,
        offset,
        ..Default::default()
    };
    match entity {
        Entity::Doc => Ok(serde_json::to_value(
            app.docs().list(opts).await.map_err(to_anyhow)?,
        )?),
        Entity::Action => Ok(serde_json::to_value(
            app.actions().list(opts).await.map_err(to_anyhow)?,
        )?),
        Entity::Type => Ok(serde_json::to_value(
            app.types().list(opts).await.map_err(to_anyhow)?,
        )?),
        Entity::File => {
            let prefix = opts.path_prefix.unwrap_or_default();
            let files = app.files().list(&prefix).await.map_err(to_anyhow)?;
            Ok(serde_json::json!({ "files": files }))
        }
    }
}

async fn handle_search(
    app: &Arc<App>,
    query: String,
    path: Option<String>,
    type_ref: Option<String>,
    linked_to: Option<String>,
    limit: Option<usize>,
    offset: Option<usize>,
) -> Result<Value> {
    let q = SearchQuery {
        q: Some(query).filter(|s| !s.is_empty()),
        path_prefix: path,
        type_ref,
        linked_to,
        limit,
        offset,
    };
    Ok(serde_json::to_value(
        app.docs().search(q).await.map_err(to_anyhow)?,
    )?)
}

async fn handle_script(app: &Arc<App>, expr: Option<String>, file: Option<String>) -> Result<Value> {
    let source = if let Some(e) = expr {
        e
    } else if let Some(f) = file {
        std::fs::read_to_string(&f).with_context(|| format!("read script {f}"))?
    } else {
        let mut buf = String::new();
        std::io::stdin().read_to_string(&mut buf)?;
        buf
    };
    let runner = AppRunner { app: app.clone() };
    execute_script(&runner, &source).await.map_err(to_anyhow)
}

/// Build a JSON object body from `--json`, else piped input, else an empty
/// object.
fn build_body(json: Option<String>, piped: Option<Value>) -> Result<Map<String, Value>> {
    let mut body = if let Some(j) = json {
        serde_json::from_str::<Value>(&j).context("parse --json body")?
    } else if let Some(p) = piped {
        p
    } else {
        Value::Object(Map::new())
    };
    let obj = body
        .as_object_mut()
        .ok_or_else(|| anyhow!("body must be a JSON object"))?;
    Ok(obj.clone())
}

// ── Script command runner ─────────────────────────────────────────────────────

/// Re-dispatches parsed script stages back through the CLI's own command
/// handlers, mirroring the old `execute_pipeline`.
struct AppRunner {
    app: Arc<App>,
}

#[async_trait]
impl CommandRunner for AppRunner {
    async fn run(
        &self,
        tokens: Vec<String>,
        piped: Option<Value>,
    ) -> solx_surface::error::Result<Value> {
        let mut argv = vec!["solx".to_string()];
        argv.extend(tokens);
        let cli = Cli::try_parse_from(&argv)
            .map_err(|e| SolxError::Invalid(format!("script parse error: {e}")))?;
        if matches!(cli.command, Commands::Script { .. }) {
            return Err(SolxError::Invalid("nested 'script' is not supported".into()));
        }
        run_command(&self.app, cli.command, piped)
            .await
            .map_err(|e| SolxError::Exec(e.to_string()))
    }
}
