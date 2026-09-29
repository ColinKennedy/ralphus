// Coverage for RAL-523 "Add REST forge connectivity checks" — the board's
// per-row manual check controls and their failure rendering:
//
// - `forgeCheckStatusHtml` renders a failed check as red status text naming
//   the verdict and the reason, truncated with an ellipsis when the daemon's
//   detail is long, and the truncated variant is the one wired to open the
//   full-message popup (short failures have nothing more to show);
// - a successful check renders green, carrying the authenticated identity
//   the daemon reported;
// - `openForgeCheckDetail`'s popup interpolates the *complete* detail (never
//   the truncated inline slice) together with the board's standard
//   copy-to-clipboard control — the piece that makes a long error recoverable;
// - every per-row check control (forge tokens, personal fork mappings, the
//   fork-registrations popup's rows and add form, and the Projects tab's
//   destination check) actually interpolates the status cell it renders into.
//
// Run with `npm test` (node --test). Same slice-the-real-source approach as
// ./board-agent-select.mjs: the RALPHUS-FORGE-CHECK:BEGIN/END marker block in
// librarian/assets/board/75-projects-machines.js calls only `esc` and the
// ambient `fetch`/`document` globals; `forgeCheckState` and
// `FORGE_CHECK_INLINE_MAX` are declared in 10-tab-registry.js, so the factory
// below supplies them (and `copyBtn`, from 20-util.js) as params.

import test from "node:test";
import assert from "node:assert/strict";
import { boardScript } from "./board-source.mjs";

const BEGIN = "// RALPHUS-FORGE-CHECK:BEGIN";
const END = "// RALPHUS-FORGE-CHECK:END";

const html = boardScript();
const from = html.indexOf(BEGIN);
const to = html.indexOf(END);
if (from === -1 || to === -1 || to < from) {
  throw new Error(
    `board-forge-check: could not find the ${BEGIN} / ${END} markers in the board chunks. ` +
      "If this logic moved, move the markers with it -- these tests are its only coverage.",
  );
}
const source = html.slice(from + BEGIN.length, to);

const esc = (s) =>
  String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;");

// eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
const factory = new Function(
  "forgeCheckState",
  "FORGE_CHECK_INLINE_MAX",
  "esc",
  "copyBtn",
  `${source}
   return {
     forgeCheckStatusHtml,
     openForgeCheckDetail,
     closeForgeCheckDetail,
     runForgeCheck,
   };`,
);

const copyPayloads = [];
const copyBtn = (text) => {
  copyPayloads.push(text);
  return `<button class="copy-btn" data-copy="x">⧉</button>`;
};

const INLINE_MAX = 140;

/** Builds the helpers against a fresh state map, tracking document writes. */
function makeForgeCheck() {
  const state = new Map();
  const created = [];
  const document = {
    createElement: () => {
      const el = {
        id: "",
        style: { cssText: "" },
        innerHTML: "",
        removed: false,
        getBoundingClientRect: () => ({ width: 300, height: 100, left: 0, top: 0, right: 300, bottom: 100 }),
      };
      created.push(el);
      return el;
    },
    getElementById: (id) => created.find((el) => el.id === id && !el.removed) || null,
    appendChild: () => {},
    body: { appendChild: () => {} },
    addEventListener: () => {},
    removeEventListener: () => {},
  };
  const api = factory(state, INLINE_MAX, esc, copyBtn);
  return { state, created, document, api };
}

test("a failed check renders red status text with its verdict and reason", () => {
  const { state, api } = makeForgeCheck();
  state.set("k", {
    loading: false,
    outcome: { ok: false, status: "not_found", detail: "the forge reports no such resource (404)" },
  });
  const html = api.forgeCheckStatusHtml("k");
  assert.ok(html.includes("var(--failed)"), "a failed check must render in the failed color");
  assert.ok(html.includes("✗"), "a failed check must carry the failure mark");
  assert.ok(html.includes("not_found"), "the verdict must be named");
  assert.ok(html.includes("no such resource"), "the reason must be shown");
  assert.ok(!html.includes("data-click"), "a short failure has nothing more to show, so no popup opener");
});

test("a long failure detail is truncated and the truncated cell opens the full-message popup", () => {
  const { state, api } = makeForgeCheck();
  const long = "x".repeat(INLINE_MAX + 50);
  state.set("k", { loading: false, outcome: { ok: false, status: "error", detail: long } });
  const out = api.forgeCheckStatusHtml("k");
  assert.ok(out.includes("…"), "a long failure must be truncated with an ellipsis");
  assert.ok(out.includes(`data-click="openForgeCheckDetail"`), "a truncated failure must open the full-message popup");
  assert.ok(out.includes(`data-key="${esc("k")}"`), "the popup opener must carry the row key");
  // The inline cell must stay bounded: the complete text lives only in the popup.
  assert.ok(!out.includes(long), "the inline status must not carry the full detail -- that is what the popup is for");
});

