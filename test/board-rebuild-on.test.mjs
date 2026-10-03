// Coverage for the `rebuild_on` review setting -- when a review's prepared
// build is torn down and rebuilt (any of "rebase", "feedback", "auto_fix"; an
// empty list means "manual only"; no value means "inherit the default").
//
// Exercised here, against the real shipped source:
// - the draft logic shared by the review Setup modal and the project Review
//   Settings modal: normalising, inherit vs explicit seeding, toggling,
//   summary text, change detection, and the request-body value (`null`
//   clears back to inherit, `[]` is manual-only);
// - `renderRebuildOnFieldsHtml`'s markup: the group, one checkbox per trigger,
//   disabled-while-inheriting, a tooltip on every control;
// - `refreshRebuildOnFields` updating the rendered control in place;
// - the wiring in the two modals' drafts, save bodies and the setup chip.
//
// Same slice-the-real-source approach as ./board-forge-check.test.mjs: the
// RALPHUS-REBUILD-ON:BEGIN/END block in
// librarian/assets/board/66-review-edit-modal.js touches only `esc` and the
// ambient `document`, which the factory below supplies.

import test from "node:test";
import assert from "node:assert/strict";
import { boardScript } from "./board-source.mjs";

const BEGIN = "// RALPHUS-REBUILD-ON:BEGIN";
const END = "// RALPHUS-REBUILD-ON:END";

const script = boardScript();
const from = script.indexOf(BEGIN);
const to = script.indexOf(END);
if (from === -1 || to === -1 || to < from) {
  throw new Error(
    `board-rebuild-on: could not find the ${BEGIN} / ${END} markers in the board chunks. ` +
      "If this logic moved, move the markers with it -- these tests are its only coverage.",
  );
}
const source = script.slice(from + BEGIN.length, to);

const esc = (s) =>
  String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");

// eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
const factory = new Function(
  "esc",
  "document",
  `${source}
   return {
     REBUILD_TRIGGERS,
     REBUILD_ON_TIP,
     REBUILD_TRIGGER_TIPS,
     normalizeRebuildOn,
     buildRebuildOnDraft,
     rebuildOnShown,
     setRebuildInherit,
     setRebuildTrigger,
     rebuildOnSummary,
     rebuildOnChipText,
     rebuildOnChanged,
     rebuildOnBodyValue,
     renderRebuildOnFieldsHtml,
     refreshRebuildOnFields,
   };`,
);

/** A minimal document: just id -> element lookup. */
function fakeDocument(elements) {
  return { getElementById: (id) => elements[id] || null };
}

const rb = factory(esc, fakeDocument({}));
const ALL = ["rebase", "feedback", "auto_fix"];

test("normalizeRebuildOn keeps known triggers, dedupes, and orders them canonically", () => {
  assert.deepEqual(rb.normalizeRebuildOn(["auto_fix", "rebase", "rebase"]), ["rebase", "auto_fix"]);
  assert.deepEqual(rb.normalizeRebuildOn(["bogus", "feedback"]), ["feedback"]);
  assert.deepEqual(rb.normalizeRebuildOn([]), []);
  assert.deepEqual(rb.normalizeRebuildOn(null), []);
  assert.deepEqual(rb.normalizeRebuildOn(undefined), []);
});

test("the trigger set is exactly rebase / feedback / auto_fix, with a tooltip for each", () => {
  assert.deepEqual(rb.REBUILD_TRIGGERS, ALL);
  for (const trigger of ALL) {
    assert.ok(rb.REBUILD_TRIGGER_TIPS[trigger].length > 20, `${trigger} needs a real tooltip`);
  }
});

test("a null or absent explicit value seeds an inheriting draft showing the effective default", () => {
  for (const explicit of [null, undefined]) {
    const draft = rb.buildRebuildOnDraft(explicit, ["rebase"]);
    assert.equal(draft.inherit, true);
    assert.deepEqual(rb.rebuildOnShown(draft, ["rebase"]), ["rebase"]);
  }
});

test("an inheriting draft with no known default falls back to every trigger", () => {
  const draft = rb.buildRebuildOnDraft(null, undefined);
  assert.equal(draft.inherit, true);
  assert.deepEqual(rb.rebuildOnShown(draft, undefined), ALL);
  assert.deepEqual(rb.rebuildOnShown(draft, null), ALL);
});

