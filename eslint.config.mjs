// ESLint flat config for the librarian's inline board.html JavaScript.
//
// board.html is a single static page whose entire client is inline <script>.
// eslint-plugin-html extracts that script so ESLint can parse and check it.
// `no-undef` is the important rule here: it catches references to helpers that
// were never defined (e.g. a missing `post()` fetch wrapper) — the kind of bug
// the Rust "does the HTML contain X" tests cannot see.
import html from "eslint-plugin-html";
import globals from "globals";

export default [
  {
    files: ["**/*.html"],
    plugins: { html },
    languageOptions: {
      ecmaVersion: 2022,
      sourceType: "script",
      globals: { ...globals.browser },
    },
    rules: {
      "no-undef": "error",
      // Handlers referenced only from inline on* attributes look "unused" to
      // ESLint (it does not parse HTML attributes), so this rule would be all
      // false positives here.
      "no-unused-vars": "off",
    },
  },
];
