// RAL-591: prCard labels an "exhausted_awaiting_ci" auto-fix outcome as
// "waiting for CI to finish" (with its own tooltip) instead of echoing the
// raw outcome slug. prCard is extracted whole from the real board chunks.

import test from "node:test";
import assert from "node:assert/strict";
import vm from "node:vm";
import { boardScript } from "./board-source.mjs";

const script = boardScript();

function extractFunction(name) {
  const start = script.indexOf(`function ${name}(`);
  assert.notEqual(start, -1, `function ${name} not found in the board chunks`);
  let depth = 0;
  for (let i = script.indexOf("{", start); i < script.length; i++) {
    if (script[i] === "{") depth += 1;
    else if (script[i] === "}" && --depth === 0) return script.slice(start, i + 1);
  }
  throw new Error(`function ${name} has no closing brace`);
}

function loadPrCard() {
  const context = vm.createContext({
    esc: (s) => String(s ?? ""),
    prSyncStatus: {},
    prColorVar: () => "var(--muted)",
  });
  vm.runInContext(`${extractFunction("prCard")}; this.prCard = prCard;`, context);
  return context.prCard;
}

const basePr = {
  id: "pr-1",
  forge: "github",
  state: "open",
  pr_number: 7,
  pr_url: "https://github.com/acme/w/pull/7",
  branch_alias: "a",
  base_ref: "main",
  ci_status: "failing",
  auto_fix_error: null,
};

test("exhausted_awaiting_ci renders the waiting-for-CI label and tooltip", () => {
  const html = loadPrCard()({ ...basePr, auto_fix_last_outcome: "exhausted_awaiting_ci" });
  assert.ok(html.includes("auto-fix: waiting for CI to finish"));
  assert.ok(!html.includes("exhausted_awaiting_ci"));
  assert.ok(html.includes("the latest CI run is still in progress"));
});

test("other outcomes keep their raw label and generic tooltip", () => {
  const html = loadPrCard()({ ...basePr, auto_fix_last_outcome: "exhausted" });
  assert.ok(html.includes("auto-fix: exhausted"));
  assert.ok(!html.includes("waiting for CI"));
});

test("the outcome badge is hidden once auto_fix_error is shown", () => {
  const html = loadPrCard()({
    ...basePr,
    auto_fix_last_outcome: "exhausted_awaiting_ci",
    auto_fix_error: "stopped",
  });
  assert.ok(!html.includes("waiting for CI to finish"));
});
