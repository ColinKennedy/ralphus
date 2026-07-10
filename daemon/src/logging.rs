//! File-based daemon logging (RAL-83).
//!
//! A single global writer is initialised once at daemon start: a file when
//! `log_path` is configured, stderr otherwise. Every `rlog!` call in the
//! daemon crate writes to whichever sink is active.
//!
//! The macro falls back to `eprintln!` when the logger has not been
//! initialised (validate subcommand, unit tests) so no output is lost.

use std::fs::OpenOptions;
use std::io::{self, Write};
use std::str::FromStr;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};

static WRITER: OnceLock<Mutex<Box<dyn Write + Send>>> = OnceLock::new();
static LOG_TO_FILE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
static MIN_LEVEL: AtomicU8 = AtomicU8::new(LogLevel::INFO as u8);

/// Severity levels for `rlog!`. Only messages at or above the configured
/// minimum level are written to the active sink.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
#[allow(non_camel_case_types)]
pub enum LogLevel {
    TRACE = 0,
    DEBUG = 1,
    INFO = 2,
    WARNING = 3,
    ERROR = 4,
}

impl FromStr for LogLevel {
    type Err = ();
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_uppercase().as_str() {
            "TRACE" => Ok(LogLevel::TRACE),
            "DEBUG" => Ok(LogLevel::DEBUG),
            "INFO" => Ok(LogLevel::INFO),
            "WARNING" | "WARN" => Ok(LogLevel::WARNING),
            "ERROR" => Ok(LogLevel::ERROR),
            _ => Err(()),
        }
    }
}

/// Initialise the global log sink. Must be called once before the first
/// `rlog!` in daemon operation. Subsequent calls (e.g. in tests) are no-ops.
pub fn init(log_path: Option<&str>, log_level: Option<&str>) {
    let level = log_level
        .and_then(|s| s.parse::<LogLevel>().ok())
        .unwrap_or(LogLevel::INFO);
    MIN_LEVEL.store(level as u8, Ordering::Relaxed);

    let writer: Box<dyn Write + Send> = match log_path {
        Some(path) => match OpenOptions::new().create(true).append(true).open(path) {
            Ok(file) => {
                LOG_TO_FILE.store(true, Ordering::Relaxed);
                Box::new(file)
            }
            Err(e) => {
                eprintln!("ralphus: could not open log file {path:?}: {e}, falling back to stderr");
                Box::new(io::stderr())
            }
        },
        None => Box::new(io::stderr()),
    };
    let _ = WRITER.set(Mutex::new(writer));
}

/// Whether the logger was initialised with a file path (used by the health
/// endpoint to emit a warning when no log file is configured).
pub fn logging_to_file() -> bool {
    LOG_TO_FILE.load(Ordering::Relaxed)
}

fn utc_hms() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    let s = secs % 60;
    let m = (secs / 60) % 60;
    let h = (secs / 3600) % 24;
    format!("{h:02}:{m:02}:{s:02}")
}

/// Write one log line to the active sink if `level` meets the minimum, or
/// fall back to stderr when the logger has not been initialised.
pub fn write_line(level: LogLevel, line: &str) {
    if (level as u8) < MIN_LEVEL.load(Ordering::Relaxed) {
        return;
    }
    if let Some(mutex) = WRITER.get() {
        if let Ok(mut w) = mutex.lock() {
            if LOG_TO_FILE.load(Ordering::Relaxed) {
                let _ = writeln!(w, "{} {line}", utc_hms());
            } else {
                let _ = writeln!(w, "{line}");
            }
            return;
        }
    }
    eprintln!("{line}");
}

/// Log a formatted message at the given level to the daemon's configured sink.
///
/// Usage: `rlog!(INFO, "ralphus [scheduler] run {id} claimed")`.
/// Valid levels: `TRACE`, `DEBUG`, `INFO`, `WARNING`, `ERROR`.
#[macro_export]
macro_rules! rlog {
    ($level:ident, $($arg:tt)*) => {
        $crate::logging::write_line(
            $crate::logging::LogLevel::$level,
            &::std::format!($($arg)*)
        )
    };
}
