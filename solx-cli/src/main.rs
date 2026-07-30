//! `solx` — the command-line interface over the solx core crates.
//!
//! Verbs: `post` (upsert), `get`, `delete`, `exec` (actions), `list`, `search`,
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
use solx_surface::path::split_ref;
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
    Post {
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
    Delete { entity: Entity, reference: String },
    /// Execute an action.
    Exec {
        /// Action reference `/path/name`.
        reference: String,
        #[arg(long, short = 'j')]
        json: Option<String>,
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
    /// Install a package directory (package.json + install.solx).
    InstallPackage { path: String },
    /// Uninstall a package by name.
    UninstallPackage { name: String },
    /// List installed packages.
    ListPackages,
    /// Emit a JSON value (handy as a pipeline source in scripts).
    Json { value: String },
    /// Generate a base64-encoded random key (for secrets encryption).
    Random {
        /// Number of random bytes (default 32, for AES-256).
        #[arg(default_value = "32")]
        bytes: usize,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let app = App::build().await?;
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

async fn run_command(app: &Arc<App>, command: Commands, piped: Option<Value>) -> Result<Value> {
    match command {
        Commands::Post {
            entity,
            reference,
            json,
            type_ref,
            file,
        } => handle_post(app, entity, reference, json, type_ref, file, piped).await,
        Commands::Get { entity, reference } => handle_get(app, entity, reference).await,
        Commands::Delete { entity, reference } => handle_delete(app, entity, reference).await,
        Commands::Exec { reference, json } => handle_exec(app, reference, json, piped).await,
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
        Commands::Random { bytes } => {
            use rand::RngCore;
            let mut buf = vec![0u8; bytes];
            rand::thread_rng().fill_bytes(&mut buf);
            Ok(serde_json::json!(base64::engine::general_purpose::STANDARD.encode(&buf)))
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_post(
    app: &Arc<App>,
    entity: Entity,
    reference: String,
    json: Option<String>,
    type_ref: Option<String>,
    file: Option<String>,
    piped: Option<Value>,
) -> Result<Value> {
    if let Entity::File = entity {
        let local = file.ok_or_else(|| anyhow!("post file requires --file <local path>"))?;
        let bytes = std::fs::read(&local).with_context(|| format!("read {local}"))?;
        let stored = app.files().put(&reference, bytes).await.map_err(to_anyhow)?;
        return Ok(serde_json::json!({ "rel_path": stored }));
    }

    let mut body = build_body(json, piped)?;
    if let Some(t) = type_ref {
        body.insert("type_ref".to_string(), Value::String(t));
    }
    let body = Value::Object(body);
    let (path, name) = split_ref(&reference).map_err(to_anyhow)?;

    match entity {
        Entity::Doc => {
            let input: DocumentInput =
                serde_json::from_value(body).context("parse document input")?;
            Ok(serde_json::to_value(
                app.docs().post(&path, &name, input).await.map_err(to_anyhow)?,
            )?)
        }
        Entity::Action => {
            let input: ActionInput = serde_json::from_value(body).context("parse action input")?;
            Ok(serde_json::to_value(
                app.actions().post(&path, &name, input).await.map_err(to_anyhow)?,
            )?)
        }
        Entity::Type => {
            let input: TypeInput = serde_json::from_value(body).context("parse type input")?;
            Ok(serde_json::to_value(
                app.types().post(&path, &name, input).await.map_err(to_anyhow)?,
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
            "rel_path": reference,
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

async fn handle_delete(app: &Arc<App>, entity: Entity, reference: String) -> Result<Value> {
    if let Entity::File = entity {
        app.files().delete(&reference).await.map_err(to_anyhow)?;
    } else {
        let (path, name) = split_ref(&reference).map_err(to_anyhow)?;
        match entity {
            Entity::Doc => app.docs().delete(&path, &name).await.map_err(to_anyhow)?,
            Entity::Action => app.actions().delete(&path, &name).await.map_err(to_anyhow)?,
            Entity::Type => app.types().delete(&path, &name).await.map_err(to_anyhow)?,
            Entity::File => unreachable!(),
        }
    }
    Ok(serde_json::json!({ "message": format!("deleted '{reference}'") }))
}

async fn handle_exec(
    app: &Arc<App>,
    reference: String,
    json: Option<String>,
    piped: Option<Value>,
) -> Result<Value> {
    let params = match json {
        Some(j) => serde_json::from_str(&j).context("parse --json params")?,
        None => piped.unwrap_or(Value::Object(Map::new())),
    };
    let (path, name) = split_ref(&reference).map_err(to_anyhow)?;
    Ok(serde_json::to_value(
        app.actions().exec(&path, &name, params).await.map_err(to_anyhow)?,
    )?)
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
