#!/usr/bin/env node
// Scans the board chunk files (librarian/assets/board/*.js) for every
// top-level declaration — function, async function, const, let — and emits
// .lint-tmp/board-globals.json as a `{ name: "readonly" }` map that
// eslint.config.mjs merges into `globals`.
//
// The chunks are plain scripts sharing one global scope at runtime (loaded
// via board.html's sequential <script> tags; see tsconfig.board.json), so a
// name declared in one chunk and used in another resolves fine in the
// browser. ESLint analyzes each file in isolation and can't see that,
// hence this generated list. Part of `npm run lint` (runs before eslint),
// it keeps `no-undef` meaningful: a typo'd name still errors even though
// cross-chunk resolution is supplied here.

import { mkdirSync, readFileSync, readdirSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const boardDir = join(repoRoot, "librarian", "assets", "board");

const FN_RE = /^ {6}(?:async\s+)?function\s+([A-Za-z_$][\w$]*)\s*\(/gm;
const VAR_RE = /^ {6}(?:const|let)\s+([A-Za-z_$][\w$]*)\b/gm;

const files = readdirSync(boardDir)
  .filter((f) => f.endsWith(".js") && !f.startsWith("."))
  .sort();

const globals = {};
for (const file of files) {
  const text = readFileSync(join(boardDir, file), "utf8");
  for (const m of text.matchAll(FN_RE)) globals[m[1]] = "readonly";
  for (const m of text.matchAll(VAR_RE)) globals[m[1]] = "readonly";
}

mkdirSync(join(repoRoot, ".lint-tmp"), { recursive: true });
writeFileSync(
  join(repoRoot, ".lint-tmp", "board-globals.json"),
  JSON.stringify(globals, null, 2) + "\n",
);
console.log(
  `generate-board-globals: ${files.length} chunks, ${Object.keys(globals).length} globals`,
);
