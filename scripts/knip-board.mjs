#!/usr/bin/env node
// Knip wrapper: runs knip (whose `html` compiler inlines the board chunks
// into the board.html shell — see knip.config.js) and rewrites its
// diagnostics so every reported line points at the real chunk file it came
// from. Lines at or before the shell's first <script src="/board/..."> tag
// are shell markup and pass through unchanged; lines after it are the
// inlined chunks, mapped through their per-chunk start lines.
//
// The layout must match compileHtml() in knip.config.js exactly: chunk files
// in sorted-filename order (= load order), concatenated, with the shell's
// line count as padding above.

import { spawnSync } from "node:child_process";
import { readFileSync, readdirSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
const shellPath = join(repoRoot, "librarian", "assets", "board.html");
const boardDir = join(repoRoot, "librarian", "assets", "board");

const files = readdirSync(boardDir)
  .filter((f) => f.endsWith(".js") && !f.startsWith("."))
  .sort();

let text = "";
const offsets = [];
for (const file of files) {
  offsets.push({ file, start: text.split("\n").length });
  text += readFileSync(join(boardDir, file), "utf8");
}
const totalLines = text.split("\n").length;
const shell = readFileSync(shellPath, "utf8");
const scriptStart = shell.slice(0, shell.search(/^.*<script src="\/board\//m)).split("\n").length;

const knipBin = join(repoRoot, "node_modules", "knip", "bin", "knip.js");
const result = spawnSync(process.execPath, [knipBin], {
  cwd: repoRoot,
  encoding: "utf8",
  maxBuffer: 64 * 1024 * 1024,
});

const SHELL_RE = /(librarian[\\/]assets[\\/]board\.html):(\d+):(\d+)/g;
const rewrite = (s) =>
  (s ?? "").replace(
    SHELL_RE,
    (_, _path, line, col) => {
      const global = Number(line) - scriptStart;
      // Outside the concatenated region (shell markup or the trailing
      // reference sink): keep the shell path.
      if (global < 1 || global > totalLines) return _;
      const chunk =
        offsets.find((o) => o.start === global) ??
        [...offsets].reverse().find((o) => o.start <= global);
      if (!chunk) return _;
      return `librarian/assets/board/${chunk.file}:${global - chunk.start + 1}:${col}`;
    },
  );

process.stdout.write(rewrite(result.stdout));
process.stderr.write(rewrite(result.stderr));
process.exit(result.status ?? 1);
