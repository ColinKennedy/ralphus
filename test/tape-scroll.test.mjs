// RAL-397 Phase 2G-A: coverage for the transcript-tape scroll reducer that
// drives the single-source, seamless Live View. See ./board-tape-scroll.mjs
// for how the reducer is sliced out of the real board chunks.

import test from "node:test";
import assert from "node:assert/strict";
import { tape } from "./board-tape-scroll.mjs";

const { emptyTapeWindow, utf8ByteLength, tapeAppend, tapePrepend, tapeTrimFront, tapeCompleteLines } = tape;

test("an empty window seeds wholesale from the first (non-contiguous) chunk", () => {
  const w = tapeAppend(emptyTapeWindow(), { start: 100, content: "hello\n", total: 106, requested: 64 });
  assert.equal(w.loadedStart, 100);
  assert.equal(w.loadedEnd, 106, "loadedEnd advances by min(requested, total-start), clamped to total");
  assert.equal(w.total, 106);
  assert.equal(w.text, "hello\n");
});

test("a contiguous append extends the window and appends text", () => {
  let w = tapeAppend(emptyTapeWindow(), { start: 0, content: "aaaa\n", total: 5, requested: 64 });
  w = tapeAppend(w, { start: 5, content: "bbbb\n", total: 10, requested: 64 });
  assert.equal(w.loadedStart, 0);
  assert.equal(w.loadedEnd, 10);
  assert.equal(w.text, "aaaa\nbbbb\n");
});

test("append advances loadedEnd by the requested limit, not the returned string length", () => {
  // A server-side redaction can shorten `content`; the byte offset must still
  // track the true bytes consumed so the next fetch doesn't re-read or skip.
  let w = tapeAppend(emptyTapeWindow(), { start: 0, content: "0123456789", total: 1000, requested: 10 });
  w = tapeAppend(w, { start: 10, content: "[REDACTED]", total: 1000, requested: 40 });
  assert.equal(w.loadedEnd, 50, "10 + 40 requested bytes, even though the redacted string is shorter");
});

test("a non-contiguous append re-seeds rather than corrupting the window", () => {
  let w = tapeAppend(emptyTapeWindow(), { start: 0, content: "aaaa\n", total: 5, requested: 64 });
  w = tapeAppend(w, { start: 900, content: "far\n", total: 904, requested: 64 });
  assert.equal(w.loadedStart, 900, "a gap means the old text is stale — adopt the new chunk wholesale");
  assert.equal(w.text, "far\n");
});

test("prepend moves loadedStart back with no byte math and prepends text", () => {
  const w = { loadedStart: 100, loadedEnd: 200, total: 500, text: "tail\n" };
  const out = tapePrepend(w, { start: 40, content: "older\n", total: 500 });
  assert.equal(out.loadedStart, 40, "loadedStart becomes the requested chunk start — redaction-proof by construction");
  assert.equal(out.loadedEnd, 200, "the tail end is untouched");
  assert.equal(out.text, "older\ntail\n");
});

test("trim-front drops the head and advances loadedStart by the dropped bytes", () => {
  const w = { loadedStart: 0, loadedEnd: 10, total: 10, text: "0123456789" };
  const out = tapeTrimFront(w, 4);
  assert.equal(out.text, "6789", "only the last maxChars are kept");
  assert.equal(out.loadedStart, 6, "loadedStart moves forward by the 6 dropped bytes so it stays a valid offset");
  assert.equal(out.loadedEnd, 10);
});

test("trim-front counts real UTF-8 byte length, not UTF-16 code-unit length", () => {
  // "😀" is one JS surrogate pair (2 code units) but 4 UTF-8 bytes; dropping it
  // must advance the byte offset by 4, or load-older would page from a bogus
  // mid-character offset.
  const emoji = "😀"; // 2 UTF-16 units, 4 UTF-8 bytes
  const w = { loadedStart: 0, loadedEnd: utf8ByteLength(emoji + "abc"), total: 99, text: emoji + "abc" };
  const out = tapeTrimFront(w, 3); // keep "abc"
  assert.equal(out.text, "abc");
  assert.equal(out.loadedStart, 4, "the dropped emoji is 4 UTF-8 bytes");
});

test("trim-front is a no-op when the window is within the cap", () => {
  const w = { loadedStart: 5, loadedEnd: 9, total: 9, text: "abcd" };
  assert.deepEqual(tapeTrimFront(w, 10), w);
});

test("complete-lines drops a partial leading line only when not at the start of the file", () => {
  const mid = { loadedStart: 50, loadedEnd: 70, total: 100, text: "tial line\nsecond\nthird" };
  // loadedStart>0 => the first (partial) line is dropped; trailing "third" held back (not ended/atEnd).
  assert.deepEqual(tapeCompleteLines(mid, false), ["second"]);

  const head = { loadedStart: 0, loadedEnd: 20, total: 100, text: "first\nsecond\nthird" };
  // loadedStart==0 => the first line is whole; trailing "third" still held.
  assert.deepEqual(tapeCompleteLines(head, false), ["first", "second"]);
});

test("complete-lines holds back a partial trailing line until the cell has ended AND the window reaches EOF", () => {
  const running = { loadedStart: 0, loadedEnd: 12, total: 20, text: "done\npartial" };
  assert.deepEqual(tapeCompleteLines(running, false), ["done"], "still running: hold the partial tail");

  const endedNotAtEnd = { loadedStart: 0, loadedEnd: 12, total: 20, text: "done\npartial" };
  assert.deepEqual(tapeCompleteLines(endedNotAtEnd, true), ["done"], "ended but window not at EOF: still hold");

  const endedAtEnd = { loadedStart: 0, loadedEnd: 20, total: 20, text: "done\nlast line no newline" };
  assert.deepEqual(
    tapeCompleteLines(endedAtEnd, true),
    ["done", "last line no newline"],
    "ended AND at EOF: the final unterminated line is real output, emit it",
  );
});