test("an explicit list seeds a non-inheriting draft, including the empty (manual only) list", () => {
  const some = rb.buildRebuildOnDraft(["feedback", "rebase"], ALL);
  assert.equal(some.inherit, false);
  assert.deepEqual(some.triggers, ["rebase", "feedback"]);

  const none = rb.buildRebuildOnDraft([], ALL);
  assert.equal(none.inherit, false, "[] is an explicit 'never', not 'inherit'");
  assert.deepEqual(none.triggers, []);
});

test("summaries read correctly for inherit, all, each single trigger, pairs, and none", () => {
  const inherit = rb.buildRebuildOnDraft(null, ALL);
  assert.equal(rb.rebuildOnSummary(inherit, ALL), "inherited: every rebase, feedback, and auto PR fix");
  assert.equal(rb.rebuildOnSummary(inherit, []), "inherited: never — rebuild manually");

  const explicit = (list) => rb.buildRebuildOnDraft(list, ALL);
  assert.equal(rb.rebuildOnSummary(explicit(ALL), ALL), "every rebase, feedback, and auto PR fix");
  assert.equal(rb.rebuildOnSummary(explicit(["rebase"]), ALL), "rebase");
  assert.equal(rb.rebuildOnSummary(explicit(["feedback"]), ALL), "reviewer feedback");
  assert.equal(rb.rebuildOnSummary(explicit(["auto_fix"]), ALL), "auto PR fix");
  assert.equal(rb.rebuildOnSummary(explicit(["rebase", "auto_fix"]), ALL), "rebase + auto PR fix");
  assert.equal(rb.rebuildOnSummary(explicit([]), ALL), "never — rebuild manually");
});

test("chip text is short for every policy", () => {
  assert.equal(rb.rebuildOnChipText(ALL), "on every change");
  assert.equal(rb.rebuildOnChipText(undefined), "on every change");
  assert.equal(rb.rebuildOnChipText([]), "manual only");
  assert.equal(rb.rebuildOnChipText(["rebase"]), "rebase");
  assert.equal(rb.rebuildOnChipText(["feedback", "auto_fix"]), "feedback + auto fix");
});

test("leaving inherit pins the inherited value; returning to inherit keeps the draft inheriting", () => {
  const draft = rb.buildRebuildOnDraft(null, ["rebase", "feedback"]);
  rb.setRebuildInherit(draft, false, ["rebase", "feedback"]);
  assert.equal(draft.inherit, false);
  assert.deepEqual(draft.triggers, ["rebase", "feedback"], "unticking 'use default' must not silently change the policy");

  rb.setRebuildTrigger(draft, "feedback", false);
  rb.setRebuildInherit(draft, true, ["rebase", "feedback"]);
  assert.equal(draft.inherit, true);
  assert.deepEqual(rb.rebuildOnShown(draft, ["rebase", "feedback"]), ["rebase", "feedback"], "inheriting shows the default again");
});

test("setRebuildTrigger toggles each trigger independently and ignores unknown ones", () => {
  const draft = rb.buildRebuildOnDraft([], ALL);
  rb.setRebuildTrigger(draft, "auto_fix", true);
  rb.setRebuildTrigger(draft, "rebase", true);
  assert.deepEqual(draft.triggers, ["rebase", "auto_fix"], "order-independent: always canonical");
  rb.setRebuildTrigger(draft, "rebase", true);
  assert.deepEqual(draft.triggers, ["rebase", "auto_fix"], "ticking twice does not duplicate");
  rb.setRebuildTrigger(draft, "rebase", false);
  assert.deepEqual(draft.triggers, ["auto_fix"]);
  rb.setRebuildTrigger(draft, "nonsense", true);
  assert.deepEqual(draft.triggers, ["auto_fix"]);
});

test("rebuildOnChanged: inheriting drafts are equal regardless of their ignored triggers", () => {
  const original = rb.buildRebuildOnDraft(null, ALL);
  const draft = rb.buildRebuildOnDraft(null, ALL);
  draft.triggers = ["rebase"];
  assert.equal(rb.rebuildOnChanged(draft, original), false);
});

