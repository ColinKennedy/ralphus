#!/usr/bin/env node
// Extracts the inline <script>...</script> body from librarian/assets/board.html
// into .lint-tmp/board.js so `tsc --checkJs` (which only understands .js/.ts
// files, not .html) can type-check board.html's JSDoc-annotated JavaScript.
// board.html itself never changes — this is a read-only lint-time step.
//
// Line numbers are preserved (blank lines are padded above the extracted
// script) so a tsc error's reported line matches board.html's line exactly.

import { readFileSync, mkdirSync, writeFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const srcPath = join(repoRoot, "librarian", "assets", "board.html");
const outDir = join(repoRoot, ".lint-tmp");
const outPath = join(outDir, "board.js");

const html = readFileSync(srcPath, "utf8");
const match = html.match(/<script>\r?\n([\s\S]*?)<\/script>/);
if (!match) {
  console.error(`extract-board-js: no <script> block found in ${srcPath}`);
  process.exit(1);
}

const scriptStartLine = html.slice(0, match.index).split("\n").length;
const padding = "\n".repeat(scriptStartLine);

mkdirSync(outDir, { recursive: true });
writeFileSync(outPath, padding + match[1], "utf8");
console.log(`extract-board-js: wrote ${outPath} (script starts at board.html:${scriptStartLine + 1})`);
