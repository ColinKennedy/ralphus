// RAL-397 Phase 2G-A: coverage for the transcript-tape line pipeline — the
// ANSI-strip + marker-classify logic that turns raw tape bytes into what the
// Live View renders, and that implements the inline "Show Debug Messages"
// behavior. See ./board-tape-lines.mjs for how it's sliced out of the chunks.

import test from "node:test";
import assert from "node:assert/strict";
import { lines } from "./board-tape-lines.mjs";

const {
  renderPaneLine,
  formatInlineTapeEvent,
  classifyTapeLine,
  renderTapeLines,
  stripThinkingPrefixes,
  THINKING_FOLDED_TEXT,
} = lines;

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
  assert.equal(classifyTapeLine("hello world", false, true), "hello world");
  assert.equal(classifyTapeLine("hello world\r", false, true), "hello world");
});

test("the done sentinel is always dropped, in both debug states", () => {
  assert.equal(classifyTapeLine("RALPHUS_TMUX_DONE: ok", false, true), null);
  assert.equal(classifyTapeLine("RALPHUS_TMUX_DONE: ok", true, true), null);
});

test("RALPHUS_EVENT lines are dropped when Debug is off", () => {
  assert.equal(classifyTapeLine('RALPHUS_EVENT: {"source":"claude-code","message":"live usage"}', false, true), null);
});

test("RALPHUS_EVENT lines render inline when Debug is on", () => {
  const out = classifyTapeLine('RALPHUS_EVENT: {"source":"claude-code","message":"live usage"}', true, true);
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
  assert.equal(renderTapeLines(raw, false, true), "building...\ndone");
});

test("renderTapeLines inlines events in place when Debug is on, still dropping the sentinel", () => {
  const raw = [
    "building...",
    'RALPHUS_EVENT: {"source":"claude-code","message":"live usage"}',
    "done",
    "RALPHUS_TMUX_DONE: ok",
  ];
  const out = renderTapeLines(raw, true, true).split("\n");
  assert.equal(out[0], "building...");
  assert.ok(out[1].includes("live usage"), "the event is rendered inline right where it occurred");
  assert.equal(out[2], "done");
  assert.equal(out.length, 3, "the done sentinel is still dropped");
});

// ---- RAL-434: the "Show Thinking" fold ----
// The runner tags each reasoning line with RALPHUS_THINKING rather than
// dropping it, so visibility is decided here, at render time, and is
// reversible. These are that contract's only coverage.

test("classifyTapeLine un-prefixes a thinking line when Thinking is on", () => {
  assert.equal(classifyTapeLine("RALPHUS_THINKING: weighing the options", false, true), "weighing the options");
});

test("classifyTapeLine keeps an empty thinking line empty rather than dropping it", () => {
  assert.equal(classifyTapeLine("RALPHUS_THINKING: ", false, true), "");
});

test("classifyTapeLine does not treat a mid-line thinking marker as a tag", () => {
  const line = "echo RALPHUS_THINKING: not a marker";
  assert.equal(classifyTapeLine(line, false, true), line);
});

test("a thinking block folds to exactly one placeholder when Thinking is off", () => {
  const raw = [
    "before",
    "RALPHUS_THINKING: first",
    "RALPHUS_THINKING: second",
    "RALPHUS_THINKING: third",
    "after",
  ];
  assert.equal(renderTapeLines(raw, false, false), `before\n${THINKING_FOLDED_TEXT}\nafter`);
});

test("two thinking blocks separated by real output stay separately folded", () => {
  const raw = [
    "RALPHUS_THINKING: a",
    "RALPHUS_THINKING: b",
    "[tool] bash(command=\"ls\")",
    "RALPHUS_THINKING: c",
  ];
  assert.equal(
    renderTapeLines(raw, false, false),
    `${THINKING_FOLDED_TEXT}\n[tool] bash(command="ls")\n${THINKING_FOLDED_TEXT}`,
  );
});

test("the same tape expands to every reasoning line when Thinking is on", () => {
  const raw = ["before", "RALPHUS_THINKING: first", "RALPHUS_THINKING: second", "after"];
  assert.equal(renderTapeLines(raw, false, true), "before\nfirst\nsecond\nafter");
});

