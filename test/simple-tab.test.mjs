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
  ntSanitizeSquadLabel,
  extractTicketId,
  slugifyTaskName,
  ntResolveNaming,
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
    reviewMode: "none",
    generateManualChecks: false,
    skipAutoBuild: true,
    generateAutoBuild: false,
    proofItems: [],
    checkItems: [],
    buildItems: [],
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
// Errors are structured ({field, message}), not plain strings, so the form
// can show each one inline next to its offending field (in addition to the
// shared error panel's generic "fix before continuing" message).

test("a valid state with the fallback template produces no errors", () => {
  const state = freshState();
  assert.deepEqual(ntValidateSimpleFields(state, NT_FALLBACK_TEMPLATE), []);
});

test("an empty prompt is rejected and keyed to the prompt field", () => {
  const state = freshState({ prompt: "   " });
  const errors = ntValidateSimpleFields(state, NT_FALLBACK_TEMPLATE);
  assert.ok(errors.some((e) => e.field === "prompt" && /prompt is required/i.test(e.message)));
});

test("a missing agent or project is rejected and keyed to its own field", () => {
  assert.ok(ntValidateSimpleFields(freshState({ agent: "" }), NT_FALLBACK_TEMPLATE).some((e) => e.field === "agent" && /agent is required/i.test(e.message)));
  assert.ok(ntValidateSimpleFields(freshState({ project: "" }), NT_FALLBACK_TEMPLATE).some((e) => e.field === "project" && /project is required/i.test(e.message)));
});

test("a required template field with no value is rejected by its label and keyed to field:<name>", () => {
  const template = { fields: [{ name: "ticket", label: "Ticket ID", required: true }] };
  const errors = ntValidateSimpleFields(freshState({ fieldValues: {} }), template);
  assert.ok(errors.some((e) => e.field === "field:ticket" && e.message.includes('"Ticket ID" is required.')));
});

test("an optional template field with no value passes", () => {
  const template = { fields: [{ name: "notes", label: "Notes", required: false }] };
  assert.deepEqual(ntValidateSimpleFields(freshState(), template), []);
});

test("a non-numeric value for a number-typed field is rejected", () => {
  const template = { fields: [{ name: "count", label: "Count", type: "number", required: false }] };
  const errors = ntValidateSimpleFields(freshState({ fieldValues: { count: "not-a-number" } }), template);
  assert.ok(errors.some((e) => e.field === "field:count" && e.message.includes('"Count" must be a number.')));
});

test("a numeric value for a number-typed field passes", () => {
  const template = { fields: [{ name: "count", label: "Count", type: "number", required: false }] };
  assert.deepEqual(ntValidateSimpleFields(freshState({ fieldValues: { count: "42" } }), template), []);
});

