// Coverage for RAL-582's "Deferred follow-ups" button and list on a review:
// the button's disabled / "follow-ups off" / badge states, and that the
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

const button = new Function("esc", `${functionSource("followupButtonHtml")}; return followupButtonHtml;`)(esc);
const list = new Function("esc", `${functionSource("followupListHtml")}; return followupListHtml;`)(esc);

const summary = (o) => ({ enabled: true, off: false, offered: false, count: 0, total: 0, ...o });

test("the button is absent until the review detail carries a summary", () => {
  assert.equal(button({ id: "g1" }), "");
});

test("the button is disabled when there are no deferred follow-ups", () => {
  const html = button({ id: "g1", pending_followups: summary({}) });
  assert.match(html, /<button[^>]* disabled /);
  assert.match(html, /data-tip="[^"]+"/);
});

test("the button shows the pending count as a badge and opens the list", () => {
  const html = button({ id: "g1", pending_followups: summary({ count: 3, total: 4 }) });
  assert.doesNotMatch(html, / disabled /);
  assert.match(html, /<span class="badge">3<\/span>/);
  assert.match(html, /data-click="openFollowupList" data-guardian-id="g1"/);
});

test("the button stays enabled when every note is ignored so they can be un-ignored", () => {
  const html = button({ id: "g1", pending_followups: summary({ count: 0, total: 2 }) });
  assert.doesNotMatch(html, / disabled /);
  assert.match(html, /<span class="badge">0<\/span>/);
});

test("the button reads follow-ups off when the review has follow-ups disabled", () => {
  const html = button({ id: "g1", pending_followups: summary({ enabled: false, off: true, count: 2, total: 2 }) });
  assert.match(html, /follow-ups off/);
  assert.match(html, / disabled /);
  assert.doesNotMatch(html, /openFollowupList/);
});

test("the list offers Ignore on live items and Un-ignore on ignored ones", () => {
  const data = {
    ...summary({ count: 1, total: 2 }),
    items: [
      { prophecy_id: 7, entity_uri: "cell:squad-1/0/0", body: "tidy <b>docs</b>", prompt: "do it", ignored: false },
      { prophecy_id: 8, entity_uri: "cell:squad-1/0/1", body: "later", prompt: null, ignored: true },
    ],
  };
  const html = list("g1", data);
  assert.match(html, /data-click="ignoreFollowup" data-guardian-id="g1" data-prophecy-id="7"/);
  assert.match(html, /data-click="unignoreFollowup" data-guardian-id="g1" data-prophecy-id="8"/);
  assert.match(html, /tidy &lt;b&gt;docs&lt;\/b&gt;/);
  assert.match(html, /cell:squad-1\/0\/0/);
  assert.match(html, /do it/);
});

test("the list has no actions once the offer snapshot is final", () => {
  const data = {
    ...summary({ offered: true, enabled: false, count: 1, total: 1 }),
    items: [{ prophecy_id: 7, entity_uri: "cell:squad-1/0/0", body: "x", ignored: false }],
  };
  assert.doesNotMatch(list("g1", data), /data-click="(un)?ignoreFollowup"/);
});

test("ignoring posts the prophecy id and un-ignoring uses the unignore route", () => {
  const src = functionSource("setFollowupIgnored");
  assert.match(src, /followup\/\$\{ignore \? "ignore" : "unignore"\}`, \{ prophecy_id: prophecyId \}/);
  assert.match(src, /g\.pending_followups = \{/);
  assert.match(script, /CLICK_HANDLERS\.ignoreFollowup = .*setFollowupIgnored\(.*, true\)/);
  assert.match(script, /CLICK_HANDLERS\.unignoreFollowup = .*setFollowupIgnored\(.*, false\)/);
});
