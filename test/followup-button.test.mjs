// Coverage for RAL-582's "Follow-ups" menu item and list on a review:
// the item's disabled / "off" / badge states, the list's cards, and that the
// ignore and un-ignore actions post the prophecy id to the daemon and refresh
// the review's summary from the answer.

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

const esc = (s) => String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");

const menuItem = new Function("esc", `${functionSource("followupMenuItem")}; return followupMenuItem;`)(esc);
const list = new Function(
  "esc",
  `${functionSource("followupCardHtml")}; ${functionSource("followupListHtml")}; return followupListHtml;`,
)(esc);

const summary = (o) => ({ enabled: true, off: false, offered: false, count: 0, total: 0, ...o });

test("the menu item is absent until the review detail carries a summary", () => {
  assert.equal(menuItem({ id: "g1" }), "");
});

test("the menu item is disabled when there are no deferred follow-ups", () => {
  const html = menuItem({ id: "g1", pending_followups: summary({}) });
  assert.match(html, /class="ctx-disabled"/);
  assert.doesNotMatch(html, /data-click/);
  assert.match(html, /data-tip="[^"]+"/);
});

test("the menu item shows the pending count as a badge and opens the list", () => {
  const html = menuItem({ id: "g1", pending_followups: summary({ count: 3, total: 4 }) });
  assert.doesNotMatch(html, /ctx-disabled/);
  assert.match(html, /<span class="badge">3<\/span>/);
  assert.match(html, /data-click="openFollowupList" data-guardian-id="g1"/);
});

test("the menu item stays enabled when every note is ignored so they can be un-ignored", () => {
  const html = menuItem({ id: "g1", pending_followups: summary({ count: 0, total: 2 }) });
  assert.doesNotMatch(html, /ctx-disabled/);
  assert.match(html, /<span class="badge">0<\/span>/);
});

test("the menu item reads off when the review has follow-ups disabled", () => {
  const html = menuItem({ id: "g1", pending_followups: summary({ enabled: false, off: true, count: 2, total: 2 }) });
  assert.match(html, /Follow-ups <span class="hint">off<\/span>/);
  assert.match(html, /ctx-disabled/);
  assert.doesNotMatch(html, /openFollowupList/);
});

test("the list says what happens at merge and toggles keep/ignore per follow-up", () => {
  const data = {
    ...summary({ count: 1, total: 2 }),
    items: [
      { prophecy_id: 7, entity_uri: "cell:squad-1:0:0", body: "tidy <b>docs</b>", prompt: "do it", ignored: false, branch: "feat/readme", task_name: "Add feature", agent: "claude-code", model: "sonnet" },
      { prophecy_id: 8, entity_uri: "cell:squad-1:0:1", body: "later", prompt: null, ignored: true },
    ],
  };
  const html = list("g1", data);
  assert.match(html, /When this review merges, each follow-up below becomes a new task/);
  assert.match(html, /data-click="ignoreFollowup" data-guardian-id="g1" data-prophecy-id="7"/);
  assert.match(html, /data-click="unignoreFollowup" data-guardian-id="g1" data-prophecy-id="8"/);
  assert.match(html, /tidy &lt;b&gt;docs&lt;\/b&gt;/);
  assert.match(html, /do it/);
});

test("a card names its branch and task rather than showing the cell URI", () => {
  const data = {
    ...summary({ count: 2, total: 2 }),
    items: [
      { prophecy_id: 7, entity_uri: "cell:squad-1:0:0", body: "a", ignored: false, branch: "feat/readme", task_name: "Add feature", agent: "claude-code", model: "sonnet" },
      { prophecy_id: 8, entity_uri: "cell:squad-1:0:1", body: "b", ignored: false, branch: null, task_name: null },
    ],
  };
  const html = list("g1", data);
  assert.match(html, />⎇ feat\/readme</);
  assert.match(html, />Add feature</);
  assert.match(html, />claude-code · sonnet</);
  assert.match(html, />branch unknown</);
  // The URI is only a tooltip, never visible card text.
  assert.doesNotMatch(html, />cell:squad-1/);
});

test("the list has no actions once the offer snapshot is final", () => {
  const data = {
    ...summary({ offered: true, enabled: false, count: 1, total: 1 }),
    items: [{ prophecy_id: 7, entity_uri: "cell:squad-1:0:0", body: "x", ignored: false }],
  };
  const html = list("g1", data);
  assert.doesNotMatch(html, /data-click="(un)?ignoreFollowup"/);
  assert.match(html, /already merged and made its follow-up offer/);
});

test("ignoring posts the prophecy id and un-ignoring uses the unignore route", () => {
  const src = functionSource("setFollowupIgnored");
  assert.match(src, /followup\/\$\{ignore \? "ignore" : "unignore"\}`, \{ prophecy_id: prophecyId \}/);
  assert.match(src, /g\.pending_followups = \{/);
  assert.match(script, /CLICK_HANDLERS\.ignoreFollowup = .*setFollowupIgnored\(.*, true\)/);
  assert.match(script, /CLICK_HANDLERS\.unignoreFollowup = .*setFollowupIgnored\(.*, false\)/);
});
