// RAL-294: coverage for the Logs modal's "copy as Markdown" table serializer.
//
// The values tab-copy exists for don't survive a naive innerText scrape: a
// cell can carry pipes, newlines, or a "click to expand" affordance that has
// to become a footnote instead of silently disappearing. These tests pin
// down mdEscapeCell/mdCellWithFootnote/mdHeadCell/mdTableToText directly, so
// a future edit can't reintroduce a broken pipe table or a dropped tooltip
// without a test failing.
//
// Run with `npm test` (node --test). See ./board-logs-markdown.mjs for how
// the helpers are loaded out of the real board.html.

import test from "node:test";
import assert from "node:assert/strict";
import { createLogsMd, logsMd } from "./board-logs-markdown.mjs";

const { mdEscapeCell, mdCellWithFootnote, mdHeadCell, mdTableToText } = logsMd;

test("mdEscapeCell escapes literal pipes so a row can't break", () => {
  assert.equal(mdEscapeCell("a | b"), "a \\| b");
});

test("mdEscapeCell collapses embedded newlines to spaces", () => {
  assert.equal(mdEscapeCell("line one\nline two"), "line one line two");
  assert.equal(mdEscapeCell("line one\rline two"), "line one line two");
  assert.equal(mdEscapeCell("line one\r\nline two"), "line one line two");
});

test("mdEscapeCell renders an empty/nullish value as an em-dash", () => {
  assert.equal(mdEscapeCell(""), "—");
  assert.equal(mdEscapeCell(null), "—");
  assert.equal(mdEscapeCell(undefined), "—");
});

test("mdEscapeCell passes a short plain value through unchanged", () => {
  assert.equal(mdEscapeCell("cell-01"), "cell-01");
});

test("mdCellWithFootnote inlines a short single-line value with no footnote", () => {
  const notes = [];
  assert.equal(mdCellWithFootnote("short error", notes), "short error");
  assert.deepEqual(notes, []);
});

test("mdCellWithFootnote footnotes a long value and preserves its full text", () => {
  const notes = [];
  const long = "x".repeat(200);
  const cell = mdCellWithFootnote(long, notes);
  assert.match(cell, /^x+… \[1\]$/);
  assert.equal(notes.length, 1);
  assert.equal(notes[0], `[1]: ${long}`);
});

test("mdCellWithFootnote footnotes a multi-line value even if short", () => {
  const notes = [];
  const cell = mdCellWithFootnote("line one\nline two", notes);
  assert.match(cell, /\[1\]$/);
  assert.equal(notes[0], "[1]: line one\nline two");
});

test("mdCellWithFootnote recognizes carriage-return line endings", () => {
  for (const newline of ["\r", "\r\n"]) {
    const notes = [];
    const cell = mdCellWithFootnote(`line one${newline}line two`, notes);
    assert.equal(cell, "line one… [1]");
    assert.equal(notes[0], `[1]: line one${newline}line two`);
  }
});

test("mdCellWithFootnote numbers footnotes sequentially across calls sharing one notes array", () => {
  const notes = [];
  mdCellWithFootnote("y".repeat(200), notes);
  const second = mdCellWithFootnote("z".repeat(200), notes);
  assert.match(second, /\[2\]$/);
  assert.equal(notes.length, 2);
});

test("mdCellWithFootnote renders an empty value as an em-dash with no footnote", () => {
  const notes = [];
  assert.equal(mdCellWithFootnote("", notes), "—");
  assert.equal(mdCellWithFootnote(null, notes), "—");
  assert.deepEqual(notes, []);
});

test("mdHeadCell passes the label through unchanged when there is no tooltip", () => {
  const notes = [];
  assert.equal(mdHeadCell("reason", undefined, notes), "reason");
  assert.deepEqual(notes, []);
});

test("mdHeadCell folds a substantive tooltip into a footnote instead of the header cell", () => {
  const notes = [];
  const head = mdHeadCell("cumulative (all restarts)", "Sum of tokens/cost across every restart.", notes);
  assert.equal(head, "cumulative (all restarts) [1]");
  assert.deepEqual(notes, ["[1]: Sum of tokens/cost across every restart."]);
});

test("mdTableToText renders a valid pipe table: header, separator, then rows", () => {
  const text = mdTableToText({ head: ["a", "b"], rows: [["1", "2"], ["3", "4"]], notes: [], warnings: [] });
  assert.equal(text, ["| a | b |", "| --- | --- |", "| 1 | 2 |", "| 3 | 4 |"].join("\n"));
});

test("mdTableToText prints warnings above the table and notes below it", () => {
  const text = mdTableToText({
    head: ["a"],
    rows: [["1 [1]"]],
    notes: ["[1]: the full value"],
    warnings: ["Note: showing 5 of 50 rows"],
  });
  const lines = text.split("\n");
  assert.equal(lines[0], "Note: showing 5 of 50 rows");
  assert.equal(lines[1], "");
  assert.equal(lines[2], "| a |");
  assert.ok(lines.includes("--- Notes ---"));
  assert.equal(lines[lines.length - 1], "[1]: the full value");
});

