#!/usr/bin/env node
// One-off transform: eslint-plugin-jsdoc's comment-parser does not reliably
// recognize an @tag that shares a physical line with the description or with
// another @tag — a single-line block like `/** Does X. @returns {void} */`
// (one tag) is sometimes fine, but `/** Does X. @param {string} a @returns
// {void} */` (2+ tags) or a lone @returns following prose on the same line
// can both make jsdoc/require-param and jsdoc/require-returns report
// (incorrect) "missing" errors. This rewrites every single-line `/** ... */`
// block in board.html that carries 1+ @tags into a standard multi-line block
// (one tag per line), which comment-parser parses correctly. Only bare
// description-only blocks (0 tags) are left as single-line.

import { readFileSync, writeFileSync } from "node:fs";

const path = "librarian/assets/board.html";
const src = readFileSync(path, "utf8");

const TAG_RE = /@(?:param|returns|type|typedef|property)\b/g;
const BLOCK_RE = /^([ \t]*)\/\*\*[ \t]+([^\n]*?)[ \t]*\*\/[ \t]*$/gm;

let count = 0;
const out = src.replace(BLOCK_RE, (whole, indent, content) => {
  const tagStarts = [...content.matchAll(TAG_RE)].map((m) => m.index);
  if (tagStarts.length < 2 && !/@(?:param|returns)\b/.test(content)) return whole; // bare @type/@typedef single-tag — fine as one line

  const parts = [];
  let prev = 0;
  for (const start of tagStarts) {
    if (start > prev) parts.push(content.slice(prev, start).trim());
    prev = start;
  }
  parts.push(content.slice(prev).trim());
  // parts[0] is the leading description (may be ""); parts[1..] are "@tag ...".
  const [description, ...tags] = parts;

  const lines = [`${indent}/**`];
  if (description) lines.push(`${indent} * ${description}`);
  for (const t of tags) lines.push(`${indent} * ${t}`);
  lines.push(`${indent} */`);
  count++;
  return lines.join("\n");
});

writeFileSync(path, out, "utf8");
console.log(`reformat-jsdoc: rewrote ${count} single-line JSDoc block(s) containing @param/@returns or 2+ tags`);
