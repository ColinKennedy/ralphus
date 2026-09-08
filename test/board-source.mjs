// Concatenates the board chunk files (librarian/assets/board/*.js) in load
// order — sorted filename order, the same order board.html's <script> tags
// and knip's pseudo-module use — so node --test loaders slice the real
// shipped source without caring which chunk a region lives in. The chunks
// are a byte-exact split of the board's script body, so this text is exactly
// what the browser executes; there is no second copy to drift from.

import { readFileSync, readdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");

/** The board chunk directory (librarian/assets/board). */
export const boardDir = join(repoRoot, "librarian", "assets", "board");

/** The page shell (markup + <script src> tags; no JS lives in it). */
export const boardHtmlPath = join(repoRoot, "librarian", "assets", "board.html");

/** Chunk filenames in load order. */
export function chunkFiles() {
  return readdirSync(boardDir).filter((f) => f.endsWith(".js") && !f.startsWith(".")).sort();
}

/** All chunk sources concatenated in load order. */
export function boardScript() {
  return chunkFiles()
    .map((f) => readFileSync(join(boardDir, f), "utf8"))
    .join("");
}
