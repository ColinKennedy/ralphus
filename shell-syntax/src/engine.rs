//! The tree-sitter-backed half of the crate (`parser` feature).

use std::time::{Duration, Instant};

use tree_sitter::{Node, ParseOptions, Parser};

use crate::{Command, PARSE_TIMEOUT_MS, Parsed, Shell, Span, SpanKind, Status};

/// Anonymous tokens drawn as operators.
const OPERATORS: &[&str] = &[
    "|", "|&", "&&", "||", ";", "&", ">", ">>", "<", "<<", "<<<", ">&", "<&", "&>", "&>>", ">|",
];

/// Row boundaries found while flattening the tree.
enum Item {
    Row(usize, usize),
    Sep(String),
}

pub(crate) fn parse(shell: Shell, text: &str) -> Parsed {
    let mut parser = Parser::new();
    let language = match shell {
        Shell::Bash => tree_sitter_bash::LANGUAGE,
        Shell::PowerShell => tree_sitter_powershell::LANGUAGE,
        Shell::Cmd => tree_sitter_batch::LANGUAGE,
    };
    if parser.set_language(&language.into()).is_err() {
        return Parsed::empty(Status::Unparsed);
    }
    let deadline = Instant::now() + Duration::from_millis(PARSE_TIMEOUT_MS);
    let mut over_budget = |_: &tree_sitter::ParseState| Instant::now() > deadline;
    // The batch grammar ends every statement with a newline, so a one-line
    // command needs one appended; the extra byte lies past every span.
    let owned;
    let bytes = if shell == Shell::Cmd && !text.ends_with(char::from(10)) {
        owned = format!("{text}{}", char::from(10));
        owned.as_bytes()
    } else {
        text.as_bytes()
    };
    let tree = parser.parse_with_options(
        &mut |offset, _| &bytes[offset.min(bytes.len())..],
        None,
        Some(ParseOptions::new().progress_callback(&mut over_budget)),
    );
    let Some(tree) = tree else {
        return Parsed::empty(Status::Timeout);
    };
    let root = tree.root_node();
    if root.has_error() {
        let mut parsed = Parsed::empty(Status::Unparsed);
        parsed.error_line = first_error(root).map(|n| {
            // An ERROR node spans everything the parser skipped; it gave up at its end.
            let at = if n.is_missing() {
                n.start_position()
            } else {
                n.end_position()
            };
            at.row as u32 + 1
        });
        return parsed;
    }
    let mut items = Vec::new();
    match shell {
        Shell::Bash => flatten_bash(root, &mut items),
        Shell::PowerShell => flatten_powershell(root, &mut items),
        Shell::Cmd => flatten_cmd(root, &mut items),
    }
    let mut spans = Vec::new();
    collect_spans(shell, root, text, &mut spans);
    Parsed {
        status: Status::Ok,
        commands: rows(text, &items),
        spans,
        error_line: None,
    }
}

/// The first (document-order) error or missing node under `node`.
fn first_error(node: Node) -> Option<Node> {
    if node.is_error() || node.is_missing() {
        return Some(node);
    }
    if !node.has_error() {
        return None;
    }
    let mut cursor = node.walk();
    node.children(&mut cursor).find_map(first_error)
}

/// Turns the flattened items into trimmed, non-empty commands carrying the
/// separator that preceded each.
fn rows(text: &str, items: &[Item]) -> Vec<Command> {
    let mut out: Vec<Command> = Vec::new();
    let mut sep = String::new();
    for item in items {
        match item {
            Item::Sep(s) => sep = s.clone(),
            Item::Row(start, end) => {
                let raw = &text[*start..*end];
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let lead = raw.len() - raw.trim_start().len();
                out.push(Command {
                    text: trimmed.to_string(),
                    start: start + lead,
                    end: start + lead + trimmed.len(),
                    sep: if out.is_empty() {
                        String::new()
                    } else {
                        std::mem::take(&mut sep)
                    },
                });
                sep.clear();
            }
        }
    }
    out
}

fn row(node: Node, items: &mut Vec<Item>) {
    items.push(Item::Row(node.start_byte(), node.end_byte()));
}

