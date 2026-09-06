//! Stamps the binary with what it was built from, so a running server can be
//! asked whether it is stale.
//!
//! A stale `solx-server` is a confident liar: it accepts requests, writes
//! rows, and answers plausibly using code paths that no longer exist in the
//! source tree. It cost real debugging time once — a binary predating the
//! FTS5 conversion kept its own Tantivy index, so its own search worked while
//! nothing else could see what it wrote. `/health` reporting this makes that
//! one request to diagnose instead of a file-mtime comparison.
//!
//! Everything here degrades to `"unknown"` rather than failing the build: a
//! source tarball with no `.git` must still compile.

use std::process::Command;

fn main() {
    let sha = git(&["rev-parse", "--short=12", "HEAD"]).unwrap_or_else(|| "unknown".into());

    // A dirty tree is exactly the case where the SHA alone misleads.
    let dirty = Command::new("git")
        .args(["status", "--porcelain", "--untracked-files=no"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| !o.stdout.is_empty())
        .unwrap_or(false);

    let commit = if dirty { format!("{sha}-dirty") } else { sha };
    println!("cargo:rustc-env=SOLX_BUILD_COMMIT={commit}");

    let built_at = Command::new("git")
        .args(["log", "-1", "--format=%cI"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=SOLX_BUILD_COMMIT_DATE={built_at}");

    // Re-run when HEAD moves, so the stamp cannot go stale on a rebuild that
    // would otherwise be a cache hit.
    if let Some(dir) = git(&["rev-parse", "--git-dir"]) {
        println!("cargo:rerun-if-changed={dir}/HEAD");
        println!("cargo:rerun-if-changed={dir}/index");
    }
}

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    if !out.status.success() {
        return None;
    }
    let s = String::from_utf8(out.stdout).ok()?.trim().to_string();
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}