test("folding a thinking block does not swallow the debug lines inside it", () => {
  const raw = [
    "RALPHUS_THINKING: a",
    'RALPHUS_EVENT: {"source":"pi","message":"llm done"}',
    "RALPHUS_THINKING: b",
  ];
  const out = renderTapeLines(raw, true, false).split("\n");
  assert.equal(out.length, 3, "the event breaks the run, so each block folds on its own");
  assert.equal(out[0], THINKING_FOLDED_TEXT);
  assert.ok(out[1].startsWith("\u27e8debug\u27e9"));
  assert.equal(out[2], THINKING_FOLDED_TEXT);
});

test("a tape with no thinking at all renders identically either way", () => {
  const raw = ["building...", "done"];
  assert.equal(renderTapeLines(raw, false, false), renderTapeLines(raw, false, true));
});

test("stripThinkingPrefixes untags a whole block of text, leaving other lines alone", () => {
  const text = "plain\nRALPHUS_THINKING: reasoning\nRALPHUS_EVENT: {}\nmore";
  assert.equal(stripThinkingPrefixes(text), "plain\nreasoning\nRALPHUS_EVENT: {}\nmore");
});

// RAL-434 follow-up: two ways a hidden thinking block still leaked its text
// into the pane, both found in a real capture
// (ralphus_squad-000000000206_ral-507.../0000.raw: 24 bare markers and 75
// leaked reasoning fragments in that one cell).
//
// 1. tmux trims each captured row's trailing whitespace, so a line of *empty*
//    reasoning arrives as a bare "RALPHUS_THINKING:" -- which the old
//    space-carrying prefix missed, rendering the raw marker as agent output.
// 2. The pane is 500 columns wide (daemon/src/tmux.rs `-x 500`, a deliberate
//    memory cap), so a longer reasoning line is wrapped across rows and only
//    the FIRST row carries the marker. The tail rows were classified as agent
//    output and shown verbatim -- mid-word, which is how this was spotted
//    ("...com|pare equality) -- robust.").
// tmux marks a wrapped row by ending it with a backspace; renderPaneLine
// consumes that as a cursor move, so renderTapeLines reads it off the raw row.

test("a bare thinking marker with no trailing space still folds when Thinking is off", () => {
  assert.equal(renderTapeLines(["before", "RALPHUS_THINKING:", "after"], false, false), `before\n${THINKING_FOLDED_TEXT}\nafter`);
});

test("a bare thinking marker with no trailing space renders as an empty line when Thinking is on", () => {
  assert.equal(classifyTapeLine("RALPHUS_THINKING:", false, true), "");
});

test("a wrapped thinking line's tail folds into the same block instead of leaking as agent output", () => {
  const raw = ["RALPHUS_THINKING: reasoning that ran past the pane width\b", "and its unmarked tail", "after"];
  assert.equal(renderTapeLines(raw, false, false), `${THINKING_FOLDED_TEXT}\nafter`);
});

test("a wrapped thinking line's tail is shown, unmarked, when Thinking is on", () => {
  const raw = ["RALPHUS_THINKING: reasoning\b", "and its unmarked tail", "after"];
  assert.equal(renderTapeLines(raw, false, true), "reasoning\nand its unmarked tail\nafter");
});

test("a thinking line wrapped across three rows folds as one block", () => {
  const raw = ["RALPHUS_THINKING: a\b", "b\b", "c", "after"];
  assert.equal(renderTapeLines(raw, false, false), `${THINKING_FOLDED_TEXT}\nafter`);
});

test("a wrapped line of ordinary agent output is untouched by the thinking fold", () => {
  const raw = ["ordinary output\b", "and its tail", "after"];
  assert.equal(renderTapeLines(raw, false, false), "ordinary output\nand its tail\nafter");
  assert.equal(renderTapeLines(raw, false, true), "ordinary output\nand its tail\nafter");
});

test("a wrapped thinking line's tail is not re-parsed as a marker of its own", () => {
  // The 500-column break can land anywhere, including right before text that
  // looks like another marker. A tail belongs to the line it continues.
  const raw = ["RALPHUS_THINKING: weighing\b", "RALPHUS_EVENT: not really an event", "after"];
  assert.equal(renderTapeLines(raw, false, false), `${THINKING_FOLDED_TEXT}\nafter`);
});

test("stripThinkingPrefixes also handles the space-less marker", () => {
  assert.equal(stripThinkingPrefixes("RALPHUS_THINKING:\nRALPHUS_THINKING: x"), "\nx");
});
