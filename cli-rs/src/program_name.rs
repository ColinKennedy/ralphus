//! Runtime resolution of the CLI executable's display name.

use std::ffi::OsStr;
use std::path::Path;

const DEFAULT_PROGRAM_NAME: &str = "ralphus";

/// Returns the running executable's basename without its platform extension.
///
/// `current_exe` is authoritative because `argv[0]` may be an alias or an
/// arbitrary caller-supplied value. The latter is retained as the required
/// fallback for environments where the executable path cannot be queried.
#[must_use]
pub fn resolve_program_name() -> String {
    let current_exe = std::env::current_exe().ok();
    let argv_zero = std::env::args_os().next();
    resolve_from(current_exe.as_deref(), argv_zero.as_deref())
}

fn resolve_from(current_exe: Option<&Path>, argv_zero: Option<&OsStr>) -> String {
    current_exe
        .and_then(path_program_name)
        .or_else(|| argv_zero.and_then(|value| path_program_name(Path::new(value))))
        .unwrap_or_else(|| DEFAULT_PROGRAM_NAME.to_string())
}

fn path_program_name(path: &Path) -> Option<String> {
    path.file_stem()
        .filter(|name| !name.is_empty())
        .map(|name| name.to_string_lossy().into_owned())
}

/// Substitutes CLI invocations written inside backticks while deliberately
/// leaving schema markers such as `ralphus:new-review/...` untouched.
#[must_use]
pub fn substitute_backticked_invocations(text: &str) -> String {
    let program = resolve_program_name();
    text.replace("`ralphus`", &format!("`{program}`"))
        .replace("`ralphus ", &format!("`{program} "))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_executable_wins_and_extension_is_removed() {
        assert_eq!(
            resolve_from(
                Some(Path::new("C:/tools/foo.exe")),
                Some(OsStr::new("ignored"))
            ),
            "foo"
        );
    }

    #[test]
    fn argv_zero_is_the_fallback() {
        assert_eq!(
            resolve_from(None, Some(OsStr::new("/opt/bin/branded"))),
            "branded"
        );
    }

    #[test]
    fn substitution_preserves_protocol_markers() {
        let text = substitute_backticked_invocations(
            "Use `ralphus submit x.toml` with `ralphus:new-review/key`.",
        );
        assert!(text.contains(" submit x.toml`"));
        assert!(text.contains("`ralphus:new-review/key`"));
    }
}
