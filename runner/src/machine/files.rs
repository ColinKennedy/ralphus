//! `read-file`, `write-file`, `remove-path`, `materialize` and `cleanup`:
//! file access under the machine's root, with no shell in between.

use std::path::Path;

use base64::Engine as _;
use serde_json::{Value, json};

use super::Events;
use super::paths::{self, Root};

fn request_path(request: &Value, root: &Root, verb: &str) -> Result<String, String> {
    let path = super::required_str(request, "path", &format!("{verb} request"))?;
    root.contain(path)
}

/// `read-file`: the file's contents. A missing or unreadable file is an
/// error, which the daemon reads as "absent".
///
/// # Errors
/// A malformed request, a path outside the root, or an unreadable file.
pub fn read_file(request: &Value, root: &Root) -> Result<String, String> {
    let path = request_path(request, root, "read-file")?;
    std::fs::read(&path)
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .map_err(|e| format!("could not read {path}: {e}"))
}

/// `write-file`: write `content` through a temporary file and a rename, so a
/// reader never sees a partial file, creating parent directories.
///
/// # Errors
/// A malformed request, a path outside the root, or a write failure.
pub fn write_file(request: &Value, root: &Root) -> Result<(), String> {
    let path = request_path(request, root, "write-file")?;
    let content = request
        .get("content")
        .and_then(Value::as_str)
        .ok_or_else(|| "write-file request is missing \"content\"".to_string())?;
    let executable = request
        .get("executable")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    write_atomically(Path::new(&path), content.as_bytes(), executable)
}

/// Write `bytes` to `path` via `<path>.tmp` and a rename; private to the
/// machine's user on Unix (`0600`, or `0700` when `executable`).
pub fn write_atomically(path: &Path, bytes: &[u8], executable: bool) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }
    let mut temporary = path.as_os_str().to_owned();
    temporary.push(".tmp");
    let temporary = std::path::PathBuf::from(temporary);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        options.mode(if executable { 0o700 } else { 0o600 });
    }
    let written = options.open(&temporary).and_then(|mut file| {
        use std::io::Write as _;
        file.write_all(bytes)?;
        file.sync_all()
    });
    if let Err(e) = written {
        let _ = std::fs::remove_file(&temporary);
        return Err(format!("could not write {}: {e}", temporary.display()));
    }
    #[cfg(unix)]
    if executable {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("could not make {} executable: {e}", temporary.display()))?;
    }
    #[cfg(not(unix))]
    let _ = executable;
    std::fs::rename(&temporary, path).map_err(|e| {
        let _ = std::fs::remove_file(&temporary);
        format!("could not move {} into place: {e}", path.display())
    })
}

/// `remove-path`: delete a file, or a directory tree when `recursive`. A
/// path that does not exist is success.
///
/// # Errors
/// A malformed request, a path outside the root, a directory without
/// `recursive`, or a removal failure.
pub fn remove_path(request: &Value, root: &Root, events: &mut Events) -> Result<(), String> {
    let path = request_path(request, root, "remove-path")?;
    let recursive = request
        .get("recursive")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    match std::fs::symlink_metadata(&path) {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(e) => return Err(format!("could not inspect {path}: {e}")),
        Ok(metadata) if metadata.is_dir() => {
            if !recursive {
                return Err(format!(
                    "{path} is a directory; removing it needs recursive"
                ));
            }
            remove_tree(Path::new(&path))?;
        }
        Ok(_) => remove_file(Path::new(&path))?,
    }
    events.emit(
        "removed remote path",
        "info",
        json!({"path": path, "recursive": recursive}),
    );
    Ok(())
}

fn remove_file(path: &Path) -> Result<(), String> {
    std::fs::remove_file(path)
        .or_else(|first| {
            // Git marks its object files read-only, which Windows refuses to
            // delete until the attribute is cleared.
            clear_readonly(path);
            std::fs::remove_file(path).map_err(|_| first)
        })
        .map_err(|e| format!("could not remove {}: {e}", path.display()))
}

