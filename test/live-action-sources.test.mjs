// RAL-687: a live action's "from" attribution renders each source cell as the
// shared linked-cell button, degrading to plain text when it cannot resolve.

import test from "node:test";
import assert from "node:assert/strict";
import vm from "node:vm";
import { boardScript } from "./board-source.mjs";

const script = boardScript();

function extractFunction(name) {
  const start = script.indexOf(`function ${name}(`);
  assert.notEqual(start, -1, `function ${name} not found`);
  let depth = 0;
  for (let i = script.indexOf("{", start); i < script.length; i++) {
    if (script[i] === "{") depth += 1;
    else if (script[i] === "}" && --depth === 0) return script.slice(start, i + 1);
  }
  throw new Error(`function ${name} has no closing brace`);
}

function load() {
  const context = vm.createContext({
    esc: (s) => String(s ?? ""),
    sdot: (s) => `[${s}]`,
    squads: [{ id: "squad-1", tasks: [{ name: "T0", cells: [{ id: "c0", name: "build", state: "done" }] }] }],
  });
  const src = ["linkedCellBtn", "parseCellUri", "liveActionSourcesHtml"].map(extractFunction).join("\n");
  vm.runInContext(`${src}; this.f = liveActionSourcesHtml;`, context);
  return context.f;
}

test("one source renders a gotoSquadItem button", () => {
  const html = load()(["cell:squad-1:0:0"]);
  assert.match(html, /data-click="gotoSquadItem" data-squad-id="squad-1" data-kind="cell" data-ti="0" data-si="0"/);
  assert.match(html, /build/);
});

test("multiple sources render one widget each", () => {
  const html = load()(["cell:squad-1:0:0", "cell:squad-1:0:0"]);
  assert.equal(html.split("gotoSquadItem").length - 1, 2);
});

test("unloaded or malformed sources stay plain text", () => {
  const f = load();
  for (const bad of ["cell:squad-9:0:0", "cell:squad-1:5:5", "garbage", "cell:squad-1:x:0"]) {
    const html = f([bad]);
    assert.ok(!html.includes("gotoSquadItem"), bad);
    assert.ok(html.includes(bad), bad);
  }
  assert.equal(f([]), "unknown");
});
