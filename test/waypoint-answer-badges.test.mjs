// Coverage for RAL-622: the waypoint detail page's survey-verdict and answer
// badges. An entry the survey released never reads "awaiting answer"; only one
// the waypoint is still holding does, and the survey's verdict is shown apart
// from the entry's own bearing decision.
//
// Run with `npm test`. The code under test is sliced out of the real board
// chunks between the RALPHUS-WAYPOINT-ANSWER-BADGES markers.

import test from "node:test";
import assert from "node:assert/strict";
import { boardScript } from "./board-source.mjs";

const BEGIN = "// RALPHUS-WAYPOINT-ANSWER-BADGES:BEGIN";
const END = "// RALPHUS-WAYPOINT-ANSWER-BADGES:END";

function load() {
  const src = boardScript();
  const from = src.indexOf(BEGIN);
  const to = src.indexOf(END);
  if (from === -1 || to === -1 || to < from) {
    throw new Error(`could not find the ${BEGIN} / ${END} markers in the board chunks`);
  }
  const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
  // eslint-disable-next-line no-new-func -- evaluating the shipped source is the point.
  return new Function(
    "esc",
    `${src.slice(from + BEGIN.length, to)}
     return { renderBearingDecisionBadge, renderSurveyVerdictBadge, renderSurveyErrorLine };`,
  )(esc);
}

const base = {
  waypoint_id: "waypoint-1",
  entry_id: "squad-1",
  mode: "block",
  survey_rationale: null,
  bearing_decision: null,
};
const released = { ...base, survey_verdict: "not_impacted", survey_state: "not_impacted", holding: false, answer_state: "released" };
const holding = { ...base, survey_verdict: "impacted", survey_state: "impacted", holding: true, answer_state: "awaiting_answer" };
const answered = { ...base, survey_verdict: "impacted", survey_state: "impacted", holding: false, answer_state: "answered", bearing_decision: "accepted" };
const halted = {
  ...base,
  survey_verdict: "impacted",
  survey_state: "halted_unjudged",
  holding: true,
  answer_state: "halted_unjudged",
  survey_rationale: "halted: survey could not run after 3 attempts (boom)",
};
const retrying = { ...base, survey_verdict: null, survey_state: "unjudged", holding: true, answer_state: "unjudged" };

test("a released entry never shows awaiting answer, and shows its verdict", () => {
  const api = load();
  assert.equal(api.renderBearingDecisionBadge(released), "");
  assert.match(api.renderSurveyVerdictBadge(released), /not impacted/);
});

test("only a holding, impacted, unanswered entry shows awaiting answer", () => {
  const api = load();
  assert.match(api.renderBearingDecisionBadge(holding), /awaiting answer/);
  assert.match(api.renderSurveyErrorLine(holding), /ralphus waypoint override waypoint-1 squad-1/);
  assert.doesNotMatch(api.renderBearingDecisionBadge(answered), /awaiting answer/);
});

test("an answered entry shows the verdict and its own decision separately", () => {
  const api = load();
  assert.match(api.renderSurveyVerdictBadge(answered), />impacted</);
  assert.match(api.renderBearingDecisionBadge(answered), />accepted</);
});

test("unjudged and fail-closed entries are never a bare impacted badge", () => {
  const api = load();
  assert.match(api.renderBearingDecisionBadge(retrying), /survey errored, retrying/);
  assert.doesNotMatch(api.renderSurveyVerdictBadge(retrying), />impacted</);
  assert.match(api.renderBearingDecisionBadge(halted), /halted: survey could not run/);
  assert.match(api.renderSurveyVerdictBadge(halted), /unjudged, held by default/);
  assert.doesNotMatch(api.renderSurveyVerdictBadge(halted), />impacted</);
  assert.match(api.renderSurveyErrorLine(halted), /wp-entry-error[^>]*data-full="halted: survey could not run after 3 attempts \(boom\)"/);
});
