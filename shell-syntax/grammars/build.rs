//! Compiles the vendored tree-sitter grammars (git submodules under
//! `shell-syntax/vendor/`) when they are checked out. A grammar whose checkout is
//! missing falls back to its crates.io release, with a build warning.

use std::path::{Path, PathBuf};

/// (grammar name, the C symbol prefix `tree_sitter_<name>`).
const GRAMMARS: [&str; 3] = ["bash", "powershell", "batch"];

/// The external-scanner entry points a grammar's `scanner.c` defines.
const SCANNER_FNS: [&str; 5] = ["create", "destroy", "serialize", "deserialize", "scan"];

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    let manifest = PathBuf::from(std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR"));
    let vendor = manifest.join("..").join("vendor");
    let mut fallback = Vec::new();

    for name in GRAMMARS {
        println!("cargo::rustc-check-cfg=cfg(vendored_{name})");
        let dir = vendor.join(format!("tree-sitter-{name}"));
        let src = dir.join("src");
        println!("cargo:rerun-if-changed={}", src.display());
        if !src.join("parser.c").is_file() {
            fallback.push(name);
            continue;
        }
        compile(name, &src);
        println!("cargo:rustc-cfg=vendored_{name}");
    }

    if !fallback.is_empty() {
        println!(
            "cargo:warning=shell-syntax: not using the vendored tree-sitter grammar for {} \
             (shell-syntax/vendor/tree-sitter-<name> is not checked out; run \
             `git submodule update --init`). Falling back to the crates.io release, so \
             parse results may not be exactly as expected.",
            fallback.join(", ")
        );
    }
}

/// Builds one vendored grammar into a static library. Every public symbol is
/// renamed with a `ralphus_vendored_` prefix so it cannot collide with the same
/// grammar's crates.io build, which is also linked as the fallback.
fn compile(name: &str, src: &Path) {
    let mut build = cc::Build::new();
    build.include(src).warnings(false).std("c11");
    let language_fn = format!("tree_sitter_{name}");
    build.define(
        &language_fn,
        Some(format!("ralphus_vendored_{language_fn}").as_str()),
    );
    for f in SCANNER_FNS {
        let symbol = format!("tree_sitter_{name}_external_scanner_{f}");
        build.define(&symbol, Some(format!("ralphus_vendored_{symbol}").as_str()));
    }
    build.file(src.join("parser.c"));
    let scanner = src.join("scanner.c");
    if scanner.is_file() {
        build.file(scanner);
    }
    build.compile(&format!("ralphus_vendored_tree_sitter_{name}"));
}