fn flatten_bash(node: Node, items: &mut Vec<Item>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named() {
            if matches!(child.kind(), ";" | "&&" | "||" | "&") {
                items.push(Item::Sep(child.kind().to_string()));
            }
            continue;
        }
        match child.kind() {
            "comment" => {}
            "list" => flatten_bash(child, items),
            "redirected_statement" => {
                let body = child.named_child(0).filter(|n| n.kind() == "list");
                match body {
                    // The grammar hangs a trailing redirect on a whole `a && b`
                    // list; the redirect belongs to the last command.
                    Some(list) => {
                        flatten_bash(list, items);
                        if let Some(Item::Row(_, end)) = items.last_mut() {
                            *end = child.end_byte();
                        }
                    }
                    None => row(child, items),
                }
            }
            _ => row(child, items),
        }
    }
}

fn flatten_powershell(node: Node, items: &mut Vec<Item>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "comment" => {}
            "statement_list" => flatten_powershell(child, items),
            "empty_statement" => items.push(Item::Sep(";".to_string())),
            "pipeline" if has_child(child, "pipeline_chain") => {
                let mut inner = child.walk();
                for part in child.children(&mut inner) {
                    match part.kind() {
                        "pipeline_chain" => row(part, items),
                        "pipeline_chain_tail" => {
                            let op = part.child(0).map(|c| c.kind()).unwrap_or_default();
                            items.push(Item::Sep(op.to_string()));
                        }
                        _ => {}
                    }
                }
            }
            _ => row(child, items),
        }
    }
}

/// `&`, `&&` and `||` nest as `command_sep` / `cond_exec` nodes; a `|` pipeline
/// (`pipe_stmt`) stays one row, like Bash and PowerShell.
fn flatten_cmd(node: Node, items: &mut Vec<Item>) {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if !child.is_named() {
            if matches!(child.kind(), "&" | "&&" | "||") {
                items.push(Item::Sep(child.kind().to_string()));
            }
            continue;
        }
        match child.kind() {
            "comment" => {}
            "command_sep" | "cond_exec" => flatten_cmd(child, items),
            _ => row(child, items),
        }
    }
}

fn has_child(node: Node, kind: &str) -> bool {
    let mut cursor = node.walk();
    node.children(&mut cursor).any(|c| c.kind() == kind)
}

fn classify(shell: Shell, node: Node, text: &str) -> Option<SpanKind> {
    let kind = node.kind();
    if !node.is_named() {
        return OPERATORS.contains(&kind).then_some(SpanKind::Operator);
    }
    match shell {
        Shell::Bash => match kind {
            "comment" => Some(SpanKind::Comment),
            "string" | "raw_string" | "ansi_c_string" | "translated_string" => {
                Some(SpanKind::String)
            }
            "heredoc_body" => Some(SpanKind::Heredoc),
            "simple_expansion" | "expansion" => Some(SpanKind::Variable),
            "command_name" => Some(SpanKind::CommandName),
            "file_descriptor" => Some(SpanKind::Operator),
            "word"
                if node.parent().is_some_and(|p| p.kind() == "command")
                    && text[node.byte_range()].starts_with('-') =>
            {
                Some(SpanKind::Flag)
            }
            _ => None,
        },
        Shell::PowerShell => match kind {
            "comment" => Some(SpanKind::Comment),
            "string_literal" => Some(if text[node.byte_range()].starts_with('@') {
                SpanKind::Heredoc
            } else {
                SpanKind::String
            }),
            "variable" | "braced_variable" => Some(SpanKind::Variable),
            "command_name" => Some(SpanKind::CommandName),
            "command_parameter" => Some(SpanKind::Flag),
            "merging_redirection_operator" | "file_redirection_operator" => {
                Some(SpanKind::Operator)
            }
            _ => None,
        },
        Shell::Cmd => match kind {
            "comment" => Some(SpanKind::Comment),
            "string" => Some(SpanKind::String),
            "variable_reference" | "for_variable" => Some(SpanKind::Variable),
            "command_name" => Some(SpanKind::CommandName),
            "command_option" | "if_option" | "for_options" => Some(SpanKind::Flag),
            "redirect_op" | "redirect_target" | "fd_redirect" | "fd_redirect_op" => {
                Some(SpanKind::Operator)
            }
            _ => None,
        },
    }
}

