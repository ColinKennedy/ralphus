// Exercises the pure agent-log parser/filter (RAL-603) sliced out of the board chunks.
import test from "node:test";
import assert from "node:assert/strict";
import { boardScript } from "./board-source.mjs";

const src = boardScript();
const a = src.indexOf("// RALPHUS-AGENT-LOG:BEGIN");
const b = src.indexOf("// RALPHUS-AGENT-LOG:END");
if (a === -1 || b < a) throw new Error("agent-log markers not found");
const { parseAgentLog, agentLogEntryVisible } = new Function(
  `${src.slice(a, b)}\nreturn { parseAgentLog, agentLogEntryVisible };`,
)();

test("pairs results to calls by order and splits tool/sub", () => {
  const e = parseAgentLog([
    "[tool.Bash.git] Bash(command=git status)",
    "[result] clean",
    "second line",
    "[tool.Read] Read(path=a)",
    "[error] nope",
    "[usage] ignored",
    "stray",
  ].join("\n"));
  assert.equal(e.length, 2);
  assert.equal(e[0].tool, "Bash");
  assert.equal(e[0].sub, "git");
  assert.equal(e[0].input, "command=git status");
  assert.equal(e[0].output, "clean\nsecond line");
  assert.equal(e[0].status, "ok");
  assert.equal(e[1].status, "error");
});

test("filters by tool, status and text; unanswered call stays pending", () => {
  const e = parseAgentLog("[tool.Grep] Grep(x)\n[result] hit\n[tool.Bash] Bash(ls)");
  const f = (o) => e.filter((x) => agentLogEntryVisible(x, { tool: "", status: "", q: "", ...o })).length;
  assert.equal(f({ tool: "Grep" }), 1);
  assert.equal(f({ status: "pending" }), 1);
  assert.equal(f({ q: "HIT" }), 1);
});

const { splitAgentLogArgs, agentLogPreview } = new Function(
  `${src.slice(a, b)}\nreturn { splitAgentLogArgs, agentLogPreview };`,
)();
const kv = (s) => splitAgentLogArgs(s).map((x) => [x.key, x.value]);

test("splits quoted args, sorted by key, tolerating commas and quotes in values", () => {
  assert.deepEqual(kv('command="git log --oneline -20", description="Check, \\"recent\\" commits"'), [
    ["command", "git log --oneline -20"],
    ["description", 'Check, "recent" commits'],
  ]);
  assert.deepEqual(kv('b=2, a=x y'), [["a", "x y"], ["b", "2"]]);
});

test("keeps multi-line values and falls back to no args for free text", () => {
  assert.deepEqual(kv('command="a\nb", z=1'), [["command", "a\nb"], ["z", "1"]]);
  assert.deepEqual(kv("just some text"), []);
  assert.deepEqual(kv('{"b":1,"a":"x"}'), [["a", "x"], ["b", "1"]]);
});

test("preview skips leading cd but expanded input keeps it", () => {
  const e = parseAgentLog('[tool.Bash.git] Bash(command="cd \\"/a b\\" && cd c; git fetch origin", description="d")')[0];
  assert.equal(agentLogPreview(e), "git fetch origin");
  assert.match(e.args[0].value, /^cd "\/a b" && cd c; git fetch/);
  const only = parseAgentLog('[tool.Bash] Bash(command="cd /x")')[0];
  assert.equal(agentLogPreview(only), "cd /x");
  const other = parseAgentLog("[tool.Read] Read(path=a)")[0];
  assert.equal(agentLogPreview(other), "path=a");
});

test("structured records merge call and result by tool-use id", () => {
  const rec = (o) => `RALPHUS_TOOL: ${JSON.stringify(o)}`;
  const e = parseAgentLog([
    rec({ id: "a", tool: "Bash", sub: "git", tag: "tool.Bash.git", input: '{"command":"git status"}' }),
    rec({ id: "b", tool: "Read", tag: "tool.Read", input: "{}" }),
    rec({ id: "b", output: "contents", is_error: false }),
    rec({ id: "a", output: "x".repeat(2000), is_error: true }),
    rec({ id: "c", tool: "exec", sub: "cargo", tag: "tool.exec.cargo", input: "cargo test", output: "ok", is_error: false }),
    "RALPHUS_TOOL: {broken",
  ].join("\n"));
  assert.equal(e.length, 3);
  assert.equal(e[0].id, "a");
  assert.equal(e[0].status, "error");
  assert.equal(e[0].output.length, 2000);
  assert.match(e[0].input, /"command": "git status"/);
  assert.equal(e[1].output, "contents");
  assert.equal(e[2].status, "ok");
});
