//! Starting a job's supervisor so it outlives the request that started it,
//! and stopping a job's whole process tree.

use std::process::{Child, Command, Stdio};

/// Why a detached start failed.
#[derive(Debug)]
pub enum DetachError {
    /// Windows refused to let the process leave the job object this one
    /// runs in (an SSH session's job, which kills everything in it when the
    /// session ends), so a detached process would not survive the request.
    #[cfg_attr(not(windows), allow(dead_code))]
    BreakawayRefused,
    /// Any other start failure.
    Other(String),
}

/// Start `command` with no stdio, in its own process group, outside any job
/// object this process is in. The child is reaped on a background thread so
/// a long-lived caller (`ralphus-outpost`) collects no zombies.
///
/// # Errors
/// [`DetachError::BreakawayRefused`] when Windows will not let the child
/// leave this process's job; [`DetachError::Other`] for anything else.
pub fn spawn_detached(mut command: Command) -> Result<(), DetachError> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let child = start_detached(command)?;
    std::thread::spawn(move || {
        let mut child = child;
        let _ = child.wait();
    });
    Ok(())
}

#[cfg(unix)]
fn start_detached(mut command: Command) -> Result<Child, DetachError> {
    use std::os::unix::process::CommandExt as _;
    command.process_group(0);
    command
        .spawn()
        .map_err(|e| DetachError::Other(e.to_string()))
}

#[cfg(windows)]
fn start_detached(mut command: Command) -> Result<Child, DetachError> {
    use std::os::windows::process::CommandExt as _;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const CREATE_BREAKAWAY_FROM_JOB: u32 = 0x0100_0000;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    const ERROR_ACCESS_DENIED: i32 = 5;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW | CREATE_BREAKAWAY_FROM_JOB);
    match command.spawn() {
        Ok(child) => Ok(child),
        Err(e) if e.raw_os_error() == Some(ERROR_ACCESS_DENIED) => {
            Err(DetachError::BreakawayRefused)
        }
        Err(e) => Err(DetachError::Other(e.to_string())),
    }
}

/// Prepare `command` so [`Tree::kill`] reaches everything it starts: its
/// own process group on Unix (assigned at spawn); on Windows a job object
/// is attached after spawn by [`Tree::confine`].
pub fn prepare_tree(command: &mut Command) {
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt as _;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
}

/// A started child and everything it starts.
pub struct Tree {
    #[cfg(windows)]
    job: Option<win32job::Job>,
}

impl Tree {
    /// Confine `child`'s future descendants with it. Call right after spawn.
    #[must_use]
    pub fn confine(child: &Child) -> Self {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle as _;
            let job = (|| -> Result<win32job::Job, win32job::JobError> {
                let job = win32job::Job::create()?;
                let mut info = win32job::ExtendedLimitInfo::new();
                info.limit_kill_on_job_close();
                job.set_extended_limit_info(&info)?;
                job.assign_process(child.as_raw_handle() as isize)?;
                Ok(job)
            })();
            Self { job: job.ok() }
        }
        #[cfg(unix)]
        {
            let _ = child;
            Self {}
        }
    }

    /// Stop `child` and every process it started.
    pub fn kill(&mut self, child: &mut Child) {
        #[cfg(windows)]
        {
            // Closing the job's only handle ends every process in it.
            self.job = None;
        }
        #[cfg(unix)]
        {
            if let Ok(pid) = i32::try_from(child.id()) {
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(pid),
                    nix::sys::signal::Signal::SIGTERM,
                );
                let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                while std::time::Instant::now() < deadline {
                    if matches!(child.try_wait(), Ok(Some(_))) {
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
                let _ = nix::sys::signal::killpg(
                    nix::unistd::Pid::from_raw(pid),
                    nix::sys::signal::Signal::SIGKILL,
                );
            }
        }
        let _ = child.kill();
    }
}
