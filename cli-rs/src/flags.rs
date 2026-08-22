//! A small hand-rolled flag scanner shared by every subcommand's argument
//! parsing, following `daemon/src/lib.rs::parse_port_flag`'s convention
//! (linear-scan the tail for `--flag value` pairs) scaled up to cover
//! `cli-rs`'s ~105 leaf subcommands. A shared utility here avoids re-deriving
//! the same scan-and-remove loop by hand at every one of those call sites.
//!
//! Unlike `argparse`, this never panics or auto-generates `--help` text --
//! an unrecognized flag is left in [`Scanner::remaining`] for the caller to
//! reject as a usage error (exit code 2), matching this workspace's existing
//! "never panic on bad input" convention for CLI parsing.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsageError(pub String);

impl std::fmt::Display for UsageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::error::Error for UsageError {}

/// Scans `args` for named flags, leaving whatever wasn't consumed as
/// [`Scanner::remaining`] (intended to become positional arguments once every
/// known flag has been taken out).
#[derive(Debug, Clone)]
pub struct Scanner {
    remaining: Vec<String>,
}

impl Scanner {
    #[must_use]
    pub fn new(args: &[String]) -> Self {
        Self {
            remaining: args.to_vec(),
        }
    }

    /// Takes `--name value` (or `--name=value`), returning its value if
    /// present. Errors if `--name` appears with no following value.
    pub fn take_value(&mut self, name: &str) -> Result<Option<String>, UsageError> {
        let prefix = format!("{name}=");
        if let Some(pos) = self.remaining.iter().position(|a| a.starts_with(&prefix)) {
            let value = self.remaining.remove(pos)[prefix.len()..].to_string();
            return Ok(Some(value));
        }
        let Some(pos) = self.remaining.iter().position(|a| a == name) else {
            return Ok(None);
        };
        if pos + 1 >= self.remaining.len() {
            return Err(UsageError(format!("{name} requires a value")));
        }
        self.remaining.remove(pos);
        Ok(Some(self.remaining.remove(pos)))
    }

    /// Like [`Self::take_value`] but parses the value with `FromStr`,
    /// reporting a usage error on a bad value rather than propagating the
    /// parse error type.
    pub fn take_parsed<T: std::str::FromStr>(
        &mut self,
        name: &str,
    ) -> Result<Option<T>, UsageError> {
        match self.take_value(name)? {
            None => Ok(None),
            Some(raw) => raw
                .parse::<T>()
                .map(Some)
                .map_err(|_| UsageError(format!("{name}: invalid value '{raw}'"))),
        }
    }

    /// Takes every occurrence of a repeatable `--name value` flag, in order.
    pub fn take_repeated(&mut self, name: &str) -> Result<Vec<String>, UsageError> {
        let mut out = Vec::new();
        while let Some(v) = self.take_value(name)? {
            out.push(v);
        }
        Ok(out)
    }

    /// Takes a boolean `--name` flag, returning whether it was present.
    pub fn take_bool(&mut self, name: &str) -> bool {
        if let Some(pos) = self.remaining.iter().position(|a| a == name) {
            self.remaining.remove(pos);
            true
        } else {
            false
        }
    }

    /// Whatever is left after every known flag has been taken -- the
    /// positional arguments (or, if non-empty after the caller expected none,
    /// evidence of an unrecognized flag).
    #[must_use]
    pub fn remaining(self) -> Vec<String> {
        self.remaining
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn v(items: &[&str]) -> Vec<String> {
        items.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn take_value_extracts_flag_and_value() {
        let mut s = Scanner::new(&v(&["pos1", "--status", "running", "pos2"]));
        assert_eq!(
            s.take_value("--status").unwrap(),
            Some("running".to_string())
        );
        assert_eq!(s.remaining(), v(&["pos1", "pos2"]));
    }

    #[test]
    fn take_value_supports_equals_form() {
        let mut s = Scanner::new(&v(&["--status=running"]));
        assert_eq!(
            s.take_value("--status").unwrap(),
            Some("running".to_string())
        );
    }

    #[test]
    fn take_value_missing_flag_returns_none() {
        let mut s = Scanner::new(&v(&["pos1"]));
        assert_eq!(s.take_value("--status").unwrap(), None);
    }

    #[test]
    fn take_value_errors_when_no_value_follows() {
        let mut s = Scanner::new(&v(&["--status"]));
        assert!(s.take_value("--status").is_err());
    }

    #[test]
    fn take_parsed_parses_and_reports_bad_values() {
        let mut s = Scanner::new(&v(&["--limit", "42"]));
        assert_eq!(s.take_parsed::<i64>("--limit").unwrap(), Some(42));
        let mut bad = Scanner::new(&v(&["--limit", "nope"]));
        assert!(bad.take_parsed::<i64>("--limit").is_err());
    }

    #[test]
    fn take_repeated_collects_every_occurrence_in_order() {
        let mut s = Scanner::new(&v(&["--check", "fmt", "--check", "test"]));
        assert_eq!(
            s.take_repeated("--check").unwrap(),
            vec!["fmt".to_string(), "test".to_string()]
        );
    }

    #[test]
    fn take_bool_removes_flag_and_reports_presence() {
        let mut s = Scanner::new(&v(&["--activate", "pos"]));
        assert!(s.take_bool("--activate"));
        assert!(!s.take_bool("--activate"));
        assert_eq!(s.remaining(), v(&["pos"]));
    }
}
