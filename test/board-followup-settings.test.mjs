// Coverage for the per-review follow-up overrides' wiring in the board's
// review Setup modal: each seeds a boolean draft from its effective value
// (on unless something turns it off), renders a tooltipped checkbox, and is
// sent only when it changed (source-level, same approach as
// ./board-auto-run.test.mjs).

import test from "node:test";
import assert from "node:assert/strict";
import { boardScript } from "./board-source.mjs";

const script = boardScript();

/** Source of one top-level function from the board chunks. */
function functionSource(name) {
  const start = script.indexOf(`function ${name}(`);
  assert.notEqual(start, -1, `function ${name} not found in the board chunks`);
  const open = script.indexOf("{", script.indexOf(")", start));
  let depth = 0;
  for (let i = open; i < script.length; i++) {
    if (script[i] === "{") depth += 1;
    else if (script[i] === "}" && --depth === 0) return script.slice(start, i + 1);
  }
  throw new Error(`function ${name} has no closing brace`);
}

test("the review Setup modal seeds the follow-up overrides as on unless the effective value is false", () => {
  const draft = functionSource("buildReviewEditDraft");
  assert.match(draft, /const followupEnabled = g\.effective_followup_enabled !== false/);
  assert.match(draft, /const followupAutoStart = g\.effective_followup_auto_start !== false/);
});

test("the review Setup modal renders both follow-up checkboxes with tooltips", () => {
  const render = functionSource("renderReviewEditModal");
  assert.match(render, /data-tip="[^"]+">\s*<input type="checkbox"[^>]*onEditFollowupEnabled\(this\.checked\)/);
  assert.match(render, /data-tip="[^"]+">\s*<input type="checkbox"[^>]*onEditFollowupAutoStart\(this\.checked\)/);
  assert.match(functionSource("onEditFollowupEnabled"), /reviewEditDraft\.followupEnabled = checked/);
  assert.match(functionSource("onEditFollowupAutoStart"), /reviewEditDraft\.followupAutoStart = checked/);
});

test("the review Setup modal saves each follow-up override only when it changed", () => {
  const save = functionSource("saveReviewEditDetails");
  assert.match(
    save,
    /if \(draft\.followupEnabled !== draft\.originalFollowupEnabled\) body\.followup_enabled = draft\.followupEnabled/,
  );
  assert.match(
    save,
    /if \(draft\.followupAutoStart !== draft\.originalFollowupAutoStart\) body\.followup_auto_start = draft\.followupAutoStart/,
  );
});
