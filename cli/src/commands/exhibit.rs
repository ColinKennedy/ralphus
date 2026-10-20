//! `ralphus exhibit <subcommand>`: capture a still or a short recording of a
//! program's window, or of the screen, for a live action's exhibits. Runs on
//! this machine only -- it never talks to the daemon. See
//! [`crate::capture`] for how each OS is captured.
//!
//! A relative output path is written into `$RALPHUS_EXHIBIT_DIR` when that
//! is set (a live action's exhibit directory), so an agent can write
//! `-o view.png` and report `view.png`.
//!
//! Parsing reads the flags only; how they combine (one target, the right
//! file type) is checked by [`CaptureArgs::check`] when the command runs.

use std::path::PathBuf;

use crate::args::GlobalOpts;
use crate::capture::{RecordFormat, Target};

/// Default recording length, in seconds.
const DEFAULT_SECONDS: u32 = 5;
/// Default recording rate, in frames per second.
const DEFAULT_FPS: u32 = 10;
/// The longest recording `record` takes.
const MAX_SECONDS: u32 = 60;
/// The highest recording rate `record` takes.
const MAX_FPS: u32 = 30;

/// What to capture and where, as given on the command line.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CaptureArgs {
    pub pid: Option<u32>,
    pub screen: bool,
    pub out: Option<String>,
}

