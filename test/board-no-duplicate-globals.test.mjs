// The board chunks (librarian/assets/board/*.js) are concatenated into one
// global script scope, so two top-level `function foo` declarations silently
// collapse into whichever chunk comes last. A second `promptBox` with a
// different argument order once made every squad cell's prompt box render its
// own element id as its text. This pins that no new duplicate appears.
//
// Duplicates that predate this check and are behaviorally identical are listed
// in KNOWN_DUPLICATES; do not add to it -- rename the new function instead.

import test from "node:test";
import assert from "node:assert/strict";
import fs from "node:fs";
import path from "node:path";

const BOARD_DIR = path.join(import.meta.dirname, "..", "librarian", "assets", "board");
const KNOWN_DUPLICATES = new Set(["cartoRefText", "ntRequestSuggestedName", "pollWatches"]);

test("no top-level board function name is declared in more than one place", () => {
  /** @type {Map<string, string[]>} */
  const seen = new Map();
  for (const file of fs.readdirSync(BOARD_DIR).filter((f) => f.endsWith(".js")).sort()) {
    const src = fs.readFileSync(path.join(BOARD_DIR, file), "utf8");
    for (const m of src.matchAll(/^\s*(?:async\s+)?function\s+([A-Za-z0-9_$]+)/gm)) {
      seen.set(m[1], [...(seen.get(m[1]) ?? []), file]);
    }
  }
  const dupes = [...seen].filter(([name, files]) => files.length > 1 && !KNOWN_DUPLICATES.has(name));
  assert.deepEqual(dupes, [], `duplicate global function names: ${JSON.stringify(dupes)}`);
});