test("every base-field error is reported at once, not just the first", () => {
  const state = freshState({ prompt: "", agent: "", project: "" });
  const errors = ntValidateSimpleFields(state, NT_FALLBACK_TEMPLATE);
  assert.equal(errors.length, 3);
  assert.deepEqual(new Set(errors.map((e) => e.field)), new Set(["prompt", "agent", "project"]));
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

test("ntPlannedGenerationKinds only includes manual_checks when reviewMode is explicit and the checkbox is set", () => {
  assert.deepEqual(ntPlannedGenerationKinds(freshState({ reviewMode: "auto", generateManualChecks: true })), [], "generateManualChecks alone (not an explicit review) must not plan a job");
  assert.deepEqual(ntPlannedGenerationKinds(freshState({ reviewMode: "none", generateManualChecks: true })), [], "generateManualChecks under No Review must not plan a job");
  assert.deepEqual(ntPlannedGenerationKinds(freshState({ reviewMode: "explicit", generateManualChecks: true })), ["manual_checks"]);
});

test("ntPlannedGenerationKinds only includes auto_build_steps when reviewMode is explicit, auto-build isn't skipped, and the checkbox is set", () => {
  assert.deepEqual(ntPlannedGenerationKinds(freshState({ reviewMode: "explicit", skipAutoBuild: true, generateAutoBuild: true })), [], "generateAutoBuild while skipped must not plan a job");
  assert.deepEqual(ntPlannedGenerationKinds(freshState({ reviewMode: "auto", skipAutoBuild: false, generateAutoBuild: true })), [], "generateAutoBuild outside an explicit review must not plan a job");
  assert.deepEqual(ntPlannedGenerationKinds(freshState({ reviewMode: "explicit", skipAutoBuild: false, generateAutoBuild: true })), ["auto_build_steps"]);
});

test("ntPlannedGenerationKinds includes every kind when everything is opted in", () => {
  assert.deepEqual(
    ntPlannedGenerationKinds(freshState({
      proofs: true, reviewMode: "explicit", generateManualChecks: true,
      skipAutoBuild: false, generateAutoBuild: true,
    })),
    ["proof_steps", "manual_checks", "auto_build_steps"],
  );
});

// ---------- blocking-submit-with-confirm flow ----------

test("submit goes straight to build-and-submit when no generation is opted in", () => {
  assert.equal(ntSimpleSubmitAction(freshState()), "submit");
});

test("the first submit click with generation opted in blocks on generate-then-confirm", () => {
  assert.equal(ntSimpleSubmitAction(freshState({ proofs: true })), "generate");
  assert.equal(ntSimpleSubmitAction(freshState({ reviewMode: "explicit", generateManualChecks: true })), "generate");
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
  // The field-error branch also calls renderNewTaskModal(), earlier in the
  // function -- search for the one after ntSimple.generating = true.
  const renderAt = fn.indexOf("renderNewTaskModal();", generatingAt);
  const awaitAt = fn.indexOf("await Promise.all(jobs);");
  assert.ok(generatingAt > -1 && renderAt > -1 && awaitAt > -1, "submitTaskSimple shape changed");
  assert.ok(generatingAt < renderAt && renderAt < awaitAt, "the generating state must be shown before the jobs are awaited");
});

test("submitTaskSimple stores field errors and re-renders (for the inline messages) before showing the generic panel message", () => {
  const body = boardSource.slice(boardSource.indexOf("async function submitTaskSimple()"));
  const fn = body.slice(0, body.indexOf("\n      }\n") + 1);
  const storeAt = fn.indexOf("ntSimple.fieldErrors = fieldErrors;");
  const renderAt = fn.indexOf("renderNewTaskModal();");
  const panelAt = fn.indexOf("Errors prevented submission");
  assert.ok(storeAt > -1 && renderAt > -1 && panelAt > -1, "submitTaskSimple shape changed");
  assert.ok(storeAt < renderAt && renderAt < panelAt, "field errors must be stored and rendered inline before the generic panel message is set");
});

// ---------- squad label sanitization ----------
// The daemon rejects a squad label containing a comma (it's split on ","
// for multi-name filtering -- see reject_label_with_comma in
// daemon/src/server.rs). ntSanitizeSquadLabel strips that up front so a
// prompt like `Add a file, each line say "blah"` doesn't fail submission.

test("ntSanitizeSquadLabel strips commas", () => {
  assert.equal(ntSanitizeSquadLabel('Add a file, each line say "blah". 5'), 'Add a file each line say "blah". 5');
});

test("ntSanitizeSquadLabel truncates to 60 chars before trimming", () => {
  const long = "a".repeat(70);
  assert.equal(ntSanitizeSquadLabel(long), "a".repeat(60));
});

test("ntSanitizeSquadLabel trims surrounding whitespace left behind by stripping", () => {
  assert.equal(ntSanitizeSquadLabel("hello, "), "hello");
  assert.equal(ntSanitizeSquadLabel(", hello"), "hello");
});

test("ntSanitizeSquadLabel returns null when nothing meaningful survives", () => {
  assert.equal(ntSanitizeSquadLabel(""), null);
  assert.equal(ntSanitizeSquadLabel("   "), null);
  assert.equal(ntSanitizeSquadLabel(",,,"), null);
});

test("ntSanitizeSquadLabel passes an ordinary prompt through unchanged (aside from truncation)", () => {
  assert.equal(ntSanitizeSquadLabel("fix the login bug"), "fix the login bug");
});

test("submitTaskSimple sanitizes the resolved naming label before submitting, and retries with no label on an invalid_label response", () => {
  const body = boardSource.slice(boardSource.indexOf("async function submitTaskSimple()"));
  const fn = body.slice(0, body.indexOf("\n      }\n") + 1);
  assert.match(fn, /ntSanitizeSquadLabel\(naming\.squadLabel\)/, "the label sent to the daemon must be sanitized first (RAL-398: derived from ntResolveNaming, not always the raw prompt)");
  const invalidLabelAt = fn.indexOf('"invalid_label"');
  const retryAt = fn.indexOf("postSquad(null)");
  assert.ok(invalidLabelAt > -1 && retryAt > -1, "submitTaskSimple must retry with no label on an invalid_label response");
  assert.ok(invalidLabelAt < retryAt, "the invalid_label check must gate the no-label retry");
});

// ---------- Task/squad naming (RAL-398) ----------
// A typed label wins outright; failing that, a ticket-id-shaped token found
// in the prompt (instant, no LLM call); failing that, both are left
// unresolved so the caller falls back to a placeholder name and asks the
// daemon to suggest one in the background (POST .../suggest-name).

test("extractTicketId finds common ticket-id shapes, case-insensitively", () => {
  assert.equal(extractTicketId("fix ABC-1234 please"), "ABC-1234");
  assert.equal(extractTicketId("see pipe-5163 for context"), "pipe-5163");
  assert.equal(extractTicketId("ref dev_1234 in the title"), "dev_1234");
  assert.equal(extractTicketId("FOO-980713-some_description needs work"), "FOO-980713-some_description");
});

test("extractTicketId returns null when no ticket-id-shaped token is present", () => {
  assert.equal(extractTicketId("just a plain description of the work"), null);
  assert.equal(extractTicketId(""), null);
});

test("slugifyTaskName lowercases and collapses non-alphanumeric runs to a single hyphen", () => {
  assert.equal(slugifyTaskName("Add Retry Logic To Upload Client!"), "add-retry-logic-to-upload-client");
  assert.equal(slugifyTaskName("PIPE-5163"), "pipe-5163");
  assert.equal(slugifyTaskName("  --leading/trailing--  "), "leading-trailing");
});

test("slugifyTaskName caps length at 60 chars", () => {
  const long = "word ".repeat(30);
  assert.ok(slugifyTaskName(long).length <= 60);
});

test("ntResolveNaming prefers a typed label over a ticket id in the prompt", () => {
  const result = ntResolveNaming("fix ABC-1234", "My Custom Label");
  assert.equal(result.squadLabel, "My Custom Label");
  assert.equal(result.taskName, "my-custom-label");
  assert.equal(result.needsGeneration, false);
});

test("ntResolveNaming falls back to a ticket id in the prompt when no label is typed", () => {
  const result = ntResolveNaming("fix PIPE-5163 in the uploader", "");
  assert.equal(result.squadLabel, "PIPE-5163");
  assert.equal(result.taskName, "pipe-5163");
  assert.equal(result.needsGeneration, false);
});

test("ntResolveNaming requests generation when neither a label nor a ticket id is available", () => {
  const result = ntResolveNaming("just fix the thing that's broken", "  ");
  assert.equal(result.squadLabel, null);
  assert.equal(result.taskName, null);
  assert.equal(result.needsGeneration, true);
});

// ---------- New Task modal reopen/resubmit resets the form to defaults ----------
// Only templateName/agent/model/project/upstreamBranch carry forward across
// a New Task modal open (e.g. after Cancel) or a successful submit -- every
// other field (prompt, review mode, the proofs/manual-checks/auto-build
// generation choices and their item lists, the generating/confirm-step view
// state) must reset to its default every time, so neither a cancelled
// submission's leftover generated items nor a completed one's leak into the
// next, unrelated task.

test("ntSimpleResetKeepingProjectFields carries forward exactly the five remembered fields", () => {
  const body = boardSource.slice(boardSource.indexOf("function ntSimpleResetKeepingProjectFields()"));
  const fn = body.slice(0, body.indexOf("\n      }\n") + 1);
  assert.match(fn, /ntSimpleReset\(\);/, "must fully reset before restoring the remembered fields");
  for (const field of ["templateName", "agent", "model", "project", "upstreamBranch"]) {
    assert.match(fn, new RegExp(`ntSimple\\.${field}\\s*=\\s*prev\\.${field}`), `must carry forward ${field}`);
  }
  // Fields that must NOT be carried forward -- they should reset to
  // ntFreshSimpleState()'s defaults, never copied from `prev`.
  for (const field of ["prompt", "reviewMode", "proofs", "generateManualChecks", "skipAutoBuild", "generateAutoBuild", "proofItems", "checkItems", "buildItems", "generating", "confirmStep"]) {
    assert.doesNotMatch(fn, new RegExp(`ntSimple\\.${field}\\s*=\\s*prev\\.${field}`), `must NOT carry forward ${field}`);
  }
});

test("openNewTask resets the Simple tab via ntSimpleResetKeepingProjectFields, not a bare full reset", () => {
  const body = boardSource.slice(boardSource.indexOf("function openNewTask()"));
  const fn = body.slice(0, body.indexOf("\n      }\n") + 1);
  assert.match(fn, /ntSimpleResetKeepingProjectFields\(\);/, "openNewTask must use the carry-forward reset");
  assert.doesNotMatch(fn, /\bntSimpleReset\(\);/, "openNewTask must not call the bare full reset directly");
});

test("a successful submitTaskSimple resets via ntSimpleResetKeepingProjectFields, not a bare full reset", () => {
  const body = boardSource.slice(boardSource.indexOf("async function submitTaskSimple()"));
  const fn = body.slice(0, body.indexOf("\n      }\n") + 1);
  assert.match(fn, /ntSimpleResetKeepingProjectFields\(\);/, "submitTaskSimple must use the carry-forward reset on success");
  assert.doesNotMatch(fn, /\bntSimpleReset\(\);/, "submitTaskSimple must not call the bare full reset directly");
});

test("submitTaskSimple's post-generation render is guarded so a cancelled modal doesn't pop back open", () => {
  const body = boardSource.slice(boardSource.indexOf("async function submitTaskSimple()"));
  const fn = body.slice(0, body.indexOf("\n      }\n") + 1);
  const awaitAt = fn.indexOf("await Promise.all(jobs);");
  const guardedRenderAt = fn.indexOf("if (ntModalOpen && ntTab ===");
  assert.ok(awaitAt > -1 && guardedRenderAt > -1, "submitTaskSimple shape changed");
  assert.ok(awaitAt < guardedRenderAt, "the render after awaiting generation jobs must be guarded on ntModalOpen");
});

// ---------- Cancel actually kills an in-flight generation call ----------
// "Cancel" must stop the agent subprocess a "Generate proof steps"/"Generate
// manual checks"/"Generate auto-build steps" call spawned, not just abandon
// the poll and let it keep running server-side.

test("ntRunGenerationStep tracks its job id in activeGenerationIds while in flight, and always untracks it in a finally block", () => {
  const body = boardSource.slice(boardSource.indexOf("async function ntRunGenerationStep("));
  const fn = body.slice(0, body.indexOf("\n      }\n") + 1);
  assert.match(fn, /ntSimple\.activeGenerationIds\.push\(id\)/, "must track the job id once it's known");
  assert.match(fn, /finally\s*{[\s\S]*ntSimple\.activeGenerationIds\s*=\s*ntSimple\.activeGenerationIds\.filter/, "must untrack the job id in a finally block, regardless of outcome");
});

test("closeModal cancels every active generation job when the New Task modal (Simple tab) closes mid-generation", () => {
  const body = boardSource.slice(boardSource.indexOf("function closeModal()"));
  const fn = body.slice(0, body.indexOf("\n      }\n") + 1);
  assert.match(fn, /ntModalOpen && ntTab === "simple" && ntSimple\.activeGenerationIds\.length/, "closeModal must only act when the Simple tab has generations in flight");
  assert.match(fn, /ntCancelActiveGenerations\(\)/, "closeModal must cancel the active generations");
});

test("ntCancelActiveGenerations posts a cancel request per active job id and clears the tracking list", () => {
  const body = boardSource.slice(boardSource.indexOf("function ntCancelActiveGenerations()"));
  const fn = body.slice(0, body.indexOf("\n      }\n") + 1);
  assert.match(fn, /ntSimple\.activeGenerationIds\s*=\s*\[\]/, "must clear the tracking list");
  assert.match(fn, /post\(`\/api\/generate\/\$\{id\}\/cancel`\)/, "must POST /api/generate/{id}/cancel for each active job");
});

// ---------- inline field errors clear when the field is fixed ----------
// A field that failed validation must stop showing its error as soon as the
// user changes it, not linger until the next submit attempt.

test("the Agent and Project selects clear their own field error on change", () => {
  assert.match(boardSource, /onchange="ntSimple\.agent=this\.value;ntSimple\.model='';ntClearFieldError\('agent'\);renderNewTaskModal\(\)"/, "Agent's onchange must clear its field error");
  assert.match(boardSource, /onchange="ntSimple\.project=this\.value;ntClearFieldError\('project'\);renderNewTaskModal\(\)"/, "Project's onchange must clear its field error");
});

test("the prompt textarea and a template field clear their error inline (no full re-render) on input", () => {
  assert.match(boardSource, /oninput="ntSimple\.prompt=this\.value;ntClearFieldErrorInline\('prompt', this\)"/, "the prompt textarea must clear its error inline as the user types");
  assert.match(boardSource, /ntClearFieldErrorInline\(\$\{JSON\.stringify\(`field:\$\{f\.name\}`\)\}, this\)/, "a template field must clear its error inline as the user types");
});

test("ntClearFieldErrorInline removes both the .err class and the field's error message element", () => {
  const body = boardSource.slice(boardSource.indexOf("function ntClearFieldErrorInline("));
  const fn = body.slice(0, body.indexOf("\n      }\n") + 1);
  assert.match(fn, /ntClearFieldError\(field\)/, "must also drop the field from ntSimple.fieldErrors");
  assert.match(fn, /el\.classList\.remove\("err"\)/, "must remove the red-border class from the field itself");
  assert.match(fn, /data-field-error/, "must locate and remove the field's error message element");
});

// ---------- Agent/Model and Project/Upstream rows stay top-aligned ----------
// .row's shared CSS default is `align-items: center`, which vertically
// centers each column by its own height -- fine when both columns are the
// same height, but an inline field error only grows ONE column (Agent's or
// Project's), which then pushes its shorter sibling (Model/Upstream branch)
// down out of alignment with it. These two rows override align-items so the
// widgets stay level regardless of which column has an error.

test("the Agent/Model and Project/Upstream rows override align-items to flex-start", () => {
  const agentRowAt = boardSource.indexOf('data-tip="Which agent backend runs the work cell.');
  const agentRowStart = boardSource.lastIndexOf('<div class="row"', agentRowAt);
  assert.match(boardSource.slice(agentRowStart, agentRowAt), /align-items:flex-start/, "the Agent/Model row must top-align its columns");

  const projectRowAt = boardSource.indexOf('data-tip="Which registered project the work runs against.');
  const projectRowStart = boardSource.lastIndexOf('<div class="row"', projectRowAt);
  assert.match(boardSource.slice(projectRowStart, projectRowAt), /align-items:flex-start/, "the Project/Upstream row must top-align its columns");
});
