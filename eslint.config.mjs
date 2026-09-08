// ESLint flat config for the librarian's board chunk files
// (librarian/assets/board/*.js).
//
// The chunks are plain scripts sharing one global scope at runtime (loaded
// via board.html's sequential <script src> tags; see tsconfig.board.json for
// the tsc counterpart). ESLint analyzes each file in isolation, so cross-chunk
// names come from the generated globals map (.lint-tmp/board-globals.json,
// built by scripts/generate-board-globals.mjs as the first half of
// `npm run lint`) — that keeps `no-undef` meaningful: a reference to a name no
// chunk declares still errors, the kind of bug the Rust "does the HTML contain
// X" tests cannot see.
//
// eslint-plugin-jsdoc enforces the JSDoc type-hint convention (see
// tsconfig.board.json / `npm run typecheck` for the actual type-checking):
// every top-level `function name(...) {}` declaration must carry a JSDoc
// comment with @param/@returns tags. It intentionally does NOT require JSDoc
// on inline arrow-function callbacks (e.g. .map((x) => ...)) — annotating
// every one of those would be noise, not signal.
import jsdoc from "eslint-plugin-jsdoc";
import globals from "globals";
import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)));

const boardGlobals = JSON.parse(
  readFileSync(join(repoRoot, ".lint-tmp", "board-globals.json"), "utf8"),
);

export default [
  {
    files: ["librarian/assets/board/**/*.js"],
    plugins: { jsdoc },
    languageOptions: {
      ecmaVersion: 2022,
      sourceType: "script",
      globals: {
        ...globals.browser,
        // The vendored xterm.js UMD globals (RAL-355 Phase 10) are read via
        // `(window).Terminal` / `(window).FitAddon`, so no named globals are
        // needed for them here.
        ...boardGlobals,
      },
    },
    rules: {
      "no-undef": "error",
      // Handlers referenced only from inline on* attributes look "unused" to
      // ESLint (it does not parse HTML attributes), so this rule would be all
      // false positives here.
      "no-unused-vars": "off",
      "jsdoc/require-jsdoc": ["error", { require: { FunctionDeclaration: true } }],
      "jsdoc/require-param": "error",
      "jsdoc/require-param-name": "error",
      "jsdoc/require-param-type": "error",
      "jsdoc/require-returns": "error",
      "jsdoc/require-returns-type": "error",
      "jsdoc/check-param-names": "error",
      "jsdoc/check-types": "error",
      "jsdoc/check-tag-names": "error",
    },
  },
];
