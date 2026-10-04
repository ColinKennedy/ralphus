// Coverage for the `auto_run` review setting's wiring in the board: both
// modals seed a boolean draft, render a tooltipped checkbox, and send the
// field only when it changed (source-level, same approach as
// ./board-skip-manual-checks.test.mjs).

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

test("the review Setup modal seeds autoRun from the effective value and saves it only when changed", () => {
  assert.match(functionSource("buildReviewEditDraft"), /const autoRun = !!g\.effective_auto_run/);
  assert.match(functionSource("renderReviewEditModal"), /onEditAutoRun\(this\.checked\)/);
  assert.match(functionSource("onEditAutoRun"), /reviewEditDraft\.autoRun = checked/);
  assert.match(
    functionSource("saveReviewEditDetails"),
    /if \(draft\.autoRun !== draft\.originalAutoRun\) body\.auto_run = draft\.autoRun/,
  );
});

test("the project Review Settings modal seeds, renders, and saves auto_run only when changed", () => {
  assert.match(
    functionSource("buildProjectReviewSettingsDraft"),
    /const autoRun = boolOr\(s\.auto_run, effective\.auto_run\)/,
  );
  assert.match(functionSource("renderProjectReviewSettingsModal"), /onProjectEditAutoRun\(this\.checked\)/);
  assert.match(functionSource("onProjectEditAutoRun"), /projectReviewSettingsDraft\.autoRun = checked/);
  assert.match(
    functionSource("saveProjectReviewSettings"),
    /if \(draft\.autoRun !== draft\.originalAutoRun\) body\.auto_run = draft\.autoRun/,
  );
});

test("both auto-run controls carry a tooltip", () => {
  for (const name of ["renderReviewEditModal", "renderProjectReviewSettingsModal"]) {
    const render = functionSource(name);
    const at = render.indexOf("auto-run manual checks after build");
    assert.notEqual(at, -1, `${name} renders the control`);
    const label = render.lastIndexOf("<label", at);
    assert.match(render.slice(label, at), /data-tip="[^"]+"/, `${name}: the label has a data-tip`);
  }
});