impl CaptureArgs {
    /// The target and output file, when the flags make sense together: one of
    /// `--pid`/`--screen`, and an output whose type matches (`.png` for a
    /// still, `.gif`/`.mp4` for a recording).
    ///
    /// # Errors
    ///
    /// A message saying what to change.
    pub fn check(&self, still: bool) -> Result<(Target, String), String> {
        let target = match (self.pid, self.screen) {
            (Some(_), true) => return Err("give --pid or --screen, not both".to_string()),
            (Some(pid), false) => Target::Pid(pid),
            (None, true) => Target::Screen,
            (None, false) => {
                return Err("say what to capture: --pid <process id> or --screen".to_string());
            }
        };
        let Some(out) = self.out.clone() else {
            return Err("give the output file with -o <file>".to_string());
        };
        if still && !out.to_ascii_lowercase().ends_with(".png") {
            return Err("a screenshot is written as a .png file".to_string());
        }
        if !still && RecordFormat::for_path(std::path::Path::new(&out)).is_none() {
            return Err("a recording is written as a .gif or .mp4 file".to_string());
        }
        Ok((target, out))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExhibitCommand {
    Help,
    Screenshot(CaptureArgs),
    Record {
        capture: CaptureArgs,
        seconds: u32,
        fps: u32,
    },
    UsageError(String),
}

fn bounded(flag: &str, value: &str, max: u32) -> Result<u32, String> {
    match value.parse::<u32>() {
        Ok(n) if (1..=max).contains(&n) => Ok(n),
        _ => Err(format!(
            "{flag} takes a whole number from 1 to {max}, got {value:?}"
        )),
    }
}

#[must_use]
pub fn parse(args: &[String]) -> ExhibitCommand {
    let Some(sub) = args.first().map(String::as_str) else {
        return ExhibitCommand::Help;
    };
    if matches!(sub, "help" | "--help" | "-h") {
        return ExhibitCommand::Help;
    }
    if !matches!(sub, "screenshot" | "record") {
        return ExhibitCommand::UsageError(format!("unknown exhibit subcommand: {sub}"));
    }
    let mut capture = CaptureArgs::default();
    let mut seconds = DEFAULT_SECONDS;
    let mut fps = DEFAULT_FPS;
    let mut rest = args[1..].iter();
    while let Some(arg) = rest.next() {
        let mut value = |flag: &str| {
            rest.next()
                .cloned()
                .ok_or_else(|| format!("{flag} needs a value"))
        };
        let step: Result<(), String> = match arg.as_str() {
            "--pid" => value("--pid").and_then(|v| {
                v.parse::<u32>()
                    .map(|pid| capture.pid = Some(pid))
                    .map_err(|_| format!("--pid takes a process id, got {v:?}"))
            }),
            "--screen" => {
                capture.screen = true;
                Ok(())
            }
            "-o" | "--output" => value("--output").map(|v| capture.out = Some(v)),
            "--seconds" if sub == "record" => value("--seconds")
                .and_then(|v| bounded("--seconds", &v, MAX_SECONDS))
                .map(|n| seconds = n),
            "--fps" if sub == "record" => value("--fps")
                .and_then(|v| bounded("--fps", &v, MAX_FPS))
                .map(|n| fps = n),
            other => Err(format!("unknown option for exhibit {sub}: {other}")),
        };
        if let Err(e) = step {
            return ExhibitCommand::UsageError(e);
        }
    }
    if sub == "screenshot" {
        ExhibitCommand::Screenshot(capture)
    } else {
        ExhibitCommand::Record {
            capture,
            seconds,
            fps,
        }
    }
}

/// Where `out` is written: as given when absolute or when no exhibit
/// directory is set, otherwise inside it.
#[must_use]
pub fn resolve_output(out: &str, exhibit_dir: Option<&std::ffi::OsStr>) -> PathBuf {
    let path = PathBuf::from(out);
    match exhibit_dir {
        Some(dir) if path.is_relative() && !dir.is_empty() => PathBuf::from(dir).join(path),
        _ => path,
    }
}

/// Run a parsed screenshot or recording on this machine, returning the file
/// written. Shared by the CLI and the MCP tools.
///
/// # Errors
///
/// When the flags do not fit together, or the capture fails.
pub fn run(cmd: &ExhibitCommand, exhibit_dir: Option<&std::ffi::OsStr>) -> Result<PathBuf, String> {
    match cmd {
        ExhibitCommand::Help => Err("no capture requested".to_string()),
        ExhibitCommand::UsageError(m) => Err(m.clone()),
        ExhibitCommand::Screenshot(capture) => {
            let (target, out) = capture.check(true)?;
            let path = resolve_output(&out, exhibit_dir);
            crate::capture::screenshot(&target, &path).map(|()| path)
        }
        ExhibitCommand::Record {
            capture,
            seconds,
            fps,
        } => {
            let (target, out) = capture.check(false)?;
            let path = resolve_output(&out, exhibit_dir);
            crate::capture::record(&target, *seconds, *fps, &path).map(|()| path)
        }
    }
}

#[must_use]
pub fn dispatch(cmd: ExhibitCommand, _opts: &GlobalOpts) -> i32 {
    match &cmd {
        ExhibitCommand::Help => {
            println!(
                "{}",
                crate::help_map::command_help(&["exhibit"]).expect("exhibit help exists")
            );
            return 0;
        }
        ExhibitCommand::UsageError(m) => {
            println!("usage error: {m}");
            return 2;
        }
        ExhibitCommand::Screenshot(capture) | ExhibitCommand::Record { capture, .. } => {
            if let Err(m) = capture.check(matches!(cmd, ExhibitCommand::Screenshot(_))) {
                println!("usage error: {m}");
                return 2;
            }
        }
    }
    let dir = std::env::var_os("RALPHUS_EXHIBIT_DIR");
    match run(&cmd, dir.as_deref()) {
        Ok(path) => {
            println!("{}", path.display());
            0
        }
        Err(e) => {
            eprintln!("ralphus exhibit: {e}");
            1
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    fn capture(pid: Option<u32>, screen: bool, out: &str) -> CaptureArgs {
        CaptureArgs {
            pid,
            screen,
            out: Some(out.to_string()),
        }
    }

    #[test]
    fn bare_exhibit_is_help() {
        assert_eq!(parse(&[]), ExhibitCommand::Help);
    }

    #[test]
    fn screenshot_parses_flags_and_checks_them_together_when_run() {
        assert_eq!(
            parse(&v(&["screenshot", "--pid", "42", "-o", "view.png"])),
            ExhibitCommand::Screenshot(capture(Some(42), false, "view.png"))
        );
        assert_eq!(
            capture(Some(42), false, "view.png").check(true),
            Ok((Target::Pid(42), "view.png".to_string()))
        );
        assert_eq!(
            capture(None, true, "a.PNG").check(true),
            Ok((Target::Screen, "a.PNG".to_string()))
        );
        for bad in [
            CaptureArgs::default(),
            capture(Some(1), true, "a.png"),
            capture(None, true, "a.gif"),
            CaptureArgs {
                pid: None,
                screen: true,
                out: None,
            },
        ] {
            assert!(bad.check(true).is_err(), "{bad:?}");
        }
        for bad in [
            v(&["screenshot", "--pid", "x", "-o", "a.png"]),
            v(&["screenshot", "--screen", "-o", "a.png", "--seconds", "3"]),
            v(&["screenshot", "--output"]),
        ] {
            assert!(
                matches!(parse(&bad), ExhibitCommand::UsageError(_)),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn record_defaults_bounds_and_formats() {
        assert_eq!(
            parse(&v(&["record", "--screen", "-o", "a.gif"])),
            ExhibitCommand::Record {
                capture: capture(None, true, "a.gif"),
                seconds: DEFAULT_SECONDS,
                fps: DEFAULT_FPS,
            }
        );
        assert_eq!(
            parse(&v(&[
                "record",
                "--pid",
                "7",
                "--seconds",
                "3",
                "--fps",
                "15",
                "-o",
                "a.mp4"
            ])),
            ExhibitCommand::Record {
                capture: capture(Some(7), false, "a.mp4"),
                seconds: 3,
                fps: 15,
            }
        );
        for bad in [
            v(&["record", "--screen", "--seconds", "0", "-o", "a.gif"]),
            v(&["record", "--screen", "--seconds", "61", "-o", "a.gif"]),
            v(&["record", "--screen", "--fps", "31", "-o", "a.gif"]),
        ] {
            assert!(
                matches!(parse(&bad), ExhibitCommand::UsageError(_)),
                "{bad:?}"
            );
        }
        assert!(capture(None, true, "a.mov").check(false).is_err());
        assert!(capture(None, true, "a.mp4").check(false).is_ok());
    }

    #[test]
    fn a_relative_output_lands_in_the_exhibit_directory() {
        let dir = std::ffi::OsString::from("ex");
        assert_eq!(
            resolve_output("view.png", Some(&dir)),
            PathBuf::from("ex").join("view.png")
        );
        assert_eq!(resolve_output("view.png", None), PathBuf::from("view.png"));
        let abs = if cfg!(windows) {
            "C:\\tmp\\v.png"
        } else {
            "/tmp/v.png"
        };
        assert_eq!(resolve_output(abs, Some(&dir)), PathBuf::from(abs));
    }
}
