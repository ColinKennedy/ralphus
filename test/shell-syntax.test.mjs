// Exercises the DOM-free shell-syntax row renderer (RAL-644) sliced out of the board chunks.
import test from "node:test";
import assert from "node:assert/strict";
import { boardScript } from "./board-source.mjs";

const src = boardScript();
const a = src.indexOf("// RALPHUS-SHELL-SYNTAX:BEGIN");
const b = src.indexOf("// RALPHUS-SHELL-SYNTAX:END");
if (a === -1 || b < a) throw new Error("shell-syntax markers not found");
const { shellSyntaxTool, shellCommandText, shellSyntaxUsable, shellSyntaxMoreCount, shellSyntaxRowsHtml } = new Function(
  `${src.slice(a, b)}\nreturn { shellSyntaxTool, shellCommandText, shellSyntaxUsable, shellSyntaxMoreCount, shellSyntaxRowsHtml };`,
)();
const esc = (s) => s.replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;");

test("recognises shell tools case-insensitively, including PowerShell", () => {
  for (const t of ["Bash", "bash", "PowerShell", "powershell", "exec"]) assert.ok(shellSyntaxTool(t), t);
  assert.ok(!shellSyntaxTool("Read"));
});

test("finds the command argument", () => {
  assert.equal(shellCommandText([{ key: "command", value: "ls" }]), "ls");
  assert.equal(shellCommandText([{ key: "cmd", value: "dir" }]), "dir");
  assert.equal(shellCommandText([{ key: "path", value: "x" }]), null);
});

test("draws one row per command with separators and colored spans", () => {
  const text = "cd /x && cargo fmt";
  const p = {
    status: "ok",
    commands: [
      { text: "cd /x", start: 0, end: 5, sep: "" },
      { text: "cargo fmt", start: 9, end: 18, sep: "&&" },
    ],
    spans: [
      { kind: "command_name", start: 0, end: 2 },
      { kind: "command_name", start: 9, end: 14 },
    ],
  };
  const html = shellSyntaxRowsHtml(text, p, esc);
  assert.equal(html.match(/alog-sh-row/g).length, 2);
  assert.match(html, /<span class="alog-sh-sep"[^>]*>&amp;&amp;<\/span><span class="alog-sh-cmd">cargo<\/span> fmt/);
  assert.ok(shellSyntaxUsable(p));
  assert.equal(shellSyntaxMoreCount(p), 0, "a leading cd row is not counted");
});

test("slices by UTF-8 bytes, not UTF-16 units", () => {
  const text = "echo é; ls";
  const p = {
    status: "ok",
    commands: [
      { text: "echo é", start: 0, end: 7, sep: "" },
      { text: "ls", start: 9, end: 11, sep: ";" },
    ],
    spans: [{ kind: "command_name", start: 9, end: 11 }],
  };
  const html = shellSyntaxRowsHtml(text, p, esc);
  assert.match(html, /echo é<\/div>/);
  assert.match(html, /<span class="alog-sh-cmd">ls<\/span>/);
  assert.equal(shellSyntaxMoreCount(p), 1);
});

test("a heredoc body becomes a collapsed block under its row", () => {
  const text = "cat <<EOF\nhi\nEOF";
  const p = {
    status: "ok",
    commands: [{ text, start: 0, end: 16, sep: "" }],
    spans: [{ kind: "heredoc", start: 10, end: 13 }],
  };
  const html = shellSyntaxRowsHtml(text, p, esc);
  assert.match(html, /<details class="alog-sh-heredoc" data-hd="0:0">.*<pre>hi\n<\/pre>/s);
});

test("anything but a clean parse is not usable", () => {
  assert.ok(!shellSyntaxUsable({ status: "unparsed", commands: [], spans: [] }));
  assert.ok(!shellSyntaxUsable({ status: "ok", commands: [], spans: [] }));
});

test("a heredoc the reader expanded renders open, independently per row and ordinal", () => {
  const text = ["cat <<A", "x", "A", "cat <<B", "y", "B"].join("\n");
  const p = {
    status: "ok",
    commands: [{ text: "", start: 0, end: 11, sep: "" }, { text: "", start: 12, end: 25, sep: "" }],
    spans: [{ kind: "heredoc", start: 8, end: 10 }, { kind: "heredoc", start: 20, end: 22 }],
  };
  const html = shellSyntaxRowsHtml(text, p, esc, (row, ord) => row === 1 && ord === 0);
  assert.match(html, /data-hd="0:0"><summary/);
  assert.match(html, /data-hd="1:0" open><summary/);
});
