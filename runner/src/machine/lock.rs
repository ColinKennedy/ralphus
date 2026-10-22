//! An exclusive lock on a file, held until dropped and released by the
//! operating system if the holder dies: `flock` on Unix (as the SSH
//! provider's scripts take it), an open handle that shares nothing on
//! Windows.

use std::path::Path;
use std::time::{Duration, Instant};

/// A held lock.
pub struct FileLock {
    #[cfg(unix)]
    _held: nix::fcntl::Flock<std::fs::File>,
    #[cfg(windows)]
    _held: std::fs::File,
}

/// Take the lock at `path`, creating the file, waiting up to `timeout`.
///
/// # Errors
/// The file cannot be opened, or another holder kept it past `timeout`.
pub fn lock(path: &Path, timeout: Duration) -> Result<FileLock, String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }
    let started = Instant::now();
    loop {
        match try_lock(path) {
            Ok(Some(lock)) => return Ok(lock),
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(100));
            }
            Ok(None) => {
                return Err(format!(
                    "{} is still locked by another operation after {}s",
                    path.display(),
                    timeout.as_secs()
                ));
            }
            Err(e) => return Err(format!("could not lock {}: {e}", path.display())),
        }
    }
}

#[cfg(unix)]
fn try_lock(path: &Path) -> std::io::Result<Option<FileLock>> {
    use nix::fcntl::{Flock, FlockArg};
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(path)?;
    match Flock::lock(file, FlockArg::LockExclusiveNonblock) {
        Ok(held) => Ok(Some(FileLock { _held: held })),
        Err((_, nix::errno::Errno::EWOULDBLOCK)) => Ok(None),
        Err((_, errno)) => Err(std::io::Error::from(errno)),
    }
}

#[cfg(windows)]
fn try_lock(path: &Path) -> std::io::Result<Option<FileLock>> {
    use std::os::windows::fs::OpenOptionsExt as _;
    const ERROR_SHARING_VIOLATION: i32 = 32;
    const ERROR_ACCESS_DENIED: i32 = 5;
    match std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .share_mode(0)
        .open(path)
    {
        Ok(file) => Ok(Some(FileLock { _held: file })),
        Err(e)
            if matches!(
                e.raw_os_error(),
                Some(ERROR_SHARING_VIOLATION | ERROR_ACCESS_DENIED)
            ) =>
        {
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_second_holder_waits_and_gets_it_after_release() {
        let dir = ralphus_test_support::TestDir::new("machine-lock");
        let path = dir.join("a.lock");
        let first = lock(&path, Duration::from_secs(1)).unwrap();
        let err = lock(&path, Duration::from_millis(300))
            .err()
            .expect("still held");
        assert!(err.contains("still locked"), "{err}");
        drop(first);
        assert!(lock(&path, Duration::from_secs(1)).is_ok());
    }
}
