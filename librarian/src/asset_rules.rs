//! The board's asset rules, shared between `build.rs` (which bakes the
//! release exe's embedded copies) and `src/assets.rs` (which serves dev-mode
//! reads from disk). One definition keeps both sides agreeing on what counts
//! as a board asset: the shell page and stylesheet, every JS chunk under
//! `board/`, and exactly the vendored xterm files — nothing else (editor temp
//! files, notes, binaries never ship).
//!
//! The module is compiled into two contexts (build script + library runtime)
//! and each context uses only part of the surface; the allow silences the
//! unused-half warning in each.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};

/// Route names that must always exist on disk and in the embedded table.
pub const REQUIRED_ASSETS: &[&str] = &[
    "board.html",
    "board.css",
    "vendor/xterm.css",
    "vendor/xterm.js",
    "vendor/xterm-fit.js",
];

/// Collect `(route name, disk path)` pairs for every board asset under
/// `assets_root`. Missing required assets panic — a broken assets tree must
/// fail loudly at build time rather than ship a release exe with a hole in
/// it. The result is sorted by route name for deterministic output.
pub fn collect_routes(assets_root: &Path) -> Vec<(String, PathBuf)> {
    let mut routes = Vec::new();
    for name in REQUIRED_ASSETS {
        let path = assets_root.join(name);
        assert!(
            path.is_file(),
            "missing required board asset: {}",
            path.display()
        );
        routes.push(((*name).to_string(), path));
    }
    collect_js_chunks(&assets_root.join("board"), "board", &mut routes);
    routes.sort();
    routes
}

/// Walk `board/` recursively, adding every `*.js` file (no dot-file
/// basenames) as a chunk. Anything else there — or anywhere else under
/// `assets_root` — is not an asset.
fn collect_js_chunks(dir: &Path, prefix: &str, routes: &mut Vec<(String, PathBuf)>) {
    let mut names: Vec<String> = Vec::new();
    let entries = fs::read_dir(dir)
        .unwrap_or_else(|e| panic!("cannot read board chunk dir {}: {e}", dir.display()));
    for entry in entries {
        let entry = entry.unwrap_or_else(|e| panic!("bad entry in {}: {e}", dir.display()));
        names.push(entry.file_name().to_string_lossy().to_string());
    }
    names.sort();
    for name in names {
        let path = dir.join(&name);
        if path.is_dir() {
            collect_js_chunks(&path, &format!("{prefix}/{name}"), routes);
        } else if name.ends_with(".js") && !name.starts_with('.') {
            routes.push((format!("{prefix}/{name}"), path));
        }
    }
}

/// Whether a route name may be served. Enforced on every dev-mode disk read
/// so the dev server can never be coaxed into reading an unrelated file, and
/// so stray files next to the real assets never leak into the page.
#[must_use]
pub fn is_allowed_route(name: &str) -> bool {
    if REQUIRED_ASSETS.contains(&name) {
        return true;
    }
    let Some(rest) = name.strip_prefix("board/") else {
        return false;
    };
    if rest.contains("..") || rest.contains('\\') {
        return false;
    }
    name.ends_with(".js")
        && !rest
            .split('/')
            .any(|seg| seg.is_empty() || seg.starts_with('.'))
}
