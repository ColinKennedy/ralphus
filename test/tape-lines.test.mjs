// RAL-397 Phase 2G-A: coverage for the transcript-tape line pipeline — the
// ANSI-strip + marker-classify logic that turns raw tape bytes into what the
// Live View renders, and that implements the inline "Show Debug Messages"
// behavior. See ./board-tape-lines.mjs for how it's sliced out of the chunks.

import test from "node:test";
import assert from "node:assert/strict";
import { lines } from "./board-tape-lines.mjs";

const { stripAnsiEscapes, formatInlineTapeEvent, classifyTapeLine, renderTapeLines } = lines;

// ---- stripAnsiEscapes (a JS mirror of terminal_log.rs::strip_ansi_escapes) ----

test("strips CSI (SGR/color, cursor) sequences", () => {
  assert.equal(stripAnsiEscapes("\x1b[31mred\x1b[0m"), "red");
  assert.equal(stripAnsiEscapes("\x1b[2K\x1b[1Gline"), "line");
});

test("strips OSC sequences terminated by BEL or ST", () => {
  assert.equal(stripAnsiEscapes("\x1b]0;window title\x07after"), "after");
  assert.equal(stripAnsiEscapes("\x1b]8;;http://x\x1b\\link"), "link");
});

test("leaves plain text and newlines untouched", () => {
  assert.equal(stripAnsiEscapes("plain text\nsecond"), "plain text\nsecond");
});

test("handles a bare trailing escape without hanging or throwing", () => {
  assert.equal(stripAnsiEscapes("tail\x1b"), "tail");
});

// ---- classifyTapeLine ----

test("agent output lines are kept verbatim (with the trailing CR of a CRLF trimmed)", () => {
  assert.equal(classifyTapeLine("hello world", false), "hello world");
  assert.equal(classifyTapeLine("hello world\r", false), "hello world");
});

test("the done sentinel is always dropped, in both debug states", () => {
  assert.equal(classifyTapeLine("RALPHUS_TMUX_DONE: ok", false), null);
  assert.equal(classifyTapeLine("RALPHUS_TMUX_DONE: ok", true), null);
});

test("RALPHUS_EVENT lines are dropped when Debug is off", () => {
  assert.equal(classifyTapeLine('RALPHUS_EVENT: {"source":"claude-code","message":"live usage"}', false), null);
});

test("RALPHUS_EVENT lines render inline when Debug is on", () => {
  const out = classifyTapeLine('RALPHUS_EVENT: {"source":"claude-code","message":"live usage"}', true);
  assert.ok(out && out.includes("claude-code"), "the event source appears inline");
  assert.ok(out && out.includes("live usage"), "the event message appears inline");
});

// ---- formatInlineTapeEvent ----

test("formats a usage event with a compact token/cost detail", () => {
  const out = formatInlineTapeEvent('{"source":"claude-code","message":"live usage","payload":{"tokens_in":10,"tokens_out":20,"cost_usd":0.1234}}');
  assert.ok(out.includes("in 10"));
  assert.ok(out.includes("out 20"));
  assert.ok(out.includes("$0.1234"));
});

test("formats a session-id event", () => {
  const out = formatInlineTapeEvent('{"source":"claude-code","message":"session","payload":{"agent_session_id":"sess-123"}}');
  assert.ok(out.includes("session sess-123"));
});

test("malformed event JSON falls back to the raw payload rather than throwing", () => {
  const out = formatInlineTapeEvent("{not valid json");
  assert.ok(out.includes("{not valid json"));
});

// ---- renderTapeLines (the whole pipeline) ----

test("renderTapeLines strips ANSI, drops sentinels/events, and joins agent output (Debug off)", () => {
  const raw = [
    "\x1b[32mbuilding...\x1b[0m",
    'RALPHUS_EVENT: {"source":"claude-code","message":"live usage"}',
    "done",
    "RALPHUS_TMUX_DONE: ok",
  ];
  assert.equal(renderTapeLines(raw, false), "building...\ndone");
});

test("renderTapeLines inlines events in place when Debug is on, still dropping the sentinel", () => {
  const raw = [
    "building...",
    'RALPHUS_EVENT: {"source":"claude-code","message":"live usage"}',
    "done",
    "RALPHUS_TMUX_DONE: ok",
  ];
  const out = renderTapeLines(raw, true).split("\n");
  assert.equal(out[0], "building...");
  assert.ok(out[1].includes("live usage"), "the event is rendered inline right where it occurred");
  assert.equal(out[2], "done");
  assert.equal(out.length, 3, "the done sentinel is still dropped");
});
