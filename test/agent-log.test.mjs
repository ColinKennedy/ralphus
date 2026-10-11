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
