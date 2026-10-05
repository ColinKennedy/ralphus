import { test } from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { turnColors } from "./board-turn-colors.mjs";

const { typeColor, turnContrast, colorizeTurnLines, TURN_COLOR_BG } = turnColors;
const KNOWN = [
  "usage", "tool", "tool.unknown", "tool.Bash", "tool.Read", "tool.Edit", "tool.Write", "tool.Glob", "tool.Grep",
  "tool.bash", "tool.read", "tool.edit", "tool.exec", "tool.exec.git", "compact", "result", "thinking", "event",
];
const THEMES = ["dark", "light"];

test("same type always gets the same color", () => {
  for (const t of THEMES) assert.equal(typeColor("tool.Bash", t), typeColor("tool.Bash", t));
});

test("every known type meets 4.5:1 against its theme background", () => {
  for (const theme of THEMES) {
    for (const type of KNOWN) {
      const c = typeColor(type, theme);
      assert.match(c, /^#[0-9a-f]{6}$/);
      assert.ok(turnContrast(c, TURN_COLOR_BG[theme]) >= 4.5, `${type}/${theme} ${c}`);
    }
  }
});

test("usage, tool and tool.Bash are mutually distinct", () => {
  for (const theme of THEMES) {
    const cs = ["usage", "tool", "tool.Bash"].map((t) => typeColor(t, theme));
    assert.equal(new Set(cs).size, 3);
    for (let i = 0; i < 3; i++) for (let j = i + 1; j < 3; j++) {
      assert.ok(turnContrast(cs[i], cs[j]) > 1.05 || cs[i] !== cs[j]);
    }
  }
});

test("well-known severity tags are pinned to semantic variables", () => {
  const cases = {
    error: "var(--turn-error)", ERROR: "var(--turn-error)", "error.foo": "var(--turn-error)",
    warning: "var(--turn-warn)", Warn: "var(--turn-warn)", warn: "var(--turn-warn)",
    thrash: "var(--turn-warn)", "THRASH.x": "var(--turn-warn)",
  };
  for (const theme of THEMES) {
    for (const [type, want] of Object.entries(cases)) assert.equal(typeColor(type, theme), want, type);
    for (const type of ["usage", "tool.Bash", "errors", "constructor"]) {
      assert.match(typeColor(type, theme), /^#[0-9a-f]{6}$/, type);
    }
  }
});

test("--turn-error/--turn-warn theme values meet 4.5:1 against the theme background", () => {
  const css = readFileSync(new URL("../librarian/assets/board.css", import.meta.url), "utf8");
  const light = css.slice(css.indexOf('[data-theme="light"]'));
  const dark = css.slice(0, css.indexOf('[data-theme="light"]'));
  /**
   * @param {string} block
   * @param {string} name
   * @returns {string}
   */
  const value = (block, name) => {
    const m = block.match(new RegExp(`${name}:\\s*([^;]+);`));
    assert.ok(m, `${name} defined`);
    const v = m[1].trim();
    const ref = v.match(/^var\((--[\w-]+)\)$/);
    return ref ? value(dark, ref[1]) : v;
  };
  for (const [theme, block] of [["dark", dark], ["light", light]]) {
    for (const name of ["--turn-error", "--turn-warn"]) {
      const hex = value(block, name);
      assert.match(hex, /^#[0-9a-f]{6}$/);
      assert.ok(turnContrast(hex, TURN_COLOR_BG[theme]) >= 4.5, `${name}/${theme} ${hex}`);
    }
  }
});

test("colorizeTurnLines pins an [error] entry and its continuation lines", () => {
  const out = colorizeTurnLines("[error] Exit code 100\n  details", "light");
  assert.ok(out.includes('<span style="color:var(--turn-error)">[error] Exit code 100</span>'));
  assert.ok(out.includes('<span style="color:var(--turn-error)">  details</span>'));
});

test("colorizeTurnLines colors continuation lines and escapes html", () => {
  const out = colorizeTurnLines("plain\n[tool.Bash] a<b\n  more\n[usage] x", "dark");
  const bash = typeColor("tool.Bash", "dark");
  assert.ok(out.startsWith("plain\n"));
  assert.ok(out.includes(`<span style="color:${bash}">[tool.Bash] a&lt;b</span>`));
  assert.ok(out.includes(`<span style="color:${bash}">  more</span>`));
  assert.ok(out.includes(`color:${typeColor("usage", "dark")}`));
});
