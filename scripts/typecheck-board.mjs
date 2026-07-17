#!/usr/bin/env node
// Runs `tsc --checkJs` over the extracted board.html script (see
// extract-board-js.mjs) and rewrites its diagnostic output so every mention
// of the throwaway `.lint-tmp/board.js` reads as `librarian/assets/board.html`
// instead. Line/column numbers are already identical between the two files
// (extract-board-js.mjs pads the extraction so they line up) — this step is
// purely textual, so a human or an AI agent reading the output (locally or in
// CI) is pointed straight at the real file to edit, never at the generated
// one. ANSI color codes are stripped so the output stays plain, readable text
// in both a terminal and a CI log, while keeping tsc's boxed source-context
// (the line + `~~~~` underline) that `--pretty` provides.

import { spawnSync } from "node:child_process";
import { createRequire } from "node:module";

const require = createRequire(import.meta.url);
const tscBin = require.resolve("typescript/bin/tsc");

const result = spawnSync(process.execPath, [tscBin, "-p", "tsconfig.board.json", "--pretty"], {
  encoding: "utf8",
});

const ANSI_RE = /\x1b\[[0-9;]*m/g;
const EXTRACTED_PATH_RE = /\.lint-tmp[\\/]board\.js/g;

const rewrite = (s) => (s ?? "").replace(ANSI_RE, "").replace(EXTRACTED_PATH_RE, "librarian/assets/board.html");

process.stdout.write(rewrite(result.stdout));
process.stderr.write(rewrite(result.stderr));

process.exit(result.status ?? 1);