test("rebuildOnChanged detects leaving or entering inherit and any trigger change", () => {
  const inherit = rb.buildRebuildOnDraft(null, ALL);
  const explicitAll = rb.buildRebuildOnDraft(ALL, ALL);
  assert.equal(rb.rebuildOnChanged(explicitAll, inherit), true, "pinning the default is still a change");
  assert.equal(rb.rebuildOnChanged(inherit, explicitAll), true, "clearing back to inherit is a change");

  const original = rb.buildRebuildOnDraft(["rebase"], ALL);
  const same = rb.buildRebuildOnDraft(["rebase"], ALL);
  assert.equal(rb.rebuildOnChanged(same, original), false);
  rb.setRebuildTrigger(same, "feedback", true);
  assert.equal(rb.rebuildOnChanged(same, original), true);
  rb.setRebuildTrigger(same, "feedback", false);
  assert.equal(rb.rebuildOnChanged(same, original), false, "toggling back is not a change");

  const none = rb.buildRebuildOnDraft([], ALL);
  assert.equal(rb.rebuildOnChanged(none, original), true);
});

test("the request body value: null clears to inherit, a canonical list sets, [] means manual only", () => {
  assert.equal(rb.rebuildOnBodyValue(rb.buildRebuildOnDraft(null, ALL)), null);
  assert.deepEqual(rb.rebuildOnBodyValue(rb.buildRebuildOnDraft(["auto_fix", "rebase"], ALL)), ["rebase", "auto_fix"]);
  assert.deepEqual(rb.rebuildOnBodyValue(rb.buildRebuildOnDraft([], ALL)), []);
});

