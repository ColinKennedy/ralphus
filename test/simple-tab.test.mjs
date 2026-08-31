// Coverage for the New Task modal's Simple tab (RAL-297): a deterministic,
// template-driven form that assembles its own TOML client-side (no LLM call)
// and submits it through the same POST /api/squads the Files/Paste tabs use.
//
// What is pinned here: the tab defaults to "simple" (falling back to it when
// no config value is set); the template picker resolves to the built-in
// fallback template when no [[templates]] are configured or the selected
// name doesn't match; form-field validation (prompt/agent/project required,
// template field required/number checks); the generic editable-list-widget
// primitives shared by the Proofs and Manual Checks lists (add/remove/
// reorder/edit, all pure array-in/array-out); which /api/generate kinds a
// submit click should launch; and the blocking generate-then-confirm vs.
// build-and-submit branch a submit click takes.
//
// Run with `npm test` (node --test). See ./board-simple-tab.mjs for how the
// view logic is loaded out of the real board.html.

import test from "node:test";
import assert from "node:assert/strict";
import { simpleTab, boardSource } from "./board-simple-tab.mjs";

const {
  NT_FALLBACK_TEMPLATE,
  ntPickTemplate,
  ntBuildEffectivePrompt,
  ntValidateSimpleFields,
  ntListInsertRow,
  ntListRemoveRow,
  ntListSwapRows,
  ntListEditRow,
  ntPlannedGenerationKinds,
  ntSimpleSubmitAction,
  ntResolveDefaultTab,
} = simpleTab;

/** A minimal valid NtSimpleState, overridable per test. */
function freshState(overrides = {}) {
  return {
    templateName: NT_FALLBACK_TEMPLATE.name,
    prompt: "do the thing",
    fieldValues: {},
    agent: "claude-code",
    model: "",
    project: "demo",
    upstreamBranch: "",
    proofs: false,
    addReview: false,
    generateManualChecks: false,
    proofItems: [],
    checkItems: [],
    generating: false,
    confirmStep: false,
    ...overrides,
  };
}

// ---------- tab default ----------

test("the New Task modal opens on the Simple tab by default", () => {
  assert.match(boardSource, /let ntTab = "simple";/);
});

test("openNewTask resets to the config-driven default tab, not a hardcoded one", () => {
  const body = boardSource.slice(boardSource.indexOf("function openNewTask()"));
  const fn = body.slice(0, body.indexOf("\n      }\n") + 1);
  assert.match(fn, /ntTab = ntConfigDefaultTab;/);
});

test("ntResolveDefaultTab falls back to simple when the config field is absent", () => {
  assert.equal(ntResolveDefaultTab(undefined), "simple");
  assert.equal(ntResolveDefaultTab({}), "simple");
  assert.equal(ntResolveDefaultTab({ default_new_task_tab: "" }), "simple");
});

test("ntResolveDefaultTab honors an explicit config value", () => {
  assert.equal(ntResolveDefaultTab({ default_new_task_tab: "files" }), "files");
  assert.equal(ntResolveDefaultTab({ default_new_task_tab: "paste" }), "paste");
});

// ---------- template picker / fallback ----------

test("ntPickTemplate falls back to the built-in template when no templates are configured", () => {
  const t = ntPickTemplate([], "anything");
  assert.equal(t, NT_FALLBACK_TEMPLATE);
});

test("ntPickTemplate resolves the named template out of a real list", () => {
  const templates = [
    { name: "a", label: "A", fields: [] },
    { name: "b", label: "B", fields: [] },
  ];
  assert.equal(ntPickTemplate(templates, "b").name, "b");
});

test("ntPickTemplate falls back to the first configured template when the name doesn't match", () => {
  const templates = [
    { name: "a", label: "A", fields: [] },
    { name: "b", label: "B", fields: [] },
  ];
  assert.equal(ntPickTemplate(templates, "nonexistent").name, "a");
});

test("ntBuildEffectivePrompt substitutes the prompt and every declared field", () => {
  const template = { prompt_template: "Context: {context}\n\n{prompt}\n\nRepo: {repo}", fields: [{ name: "context" }, { name: "repo" }] };
  const text = ntBuildEffectivePrompt(template, "fix the bug", { context: "billing service", repo: "acme/billing" });
  assert.equal(text, "Context: billing service\n\nfix the bug\n\nRepo: acme/billing");
});

