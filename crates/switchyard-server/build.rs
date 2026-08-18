//! Build script: discover external filter crates and generate their
//! registration code.
//!
//! Mirrors praxis-ai's `server/build.rs` (via `praxis-ai-build-support`): scan
//! this crate's direct runtime dependencies for the
//! `[package.metadata.praxis-filters]` marker and emit
//! `fn register_external_filters(&mut FilterRegistry)` into `$OUT_DIR`, which
//! `src/main.rs` includes.

use std::path::Path;

/// Discovers external filter crates and writes the generated registration code.
///
/// # Errors
///
/// Returns an error if `cargo metadata` fails, `OUT_DIR` is unset, or the
/// generated file cannot be written.
fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut command = cargo_metadata::MetadataCommand::new();
    command.manifest_path(concat!(env!("CARGO_MANIFEST_DIR"), "/Cargo.toml"));
    let metadata = command.exec()?;

    let crates = build_support::discover_external_filter_crate_names(&metadata);
    let code = build_support::generate_registration_code(&crates);

    let out_dir = std::env::var("OUT_DIR")?;
    let dest = Path::new(&out_dir).join("external_filters.rs");
    std::fs::write(&dest, code)?;

    println!("cargo:rerun-if-changed=Cargo.toml");
    println!("cargo:rerun-if-changed=build.rs");
    Ok(())
}
