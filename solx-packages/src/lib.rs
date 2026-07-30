//! `solx-packages` — install/uninstall solx packages.
//!
//! A package is a directory containing `package.json` (at least `name`, and a
//! `version`) and an `install.solx` script (and optionally `uninstall.solx`).
//! Installation runs the script through [`solx_scripts`] against the CLI's
//! command runner, then records the package in `solx-config.json`. There is
//! currently no allowlist gate on the `Command`/`Webhook` actions a package's
//! `install.solx` may register — anything it `post`s to the actions DB is
//! immediately executable (see `docs/design-and-progress.md`'s "Suggested
//! next steps" for the deferred hardening plan: package signing, a
//! permissions module, secrets masking).

use std::path::Path;

use solx_config::{ConfigService, InstalledPackage};
use solx_scripts::{execute_script, CommandRunner};
use solx_surface::error::{Result, SolxError};

/// Install the package at `dir`: run `install.solx`, then register it.
pub async fn install_package(
    runner: &dyn CommandRunner,
    config: &ConfigService,
    dir: &Path,
) -> Result<InstalledPackage> {
    let meta_path = dir.join("package.json");
    let install_path = dir.join("install.solx");

    let meta_str = std::fs::read_to_string(&meta_path)
        .map_err(|e| SolxError::Io(format!("read {}: {e}", meta_path.display())))?;
    let meta: serde_json::Value = serde_json::from_str(&meta_str)?;
    let name = meta
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| SolxError::Invalid("package.json missing 'name'".into()))?
        .to_string();
    let version = meta
        .get("version")
        .and_then(|v| v.as_str())
        .unwrap_or("0.0.0")
        .to_string();

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
    };
    config.register_package(pkg.clone())?;
    Ok(pkg)
}

/// Uninstall `name`: run `uninstall.solx` (if present), then unregister.
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
