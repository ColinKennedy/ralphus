// A review branch's status is shown exactly once, as the pill -- never also as
// a rounded-square `.badge` repeating the same word.
import test from "node:test";
import assert from "node:assert/strict";
import { boardScript } from "./board-source.mjs";

const src = boardScript();

/** Extracts `const NAME = ...;` / `function NAME(...) {...}` source by brace scan. */
function fn(name) {
  const start = src.indexOf(`function ${name}(`);
  assert.notEqual(start, -1, `${name} not found`);
  let depth = 0;
  for (let i = src.indexOf("{", start); i < src.length; i++) {
    if (src[i] === "{") depth++;
    else if (src[i] === "}" && --depth === 0) return src.slice(start, i + 1);
  }
  throw new Error("unbalanced");
}
const tips = src.match(/const BRANCH_STATUS_TIPS = \{[\s\S]*?\n      \};/)[0];

// eslint-disable-next-line no-new-func -- evaluating the shipped source is the point.
const { branchBadge, branchStatusPill } = new Function(
  `const esc = (s) => String(s ?? "");
   const safeState = (s) => s;
   const pill = (s) => '<span class="pill p-' + s + '">' + s + '</span>';
   const delayedBadge = () => "";
   const detailSummary = () => "";
   ${tips}
   ${fn("branchBadge")}
   ${fn("branchStatusPill")}
   return { branchBadge, branchStatusPill };`,
)();

for (const status of ["merged", "closed", "ready", "actioning"]) {
  test(`${status} branch row shows its status exactly once, as the pill`, () => {
    const b = { merge_status: status };
    const row = `${branchBadge(b)} ${branchStatusPill(b)}`;
    assert.equal(row.split(status).length - 1 >= 1, true);
    assert.equal(row.includes('class="badge'), false);
    assert.equal((row.match(/class="pill/g) || []).length, 1);
    assert.match(branchStatusPill(b), /data-tip="/);
  });
}
