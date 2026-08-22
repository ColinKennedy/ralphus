//! Obfuscates the repo's `LICENSE` file into `$OUT_DIR/license.obf` at build
//! time (see `src/license.rs`), so every crate that depends on
//! `ralphus-core` picks up the current `LICENSE` contents automatically.
//! `cargo:rerun-if-changed` ties the regeneration to the source file, so a
//! `LICENSE` edit can never go stale relative to what gets embedded.

use std::env;
use std::fs;
use std::path::Path;

/// Must match `src/license.rs::KEY`.
const KEY: &[u8] = b"ralphus-license-obfuscation-key";

fn main() {
    let license_path = Path::new(env!("CARGO_MANIFEST_DIR")).join("../LICENSE");
    println!("cargo:rerun-if-changed={}", license_path.display());

    let plaintext = fs::read(&license_path)
        .unwrap_or_else(|e| panic!("could not read {}: {e}", license_path.display()));
    let obfuscated: Vec<u8> = plaintext
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ KEY[i % KEY.len()])
        .collect();

    let out_dir = env::var("OUT_DIR").expect("OUT_DIR set by cargo");
    let out_path = Path::new(&out_dir).join("license.obf");
    fs::write(&out_path, obfuscated)
        .unwrap_or_else(|e| panic!("could not write {}: {e}", out_path.display()));
}
