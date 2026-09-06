//! Output helpers shared by every CLI subcommand, ported from
//! `cli/src/ralphus/output.py`: the `--json` contract, dependency-free table
//! rendering, and the exit-code convention.
//!
//! Exit-code convention (also documented in `ralphus --help`):
//! - 0 ok
//! - 1 domain error (daemon reached, request rejected)
//! - 2 usage/local error (bad args, file unreadable)
//! - 3 not found (HTTP 404)
//! - 4 conflict (HTTP 409, e.g. "already merging")
//!
//! Every `println!` here is the CLI's actual product output, not a log line
//! -- the module carries the workspace's one legitimate opt-out from
//! `clippy::print_stdout = "deny"` (that lint exists to protect the
//! runner's reserved stdout JSON channel, which this binary never writes to).

#![allow(clippy::print_stdout)]

use crate::client::DaemonError;

/// Prints `data` as raw JSON if `json_mode`, else calls `human(data)`. Both
/// branches consume the same value, so a handler builds its response once
/// and only branches on how to render it.
pub fn emit(json_mode: bool, data: &serde_json::Value, human: impl FnOnce(&serde_json::Value)) {
    if json_mode {
        println!("{}", serde_json::to_string_pretty(data).unwrap_or_default());
    } else {
        human(data);
    }
}

/// Prints a left-aligned, space-padded table (no external dependency).
pub fn print_table(headers: &[&str], rows: &[Vec<String>]) {
    if rows.is_empty() {
        println!("{}", headers.join("  "));
        return;
    }
    let widths: Vec<usize> = headers
        .iter()
        .enumerate()
        .map(|(i, h)| {
            rows.iter()
                .map(|r| r[i].len())
                .chain(std::iter::once(h.len()))
                .max()
                .unwrap_or(0)
        })
        .collect();
    let fmt_row = |cells: &[String]| -> String {
        cells
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect::<Vec<_>>()
            .join("  ")
    };
    println!(
        "{}",
        fmt_row(&headers.iter().map(|h| (*h).to_string()).collect::<Vec<_>>())
    );
    for row in rows {
        println!("{}", fmt_row(row));
    }
}

/// Prints `key : value` lines, aligning the colons.
pub fn print_kv(pairs: &[(&str, String)]) {
    if pairs.is_empty() {
        return;
    }
    let width = pairs.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    for (key, value) in pairs {
        println!("{key:<width$} : {value}");
    }
}

/// Maps a [`DaemonError`] to the CLI's process exit code.
#[must_use]
pub fn exit_code_for(err: &DaemonError) -> i32 {
    match err.status_code {
        Some(404) => 3,
        Some(409) => 4,
        _ => 1,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exit_code_maps_known_statuses() {
        assert_eq!(
            exit_code_for(&DaemonError {
                message: String::new(),
                status_code: Some(404)
            }),
            3
        );
        assert_eq!(
            exit_code_for(&DaemonError {
                message: String::new(),
                status_code: Some(409)
            }),
            4
        );
        assert_eq!(
            exit_code_for(&DaemonError {
                message: String::new(),
                status_code: Some(500)
            }),
            1
        );
        assert_eq!(
            exit_code_for(&DaemonError {
                message: String::new(),
                status_code: None
            }),
            1
        );
    }

    #[test]
    fn print_table_handles_empty_rows() {
        print_table(&["a", "b"], &[]);
    }

    #[test]
    fn print_table_pads_columns() {
        print_table(
            &["name", "state"],
            &[
                vec!["short".to_string(), "done".to_string()],
                vec!["a-much-longer-name".to_string(), "running".to_string()],
            ],
        );
    }

    #[test]
    fn print_kv_handles_empty() {
        print_kv(&[]);
    }
}
