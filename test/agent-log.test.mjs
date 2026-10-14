// Exercises the pure agent-log parser/filter (RAL-603) sliced out of the board chunks.
import test from "node:test";
import assert from "node:assert/strict";
import { boardScript } from "./board-source.mjs";

const src = boardScript();
const a = src.indexOf("// RALPHUS-AGENT-LOG:BEGIN");
const b = src.indexOf("// RALPHUS-AGENT-LOG:END");
if (a === -1 || b < a) throw new Error("agent-log markers not found");
const { parseAgentLog, agentLogEntryVisible, agentLogPrettyInput } = new Function(
  `${src.slice(a, b)}\nreturn { parseAgentLog, agentLogEntryVisible, agentLogPrettyInput };`,
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
  assert.equal(agentLogPreview(other), "a");
});
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

const rec = (o) => `RALPHUS_TOOL: ${JSON.stringify(o)}`;
const parse = (...lines) => parseAgentLog(lines.join("\n"));

test("unparsable record lines are skipped without disturbing neighbours", () => {
  const e = parse(
    "RALPHUS_TOOL: ",
    "RALPHUS_TOOL: {broken",
    "RALPHUS_TOOL: {\"id\":",
    "RALPHUS_TOOL: not json at all",
    rec({ id: "a", tool: "Read", tag: "tool.Read", input: "{}" }),
  );
  assert.equal(e.length, 1);
  assert.equal(e[0].id, "a");
});

test("records whose JSON is valid but not an object never throw", () => {
  for (const body of ["null", "5", "true", '"str"', "[]", "[1,2]"]) {
    assert.doesNotThrow(() => parse(`RALPHUS_TOOL: ${body}`), body);
  }
  assert.equal(parse("RALPHUS_TOOL: null", "RALPHUS_TOOL: []", "RALPHUS_TOOL: 5").length, 0);
  const e = parse("RALPHUS_TOOL: null", rec({ id: "a", tool: "Read", input: "x" }));
  assert.equal(e.length, 1);
  assert.equal(e[0].tool, "Read");
});

test("wrongly typed record fields fall back to defaults", () => {
  const e = parse(
    rec({ id: 5, tool: 7, sub: 1, tag: 2, input: { a: 1 }, output: 9 }),
    rec({ id: "t", tool: "Bash", sub: 3, tag: null, input: 4, output: ["x"] }),
  );
  assert.equal(e.length, 2);
  assert.equal(e[0].id, "");
  assert.equal(e[0].tool, "tool");
  assert.equal(e[0].input, "");
  assert.equal(e[0].output, "");
  assert.equal(e[0].status, "pending");
  assert.equal(e[1].tool, "Bash");
  assert.equal(e[1].sub, "");
  assert.equal(e[1].tag, "tool.Bash");
  assert.equal(e[1].input, "");
  assert.equal(e[1].status, "pending");
});

test("a result with an unknown id still produces an entry", () => {
  const e = parse(rec({ id: "ghost", output: "late", is_error: false }));
  assert.equal(e.length, 1);
  assert.equal(e[0].tool, "tool");
  assert.equal(e[0].output, "late");
  assert.equal(e[0].status, "ok");
});

test("a result may arrive before its call and the call fills in the rest", () => {
  const e = parse(
    rec({ id: "a", output: "early", is_error: true }),
    rec({ id: "a", tool: "Bash", sub: "ls", tag: "tool.Bash.ls", input: "ls" }),
  );
  assert.equal(e.length, 1);
  assert.equal(e[0].tool, "Bash");
  assert.equal(e[0].output, "early");
  assert.equal(e[0].status, "error");
});

test("a repeated id updates one entry instead of adding another", () => {
  const e = parse(
    rec({ id: "a", tool: "Bash", input: "one" }),
    rec({ id: "a", tool: "Bash", input: "two" }),
  );
  assert.equal(e.length, 1);
  assert.equal(e[0].input, "two");
});

test("records with an empty or missing id are not merged by id", () => {
  const e = parse(
    rec({ tool: "Bash", input: "one" }),
    rec({ id: "", tool: "Read", input: "two" }),
  );
  assert.equal(e.length, 2);
  assert.deepEqual(e.map((x) => x.id), ["", ""]);
});

test("an id-less result attaches to the latest pending id-less call", () => {
  const e = parse(
    rec({ tool: "Bash", input: "one" }),
    rec({ tool: "Read", input: "two" }),
    rec({ output: "r", is_error: false }),
  );
  assert.equal(e.length, 2);
  assert.equal(e[0].status, "pending");
  assert.equal(e[1].output, "r");
  assert.equal(e[1].status, "ok");
});

test("a record line mixed with ordinary text is read from records only", () => {
  const e = parse("[tool.Bash] Bash(x)", "[result] y", rec({ id: "a", tool: "Read", input: "z" }));
  assert.equal(e.length, 1);
  assert.equal(e[0].tool, "Read");
});

test("agentLogPrettyInput pretty-prints valid JSON and leaves everything else alone", () => {
  assert.equal(agentLogPrettyInput('{"a":1}'), '{\n  "a": 1\n}');
  assert.equal(agentLogPrettyInput("[1]"), "[\n  1\n]");
  assert.equal(agentLogPrettyInput("{not json"), "{not json");
  assert.equal(agentLogPrettyInput("[oops"), "[oops");
  assert.equal(agentLogPrettyInput("plain"), "plain");
  assert.equal(agentLogPrettyInput(""), "");
});

const { redactAgentLogEntry } = new Function(`${src.slice(a, b)}\nreturn { redactAgentLogEntry };`)();

test("record entries get sorted args, and previews pick a meaningful value", () => {
  const e = parse(
    rec({ id: "r", tool: "Read", input: '{"limit":5,"file_path":"/a/b.rs"}' }),
    rec({ id: "b", tool: "Bash", input: '{"description":"d","command":"cd /x && ls"}' }),
    rec({ id: "g", tool: "Grep", input: '{"glob":"*.rs","pattern":"foo"}' }),
    rec({ id: "o", tool: "Other", input: '{"zeta":"z","alpha":"first"}' }),
    rec({ id: "n", tool: "Read" }),
  );
  assert.deepEqual(e[0].args.map((x) => [x.key, x.value]), [["file_path", "/a/b.rs"], ["limit", "5"]]);
  assert.equal(agentLogPreview(e[0]), "/a/b.rs");
  assert.equal(agentLogPreview(e[1]), "ls");
  assert.equal(agentLogPreview(e[2]), "foo");
  assert.equal(agentLogPreview(e[3]), "first");
  assert.equal(e[4].args.length, 0);
});

test("argument values are redacted on the record and text paths", () => {
  const redact = (t) => t.replace(/(token\s*=\s*)\S+/g, "$1[REDACTED]").replace(/sk-[a-z0-9]{8,}/g, "[REDACTED]");
  const r = parse(rec({ id: "a", tool: "Http", input: '{"token":"hunter2","url":"u","note":"sk-abcdefgh12"}' }))[0];
  const t = parseAgentLog('[tool.Http] Http(token="hunter2", url=u)')[0];
  for (const e of [r, t]) {
    redactAgentLogEntry(e, redact);
    assert.equal(e.args.find((x) => x.key === "token").value, "[REDACTED]");
    assert.equal(e.args.find((x) => x.key === "url").value, "u");
  }
  assert.equal(r.args.find((x) => x.key === "note").value, "[REDACTED]");
});
