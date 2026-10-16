//! Reads JSONL `{"id","tool","command"}` on stdin, runs the real
//! `ralphus_shell_syntax::parse` for every grammar, writes JSONL results.
use ralphus_shell_syntax::{Shell, parse, shell_for_tool};
use std::io::{BufRead, Write};

fn name(s: Shell) -> &'static str {
    match s {
        Shell::Bash => "bash",
        Shell::PowerShell => "powershell",
        Shell::Cmd => "cmd",
    }
}

fn main() {
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    for line in std::io::stdin().lock().lines() {
        let v: serde_json::Value = serde_json::from_str(&line.unwrap()).unwrap();
        let cmd = v["command"].as_str().unwrap_or("");
        let tool = v["tool"].as_str().unwrap_or("");
        let mut res = serde_json::Map::new();
        for sh in [Shell::Bash, Shell::PowerShell, Shell::Cmd] {
            let p = parse(sh, cmd);
            res.insert(
                name(sh).into(),
                serde_json::json!({"status": format!("{:?}", p.status), "errors": p.error_count, "line": p.error_line}),
            );
        }
        let native = shell_for_tool(tool, true).map(name);
        let row = serde_json::json!({"id": v["id"], "tool": tool, "native": native, "res": res});
        writeln!(out, "{row}").unwrap();
    }
}
