//! The Bash, PowerShell and batch (cmd) tree-sitter grammars ralphus parses
//! agent shell commands with.
//!
//! Each grammar is the patched copy vendored as a git submodule under
//! `shell-syntax/vendor/` when that checkout exists at build time, and the
//! crates.io release otherwise (`build.rs` prints a warning in that case).
//! [`sources`] reports which one a build ended up with so a binary can say so
//! at run time.

use tree_sitter_language::LanguageFn;

/// Which implementation of a grammar is compiled in.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Source {
    /// The patched submodule under `shell-syntax/vendor/`.
    Vendored,
    /// The unpatched crates.io release.
    CratesIo,
}

/// A grammar and where it came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Grammar {
    pub name: &'static str,
    pub source: Source,
}

macro_rules! grammar {
    ($fn_name:ident, $cfg:ident, $symbol:ident, $krate:ident) => {
        #[cfg($cfg)]
        pub fn $fn_name() -> LanguageFn {
            unsafe extern "C" {
                fn $symbol() -> *const ();
            }
            // SAFETY: `$symbol` is the grammar's generated `tree_sitter_<name>`
            // entry point, which takes no arguments and returns a static `TSLanguage`.
            unsafe { LanguageFn::from_raw($symbol) }
        }

        #[cfg(not($cfg))]
        pub fn $fn_name() -> LanguageFn {
            $krate::LANGUAGE
        }
    };
}

grammar!(
    bash,
    vendored_bash,
    ralphus_vendored_tree_sitter_bash,
    tree_sitter_bash
);
grammar!(
    powershell,
    vendored_powershell,
    ralphus_vendored_tree_sitter_powershell,
    tree_sitter_powershell
);
grammar!(
    batch,
    vendored_batch,
    ralphus_vendored_tree_sitter_batch,
    tree_sitter_batch
);

const fn source(vendored: bool) -> Source {
    if vendored {
        Source::Vendored
    } else {
        Source::CratesIo
    }
}

/// The source of every grammar in this build.
pub const fn sources() -> [Grammar; 3] {
    [
        Grammar {
            name: "bash",
            source: source(cfg!(vendored_bash)),
        },
        Grammar {
            name: "powershell",
            source: source(cfg!(vendored_powershell)),
        },
        Grammar {
            name: "batch",
            source: source(cfg!(vendored_batch)),
        },
    ]
}

/// Names of the grammars that fell back to the crates.io release.
pub fn fallbacks() -> Vec<&'static str> {
    sources()
        .iter()
        .filter(|g| g.source == Source::CratesIo)
        .map(|g| g.name)
        .collect()
}
