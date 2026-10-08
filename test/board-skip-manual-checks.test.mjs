// Coverage for the `skip_manual_checks` review setting's wiring in the board:
// the review Setup modal and the project Review Settings modal both seed a
// boolean draft, render a tooltipped checkbox, and send the field only when it
// changed (source-level, same approach as ./board-rebuild-on.test.mjs).

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

test("the review Setup modal seeds skipManualChecks from the effective value and saves it only when changed", () => {
  const draft = functionSource("buildReviewEditDraft");
  assert.match(draft, /const skipManualChecks = !!g\.effective_skip_manual_checks/);
  assert.match(draft, /skipManualChecks, originalSkipManualChecks: skipManualChecks/);
  const render = functionSource("renderReviewEditModal");
  assert.match(render, /onEditSkipManualChecks\(this\.checked\)/);
  assert.match(render, /skip auto action generation/);
  const save = functionSource("saveReviewEditDetails");
  assert.match(save, /if \(draft\.skipManualChecks !== draft\.originalSkipManualChecks\) body\.skip_manual_checks = draft\.skipManualChecks/);
});

test("the review Setup modal's skip-manual-checks handler stages the draft", () => {
  const handler = functionSource("onEditSkipManualChecks");
  assert.match(handler, /reviewEditDraft\.skipManualChecks = checked/);
});

test("the project Review Settings modal seeds, renders, and saves skip_manual_checks only when changed", () => {
  assert.match(
    functionSource("buildProjectReviewSettingsDraft"),
    /const skipManualChecks = boolOr\(s\.skip_manual_checks, effective\.skip_manual_checks\)/,
  );
  assert.match(functionSource("renderProjectReviewSettingsModal"), /onProjectEditSkipManualChecks\(this\.checked\)/);
  assert.match(functionSource("onProjectEditSkipManualChecks"), /projectReviewSettingsDraft\.skipManualChecks = checked/);
  assert.match(
    functionSource("saveProjectReviewSettings"),
    /if \(draft\.skipManualChecks !== draft\.originalSkipManualChecks\) body\.skip_manual_checks = draft\.skipManualChecks/,
  );
});

test("both skip-manual-checks controls carry a tooltip", () => {
  for (const name of ["renderReviewEditModal", "renderProjectReviewSettingsModal"]) {
    const render = functionSource(name);
    const at = render.indexOf("skip auto action generation");
    assert.notEqual(at, -1, `${name} renders the control`);
    const label = render.lastIndexOf("<label", at);
    assert.match(render.slice(label, at), /data-tip="[^"]+"/, `${name}: the label has a data-tip`);
  }
});
