// ESLint flat config for the librarian's inline board.html JavaScript.
//
// board.html is a single static page whose entire client is inline <script>.
// eslint-plugin-html extracts that script so ESLint can parse and check it.
// `no-undef` is the important rule here: it catches references to helpers that
// were never defined (e.g. a missing `post()` fetch wrapper) — the kind of bug
// the Rust "does the HTML contain X" tests cannot see.
//
// eslint-plugin-jsdoc enforces the JSDoc type-hint convention (see
// tsconfig.board.json / `npm run typecheck` for the actual type-checking):
// every top-level `function name(...) {}` declaration must carry a JSDoc
// comment with @param/@returns tags. It intentionally does NOT require JSDoc
// on inline arrow-function callbacks (e.g. `.map((x) => ...)`) — annotating
// every one of those would be noise, not signal.
import html from "eslint-plugin-html";
import jsdoc from "eslint-plugin-jsdoc";
import globals from "globals";

export default [
  {
    files: ["**/*.html"],
    plugins: { html, jsdoc },
    languageOptions: {
      ecmaVersion: 2022,
      sourceType: "script",
      globals: {
        ...globals.browser,
        // Vendored xterm.js UMD globals (RAL-355 Phase 10) -- see the
        // "Vendored" HTML comment above their <script id="xterm-vendor">
        // tags in board.html. The vendored scripts themselves are excluded
        // from linting via `<!-- eslint-disable-next-script -->`; these
        // entries are only so the *real* inline script's references to them
        // don't trip `no-undef`.
        Terminal: "readonly",
        FitAddon: "readonly",
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
