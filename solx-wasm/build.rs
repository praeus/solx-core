use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let manifest_dir = PathBuf::from(
        env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR not set"),
    );
    let wit_source = manifest_dir.join("wit").join("custom-action.wit");

    println!("cargo:rerun-if-changed=wit/custom-action.wit");
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=Cargo.toml");

    if !wit_source.exists() {
        // Allow `cargo check` etc. to succeed even before the WIT is generated.
        // Downstream consumers that actually need the WIT will fail loudly
        // when they cannot find it.
        return;
    }

    // Copy the canonical WIT into OUT_DIR so downstream crates depending on
    // this one can locate it via the `DEP_SOL_CUSTOM_WIT_CUSTOM_WIT` env var
    // (emitted by Cargo from the `cargo:CUSTOM_WIT=...` directive below).
    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("OUT_DIR not set"));
    let dest = out_dir.join("custom-action.wit");
    fs::create_dir_all(&out_dir).expect("create OUT_DIR");
    fs::copy(&wit_source, &dest).expect("copy canonical custom-action.wit to OUT_DIR");

    println!("cargo:CUSTOM_WIT={}", dest.display());
}
