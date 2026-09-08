//! Board asset resolution for the librarian.
//!
//! Every board asset (page shell, stylesheet, JS chunks, vendored xterm
//! files) exists in two forms under the same route name: an embedded copy
//! baked into the exe by `build.rs` (used whenever no dev override is set, so
//! a release binary is self-contained), and an on-disk copy under the assets
//! folder (used when `RALPHUS_BOARD_ASSETS_DIR` points at it — the
//! day-to-day dev loop: edit a chunk, refresh the browser, no rebuild).
//!
//! Route names are validated by the shared rules in [`crate::asset_rules`]
//! before any disk read.
include!(concat!(env!("OUT_DIR"), "/board_assets.rs"));

/// Environment variable that switches the librarian into dev mode: board
/// assets are read from this folder on every request instead of the embedded
/// copies.
pub const DEV_ASSETS_DIR_ENV: &str = "RALPHUS_BOARD_ASSETS_DIR";

/// Re-exported for the server's route validation.
#[must_use]
pub fn is_allowed_route(name: &str) -> bool {
    crate::asset_rules::is_allowed_route(name)
}

/// Resolve a board asset route name (e.g. `board/05-engines.js`,
/// `board.css`) to bytes. Returns `None` for unknown routes and for
/// dev-mode files that don't exist on disk.
#[must_use]
pub fn board_asset(name: &str) -> Option<String> {
    match std::env::var_os(DEV_ASSETS_DIR_ENV) {
        Some(dir) if !dir.is_empty() => dev_disk_asset(std::path::Path::new(&dir), name),
        _ => embedded_asset(name).map(str::to_string),
    }
}

/// The embedded copy of a board asset (release serving path).
#[must_use]
pub fn embedded_asset(name: &str) -> Option<&'static str> {
    BOARD_ASSETS
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(_, s)| *s)
}

/// Dev-mode read: fetch `name` from `dev_dir` after validating it against
/// the shared asset rules (unknown or traversal-y names are rejected, never
/// read from disk). Missing files on disk yield `None` — dev mode serves
/// only real, current files.
#[must_use]
pub fn dev_disk_asset(dev_dir: &std::path::Path, name: &str) -> Option<String> {
    if !crate::asset_rules::is_allowed_route(name) {
        return None;
    }
    std::fs::read_to_string(dev_dir.join(name)).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn assets_root() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("assets")
    }

    /// build.rs's generated table must match an independent walk of the
    /// assets folder under the shared rules — catching both under-grab (a
    /// real asset missing from the exe) and over-grab (a stray file baked in).
    #[test]
    fn embedded_table_matches_the_assets_folder() {
        let mut expected: Vec<String> = crate::asset_rules::collect_routes(&assets_root())
            .into_iter()
            .map(|(n, _)| n)
            .collect();
        let mut actual: Vec<&str> = BOARD_ASSETS.iter().map(|(n, _)| *n).collect();
        expected.sort();
        actual.sort();
        assert_eq!(actual, expected);
    }

    /// Every local page reference (link href / script src) must be embedded —
    /// otherwise a release exe would serve a board whose first page load 404s
    /// half its scripts.
    #[test]
    fn every_page_reference_is_embedded() {
        let shell = embedded_asset("board.html").expect("shell embedded");
        for quote in ["src=\"", "href=\""] {
            let mut from = 0;
            while let Some(at) = shell[from..].find(quote) {
                let rest = &shell[from + at + quote.len()..];
                let end = rest.find('"').expect("unterminated page reference");
                let target = &rest[..end];
                if let Some(route) = target.strip_prefix('/') {
                    assert!(
                        embedded_asset(route).is_some(),
                        "board.html references /{route} which is not embedded"
                    );
                }
                from += at + quote.len() + end + 1;
            }
        }
    }

    /// The converse: everything embedded is the shell itself or referenced
    /// from it — nothing dead ships in the exe.
    #[test]
    fn every_embedded_asset_is_referenced_or_the_shell() {
        let shell = embedded_asset("board.html").expect("shell embedded");
        for (name, _) in BOARD_ASSETS {
            if *name == "board.html" {
                continue;
            }
            let needle = format!("=\"/{}\"", name);
            assert!(
                shell.contains(&needle),
                "embedded asset {name} is never referenced by board.html"
            );
        }
    }

    /// Dev-mode disk reads return the on-disk bytes and reject junk route
    /// names (editor temp files, traversal attempts, unrelated files).
    #[test]
    fn dev_disk_asset_reads_real_files_and_rejects_junk() {
        let dev_dir = assets_root();
        let css = dev_disk_asset(&dev_dir, "board.css").expect("board.css on disk");
        assert_eq!(
            css,
            std::fs::read_to_string(dev_dir.join("board.css")).unwrap()
        );
        assert!(dev_disk_asset(&dev_dir, "board/foo.js~").is_none());
        assert!(dev_disk_asset(&dev_dir, "board/../Cargo.toml").is_none());
        assert!(dev_disk_asset(&dev_dir, "unknown.txt").is_none());
    }
}
