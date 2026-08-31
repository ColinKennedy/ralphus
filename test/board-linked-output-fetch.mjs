// Loads `fetchLinkedOutputText` out of librarian/assets/board.html so its
// terminal-log-attempts-first/pane-fallback logic can be exercised under
// `node --test` with no browser and no build step.
//
// Same slice-the-real-source approach as ./board-peek-state.mjs — see that
// file's header for why board.html can't simply be imported. The region
// between the RALPHUS-LINKED-OUTPUT-FETCH markers calls only
// `terminalLogAttemptsUrlFor`/`peekUrlFor`/`scrubSecrets` (injected below as
// parameters) and the ambient `fetch` global, which tests stub per-case via
// `globalThis.fetch` — evaluating the function in non-strict `Function`
// scope resolves the bare `fetch` identifier straight through to whatever
// `globalThis.fetch` is at call time.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const BEGIN = "// RALPHUS-LINKED-OUTPUT-FETCH:BEGIN";
const END = "// RALPHUS-LINKED-OUTPUT-FETCH:END";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
export const boardPath = join(repoRoot, "librarian", "assets", "board.html");

const html = readFileSync(boardPath, "utf8");
const from = html.indexOf(BEGIN);
const to = html.indexOf(END);
if (from === -1 || to === -1 || to < from) {
  throw new Error(
    `board-linked-output-fetch: could not find the ${BEGIN} / ${END} markers in ${boardPath}. ` +
      "If fetchLinkedOutputText moved, move the markers with it — these tests are its only coverage.",
  );
}
const source = html.slice(from + BEGIN.length, to);

// eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
const factory = new Function(
  "terminalLogAttemptsUrlFor",
  "peekUrlFor",
  "scrubSecrets",
  `${source}\nreturn { fetchLinkedOutputText };`,
);

/**
 * Builds `fetchLinkedOutputText`, evaluated straight from board.html, wired
 * to the given key-resolution stubs. `scrubSecrets` defaults to a no-op
 * pass-through — its own redaction behaviour has its own coverage elsewhere
 * and is out of scope here.
 * @param {{terminalLogAttemptsUrlFor?: (key: string, attempt?: number) => string|null, peekUrlFor?: (key: string) => string|null, scrubSecrets?: (text: string) => string}} deps
 * @returns {(key: string) => Promise<string|null>}
 */
export function makeFetchLinkedOutputText(deps = {}) {
  const {
    terminalLogAttemptsUrlFor = () => null,
    peekUrlFor = () => null,
    scrubSecrets = (text) => text,
  } = deps;
  return factory(terminalLogAttemptsUrlFor, peekUrlFor, scrubSecrets).fetchLinkedOutputText;
}
