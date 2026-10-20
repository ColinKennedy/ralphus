// Coverage for the exhibit settings controls shared by the review Setup modal
// and the project Review Settings modal: seeding a draft from the daemon's raw
// values, client-side validation of the counts and the kind order, the
// request-body patch (only changed fields; `null` or a `clear_*` flag to
// inherit), and the rendered markup's tooltips.
//
// Same slice-the-real-source approach as ./board-rebuild-on.test.mjs: the
// RALPHUS-EXHIBIT-SETTINGS:BEGIN/END block in
// librarian/assets/board/66-review-edit-modal.js touches only `esc` and
// `REVIEW_EDIT_INPUT_STYLE`, which the factory below supplies.

import test from "node:test";
import assert from "node:assert/strict";
import { boardScript } from "./board-source.mjs";

const BEGIN = "// RALPHUS-EXHIBIT-SETTINGS:BEGIN";
const END = "// RALPHUS-EXHIBIT-SETTINGS:END";

const script = boardScript();
const from = script.indexOf(BEGIN);
const to = script.indexOf(END);
if (from === -1 || to === -1 || to < from) {
  throw new Error(
    `board-exhibit-settings: could not find the ${BEGIN} / ${END} markers in the board chunks. ` +
      "If this logic moved, move the markers with it -- these tests are its only coverage.",
  );
}
const source = script.slice(from + BEGIN.length, to);

const esc = (s) =>
  String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");

// eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
const factory = new Function(
  "esc",
  "REVIEW_EDIT_INPUT_STYLE",
  `${source}
   return {
     EXHIBIT_KINDS,
     buildExhibitSettingsDraft,
     parseExhibitKinds,
     parseExhibitCount,
     exhibitSettingsPatch,
     renderExhibitFieldsHtml,
   };`,
);

const ex = factory(esc, "");

test("a draft seeds blank fields for inherited settings and text for set ones", () => {
  assert.deepEqual(ex.buildExhibitSettingsDraft({}), {
    enabled: "", maxPerPr: "", infer: "", kinds: "", maxVideoSeconds: "",
  });
  assert.deepEqual(
    ex.buildExhibitSettingsDraft({
      exhibits_enabled: false,
      exhibits_max_per_pr: 3,
      infer_exhibits: null,
      exhibit_kind_preference: ["screenshot", "gif"],
      exhibit_max_video_seconds: 20,
    }),
    { enabled: "false", maxPerPr: "3", infer: "", kinds: "screenshot, gif", maxVideoSeconds: "20" },
  );
});

test("kind lists keep their order and reject empty, unknown and repeated kinds", () => {
  assert.deepEqual(ex.parseExhibitKinds(""), { kinds: null, error: "" });
  assert.deepEqual(ex.parseExhibitKinds(" Video , gif "), { kinds: ["video", "gif"], error: "" });
  assert.match(ex.parseExhibitKinds(",").error, /at least one/);
  assert.match(ex.parseExhibitKinds("gif, output").error, /"output"/);
  assert.match(ex.parseExhibitKinds("gif,gif").error, /more than once/);
});

test("counts must be whole numbers of at least 1; blank inherits", () => {
  assert.deepEqual(ex.parseExhibitCount("", "x"), { value: null, error: "" });
  assert.deepEqual(ex.parseExhibitCount(" 4 ", "x"), { value: 4, error: "" });
  for (const bad of ["0", "-1", "1.5", "two"]) assert.match(ex.parseExhibitCount(bad, "x").error, /at least 1/, bad);
});

test("the patch sends only changed fields, clearing with null or clear_* flags", () => {
  const original = ex.buildExhibitSettingsDraft({ exhibits_max_per_pr: 3, exhibit_kind_preference: ["gif"] });
  assert.deepEqual(ex.exhibitSettingsPatch({ ...original }, original, false), { body: {}, error: "" });
  const draft = { ...original, enabled: "false", infer: "true", maxPerPr: "", kinds: "screenshot", maxVideoSeconds: "9" };
  assert.deepEqual(ex.exhibitSettingsPatch(draft, original, false).body, {
    exhibits_enabled: false,
    infer_exhibits: true,
    exhibits_max_per_pr: null,
    exhibit_max_video_seconds: 9,
    exhibit_kind_preference: ["screenshot"],
  });
  assert.deepEqual(ex.exhibitSettingsPatch(draft, original, true).body, {
    exhibits_enabled: false,
    infer_exhibits: true,
    clear_exhibits_max_per_pr: true,
    exhibit_max_video_seconds: 9,
    exhibit_kind_preference: ["screenshot"],
  });
  const back = { ...original, enabled: "", kinds: "" };
  const changed = { ...original, enabled: "true" };
  assert.deepEqual(ex.exhibitSettingsPatch(back, changed, false).body, { exhibits_enabled: null, exhibit_kind_preference: null });
  // Whitespace-only edits of the kind list are not a change.
  assert.deepEqual(ex.exhibitSettingsPatch({ ...original, kinds: " gif " }, original, false).body, {});
});

test("an invalid field blocks the whole patch with its error", () => {
  const original = ex.buildExhibitSettingsDraft({});
  assert.match(ex.exhibitSettingsPatch({ ...original, maxPerPr: "0" }, original, false).error, /exhibits per PR/);
  assert.match(ex.exhibitSettingsPatch({ ...original, kinds: "png" }, original, true).error, /"png"/);
});

test("every rendered control carries a tooltip and shows the inherited value", () => {
  const html = ex.renderExhibitFieldsHtml(
    ex.buildExhibitSettingsDraft({}),
    { enabled: true, maxPerPr: 6, infer: false, kinds: ["gif", "video", "screenshot"], maxVideoSeconds: 15 },
    "onX",
    "project default",
  );
  const controls = html.match(/<(select|input)\b[^>]*>/g) || [];
  assert.equal(controls.length, 5);
  for (const c of controls) assert.match(c, /data-tip="/, c);
  assert.match(html, /inherit project default \(on\)/);
  assert.match(html, /inherit project default \(off\)/);
  assert.match(html, /placeholder="gif, video, screenshot"/);
  assert.match(html, /min="1"/);
  assert.match(html, /onX\('kinds',this.value\)/);
});
