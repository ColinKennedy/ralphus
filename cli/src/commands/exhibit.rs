//! `ralphus exhibit <subcommand>`: capture a still or a short recording of a
//! program's window, or of the screen, for a live action's exhibits. Runs on
//! this machine only -- it never talks to the daemon. See
//! [`crate::capture`] for how each OS is captured.
//!
//! A relative output path is written into `$RALPHUS_EXHIBIT_DIR` when that
//! is set (a live action's exhibit directory), so an agent can write
//! `-o view.png` and report `view.png`.

use std::path::PathBuf;

use crate::args::GlobalOpts;
use crate::capture::Target;

/// Default recording length, in seconds.
const DEFAULT_SECONDS: u32 = 5;
/// Default recording rate, in frames per second.
const DEFAULT_FPS: u32 = 10;
/// The longest recording `record` takes.
const MAX_SECONDS: u32 = 60;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExhibitCommand {
    Help,
    Screenshot {
        target: Target,
        out: String,
    },
    Record {
        target: Target,
        seconds: u32,
        fps: u32,
        out: String,
    },
    UsageError(String),
}

fn parse_target(pid: Option<&String>, screen: bool) -> Result<Target, String> {
    match (pid, screen) {
        (Some(_), true) => Err("give --pid or --screen, not both".to_string()),
        (Some(pid), false) => pid
            .parse::<u32>()
            .map(Target::Pid)
            .map_err(|_| format!("--pid takes a process id, got {pid:?}")),
        (None, true) => Ok(Target::Screen),
        (None, false) => Err("say what to capture: --pid <process id> or --screen".to_string()),
    }
}

fn positive(flag: &str, value: &str, max: u32) -> Result<u32, String> {
    match value.parse::<u32>() {
        Ok(n) if (1..=max).contains(&n) => Ok(n),
        _ => Err(format!("{flag} takes a whole number from 1 to {max}, got {value:?}")),
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
    let mut pid: Option<String> = None;
    let mut screen = false;
    let mut out: Option<String> = None;
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
            "--pid" => value("--pid").map(|v| pid = Some(v)),
            "--screen" => {
                screen = true;
                Ok(())
            }
            "-o" | "--output" => value("--output").map(|v| out = Some(v)),
            "--seconds" if sub == "record" => value("--seconds")
                .and_then(|v| positive("--seconds", &v, MAX_SECONDS))
                .map(|n| seconds = n),
            "--fps" if sub == "record" => value("--fps")
                .and_then(|v| positive("--fps", &v, 30))
                .map(|n| fps = n),
            other => Err(format!("unknown option for exhibit {sub}: {other}")),
        };
        if let Err(e) = step {
            return ExhibitCommand::UsageError(e);
        }
    }
    let target = match parse_target(pid.as_ref(), screen) {
        Ok(t) => t,
        Err(e) => return ExhibitCommand::UsageError(e),
    };
    let Some(out) = out else {
        return ExhibitCommand::UsageError("give the output file with -o <file>".to_string());
    };
    if sub == "screenshot" {
        if !out.to_ascii_lowercase().ends_with(".png") {
            return ExhibitCommand::UsageError("a screenshot is written as a .png file".to_string());
        }
        ExhibitCommand::Screenshot { target, out }
    } else {
        if crate::capture::RecordFormat::for_path(std::path::Path::new(&out)).is_none() {
            return ExhibitCommand::UsageError("a recording is written as a .gif or .mp4 file".to_string());
        }
        ExhibitCommand::Record {
            target,
            seconds,
            fps,
            out,
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

#[must_use]
pub fn dispatch(cmd: ExhibitCommand, _opts: &GlobalOpts) -> i32 {
    let dir = std::env::var_os("RALPHUS_EXHIBIT_DIR");
    let result = match cmd {
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
        ExhibitCommand::Screenshot { target, out } => {
            let path = resolve_output(&out, dir.as_deref());
            crate::capture::screenshot(&target, &path).map(|()| path)
        }
        ExhibitCommand::Record {
            target,
            seconds,
            fps,
            out,
        } => {
            let path = resolve_output(&out, dir.as_deref());
            crate::capture::record(&target, seconds, fps, &path).map(|()| path)
        }
    };
    match result {
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

    #[test]
    fn bare_exhibit_is_help() {
        assert_eq!(parse(&[]), ExhibitCommand::Help);
    }

    #[test]
    fn screenshot_takes_a_pid_or_the_screen_and_a_png() {
        assert_eq!(
            parse(&v(&["screenshot", "--pid", "42", "-o", "view.png"])),
            ExhibitCommand::Screenshot {
                target: Target::Pid(42),
                out: "view.png".into()
            }
        );
        assert_eq!(
            parse(&v(&["screenshot", "--screen", "--output", "a.PNG"])),
            ExhibitCommand::Screenshot {
                target: Target::Screen,
                out: "a.PNG".into()
            }
        );
        for bad in [
            v(&["screenshot", "-o", "a.png"]),
            v(&["screenshot", "--pid", "1", "--screen", "-o", "a.png"]),
            v(&["screenshot", "--pid", "x", "-o", "a.png"]),
            v(&["screenshot", "--screen", "-o", "a.gif"]),
            v(&["screenshot", "--screen"]),
            v(&["screenshot", "--screen", "-o", "a.png", "--seconds", "3"]),
        ] {
            assert!(matches!(parse(&bad), ExhibitCommand::UsageError(_)), "{bad:?}");
        }
    }

    #[test]
    fn record_defaults_and_bounds() {
        assert_eq!(
            parse(&v(&["record", "--screen", "-o", "a.gif"])),
            ExhibitCommand::Record {
                target: Target::Screen,
                seconds: DEFAULT_SECONDS,
                fps: DEFAULT_FPS,
                out: "a.gif".into()
            }
        );
        assert_eq!(
            parse(&v(&["record", "--pid", "7", "--seconds", "3", "--fps", "15", "-o", "a.mp4"])),
            ExhibitCommand::Record {
                target: Target::Pid(7),
                seconds: 3,
                fps: 15,
                out: "a.mp4".into()
            }
        );
        for bad in [
            v(&["record", "--screen", "--seconds", "0", "-o", "a.gif"]),
            v(&["record", "--screen", "--seconds", "61", "-o", "a.gif"]),
            v(&["record", "--screen", "-o", "a.mov"]),
        ] {
            assert!(matches!(parse(&bad), ExhibitCommand::UsageError(_)), "{bad:?}");
        }
    }

    #[test]
    fn a_relative_output_lands_in_the_exhibit_directory() {
        let dir = std::ffi::OsString::from("ex");
        assert_eq!(resolve_output("view.png", Some(&dir)), PathBuf::from("ex").join("view.png"));
        assert_eq!(resolve_output("view.png", None), PathBuf::from("view.png"));
        let abs = if cfg!(windows) { "C:\\tmp\\v.png" } else { "/tmp/v.png" };
        assert_eq!(resolve_output(abs, Some(&dir)), PathBuf::from(abs));
    }
}