test("ntBuildEffectivePrompt defaults an unset field to an empty string", () => {
  const template = { prompt_template: "[{missing}] {prompt}", fields: [{ name: "missing" }] };
  assert.equal(ntBuildEffectivePrompt(template, "go", {}), "[] go");
});

test("ntBuildEffectivePrompt falls back to a bare {prompt} template when none is declared", () => {
  assert.equal(ntBuildEffectivePrompt({}, "hello", {}), "hello");
});

// ---------- form field validation ----------

test("a valid state with the fallback template produces no errors", () => {
  const state = freshState();
  assert.deepEqual(ntValidateSimpleFields(state, NT_FALLBACK_TEMPLATE), []);
});

test("an empty prompt is rejected", () => {
  const state = freshState({ prompt: "   " });
  const errors = ntValidateSimpleFields(state, NT_FALLBACK_TEMPLATE);
  assert.ok(errors.some((e) => /prompt is required/i.test(e)));
});

test("a missing agent or project is rejected", () => {
  assert.ok(ntValidateSimpleFields(freshState({ agent: "" }), NT_FALLBACK_TEMPLATE).some((e) => /agent is required/i.test(e)));
  assert.ok(ntValidateSimpleFields(freshState({ project: "" }), NT_FALLBACK_TEMPLATE).some((e) => /project is required/i.test(e)));
});

test("a required template field with no value is rejected by its label", () => {
  const template = { fields: [{ name: "ticket", label: "Ticket ID", required: true }] };
  const errors = ntValidateSimpleFields(freshState({ fieldValues: {} }), template);
  assert.ok(errors.some((e) => e.includes('"Ticket ID" is required.')));
});

test("an optional template field with no value passes", () => {
  const template = { fields: [{ name: "notes", label: "Notes", required: false }] };
  assert.deepEqual(ntValidateSimpleFields(freshState(), template), []);
});

test("a non-numeric value for a number-typed field is rejected", () => {
  const template = { fields: [{ name: "count", label: "Count", type: "number", required: false }] };
  const errors = ntValidateSimpleFields(freshState({ fieldValues: { count: "not-a-number" } }), template);
  assert.ok(errors.some((e) => e.includes('"Count" must be a number.')));
});

test("a numeric value for a number-typed field passes", () => {
  const template = { fields: [{ name: "count", label: "Count", type: "number", required: false }] };
  assert.deepEqual(ntValidateSimpleFields(freshState({ fieldValues: { count: "42" } }), template), []);
});

test("every base-field error is reported at once, not just the first", () => {
  const state = freshState({ prompt: "", agent: "", project: "" });
  const errors = ntValidateSimpleFields(state, NT_FALLBACK_TEMPLATE);
  assert.equal(errors.length, 3);
});

// ---------- generic editable-list-widget primitive ----------
// Exercised once here as the shared primitive; both the Proofs and Manual
// Checks lists in the real form are just two call sites over the same rows.

test("ntListInsertRow appends one blank row without mutating the input", () => {
  const items = [{ label: "a", value: "1" }];
  const next = ntListInsertRow(items);
  assert.equal(items.length, 1, "input must not be mutated");
  assert.deepEqual(next, [{ label: "a", value: "1" }, { label: "", value: "" }]);
});

test("ntListRemoveRow drops exactly the given index", () => {
  const items = [{ label: "a", value: "1" }, { label: "b", value: "2" }, { label: "c", value: "3" }];
  assert.deepEqual(ntListRemoveRow(items, 1), [{ label: "a", value: "1" }, { label: "c", value: "3" }]);
  assert.equal(items.length, 3, "input must not be mutated");
});

test("ntListSwapRows swaps a row with its neighbor", () => {
  const items = [{ label: "a", value: "1" }, { label: "b", value: "2" }];
  assert.deepEqual(ntListSwapRows(items, 0, 1), [{ label: "b", value: "2" }, { label: "a", value: "1" }]);
  assert.deepEqual(ntListSwapRows(items, 1, -1), [{ label: "b", value: "2" }, { label: "a", value: "1" }]);
});

test("ntListSwapRows is a no-op (same values) when the neighbor is out of range", () => {
  const items = [{ label: "a", value: "1" }, { label: "b", value: "2" }];
  assert.deepEqual(ntListSwapRows(items, 0, -1), items);
  assert.deepEqual(ntListSwapRows(items, 1, 1), items);
});

