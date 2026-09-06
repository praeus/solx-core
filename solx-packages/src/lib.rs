//! `solx-packages` — install/uninstall solx packages.
//!
//! A package is a directory containing `solx-package.json` (at least `name`,
//! and a `version`) and an `install.solx` script (and optionally
//! `uninstall.solx`).
//!
//! The manifest used to be `package.json`, which is npm's file. Most package
//! directories here are Rust or wasm and were never node projects, so a
//! `package.json` in them misled every tool that saw one; the directories that
//! *are* node projects now keep the two manifests apart instead of overloading
//! shared keys like `name` and `version`, whose rules differ between the two.
//! `package.json` is still read as a fallback (with a warning) so third-party
//! packages keep installing.
//! Installation runs the script through [`solx_scripts`] against the CLI's
//! command runner, then records the package in `solx-config.json`.
//!
//! ## Command/Webhook allowlist grants
//!
//! solx's outbound allowlists are deny-by-default (see `docs/next-steps.md`
//! §1): a Command action's `fn_name` must be a key registered in
//! `command_actions`, and any outbound URL — a Webhook action's, or one
//! handed to a `/builtin/web/*` built-in — must match a prefix in
//! `allowed_base_urls`. A package that registers such actions in its
//! `install.solx`, or that reaches the network from a wasm guest, needs its
//! own entries granted, or it is unusable the moment it's installed.
//!
//! `solx-package.json` may declare `command_actions` and `allowed_base_urls`
//! in the exact same shape as `solx-config.json` itself (see
//! [`solx_config::CommandDef`]).
//! [`install_package`] grants every declared entry into the global
//! allowlist before running `install.solx` (so the script — or a later
//! `verify.solx` — can exec the package's own actions), and records exactly
//! what it granted on the [`InstalledPackage`] row. [`uninstall_package`]
//! revokes exactly that, unless another currently-installed package also
//! declares the same key/prefix (checked via every other package's own
//! recorded grants), in which case it's left alone.
//!
//! A key or prefix already granted by a **different** installed package is
//! not an install error — the newer package's value wins (last-write-wins,
//! the same semantics as reinstalling a package over itself), but a warning
//! naming both packages is logged and returned in [`InstallOutcome::warnings`]
//! so the collision isn't silent.
//!
//! Existing packages predating this feature (or a manifest with neither
//! field) are simply not affected — reinstalling is how they pick up
//! allowlist grants, exactly as before this feature existed for anything
//! else in the manifest.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};
use solx_config::{CommandDef, ConfigService, InstalledPackage};
use solx_scripts::{execute_script, CommandRunner};
use solx_surface::error::{Result, SolxError};

/// [`install_package`]'s result: the recorded package plus any
/// allowlist-collision warnings. Warnings never block the install —
/// see the module doc.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InstallOutcome {
    #[serde(flatten)]
    pub package: InstalledPackage,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

/// The manifest filename, and the npm-owned name still accepted as a
/// fallback so packages written before the split keep installing.
const MANIFEST: &str = "solx-package.json";
const MANIFEST_LEGACY: &str = "package.json";

/// A manifest's allowlist declarations, parsed once and reused for both
/// granting (install) and the `InstalledPackage.granted_*` record.
struct Grants {
    commands: HashMap<String, CommandDef>,
    base_urls: Vec<String>,
}

fn parse_grants(meta: &serde_json::Value, file: &str) -> Result<Grants> {
    let commands: HashMap<String, CommandDef> = match meta.get("command_actions") {
        Some(v) if !v.is_null() => serde_json::from_value(v.clone())
            .map_err(|e| SolxError::Invalid(format!("{file} 'command_actions': {e}")))?,
        _ => HashMap::new(),
    };
    // `allowed_webhook_base_urls` is the former spelling, still read for the
    // same reason the config key is — see `solx_config`'s `merged_base_urls`.
    let (key, raw) = match meta.get("allowed_base_urls") {
        Some(v) if !v.is_null() => ("allowed_base_urls", Some(v)),
        _ => match meta.get("allowed_webhook_base_urls") {
            Some(v) if !v.is_null() => ("allowed_webhook_base_urls", Some(v)),
            _ => ("allowed_base_urls", None),
        },
    };
    let base_urls: Vec<String> = match raw {
        Some(v) => serde_json::from_value(v.clone())
            .map_err(|e| SolxError::Invalid(format!("{file} '{key}': {e}")))?,
        None => Vec::new(),
    };
    Ok(Grants { commands, base_urls })
}

/// Locate the package manifest, preferring [`MANIFEST`]. Returns the path,
/// its filename (for error messages), and whether the legacy name was used.
fn manifest_path(dir: &Path) -> (std::path::PathBuf, &'static str, bool) {
    let current = dir.join(MANIFEST);
    if current.exists() {
        (current, MANIFEST, false)
    } else {
        (dir.join(MANIFEST_LEGACY), MANIFEST_LEGACY, true)
    }
}