test("a successful check renders green with the authenticated identity", () => {
  const { state, api } = makeForgeCheck();
  state.set("k", {
    loading: false,
    outcome: { ok: true, status: "ok", detail: "the forge accepted this token", identity: "alice" },
  });
  const out = api.forgeCheckStatusHtml("k");
  assert.ok(out.includes("var(--done)"), "a successful check must render in the done color");
  assert.ok(out.includes("✓ ok"), "a successful check must carry the ok mark");
  assert.ok(out.includes("alice"), "the authenticated identity must be shown");
});

test("an in-flight check renders the transient checking badge", () => {
  const { state, api } = makeForgeCheck();
  state.set("k", { loading: true, outcome: null });
  assert.ok(api.forgeCheckStatusHtml("k").includes("Checking"));
});

test("the full-message popup carries the complete detail plus a copy control", async () => {
  const { state, created, document, api } = makeForgeCheck();
  const long = "FULL-".repeat(80);
  state.set("k", { loading: false, outcome: { ok: false, status: "error", detail: long } });
  const stubEvent = { clientX: 10, clientY: 10, stopPropagation: () => {} };
  const realDocument = globalThis.document;
  const realWindow = globalThis.window;
  globalThis.document = /** @type {typeof document} */ (/** @type {unknown} */ (document));
  globalThis.window = /** @type {typeof window} */ (/** @type {unknown} */ ({ innerWidth: 1200, innerHeight: 800 }));
  try {
    api.openForgeCheckDetail(stubEvent, "k");
    // openForgeCheckDetail registers its outside-click dismissal on a
    // following macrotask; drain it while the stubs are still installed.
    await new Promise((resolve) => setTimeout(resolve, 10));
  } finally {
    globalThis.document = realDocument;
    globalThis.window = realWindow;
  }
  assert.equal(created.length, 1, "the popup is created exactly once");
  assert.ok(created[0].innerHTML.includes(long), "the popup must contain the complete, untruncated message");
  assert.ok(created[0].innerHTML.includes("copy-btn"), "the popup must carry the board's copy-to-clipboard control");
  // The copy button's payload is the full message, not a truncated slice.
  assert.deepEqual(copyPayloads, [long]);
});

test("every per-row check control renders the status cell next to it", () => {
  // Source-level wiring assertions (the render functions live outside the
  // marker block, next to the tables they belong to).
  assert.ok(
    html.includes('data-click="checkPreferenceForgeToken"') && html.includes("forgeCheckStatusHtml(`token|"),
    "the forge-token rows must render a Check control and its status cell",
  );
  assert.ok(
    html.includes('data-click="checkPreferenceFork"') && html.includes("forgeCheckStatusHtml(`preffork|"),
    "the personal fork-mapping rows must render a Check control and its status cell",
  );
  assert.ok(
    html.includes('data-click="checkProjectFork"') && html.includes("forgeCheckStatusHtml(`fork|"),
    "the fork-registrations rows must render a Check control and its status cell",
  );
  assert.ok(
    html.includes("checkProjectForkDraft()") && html.includes("forgeCheckStatusHtml(`fork-add|"),
    "the fork-registrations add form must render a check-URL control and its status",
  );
  assert.ok(
    html.includes('data-click="checkProjectDestination"') && html.includes("forgeCheckStatusHtml(`dest|"),
    "the Projects tab rows must render a destination Check control and its status cell",
  );
});

test("runForgeCheck records the daemon's outcome and re-renders on both exits", () => {
  const { state, api } = makeForgeCheck();
  let renders = 0;
  const rerender = () => {
    renders++;
  };
  // The daemon answers 200 with a ForgeCheckOutcome even when the check itself
  // failed, so this stub exercises the normal completion path.
  const realFetch = globalThis.fetch;
  globalThis.fetch = (async () => ({
    ok: true,
    json: async () => ({ ok: false, status: "unreachable", detail: "could not reach the forge" }),
  }));
  const run = api.runForgeCheck("k", "/api/x", null, rerender);
  globalThis.fetch = realFetch;
  return run.then(() => {
    assert.equal(renders, 2, "re-render once when the check starts and once when it lands");
    const st = state.get("k");
    assert.equal(st.loading, false);
    assert.equal(st.outcome.status, "unreachable");
  });
});
