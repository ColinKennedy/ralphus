// Loads the pure resolver-settings draft/request helpers out of the shipped
// librarian/assets/board.html so their behavior cannot drift from the UI.

import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const BEGIN = "// RALPHUS-RESOLVER-SETTINGS:BEGIN";
const END = "// RALPHUS-RESOLVER-SETTINGS:END";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
export const boardPath = join(repoRoot, "librarian", "assets", "board.html");

const html = readFileSync(boardPath, "utf8");
const from = html.indexOf(BEGIN);
const to = html.indexOf(END);
if (from === -1 || to === -1 || to < from) {
  throw new Error(
    `board-resolver-settings: could not find the ${BEGIN} / ${END} markers in ${boardPath}. ` +
      "If the resolver-settings logic moved, move the markers with it.",
  );
}
const source = html.slice(from + BEGIN.length, to);
const exported = ["resolverDraftFor", "resolverSettingsBody"];
// eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point.
const factory = new Function(`${source}\nreturn { ${exported.join(", ")} };`);

/** Resolver-settings helpers evaluated from the real board source. */
export const resolverSettings = factory();

/** Raw board source for assertions about DOM/event wiring. */
export const boardSource = html;
