//! Confining every path a request names to the machine's `remote_root`.
//!
//! Paths are handled as text with `/` separators, the form the daemon sends
//! (`daemon/src/workspace.rs::remote_path_string`) and every path this module
//! returns: Windows' file APIs and git accept `/`, so one form serves both
//! kinds of machine. A path is inside the root when, after turning `\` into
//! `/`, it is the root or continues it with a `/` -- compared without regard
//! to case on Windows, whose file system is case-insensitive -- and has no
//! `.` or `..` component that could walk back out.

/// A validated, normalized storage root.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Root(String);

impl Root {
    /// Validate `root`: absolute on this machine, with no `.`/`..`
    /// components, and not the file-system root itself.
    ///
    /// # Errors
    /// Why `root` cannot be a storage root.
    pub fn new(root: &str) -> Result<Self, String> {
        let normalized = normalize(root);
        if !is_absolute(&normalized) {
            return Err(format!(
                "remote_root {root:?} must be an absolute path on this machine"
            ));
        }
        if has_dot_components(&normalized) {
            return Err(format!(
                "remote_root {root:?} must not contain '.' or '..' components"
            ));
        }
        let rest = strip_anchor(&normalized);
        if rest.trim_matches('/').is_empty() {
            return Err(format!(
                "remote_root {root:?} must be a directory below the file-system root"
            ));
        }
        Ok(Self(normalized))
    }

    /// The root as text, `/`-separated, without a trailing `/`.
    #[must_use]
    pub fn as_str(&self) -> &str {
        &self.0
    }

    /// `path` normalized, proven to be the root or inside it.
    ///
    /// # Errors
    /// `path` is relative, has a `.`/`..` component, or is outside the root.
    pub fn contain(&self, path: &str) -> Result<String, String> {
        let normalized = normalize(path);
        if !is_absolute(&normalized) {
            return Err(format!(
                "remote path {path:?} must be an absolute path on this machine"
            ));
        }
        if has_dot_components(&normalized) {
            return Err(format!(
                "remote path {path:?} must not contain '.' or '..' components"
            ));
        }
        let (candidate, root) = if cfg!(windows) {
            (normalized.to_lowercase(), self.0.to_lowercase())
        } else {
            (normalized.clone(), self.0.clone())
        };
        if candidate == root
            || candidate
                .strip_prefix(&root)
                .is_some_and(|rest| rest.starts_with('/'))
        {
            Ok(normalized)
        } else {
            Err(format!(
                "remote path {path:?} is outside configured remote_root {:?}",
                self.0
            ))
        }
    }

    /// `relative` joined onto the root.
    #[must_use]
    pub fn join(&self, relative: &str) -> String {
        format!("{}/{}", self.0, relative.trim_start_matches('/'))
    }
}

/// `path` with `\` turned into `/`, repeated `/` collapsed (except a leading
/// `//` UNC prefix) and no trailing `/` (except on a bare anchor).
#[must_use]
pub fn normalize(path: &str) -> String {
    let slashed = path.trim().replace('\\', "/");
    let (prefix, rest) = match slashed.strip_prefix("//") {
        Some(rest) => ("//", rest.to_string()),
        None => ("", slashed),
    };
    let mut out = String::from(prefix);
    let mut previous_slash = false;
    for c in rest.chars() {
        if c == '/' {
            if previous_slash {
                continue;
            }
            previous_slash = true;
        } else {
            previous_slash = false;
        }
        out.push(c);
    }
    while out.len() > 1 && out.ends_with('/') && !is_bare_anchor(&out) {
        out.pop();
    }
    out
}

fn is_bare_anchor(path: &str) -> bool {
    path == "/" || (path.len() == 3 && is_drive_prefix(path) && path.ends_with('/'))
}

fn is_drive_prefix(path: &str) -> bool {
    let bytes = path.as_bytes();
    bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':'
}

/// Whether `normalized` is absolute on this machine: `/x` elsewhere; `C:/x`
/// or `//server/share/x` on Windows.
fn is_absolute(normalized: &str) -> bool {
    if cfg!(windows) {
        (is_drive_prefix(normalized) && normalized[2..].starts_with('/'))
            || normalized.starts_with("//")
    } else {
        normalized.starts_with('/')
    }
}

/// `normalized` without its anchor (`/`, `C:/`, `//`).
fn strip_anchor(normalized: &str) -> &str {
    if is_drive_prefix(normalized) {
        &normalized[2..]
    } else {
        normalized
    }
}

fn has_dot_components(normalized: &str) -> bool {
    strip_anchor(normalized)
        .split('/')
        .any(|part| part == "." || part == "..")
}

/// The parent of a normalized path, if it has one below its anchor.
#[must_use]
pub fn parent(normalized: &str) -> Option<&str> {
    let (parent, name) = normalized.rsplit_once('/')?;
    if name.is_empty() || parent.is_empty() || parent.ends_with(':') {
        return None;
    }
    Some(parent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root() -> Root {
        Root::new(if cfg!(windows) {
            r"C:\ralphus\fleet"
        } else {
            "/srv/ralphus"
        })
        .unwrap()
    }

    fn inside() -> &'static str {
        if cfg!(windows) {
            "C:/ralphus/fleet/projects/p/worktrees/w"
        } else {
            "/srv/ralphus/projects/p/worktrees/w"
        }
    }

    #[test]
    fn normalize_uses_forward_slashes_without_doubles_or_trailing_ones() {
        assert_eq!(normalize(r"C:\a\\b\"), "C:/a/b");
        assert_eq!(normalize("/a//b/"), "/a/b");
        assert_eq!(normalize("/"), "/");
        assert_eq!(normalize("C:/"), "C:/");
        assert_eq!(normalize(r"\\server\share\x"), "//server/share/x");
    }

    #[test]
    fn a_path_inside_the_root_is_contained() {
        assert_eq!(root().contain(inside()).unwrap(), inside());
        assert_eq!(root().contain(root().as_str()).unwrap(), root().as_str());
    }

    #[test]
    fn a_sibling_with_the_root_as_a_prefix_is_outside() {
        let sibling = format!("{}-evil/x", root().as_str());
        assert!(root().contain(&sibling).is_err());
    }

    #[test]
    fn dot_dot_cannot_walk_out_of_the_root() {
        let escape = format!("{}/../../etc", root().as_str());
        let err = root().contain(&escape).unwrap_err();
        assert!(err.contains(".."), "{err}");
    }

    #[test]
    fn relative_paths_and_bad_roots_are_refused() {
        assert!(root().contain("relative/x").is_err());
        assert!(Root::new("relative").is_err());
        assert!(Root::new("/").is_err());
        assert!(Root::new("C:/").is_err());
        assert!(Root::new("/srv/../etc").is_err());
    }

    #[cfg(windows)]
    #[test]
    fn windows_compares_without_case_and_accepts_backslashes() {
        assert!(root().contain(r"c:\RALPHUS\fleet\projects").is_ok());
        assert!(root().contain("/ralphus/fleet").is_err(), "drive-relative");
    }

    #[test]
    fn parent_stops_at_the_anchor() {
        assert_eq!(parent("/a/b/c"), Some("/a/b"));
        assert_eq!(parent("C:/a/b"), Some("C:/a"));
        assert_eq!(parent("C:/a"), None);
        assert_eq!(parent("/a"), None);
    }
}
