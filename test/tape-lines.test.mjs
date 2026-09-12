// RAL-397 Phase 2G-A: coverage for the transcript-tape line pipeline — the
// ANSI-strip + marker-classify logic that turns raw tape bytes into what the
// Live View renders, and that implements the inline "Show Debug Messages"
// behavior. See ./board-tape-lines.mjs for how it's sliced out of the chunks.

import test from "node:test";
import assert from "node:assert/strict";
import { lines } from "./board-tape-lines.mjs";

const { renderPaneLine, formatInlineTapeEvent, classifyTapeLine, renderTapeLines } = lines;

// ---- renderPaneLine (a JS mirror of terminal_log.rs::render_pane_line) ----
// Kept assertion-for-assertion in step with the Rust tests of the same name;
// the tape and the durable `.log` are the same bytes rendered twice.

test("strips CSI (SGR/color) sequences", () => {
  assert.equal(renderPaneLine("\x1b[31mred\x1b[0m"), "red");
  assert.equal(renderPaneLine("\x1b[2K\x1b[1Gline"), "line");
});

test("strips OSC sequences terminated by BEL or ST", () => {
  assert.equal(renderPaneLine("\x1b]0;window title\x07after"), "after");
  assert.equal(renderPaneLine("\x1b]8;;http://x\x1b\\link"), "link");
});

test("leaves plain text untouched", () => {
  assert.equal(renderPaneLine("plain text"), "plain text");
});

test("handles a bare trailing escape without hanging or throwing", () => {
  assert.equal(renderPaneLine("tail\x1b"), "tail");
});

test("collapses a carriage-return progress bar to its final frame", () => {
  assert.equal(renderPaneLine("  10%\r  50%\r 100%"), " 100%");
});

test("uses real overwrite semantics, not text-after-the-last-CR", () => {
  assert.equal(renderPaneLine("abcdef\rxy"), "xycdef");
});

test("honors absolute column moves (CHA)", () => {
  assert.equal(renderPaneLine("abcdef\x1b[1Gxy"), "xycdef");
  assert.equal(renderPaneLine("abcdef\x1b[4GZ"), "abcZef");
});

test("honors the column of a cursor-position move and ignores the row (CUP)", () => {
  assert.equal(renderPaneLine("abcdef\x1b[1;1HXY"), "XYcdef");
  assert.equal(renderPaneLine("abcdef\x1b[9;1HXY"), "XYcdef");
});

test("honors relative column moves and backspace", () => {
  assert.equal(renderPaneLine("abc\x1b[2DX"), "aXc");
  assert.equal(renderPaneLine("abc\x1b[1GZ\x1b[2CQ"), "ZbcQ");
  assert.equal(renderPaneLine("abc\bX"), "abX");
});

test("honors erase-in-line, which does not move the cursor", () => {
  assert.equal(renderPaneLine("abcdef\x1b[4G\x1b[0K"), "abc");
  assert.equal(renderPaneLine("abcdef\x1b[4G\x1b[1K"), "   def");
  assert.equal(renderPaneLine("abcdef\x1b[2Kxy"), "      xy");
});

test("collapses a PowerShell-style prompt redraw into one line", () => {
  const raw = "PS C:\\r> \x1b[1;1HPS C:\\r> echo hi\x1b[1;1HPS C:\\r> echo hi!";
  assert.equal(renderPaneLine(raw), "PS C:\\r> echo hi!");
});

test("bounds an absurd column move instead of padding unboundedly", () => {
  assert.ok(renderPaneLine("a\x1b[999999999GX").length <= 10008);
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

test("tags a 'live usage' event's numbers as an estimate", () => {
  const out = formatInlineTapeEvent('{"source":"claude-code","message":"live usage","payload":{"tokens_in":2,"tokens_out":3,"cost_usd":0.1172}}');
  assert.ok(out.includes("(est.)"), "the mid-run snapshot must read as an estimate");
});

test("does not tag the cell's final 'llm done' usage as an estimate", () => {
  const out = formatInlineTapeEvent('{"source":"runner","message":"llm done","payload":{"tokens_in":2,"tokens_out":13,"cost_usd":0.0663}}');
  assert.ok(!out.includes("(est.)"), "the authoritative final tally is not an estimate");
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
