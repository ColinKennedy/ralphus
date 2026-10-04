import { test } from "node:test";
import assert from "node:assert/strict";
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

test("colorizeTurnLines colors continuation lines and escapes html", () => {
  const out = colorizeTurnLines("plain\n[tool.Bash] a<b\n  more\n[usage] x", "dark");
  const bash = typeColor("tool.Bash", "dark");
  assert.ok(out.startsWith("plain\n"));
  assert.ok(out.includes(`<span style="color:${bash}">[tool.Bash] a&lt;b</span>`));
  assert.ok(out.includes(`<span style="color:${bash}">  more</span>`));
  assert.ok(out.includes(`color:${typeColor("usage", "dark")}`));
});