/// Remove a directory tree, clearing read-only attributes (git's object
/// files on Windows) when a plain removal is refused. A tree that is already
/// gone is success.
pub fn remove_tree(path: &Path) -> Result<(), String> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => return Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(_) => {}
    }
    let mut pending = vec![path.to_path_buf()];
    while let Some(dir) = pending.pop() {
        clear_readonly(&dir);
        let Ok(entries) = std::fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let entry_path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => pending.push(entry_path),
                _ => clear_readonly(&entry_path),
            }
        }
    }
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(format!("could not remove {}: {e}", path.display())),
    }
}

fn clear_readonly(path: &Path) {
    if let Ok(metadata) = std::fs::symlink_metadata(path) {
        let mut permissions = metadata.permissions();
        if permissions.readonly() && !metadata.file_type().is_symlink() {
            #[allow(clippy::permissions_set_readonly_false)]
            permissions.set_readonly(false);
            let _ = std::fs::set_permissions(path, permissions);
        }
    }
}

/// `materialize`: the file at `source`, or every file under the directory at
/// `source`, as `{path, content_base64, executable}` entries with
/// `/`-separated paths relative to `source` (`""` for a single file). A
/// symbolic link inside the tree is followed when it stays inside `source`
/// and refused when it leaves it.
///
/// # Errors
/// A malformed request, a source outside the root, a missing source, an
/// escaping link, or an unreadable file.
pub fn materialize(request: &Value, root: &Root) -> Result<Vec<Value>, String> {
    let source = super::required_str(request, "source", "materialize request")?;
    let source = root.contain(source)?;
    let executable = request
        .get("executable")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let encode = |path: &Path| {
        std::fs::read(path)
            .map(|bytes| base64::engine::general_purpose::STANDARD.encode(bytes))
            .map_err(|e| format!("could not read {}: {e}", path.display()))
    };
    let source_path = Path::new(&source);
    let metadata = std::fs::metadata(source_path)
        .map_err(|e| format!("materialize source {source} is missing or unreadable: {e}"))?;
    if metadata.is_file() {
        return Ok(vec![json!({
            "path": "",
            "content_base64": encode(source_path)?,
            "executable": executable,
        })]);
    }
    let canonical_root = source_path
        .canonicalize()
        .map_err(|e| format!("could not resolve {source}: {e}"))?;
    let mut files = Vec::new();
    let mut pending = vec![source_path.to_path_buf()];
    while let Some(dir) = pending.pop() {
        let entries = std::fs::read_dir(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        for entry in entries {
            let path = entry.map_err(|e| e.to_string())?.path();
            let link = std::fs::symlink_metadata(&path)
                .map_err(|e| e.to_string())?
                .file_type()
                .is_symlink();
            if link {
                let resolved = path.canonicalize().map_err(|e| e.to_string())?;
                if !resolved.starts_with(&canonical_root) {
                    return Err(format!(
                        "materialize source contains an escaping symlink: {}",
                        path.display()
                    ));
                }
            }
            if path.is_dir() {
                if !link {
                    pending.push(path);
                }
                continue;
            }
            let relative = path
                .strip_prefix(source_path)
                .map_err(|e| e.to_string())?
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            files.push(json!({
                "path": relative,
                "content_base64": encode(&path)?,
                "executable": false,
            }));
        }
    }
    Ok(files)
}

/// `cleanup`: remove one provisioned worktree (`branch` given) or a whole
/// project directory, returning the path removed.
///
/// # Errors
/// A malformed request, a derived path outside the root, or a removal
/// failure.
pub fn cleanup(request: &Value, root: &Root) -> Result<String, String> {
    let project = super::required_str(request, "project", "cleanup request")?;
    let clone_url = super::required_str(request, "clone_url", "cleanup request")?;
    if let Some(asked) = request
        .get("remote_root")
        .and_then(Value::as_str)
        .filter(|r| !r.trim().is_empty())
    {
        if paths::normalize(asked) != root.as_str() {
            return Err(format!(
                "cleanup asked for remote_root {asked:?}, but this machine's is {:?}",
                root.as_str()
            ));
        }
    }
    let project_dir = ralphus_core::machine_layout::project_dir(root.as_str(), project, clone_url);
    let target = match request
        .get("branch")
        .and_then(Value::as_str)
        .filter(|b| !b.trim().is_empty())
    {
        Some(branch) => ralphus_core::machine_layout::worktree_dir(&project_dir, branch),
        None => project_dir,
    };
    let target = root.contain(&target)?;
    remove_tree(Path::new(&target))?;
    Ok(target)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn root(dir: &Path) -> Root {
        Root::new(&dir.to_string_lossy()).unwrap()
    }

    fn slashed(path: &Path) -> String {
        path.to_string_lossy().replace('\\', "/")
    }

    #[test]
    fn a_written_file_reads_back_and_is_removed() {
        let dir = ralphus_test_support::TestDir::new("machine-files");
        let root = root(&dir);
        let path = format!("{}/nested/a/file.txt", slashed(&dir));
        write_file(&json!({"path": path, "content": "hello\n"}), &root).unwrap();
        assert_eq!(read_file(&json!({"path": path}), &root).unwrap(), "hello\n");
        let mut events = Events::default();
        remove_path(&json!({"path": path}), &root, &mut events).unwrap();
        assert!(read_file(&json!({"path": path}), &root).is_err());
        remove_path(&json!({"path": path}), &root, &mut events).expect("already gone is fine");
        let nested = format!("{}/nested", slashed(&dir));
        assert!(remove_path(&json!({"path": nested}), &root, &mut events).is_err());
        remove_path(
            &json!({"path": nested, "recursive": true}),
            &root,
            &mut events,
        )
        .unwrap();
        assert!(!Path::new(&nested).exists());
    }

    #[test]
    fn files_outside_the_root_are_refused() {
        let dir = ralphus_test_support::TestDir::new("machine-files-outside");
        let outside = format!("{}/x.txt", slashed(dir.parent().unwrap()));
        let err = write_file(&json!({"path": outside, "content": "x"}), &root(&dir)).unwrap_err();
        assert!(err.contains("outside"), "{err}");
        assert!(!Path::new(&outside).exists());
    }

    #[test]
    fn a_materialized_tree_comes_back_with_relative_paths() {
        let dir = ralphus_test_support::TestDir::new("machine-materialize");
        std::fs::create_dir_all(dir.join("out").join("clips")).unwrap();
        std::fs::write(dir.join("out").join("a.png"), "a").unwrap();
        std::fs::write(dir.join("out").join("clips").join("b.gif"), "b").unwrap();
        let source = format!("{}/out", slashed(&dir));
        let mut paths: Vec<String> = materialize(&json!({"source": source}), &root(&dir))
            .unwrap()
            .iter()
            .map(|f| f["path"].as_str().unwrap().to_string())
            .collect();
        paths.sort();
        assert_eq!(paths, ["a.png", "clips/b.gif"]);
        let single = format!("{}/out/a.png", slashed(&dir));
        let files =
            materialize(&json!({"source": single, "executable": true}), &root(&dir)).unwrap();
        assert_eq!(files[0]["path"], "");
        assert_eq!(files[0]["content_base64"], "YQ==");
        assert_eq!(files[0]["executable"], true);
    }

    #[test]
    fn cleanup_removes_one_worktree_or_the_whole_project() {
        let dir = ralphus_test_support::TestDir::new("machine-cleanup");
        let root = root(&dir);
        let project =
            ralphus_core::machine_layout::project_dir(root.as_str(), "proj", "file:///x.git");
        let worktree = ralphus_core::machine_layout::worktree_dir(&project, "feature/x");
        std::fs::create_dir_all(&worktree).unwrap();
        std::fs::write(Path::new(&worktree).join("f"), "x").unwrap();
        let removed = cleanup(
            &json!({"project": "proj", "clone_url": "file:///x.git", "branch": "feature/x"}),
            &root,
        )
        .unwrap();
        assert_eq!(removed, worktree);
        assert!(!Path::new(&worktree).exists());
        assert!(Path::new(&project).exists());
        let removed = cleanup(
            &json!({"project": "proj", "clone_url": "file:///x.git"}),
            &root,
        )
        .unwrap();
        assert_eq!(removed, project);
        assert!(!Path::new(&project).exists());
    }
}
