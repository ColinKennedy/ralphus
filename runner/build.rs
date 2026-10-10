//! libgit2-sys 0.17 does not link `advapi32` on Windows, yet libgit2's owner
//! check (`git_fs_path_owner_is`) calls `OpenProcessToken`, `EqualSid` and
//! friends from it; a standalone link fails with LNK2019 unless something else
//! in the dependency graph happens to supply the library. Emitting it here
//! makes this crate's link (and its dependents', which inherit native-library
//! directives) self-contained.

/// A build script's stdout is the cargo directive channel, not stray output.
#[allow(clippy::print_stdout)]
fn main() {
    if std::env::var("CARGO_CFG_TARGET_OS").as_deref() == Ok("windows") {
        println!("cargo:rustc-link-lib=advapi32");
    }
    println!("cargo:rerun-if-changed=build.rs");
}
