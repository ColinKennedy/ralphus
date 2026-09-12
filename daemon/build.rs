//! Embeds `windows_resource.rc` (FileDescription/ProductName = "Ralphus") into
//! `ralphus-daemon.exe` so Task Manager shows a friendly name instead of the
//! raw filename (RAL-99). A no-op when not building for Windows.
//!
//! Also checks, when the `embedded-tmux` feature is on, that the binary
//! `daemon/src/tmux.rs`'s `embedded` module `include_bytes!`s has actually
//! been built (RAL-347: vendorized from the `vendor/psmux` git submodule via
//! `scripts/build-vendored-tmux.ps1`) -- turning a confusing
//! `include_bytes!` compile error into an actionable one.

fn main() {
    embed_resource::compile("windows_resource.rc", embed_resource::NONE)
        .manifest_optional()
        .unwrap();

    let targeting_windows = std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows");
    if targeting_windows && std::env::var_os("CARGO_FEATURE_EMBEDDED_TMUX").is_some() {
        let asset = std::path::Path::new("assets/tmux/windows/tmux.exe");
        if !asset.is_file() {
            panic!(
                "`embedded-tmux` is enabled but {} is missing. Build the vendored psmux \
                 submodule first: scripts\\build-vendored-tmux.ps1 (see docs/tmux-embedding.md \
                 and daemon/assets/tmux/windows/README.md).",
                asset.display()
            );
        }
        println!("cargo:rerun-if-changed=assets/tmux/windows/tmux.exe");
    }
}