test("ntListEditRow replaces only the targeted field of the targeted row", () => {
  const items = [{ label: "a", value: "1" }, { label: "b", value: "2" }];
  assert.deepEqual(ntListEditRow(items, 1, "value", "changed"), [{ label: "a", value: "1" }, { label: "b", value: "changed" }]);
  assert.deepEqual(ntListEditRow(items, 0, "label", "changed"), [{ label: "changed", value: "1" }, { label: "b", value: "2" }]);
  assert.equal(items[1].value, "2", "input row objects must not be mutated");
});

// ---------- generic generation-step primitive ----------

test("ntPlannedGenerationKinds is empty when neither generation checkbox is set", () => {
  assert.deepEqual(ntPlannedGenerationKinds(freshState()), []);
});

test("ntPlannedGenerationKinds includes proof_steps when the Proofs checkbox is set", () => {
  assert.deepEqual(ntPlannedGenerationKinds(freshState({ proofs: true })), ["proof_steps"]);
});

test("ntPlannedGenerationKinds only includes manual_checks when review and the checkbox are both set", () => {
  assert.deepEqual(ntPlannedGenerationKinds(freshState({ addReview: false, generateManualChecks: true })), [], "generateManualChecks alone (no review) must not plan a job");
  assert.deepEqual(ntPlannedGenerationKinds(freshState({ addReview: true, generateManualChecks: true })), ["manual_checks"]);
});

test("ntPlannedGenerationKinds includes both kinds when everything is opted in", () => {
  assert.deepEqual(
    ntPlannedGenerationKinds(freshState({ proofs: true, addReview: true, generateManualChecks: true })),
    ["proof_steps", "manual_checks"],
  );
});

// ---------- blocking-submit-with-confirm flow ----------

test("submit goes straight to build-and-submit when no generation is opted in", () => {
  assert.equal(ntSimpleSubmitAction(freshState()), "submit");
});

test("the first submit click with generation opted in blocks on generate-then-confirm", () => {
  assert.equal(ntSimpleSubmitAction(freshState({ proofs: true })), "generate");
  assert.equal(ntSimpleSubmitAction(freshState({ addReview: true, generateManualChecks: true })), "generate");
});

test("a second submit click, now past confirmStep, proceeds to build-and-submit", () => {
  assert.equal(ntSimpleSubmitAction(freshState({ proofs: true, confirmStep: true })), "submit");
});

test("confirmStep alone (generation never opted in) never blocks a submit", () => {
  assert.equal(ntSimpleSubmitAction(freshState({ confirmStep: true })), "submit");
});

// The two assertions below are about wiring rather than pure logic: they
// read the shipped board.html directly, because the code they cover needs a
// live document/fetch and so cannot be evaluated here.

test("submitTaskSimple checks field and upstream errors before ever blocking on generation", () => {
  const body = boardSource.slice(boardSource.indexOf("async function submitTaskSimple()"));
  const fn = body.slice(0, body.indexOf("\n      }\n") + 1);
  const fieldAt = fn.indexOf("ntSimpleValidateFields()");
  const upstreamAt = fn.indexOf("ntSimpleValidateUpstream()");
  const actionAt = fn.indexOf("ntSimpleSubmitAction(ntSimple)");
  assert.ok(fieldAt > -1 && upstreamAt > -1 && actionAt > -1, "submitTaskSimple shape changed");
  assert.ok(fieldAt < upstreamAt && upstreamAt < actionAt, "validation must run before the generate-or-submit branch");
});

test("submitTaskSimple shows the generating notice before awaiting the generation jobs", () => {
  const body = boardSource.slice(boardSource.indexOf("async function submitTaskSimple()"));
  const fn = body.slice(0, body.indexOf("\n      }\n") + 1);
  const generatingAt = fn.indexOf("ntSimple.generating = true;");
  const renderAt = fn.indexOf("renderNewTaskModal();");
  const awaitAt = fn.indexOf("await Promise.all(jobs);");
  assert.ok(generatingAt > -1 && renderAt > -1 && awaitAt > -1, "submitTaskSimple shape changed");
  assert.ok(generatingAt < renderAt && renderAt < awaitAt, "the generating state must be shown before the jobs are awaited");
});
