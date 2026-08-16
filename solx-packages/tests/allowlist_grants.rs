//! `package.json`-declared `command_actions`/`allowed_webhook_base_urls`
//! grants — see the module doc on `solx_packages::install_package`.
//!
//! Uses a `CommandRunner` fake that ignores every script stage: these tests
//! are about the grant/revoke bookkeeping around `install.solx`/
//! `uninstall.solx`, not about what the scripts themselves do.

use std::path::Path;

use async_trait::async_trait;
use serde_json::{json, Value};
use solx_config::ConfigService;
use solx_packages::{install_package, uninstall_package};
use solx_scripts::CommandRunner;

struct NoopRunner;

#[async_trait]
impl CommandRunner for NoopRunner {
    async fn run(&self, _tokens: Vec<String>, _piped: Option<Value>) -> solx_surface::error::Result<Value> {
        Ok(Value::Null)
    }
}

/// Writes a minimal package directory: `package.json` (with the given extra
/// top-level fields merged in) plus trivial `install.solx`/`uninstall.solx`.
fn write_package(dir: &Path, name: &str, extra: Value) {
    std::fs::create_dir_all(dir).unwrap();
    let mut meta = json!({ "name": name, "version": "0.0.1" });
    for (k, v) in extra.as_object().unwrap() {
        meta.as_object_mut().unwrap().insert(k.clone(), v.clone());
    }
    std::fs::write(dir.join("package.json"), serde_json::to_string(&meta).unwrap()).unwrap();
    std::fs::write(dir.join("install.solx"), "json '{}'").unwrap();
    std::fs::write(dir.join("uninstall.solx"), "json '{}'").unwrap();
}

#[tokio::test]
async fn install_grants_declared_command_and_webhook_entries() {
    let appdata = tempfile::tempdir().unwrap();
    let cfg = ConfigService::open_in(appdata.path()).unwrap();
    let runner = NoopRunner;

    let pkg_dir = tempfile::tempdir().unwrap();
    write_package(
        pkg_dir.path(),
        "pkg-a",
        json!({
            "command_actions": { "pkg-a-cmd": { "command": "echo 1" } },
            "allowed_webhook_base_urls": ["https://pkg-a.example.com"]
        }),
    );

    let outcome = install_package(&runner, &cfg, pkg_dir.path()).await.unwrap();
    assert!(outcome.warnings.is_empty());
    assert_eq!(outcome.package.granted_commands, vec!["pkg-a-cmd".to_string()]);
    assert_eq!(
        outcome.package.granted_webhook_prefixes,
        vec!["https://pkg-a.example.com".to_string()]
    );

    assert_eq!(cfg.command_actions()["pkg-a-cmd"].command, "echo 1");
    assert!(cfg
        .allowed_webhook_base_urls()
        .contains(&"https://pkg-a.example.com".to_string()));
}

#[tokio::test]
async fn install_with_no_manifest_grants_is_unaffected() {
    let appdata = tempfile::tempdir().unwrap();
    let cfg = ConfigService::open_in(appdata.path()).unwrap();
    let runner = NoopRunner;

    let pkg_dir = tempfile::tempdir().unwrap();
    write_package(pkg_dir.path(), "pkg-plain", json!({}));

    let outcome = install_package(&runner, &cfg, pkg_dir.path()).await.unwrap();
    assert!(outcome.warnings.is_empty());
    assert!(outcome.package.granted_commands.is_empty());
    assert!(outcome.package.granted_webhook_prefixes.is_empty());
    assert!(cfg.command_actions().is_empty());
    assert!(cfg.allowed_webhook_base_urls().is_empty());
}

#[tokio::test]
async fn uninstall_revokes_grants_no_other_package_claims() {
    let appdata = tempfile::tempdir().unwrap();
    let cfg = ConfigService::open_in(appdata.path()).unwrap();
    let runner = NoopRunner;

    let pkg_dir = tempfile::tempdir().unwrap();
    write_package(
        pkg_dir.path(),
        "pkg-solo",
        json!({ "command_actions": { "solo-cmd": { "command": "echo 1" } } }),
    );
    install_package(&runner, &cfg, pkg_dir.path()).await.unwrap();
    assert!(cfg.command_actions().contains_key("solo-cmd"));

    uninstall_package(&runner, &cfg, "pkg-solo").await.unwrap();
    assert!(!cfg.command_actions().contains_key("solo-cmd"));
}

#[tokio::test]
async fn colliding_key_warns_and_overwrites_but_survives_the_original_owners_uninstall() {
    let appdata = tempfile::tempdir().unwrap();
    let cfg = ConfigService::open_in(appdata.path()).unwrap();
    let runner = NoopRunner;

    let dir_a = tempfile::tempdir().unwrap();
    write_package(
        dir_a.path(),
        "pkg-a",
        json!({ "command_actions": { "shared-key": { "command": "echo from-a" } } }),
    );
    let outcome_a = install_package(&runner, &cfg, dir_a.path()).await.unwrap();
    assert!(outcome_a.warnings.is_empty(), "first installer should see no collision");

    let dir_b = tempfile::tempdir().unwrap();
    write_package(
        dir_b.path(),
        "pkg-b",
        json!({ "command_actions": { "shared-key": { "command": "echo from-b" } } }),
    );
    let outcome_b = install_package(&runner, &cfg, dir_b.path()).await.unwrap();
    assert_eq!(outcome_b.warnings.len(), 1, "{:?}", outcome_b.warnings);
    assert!(outcome_b.warnings[0].contains("pkg-a"), "{:?}", outcome_b.warnings);

    // Last-write-wins: the key now points at pkg-b's command.
    assert_eq!(cfg.command_actions()["shared-key"].command, "echo from-b");

    // pkg-a no longer claims it (pkg-b does), so uninstalling pkg-a must not
    // revoke the key out from under pkg-b.
    uninstall_package(&runner, &cfg, "pkg-a").await.unwrap();
    assert_eq!(
        cfg.command_actions()["shared-key"].command,
        "echo from-b",
        "uninstalling the original owner must not break the package that now owns the key"
    );

    uninstall_package(&runner, &cfg, "pkg-b").await.unwrap();
    assert!(!cfg.command_actions().contains_key("shared-key"));
}

#[tokio::test]
async fn reinstalling_the_same_package_is_not_a_collision() {
    let appdata = tempfile::tempdir().unwrap();
    let cfg = ConfigService::open_in(appdata.path()).unwrap();
    let runner = NoopRunner;

    let pkg_dir = tempfile::tempdir().unwrap();
    write_package(
        pkg_dir.path(),
        "pkg-a",
        json!({ "command_actions": { "own-key": { "command": "echo v1" } } }),
    );
    install_package(&runner, &cfg, pkg_dir.path()).await.unwrap();

    // Bump the manifest and reinstall over itself.
    write_package(
        pkg_dir.path(),
        "pkg-a",
        json!({ "command_actions": { "own-key": { "command": "echo v2" } } }),
    );
    let outcome = install_package(&runner, &cfg, pkg_dir.path()).await.unwrap();
    assert!(outcome.warnings.is_empty(), "reinstalling your own package must not warn");
    assert_eq!(cfg.command_actions()["own-key"].command, "echo v2");
}
