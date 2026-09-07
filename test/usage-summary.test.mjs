// Coverage for the board's token/cost-summary line and the RAL-373
// compaction count/cost row.
//
// The double-count rule this file exists to pin: `compaction_input_tokens`
// is billed at the model's uncached input rate, and that figure is a
// *slice* of `cost_usd` (which already bills the compaction requests as
// part of the backend's authoritative total) — never additional spend. And
// a nonzero `compaction_count` paired with `compaction_input_tokens === 0`
// must read as "size not reported", not as a free "$0.0000" compaction.
//
// Run with `npm test` (node --test). See ./board-usage-summary.mjs for how
// the view logic is loaded out of the real board.html.

import test from "node:test";
import assert from "node:assert/strict";
import { usageSummaryHelpers, boardSource } from "./board-usage-summary.mjs";

const { fmtCostUsd, usageSummary, uncachedInputRateUsd, compactionCostUsd, compactionSummary } = usageSummaryHelpers;

test("fmtCostUsd renders N/A for a zero or absent cost, not $0.0000", () => {
  assert.equal(fmtCostUsd(0), "N/A");
  assert.equal(fmtCostUsd(undefined), "N/A");
  assert.equal(fmtCostUsd(null), "N/A");
  assert.equal(fmtCostUsd(0.6807), "$0.6807 USD");
});

test("usageSummary omits the cache segment when both cache figures are zero", () => {
  const line = usageSummary({ tokens_in: 100, tokens_out: 50, cost_usd: 0.01 });
  assert.doesNotMatch(line, /cache/);
  assert.match(line, /^input 100 · output 50 · \$0\.0100 USD$/);
});

test("usageSummary shows the cache segment when either cache figure is nonzero", () => {
  const line = usageSummary({ tokens_in: 100, tokens_out: 50, cache_creation_tokens: 10, cache_read_tokens: 0, cost_usd: 0.01 });
  assert.match(line, /cache write 10 · cache read 0/);
});

test("usageSummary omits the compaction-input segment when it is zero", () => {
  const line = usageSummary({ tokens_in: 100, tokens_out: 50, compaction_input_tokens: 0, cost_usd: 0.01 });
  assert.doesNotMatch(line, /compaction/);
});

test("usageSummary surfaces compaction input tokens as their own segment", () => {
  const line = usageSummary({ tokens_in: 100, tokens_out: 50, compaction_input_tokens: 115_000, cost_usd: 0.6807 });
  assert.match(line, /compaction input 115000/);
});

test("uncachedInputRateUsd matches the runner's per-model rate table (claude_code_backend.rs estimate_cost_usd)", () => {
  assert.equal(uncachedInputRateUsd("claude-haiku-4-5"), 1.0);
  assert.equal(uncachedInputRateUsd("claude-sonnet-4-5"), 3.0);
  assert.equal(uncachedInputRateUsd("claude-fable-5"), 3.0);
  assert.equal(uncachedInputRateUsd("mythos"), 3.0);
  assert.equal(uncachedInputRateUsd("claude-opus-5"), 15.0);
  assert.equal(uncachedInputRateUsd(null), 15.0);
  assert.equal(uncachedInputRateUsd(undefined), 15.0);
});

test("compactionCostUsd prices compaction-input tokens at the model's uncached input rate", () => {
  assert.equal(compactionCostUsd("claude-sonnet-4-5", 115_000), (115_000 * 3.0) / 1_000_000);
  assert.equal(compactionCostUsd("claude-opus-5", 1_000_000), 15.0);
});

test("compactionSummary shows a bare 0 when there were no compactions", () => {
  assert.equal(compactionSummary({}, "claude-sonnet-4-5"), "0");
  assert.equal(compactionSummary({ compaction_count: 0, compaction_input_tokens: 0 }, "claude-sonnet-4-5"), "0");
});

test("compactionSummary derives a dollar figure from count + tokens", () => {
  const line = compactionSummary({ compaction_count: 1, compaction_input_tokens: 115_000 }, "claude-sonnet-4-5");
  assert.equal(line, "1 - $0.3450 USD");
});

test("compactionSummary reads a nonzero count with zero tokens as unreported size, not a free compaction", () => {
  const line = compactionSummary({ compaction_count: 7, compaction_input_tokens: 0 }, "claude-sonnet-4-5");
  assert.equal(line, "7 - size not reported");
  assert.doesNotMatch(line, /\$0\.0000/);
});

// The assertion below is about the double-count rule at the call-site level
// rather than the pure helpers above: it pins that the compaction row is
// wired up as a breakdown of `cost_usd`, never summed into it.

test("neither cell nor proof detail pane adds compactionCostUsd on top of cost_usd", () => {
  const additions = boardSource.match(/cost_usd\s*\+\s*compactionCostUsd|compactionCostUsd\([^)]*\)\s*\+\s*[a-zA-Z_.]*cost_usd/g);
  assert.equal(additions, null, "compaction cost must never be added on top of cost_usd — it is already included in it");
});