/// Collects non-overlapping spans: a classified node is emitted whole and not
/// descended into, so a variable inside a string stays part of the string.
fn collect_spans(shell: Shell, node: Node, text: &str, out: &mut Vec<Span>) {
    if let Some(kind) = classify(shell, node, text) {
        out.push(Span {
            kind,
            start: node.start_byte(),
            end: node.end_byte(),
        });
        return;
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        collect_spans(shell, child, text, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(p: &Parsed) -> Vec<(&str, &str)> {
        p.commands
            .iter()
            .map(|c| (c.sep.as_str(), c.text.as_str()))
            .collect()
    }

    fn has(p: &Parsed, text: &str, kind: SpanKind, needle: &str) -> bool {
        p.spans
            .iter()
            .any(|s| s.kind == kind && &text[s.start..s.end] == needle)
    }

    #[test]
    fn bash_splits_on_semicolon_and_list_operators_but_not_pipes() {
        let t = "cd /x && cargo fmt --all; ls -la \"$HOME/a\" 'b' 2>&1 | grep x || echo hi # c";
        let p = parse(Shell::Bash, t);
        assert_eq!(p.status, Status::Ok);
        assert_eq!(
            texts(&p),
            vec![
                ("", "cd /x"),
                ("&&", "cargo fmt --all"),
                (";", "ls -la \"$HOME/a\" 'b' 2>&1 | grep x"),
                ("||", "echo hi"),
            ]
        );
        for c in &p.commands {
            assert_eq!(&t[c.start..c.end], c.text);
        }
        assert!(has(&p, t, SpanKind::CommandName, "cargo"));
        assert!(has(&p, t, SpanKind::Flag, "--all"));
        assert!(has(&p, t, SpanKind::String, "\"$HOME/a\""));
        assert!(has(&p, t, SpanKind::String, "'b'"));
        assert!(has(&p, t, SpanKind::Operator, "|"));
        assert!(has(&p, t, SpanKind::Operator, "&&"));
        assert!(has(&p, t, SpanKind::Comment, "# c"));
    }

    #[test]
    fn bash_variable_and_heredoc_and_newlines() {
        let t = "echo $y\ncat <<EOF\nhello\nEOF\nif a; then b; fi";
        let p = parse(Shell::Bash, t);
        assert_eq!(p.status, Status::Ok);
        assert_eq!(p.commands.len(), 3);
        assert_eq!(p.commands[1].sep, "");
        assert!(p.commands[1].text.contains("hello"));
        assert!(has(&p, t, SpanKind::Variable, "$y"));
        assert!(has(&p, t, SpanKind::Heredoc, "hello\n"));
    }

    #[test]
    fn powershell_pipeline_is_one_row_and_semicolons_split() {
        let t = "cargo fmt --all; cargo nextest run -p x --lib 2>&1 | Select-Object -Last 50";
        let p = parse(Shell::PowerShell, t);
        assert_eq!(p.status, Status::Ok);
        assert_eq!(
            texts(&p),
            vec![
                ("", "cargo fmt --all"),
                (
                    ";",
                    "cargo nextest run -p x --lib 2>&1 | Select-Object -Last 50"
                ),
            ]
        );
        assert!(has(&p, t, SpanKind::CommandName, "Select-Object"));
        assert!(has(&p, t, SpanKind::Flag, "-Last"));
        assert!(has(&p, t, SpanKind::Operator, "|"));
        assert!(has(&p, t, SpanKind::Operator, "2>&1"));
    }

    #[test]
    fn powershell_chain_operators_strings_variables_comments() {
        let t = "$x = \"a $b\"\nfoo && bar # hi\n@'\nhi\n'@";
        let p = parse(Shell::PowerShell, t);
        assert_eq!(p.status, Status::Ok);
        assert_eq!(
            texts(&p),
            vec![
                ("", "$x = \"a $b\""),
                ("", "foo"),
                ("&&", "bar"),
                ("", "@'\nhi\n'@"),
            ]
        );
        assert!(has(&p, t, SpanKind::String, "\"a $b\""));
        assert!(has(&p, t, SpanKind::Variable, "$x"));
        assert!(has(&p, t, SpanKind::Comment, "# hi"));
        assert!(has(&p, t, SpanKind::Heredoc, "@'\nhi\n'@"));
    }

    #[test]
    fn grammar_errors_yield_unparsed_not_a_failure() {
        // The PowerShell grammar rejects a bare `--` argument.
        let p = parse(Shell::PowerShell, "cargo clippy -- -D warnings");
        assert_eq!(p.status, Status::Unparsed);
        assert!(p.commands.is_empty() && p.spans.is_empty());
        assert_eq!(parse(Shell::Bash, "if then ((").status, Status::Unparsed);
    }

    #[test]
    fn spans_never_overlap() {
        let t = "echo \"a $b\" 'c' --x=1 $(date -u) | wc -l > out";
        let p = parse(Shell::Bash, t);
        let mut last = 0;
        for s in &p.spans {
            assert!(s.start >= last, "{s:?} overlaps");
            last = s.end;
        }
    }

    #[test]
    fn oversize_input_is_rejected_before_parsing() {
        let t = "a;".repeat(crate::MAX_INPUT_BYTES);
        assert_eq!(crate::parse(Shell::Bash, &t).status, Status::TooLong);
    }

    #[test]
    fn empty_input_parses_to_no_commands() {
        let p = parse(Shell::Bash, "");
        assert_eq!(p.status, Status::Ok);
        assert!(p.commands.is_empty());
    }

    #[test]
    fn cmd_splits_on_ampersands_but_not_pipes() {
        let t = "echo hi & dir && cd x || echo \"a b\" 2>nul";
        let p = parse(Shell::Cmd, t);
        assert_eq!(p.status, Status::Ok);
        assert_eq!(
            texts(&p),
            vec![
                ("", "echo hi"),
                ("&", "dir"),
                ("&&", "cd x"),
                ("||", "echo \"a b\" 2>nul"),
            ]
        );
        for c in &p.commands {
            assert_eq!(&t[c.start..c.end], c.text);
        }
        assert!(has(&p, t, SpanKind::CommandName, "dir"));
        assert!(has(&p, t, SpanKind::String, "\"a b\""));
        assert!(has(&p, t, SpanKind::Operator, "&&"));
        assert!(has(&p, t, SpanKind::Operator, "2>"));
        assert!(has(&p, t, SpanKind::Operator, "nul"));
    }

    #[test]
    fn cmd_newlines_pipes_and_caret_escapes() {
        let t = "echo a ^& b ^^ c | findstr /i x >nul 2>&1\r\ndir";
        let p = parse(Shell::Cmd, t);
        assert_eq!(p.status, Status::Ok);
        assert_eq!(
            texts(&p),
            vec![
                ("", "echo a ^& b ^^ c | findstr /i x >nul 2>&1"),
                ("", "dir"),
            ]
        );
        assert!(has(&p, t, SpanKind::Operator, "|"));
        assert!(has(&p, t, SpanKind::Flag, "/i"));
        assert!(has(&p, t, SpanKind::Operator, "2>&1"));
        // An escaped `&` is an argument, not a separator.
        assert!(!has(&p, t, SpanKind::Operator, "&"));
    }

    #[test]
    fn cmd_variables_and_if_for_one_liners() {
        let t = "for %%i in (a b) do echo %%i %PATH%";
        let p = parse(Shell::Cmd, t);
        assert_eq!(p.status, Status::Ok);
        assert_eq!(p.commands.len(), 1);
        assert!(has(&p, t, SpanKind::Variable, "%%i"));
        assert!(has(&p, t, SpanKind::Variable, "%PATH%"));

        let t = "if exist a.txt (del a.txt) else echo no";
        let p = parse(Shell::Cmd, t);
        assert_eq!(p.status, Status::Ok);
        assert_eq!(p.commands.len(), 1);
        assert!(has(&p, t, SpanKind::CommandName, "del"));
    }

    #[test]
    fn cmd_parse_failure_is_unparsed() {
        let p = parse(Shell::Cmd, "if exist");
        assert_eq!(p.status, Status::Unparsed);
        assert!(p.commands.is_empty() && p.spans.is_empty());
    }
}
