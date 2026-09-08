import assert from "node:assert/strict";
import path from "node:path";
import { boardScript as loadBoardScript } from "../../test/board-source.mjs";
import test from "node:test";
import { fileURLToPath } from "node:url";
import vm from "node:vm";

const repoRoot = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..");
const boardScript = loadBoardScript();

function extractFunctionSource(name) {
  const needle = `function ${name}(`;
  const start = boardScript.indexOf(needle);
  if (start < 0) {
    throw new Error(`function ${name} not found in the board chunks`);
  }
  const bodyStart = boardScript.indexOf("{", start);
  if (bodyStart < 0) {
    throw new Error(`function ${name} has no body start`);
  }
  let depth = 0;
  for (let i = bodyStart; i < boardScript.length; i++) {
    const ch = boardScript[i];
    if (ch === "{") {
      depth += 1;
    } else if (ch === "}") {
      depth -= 1;
      if (depth === 0) {
        return boardScript.slice(start, i + 1);
      }
    }
  }
  throw new Error(`function ${name} has no matching closing brace`);
}

function loadFunctions(names, contextExtras = {}) {
  const context = vm.createContext({
    Date,
    Set,
    console,
    WAITING_TIP: "waiting",
    LIFETIME_COST_TIP: "lifetime",
    TOKENS_COST_TIP: "tokens",
    window: { _daemonStatus: null },
    sel: { kind: "session", taskIdx: 0, sessionIdx: 0, verifyIdx: -1 },
    runPaths: {
      "run-1": {
        "0:0": {
          project: "/repo",
          upstream: "origin/main",
        },
      },
    },
    peekOpen: {},
    cartoExpanded: new Set(),
    cartoFilter: { sortCol: "time", dir: "desc" },
    esc: (s) => String(s ?? ""),
    pill: (s) => `<pill:${s}>`,
    gdot: (s) => `<gdot:${s}>`,
    sdot: (s) => `<sdot:${s}>`,
    runLogsBtn: (id) => `<logs:${id}>`,
    failLogBtn: (err) => `<fail:${err}>`,
    copyBtn: () => "",
    cmdBox: (text) => `<cmd>${text}</cmd>`,
    editBtn: () => "",
    envOverridesSection: () => "",
    sessionEnvOverridesSection: () => "",
    isDowntimeWaiting: () => false,
    runDisplayState: (run) => run.state,
    loadRunPaths: () => {},
    terminalMenuItem: () => "",
    terminalMenuHtml: () => "",
    peekBox: () => "",
    cartoLevelPill: (level) => `<level:${level}>`,
    cartoRefChip: (row) => row.run_id || "—",
    cartoRowCopyBtn: (id) => `<copy:${id}>`,
    ...contextExtras,
  });

  for (const name of names) {
    vm.runInContext(`${extractFunctionSource(name)}; globalThis.${name} = ${name};`, context);
  }
  return context;
}

test("runView keeps a synthetic old running run healthy in the details pane", () => {
  const context = loadFunctions(["runView"]);
  const run = {
    id: "run-1",
    label: "Long runner",
    state: "running",
    created_at_ms: 0,
    tasks: [
      {
        name: "build",
        state: "running",
      },
    ],
    reviews: [],
    env_overrides: {},
  };

  const html = context.runView(run);
  assert.match(html, /<pill:running>/);
  assert.doesNotMatch(html, /\bstuck\b/i);
  assert.doesNotMatch(html, /\bstalled\b/i);
  assert.doesNotMatch(html, /class="warn"/);
});

test("sessionView keeps a synthetic old running session healthy in the details pane", () => {
  const context = loadFunctions(["sessionView"]);
  const run = {
    id: "run-1",
    label: "Long runner",
    state: "running",
    created_at_ms: 0,
    tasks: [],
  };
  const task = { name: "build" };
  const session = {
    id: "worker",
    name: "worker",
    state: "running",
    agent: "codex",
    model: "gpt-5",
    cwd: "/repo",
    tokens_in: 0,
    tokens_out: 0,
    cost_usd: 0,
    maximum_budget_usd: null,
    prompt: "keep going",
    command: null,
    reviews: [],
    verify: [],
    env_overrides: {},
    agent_session_id: null,
    error: null,
  };

  const html = context.sessionView(run, task, session);
  assert.match(html, /<pill:running>/);
  assert.doesNotMatch(html, /\bstuck\b/i);
  assert.doesNotMatch(html, /\bstalled\b/i);
  assert.doesNotMatch(html, /class="warn"/);
  assert.doesNotMatch(html, /<fail:/);
});

test("cartoTableHtml renders a synthetic old running event as an ordinary row", () => {
  const context = loadFunctions(["cartoTableHtml"]);
  const rows = [
    {
      id: 7,
      at_ms: 0,
      level: "info",
      source: "scheduler",
      scope: "run",
      task: "build",
      run_id: "run-1",
      message: "run still running",
      payload: {},
    },
  ];

  const html = context.cartoTableHtml(rows, false);
  assert.match(html, /run still running/);
  assert.match(html, /<level:info>/);
  assert.doesNotMatch(html, /\bstuck\b/i);
  assert.doesNotMatch(html, /\bstalled\b/i);
  assert.doesNotMatch(html, /class="warn"/);
});