/// Install the package at `dir`: grant its declared allowlist entries, run
/// `install.solx`, then register it.
pub async fn install_package(
    runner: &dyn CommandRunner,
    config: &ConfigService,
    dir: &Path,
) -> Result<InstallOutcome> {
    let (meta_path, meta_file, legacy_manifest) = manifest_path(dir);
    let install_path = dir.join("install.solx");

    let meta_str = std::fs::read_to_string(&meta_path)
        .map_err(|e| SolxError::Io(format!("read {}: {e}", meta_path.display())))?;
    let meta: serde_json::Value = serde_json::from_str(&meta_str)?;
    let name = meta
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| SolxError::Invalid(format!("{meta_file} missing 'name'")))?
        .to_string();
    let version = meta
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("0.0.0")
        .to_string();
    let grants = parse_grants(&meta, meta_file)?;

    let mut warnings = Vec::new();
    if legacy_manifest {
        let msg = format!(
            concat!(
                "package '{name}' has no {manifest}; read its manifest from ",
                "{legacy} instead. Rename it: {legacy} is npm's file, and ",
                "solx will stop falling back to it."
            ),
            name = name,
            manifest = MANIFEST,
            legacy = MANIFEST_LEGACY
        );
        tracing::warn!("{msg}");
        warnings.push(msg);
    }

    // Grant before running the script, so install.solx (or a later
    // verify.solx) can exec the package's own Command/Webhook actions.
    // Collisions with a *different* installed package are warned about,
    // not fatal — see the module doc.
    let others: Vec<InstalledPackage> =
        config.list_packages().into_iter().filter(|p| p.name != name).collect();

    for (key, def) in &grants.commands {
        if let Some(owner) = others.iter().find(|p| p.granted_commands.iter().any(|k| k == key)) {
            let msg = format!(
                "command key '{key}' was granted by package '{}'; now overwritten by '{name}'",
                owner.name
            );
            tracing::warn!("{msg}");
            warnings.push(msg);
        }
        config.register_command(key, def.clone())?;
    }
    for prefix in &grants.base_urls {
        if let Some(owner) = others.iter().find(|p| p.granted_base_urls.iter().any(|p| p == prefix))
        {
            let msg = format!(
                "base URL prefix '{prefix}' was granted by package '{}'; now also granted to '{name}'",
                owner.name
            );
            tracing::warn!("{msg}");
            warnings.push(msg);
        }
        config.add_allowed_base_url(prefix)?;
    }

    let script = std::fs::read_to_string(&install_path)
        .map_err(|e| SolxError::Io(format!("read {}: {e}", install_path.display())))?;

    run_in_dir(runner, dir, &script).await?;

    let pkg = InstalledPackage {
        name,
        version,
        path: dir
            .canonicalize()
            .unwrap_or_else(|_| dir.to_path_buf())
            .to_string_lossy()
            .into_owned(),
        installed_at: chrono::Utc::now().to_rfc3339(),
        granted_commands: grants.commands.into_keys().collect(),
        granted_base_urls: grants.base_urls,
    };
    config.register_package(pkg.clone())?;
    Ok(InstallOutcome { package: pkg, warnings })
}

/// Uninstall `name`: run `uninstall.solx` (if present), revoke whatever this
/// package's install granted (unless another installed package still
/// declares the same key/prefix), then unregister.
pub async fn uninstall_package(
    runner: &dyn CommandRunner,
    config: &ConfigService,
    name: &str,
) -> Result<()> {
    let pkg = config
        .list_packages()
        .into_iter()
        .find(|p| p.name == name)
        .ok_or_else(|| SolxError::NotFound(format!("package '{name}'")))?;

    let dir = Path::new(&pkg.path);
    let uninstall_path = dir.join("uninstall.solx");
    if uninstall_path.exists() {
        let script = std::fs::read_to_string(&uninstall_path)
            .map_err(|e| SolxError::Io(format!("read {}: {e}", uninstall_path.display())))?;
        run_in_dir(runner, dir, &script).await?;
    }

    // Revoke after the script ran (symmetric with granting before install.solx
    // runs), and only what no other installed package still claims.
    let others: Vec<InstalledPackage> =
        config.list_packages().into_iter().filter(|p| p.name != name).collect();
    for key in &pkg.granted_commands {
        if !others.iter().any(|p| p.granted_commands.iter().any(|k| k == key)) {
            config.deregister_command(key)?;
        }
    }
    for prefix in &pkg.granted_base_urls {
        if !others.iter().any(|p| p.granted_base_urls.iter().any(|p| p == prefix)) {
            config.remove_allowed_base_url(prefix)?;
        }
    }

    config.unregister_package(name)?;
    Ok(())
}

/// List installed packages.
pub fn list_packages(config: &ConfigService) -> Vec<InstalledPackage> {
    config.list_packages()
}

/// Run a package script with the process CWD temporarily set to the package
/// directory (so relative paths in the script resolve), restoring it after.
async fn run_in_dir(runner: &dyn CommandRunner, dir: &Path, script: &str) -> Result<()> {
    let original =
        std::env::current_dir().map_err(|e| SolxError::Io(format!("read cwd: {e}")))?;
    std::env::set_current_dir(dir)
        .map_err(|e| SolxError::Io(format!("set cwd to {}: {e}", dir.display())))?;
    let result = execute_script(runner, script).await;
    let restore = std::env::set_current_dir(&original)
        .map_err(|e| SolxError::Io(format!("restore cwd: {e}")));
    result?;
    restore?;
    Ok(())
}