test("the rendered group has the inherit checkbox, a checkbox per trigger, a summary, and tooltips throughout", () => {
  const draft = rb.buildRebuildOnDraft(["rebase"], ALL);
  const html = rb.renderRebuildOnFieldsHtml("review", draft, ALL, "use the project default", "inherit tip", "onInherit", "onTrigger");
  assert.match(html, /id="review-rebuild-on"/);
  assert.match(html, /id="review-rebuild-inherit"/);
  assert.match(html, /onchange="onInherit\(this\.checked\)"/);
  for (const trigger of ALL) {
    assert.match(html, new RegExp(`id="review-rebuild-${trigger}"`));
    assert.ok(html.includes(`onTrigger('${trigger}',this.checked)`), `${trigger} checkbox calls the trigger handler`);
  }
  assert.match(html, /id="review-rebuild-summary"[^>]*>rebase</);
  // Every <label>, the wrapper and the summary carry a tooltip.
  const tipped = (html.match(/data-tip="/g) || []).length;
  assert.ok(tipped >= 6, `expected a tooltip on the wrapper, 4 labels and the summary, saw ${tipped}`);
  assert.ok(html.includes(esc(rb.REBUILD_ON_TIP)), "the group tooltip explains teardown + reset + rebuild");
  assert.match(rb.REBUILD_ON_TIP, /before_reset_command/);
});

test("while inheriting, the trigger boxes are disabled and show the default; explicit leaves them editable", () => {
  const inheriting = rb.renderRebuildOnFieldsHtml("p", rb.buildRebuildOnDraft(null, ["feedback"]), ["feedback"], "l", "t", "a", "b");
  assert.match(inheriting, /id="p-rebuild-inherit" checked/);
  assert.match(inheriting, /id="p-rebuild-feedback" checked disabled/);
  assert.match(inheriting, /id="p-rebuild-rebase"\s+disabled/);
  assert.match(inheriting, /inherited: reviewer feedback/);

  const explicit = rb.renderRebuildOnFieldsHtml("p", rb.buildRebuildOnDraft(["rebase"], ALL), ALL, "l", "t", "a", "b");
  assert.doesNotMatch(explicit, /id="p-rebuild-inherit" checked/);
  assert.doesNotMatch(explicit, /disabled/);
  assert.match(explicit, /id="p-rebuild-rebase" checked/);
  assert.doesNotMatch(explicit, /id="p-rebuild-feedback" checked/);
});

test("the group escapes caller-supplied label and tooltip text", () => {
  const html = rb.renderRebuildOnFieldsHtml("p", rb.buildRebuildOnDraft(null, ALL), ALL, "<b>x</b>", 'say "hi"', "a", "b");
  assert.ok(!html.includes("<b>x</b>"));
  assert.ok(html.includes("&lt;b&gt;x&lt;/b&gt;"));
  assert.ok(html.includes("say &quot;hi&quot;"));
});

test("refreshRebuildOnFields updates the rendered checkboxes and summary in place", () => {
  const elements = {};
  for (const t of ALL) elements[`review-rebuild-${t}`] = { checked: false, disabled: false };
  elements["review-rebuild-summary"] = { textContent: "" };
  const live = factory(esc, fakeDocument(elements));

  const draft = live.buildRebuildOnDraft(["feedback"], ALL);
  live.refreshRebuildOnFields("review", draft, ALL);
  assert.equal(elements["review-rebuild-feedback"].checked, true);
  assert.equal(elements["review-rebuild-rebase"].checked, false);
  assert.equal(elements["review-rebuild-feedback"].disabled, false);
  assert.equal(elements["review-rebuild-summary"].textContent, "reviewer feedback");

  live.setRebuildInherit(draft, true, ["rebase"]);
  live.refreshRebuildOnFields("review", draft, ["rebase"]);
  assert.equal(elements["review-rebuild-rebase"].checked, true, "shows the inherited default");
  assert.equal(elements["review-rebuild-feedback"].checked, false);
  assert.ok(ALL.every((t) => elements[`review-rebuild-${t}`].disabled), "all boxes disabled while inheriting");
  assert.equal(elements["review-rebuild-summary"].textContent, "inherited: rebase");

  // A missing element (modal closed mid-update) must not throw.
  assert.doesNotThrow(() => live.refreshRebuildOnFields("gone", draft, ALL));
});

// ---- wiring in the modals and the review pane (source-level, like board-long-running.test.mjs) ----

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

test("the review Setup modal seeds the draft, renders the group, and saves rebuild_on only when changed", () => {
  assert.match(functionSource("buildReviewEditDraft"), /rebuildOn: buildRebuildOnDraft\(g\.rebuild_on, rebuildOnEffective\)/);
  assert.match(functionSource("buildReviewEditDraft"), /g\.effective_rebuild_on/);
  const render = functionSource("renderReviewEditModal");
  assert.match(render, /grp\("rebuild"\)/, "the group the chip focuses");
  assert.match(render, /renderRebuildOnFieldsHtml\("review"/);
  assert.match(render, /onEditRebuildOnInherit/);
  assert.match(render, /onEditRebuildOnTrigger/);
  const save = functionSource("saveReviewEditDetails");
  assert.match(save, /if \(rebuildOnChanged\(draft\.rebuildOn, draft\.originalRebuildOn\)\) body\.rebuild_on = rebuildOnBodyValue\(draft\.rebuildOn\)/);
  assert.match(save, /`\/api\/guardians\/\$\{draft\.gid\}\/details`/, "saves through the same /details request as its neighbours");
});

test("the project Review Settings modal seeds, renders, and saves rebuild_on only when changed", () => {
  assert.match(functionSource("buildProjectReviewSettingsDraft"), /rebuildOn: buildRebuildOnDraft\(s\.rebuild_on, effective\.rebuild_on\)/);
  assert.match(functionSource("renderProjectReviewSettingsModal"), /renderRebuildOnFieldsHtml\("project"/);
  assert.match(
    functionSource("saveProjectReviewSettings"),
    /if \(rebuildOnChanged\(draft\.rebuildOn, draft\.originalRebuildOn\)\) body\.rebuild_on = rebuildOnBodyValue\(draft\.rebuildOn\)/,
  );
});

test("the setup strip shows a rebuild chip that jumps to the group, only when the daemon reports a policy", () => {
  const strip = script.slice(script.indexOf("function reviewSetupStrip("));
  assert.match(strip, /g\.effective_rebuild_on === undefined \? "" : setupChip\(g\.id, "rebuild"/);
});

test("Rebuild now confirms with the teardown warning, posts to /rebuild, and locks while preparing", () => {
  const rebuild = functionSource("rebuildPreparationNow");
  assert.match(rebuild, /confirm\(/);
  assert.match(rebuild, /This cannot be undone\./);
  assert.match(rebuild, /`\/api\/guardians\/\$\{id\}\/rebuild`/);

  const control = functionSource("rebuildNowControl");
  assert.match(control, /g\.status !== "in_review"/);
  assert.match(control, /g\.post_merge_status === "running"/);
  assert.match(control, /This cannot be undone\./);
  assert.match(control, /data-click="rebuildPreparationNow"/);
  assert.match(script, /CLICK_HANDLERS\.rebuildPreparationNow = /);
});
