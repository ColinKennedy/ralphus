//! Binary-safe provider-to-daemon artifact materialization.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::Deserialize;

use crate::exec::EffectiveConfig;
use crate::job::{optional_remote_root, require_under_root};
use crate::transport::shell_quote_single;
use crate::{ssh, uri};

#[derive(Deserialize)]
struct MaterializeRequest {
    source: String,
    destination: String,
    #[serde(default)]
    executable: bool,
}

pub fn run(uri: &str, payload: &str, config: &EffectiveConfig) -> Result<(), String> {
    let target = uri::parse(uri).map_err(|e| e.to_string())?;
    let request: MaterializeRequest = serde_json::from_str(payload)
        .map_err(|e| format!("could not parse materialize request: {e}"))?;
    if !request.source.starts_with('/') {
        return Err("materialize source must be an absolute POSIX path".to_string());
    }
    if let Some(root) = optional_remote_root(config)? {
        require_under_root(&request.source, &root)?;
    }
    let destination = PathBuf::from(&request.destination);
    if !destination.is_absolute() {
        return Err("materialize destination must be absolute on the daemon host".to_string());
    }
    let (remote_parent, remote_name) = request
        .source
        .trim_end_matches('/')
        .rsplit_once('/')
        .filter(|(_, name)| !name.is_empty() && *name != "." && *name != "..")
        .ok_or_else(|| {
            format!(
                "materialize source is not a file or directory path: {:?}",
                request.source
            )
        })?;

    let parent = destination.parent().ok_or_else(|| {
        format!(
            "materialize destination has no parent: {}",
            destination.display()
        )
    })?;
    std::fs::create_dir_all(parent).map_err(|e| {
        format!(
            "could not create materialize destination {}: {e}",
            parent.display()
        )
    })?;
    let mut nonce = [0_u8; 8];
    getrandom::getrandom(&mut nonce).map_err(|e| format!("could not create transfer id: {e}"))?;
    let suffix = nonce
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let temporary = parent.join(format!(".ralphus-materialize-{suffix}"));
    std::fs::create_dir(&temporary).map_err(|e| {
        format!(
            "could not create transfer staging directory {}: {e}",
            temporary.display()
        )
    })?;

    let remote_command = format!(
        "set -eu; test -e {source}; tar -cf - -C {parent} {name}",
        source = shell_quote_single(&request.source),
        parent = shell_quote_single(remote_parent),
        name = shell_quote_single(remote_name),
    );
    let args = ssh::command_args(
        &target.to_string(),
        config.connect_timeout_secs,
        &remote_command,
        config.ssh_config_file.as_deref(),
    );
    let mut ssh_child = Command::new("ssh")
        .args(&args)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not start ssh for artifact materialization: {e}"))?;
    let ssh_stdout = ssh_child
        .stdout
        .take()
        .ok_or_else(|| "ssh artifact stream has no stdout".to_string())?;
    let tar_output = Command::new("tar")
        .args(["-xf", "-", "-C"])
        .arg(&temporary)
        .stdin(Stdio::from(ssh_stdout))
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .output()
        .map_err(|e| format!("could not start local tar for artifact materialization: {e}"));
    let ssh_output = ssh_child
        .wait_with_output()
        .map_err(|e| format!("could not wait for artifact ssh stream: {e}"));
    let result = match (tar_output, ssh_output) {
        (Ok(tar), Ok(ssh_result)) if tar.status.success() && ssh_result.status.success() => {
            let extracted = temporary.join(remote_name);
            validate_extracted_tree(&extracted)?;
            remove_existing(&destination)?;
            std::fs::rename(&extracted, &destination).map_err(|e| {
                format!(
                    "could not publish artifact at {}: {e}",
                    destination.display()
                )
            })?;
            if request.executable && destination.is_file() {
                set_executable(&destination)?;
            }
            Ok(())
        }
        (Ok(_), Ok(ssh_result)) if !ssh_result.status.success() => {
            let stderr = String::from_utf8_lossy(&ssh_result.stderr);
            Err(ssh::interpret_failure(
                "ssh",
                ssh_result.status.code(),
                &stderr,
            ))
        }
        (Ok(tar), Ok(_)) => Err(format!(
            "local tar could not extract the artifact: {}",
            String::from_utf8_lossy(&tar.stderr).trim()
        )),
        (Err(error), _) | (_, Err(error)) => Err(error),
    };
    let _ = std::fs::remove_dir_all(&temporary);
    result
}

fn remove_existing(path: &Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            std::fs::remove_dir_all(path).map_err(|e| e.to_string())
        }
        Ok(_) => std::fs::remove_file(path).map_err(|e| e.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.to_string()),
    }
}

fn validate_extracted_tree(root: &Path) -> Result<(), String> {
    let canonical_root = root.canonicalize().map_err(|e| {
        format!(
            "materialized artifact {} is missing or unreadable: {e}",
            root.display()
        )
    })?;
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let metadata = std::fs::symlink_metadata(&path).map_err(|e| e.to_string())?;
        if metadata.file_type().is_symlink() {
            let resolved = path.canonicalize().map_err(|e| e.to_string())?;
            if !resolved.starts_with(&canonical_root) {
                return Err(format!(
                    "materialized artifact contains an escaping symlink: {}",
                    path.display()
                ));
            }
        } else if metadata.is_dir() {
            for entry in std::fs::read_dir(&path).map_err(|e| e.to_string())? {
                pending.push(entry.map_err(|e| e.to_string())?.path());
            }
        }
    }
    Ok(())
}

#[cfg(unix)]
fn set_executable(path: &Path) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mut permissions = std::fs::metadata(path)
        .map_err(|e| e.to_string())?
        .permissions();
    permissions.set_mode(permissions.mode() | 0o111);
    std::fs::set_permissions(path, permissions).map_err(|e| e.to_string())
}

#[cfg(not(unix))]
fn set_executable(_path: &Path) -> Result<(), String> {
    Ok(())
}