test("mdTableToText renders an empty (zero-row) table as just header + separator", () => {
  const text = mdTableToText({ head: ["a", "b"], rows: [], notes: [], warnings: [] });
  assert.equal(text, "| a | b |\n| --- | --- |");
});

const longError = `failure: ${"detail ".repeat(24)}`;
const squad = {
  id: "squad-294",
  tasks: [
    {
      name: "build | test",
      state: "failed",
      error: longError,
      proof: [{ id: "task-proof", kind: "command", state: "failed", output: "first line\nfull expandable output" }],
      cells: [
        {
          id: "cell-1",
          state: "failed",
          tokens_in: 10,
          tokens_out: 5,
          cost_usd: 0.25,
          maximum_budget_usd: 1,
          error: "cell line one\ncell line two",
          proof: [{ id: "cell-proof", kind: "prompt", state: "passed", output: "short output" }],
        },
      ],
    },
  ],
};

const eventRow = (message, payload = {}) => ({
  at_ms: 1_700_000_000_000,
  level: "info",
  source: "scheduler",
  scope: "cell",
  task: "build | test",
  squad_id: "squad-294",
  guardian_id: null,
  cell_id: "cell-1",
  message,
  payload,
});

test("events tab serializes expandable payload text as a footnote", async () => {
  const api = createLogsMd({
    cartoModalCache: { "squad:squad-294": { rows: [eventRow("completed", { result: "a|b" })], total: 1 } },
  });
  const table = await api.buildLogsMdTable("events", squad, "current");
  assert.deepEqual(table.head, ["time", "level", "source", "scope", "task", "refs", "message"]);
  assert.equal(table.rows.length, 1);
  assert.match(table.rows[0][4], /build \\| test/);
  assert.match(table.rows[0][6], /^completed… \[1\]$/);
  assert.equal(table.notes[0], '[1]: completed\npayload: {"result":"a|b"}');
});

test("tasks tab serializes its tooltip and truncated failure reason as footnotes", async () => {
  const table = await logsMd.buildLogsMdTable("tasks", squad, "current");
  assert.equal(table.head[3], "reason [1]");
  assert.match(table.rows[0][0], /build \\| test/);
  assert.match(table.rows[0][3], /… \[2\]$/);
  assert.equal(table.notes[1], `[2]: ${longError.trim()}`);
});

test("cells tab serializes cumulative usage and expandable errors", async () => {
  const api = createLogsMd({
    cartoModalCache: {
      "cellstotals:squad-294": {
        rows: [eventRow("cell completed", { tokens_in: 20, tokens_out: 8, cost_usd: 0.5 })],
        total: 1,
      },
    },
  });
  const table = await api.buildLogsMdTable("cells", squad, "current");
  assert.equal(table.head[5], "cumulative (all restarts) [1]");
  assert.equal(table.rows[0][5], "input 20 · output 8 · $0.5000 (1 attempt)");
  assert.equal(table.rows[0][6], "cell line one… [2]");
  assert.equal(table.notes[1], "[2]: cell line one\ncell line two");
});

test("proofs tab serializes task and cell proof outputs, footnoting expandable output", async () => {
  const table = await logsMd.buildLogsMdTable("proofs", squad, "current");
  assert.equal(table.rows.length, 2);
  assert.equal(table.rows[0][1], "task");
  assert.equal(table.rows[0][5], "first line… [1]");
  assert.equal(table.notes[0], "[1]: first line\nfull expandable output");
  assert.equal(table.rows[1][1], "cell cell-1");
  assert.equal(table.rows[1][5], "short output");
});

test("scope option uses cached current view and fetches all event and cell rows", async () => {
  const calls = [];
  const currentEvent = eventRow("current event");
  const allEvents = [currentEvent, eventRow("older event")];
  const currentCell = eventRow("cell completed", { tokens_in: 1, tokens_out: 2, cost_usd: 0.1 });
  const allCells = [currentCell, eventRow("cell completed", { tokens_in: 3, tokens_out: 4, cost_usd: 0.2 })];
  const api = createLogsMd({
    cartoModalCache: {
      "squad:squad-294": { rows: [currentEvent], total: 2 },
      "cellstotals:squad-294": { rows: [currentCell], total: 2 },
    },
    cartoFetch: async (filter) => {
      calls.push(filter);
      const rows = filter.q === "cell completed" ? allCells : allEvents;
      return { rows, total: rows.length };
    },
  });

  const currentEvents = await api.buildLogsMdTable("events", squad, "current");
  const currentCells = await api.buildLogsMdTable("cells", squad, "current");
  assert.equal(calls.length, 0);
  assert.equal(currentEvents.rows.length, 1);
  assert.match(currentEvents.warnings[0], /choose "All rows"/);
  assert.equal(currentCells.rows[0][5], "input 1 · output 2 · $0.1000 (1 attempt)");

  const allEventTable = await api.buildLogsMdTable("events", squad, "all");
  const allCellTable = await api.buildLogsMdTable("cells", squad, "all");
  assert.equal(calls.length, 2);
  assert.equal(allEventTable.rows.length, 2);
  assert.equal(allCellTable.rows[0][5], "input 4 · output 6 · $0.3000 (2 attempts)");
  assert.equal(calls[0].squad_id, "squad-294");
  assert.equal(calls[1].q, "cell completed");
});
