// Loads the RAL-430 scroll/focus-preservation helpers out of the board chunk
// files (librarian/assets/board/) so they can be exercised under `node --test`
// with no browser and no build step.
//
// Same slice-the-real-source approach as ./board-review-badge-nav.mjs and
// ./board-filter-selection-scroll.mjs -- the regions are the shipped code
// itself, so the tests can't silently drift from what the librarian serves.
//
// `preserveUserState` only ever touches `el`/`document`/`window`, so it's
// exercised here against a minimal fake DOM (below) rather than a stub --
// that fake DOM models the one thing that actually matters: an innerHTML
// swap destroys every existing node and mints brand-new ones (same ids,
// fresh scrollTop/value/selection), which is exactly what resets scroll
// position in the real browser.

import { boardScript } from "./board-source.mjs";
import { fileURLToPath } from "node:url";
import { dirname, join } from "node:path";

const repoRoot = join(dirname(fileURLToPath(import.meta.url)), "..");
export const boardPath = join(repoRoot, "librarian", "assets", "board");

const html = boardScript();

/** Slices one marker-delimited region out of the real board script. */
function sliceRegion([BEGIN, END]) {
  const from = html.indexOf(BEGIN);
  const to = html.indexOf(END);
  if (from === -1 || to === -1 || to < from) {
    throw new Error(
      `board-preserve-scroll: could not find the ${BEGIN} / ${END} markers in ${boardPath}. ` +
        "If this logic moved, move the markers with it -- these tests are its only coverage.",
    );
  }
  return html.slice(from + BEGIN.length, to);
}

const REGIONS = {
  preserve: ["// RALPHUS-PRESERVE-USER-STATE:BEGIN", "// RALPHUS-PRESERVE-USER-STATE:END"],
  rerenderOwningPane: ["// RALPHUS-RERENDER-OWNING-PANE:BEGIN", "// RALPHUS-RERENDER-OWNING-PANE:END"],
  renderAll: ["// RALPHUS-RENDER-ALL:BEGIN", "// RALPHUS-RENDER-ALL:END"],
};

/** The raw board script (all chunks concatenated), for source-level wiring assertions. */
export const boardSource = html;

// ---------- minimal fake DOM ----------
// Just enough of the Element/Document surface `preserveUserState` reads:
// scrollTop, id, contains(), querySelectorAll("[id]"), and (for the focused-
// input branch) value/selectionStart/selectionEnd/setSelectionRange/focus.

class FakeNode {
  constructor(id = "") {
    this.id = id;
    this.scrollTop = 0;
    this.children = [];
    this.parent = null;
  }
  appendChild(child) {
    child.parent = this;
    this.children.push(child);
    return child;
  }
  contains(node) {
    for (let n = node; n; n = n.parent) if (n === this) return true;
    return false;
  }
  querySelectorAll(sel) {
    if (sel !== "[id]") throw new Error(`FakeNode.querySelectorAll: unsupported selector ${sel}`);
    const out = [];
    const walk = (node) => {
      for (const c of node.children) {
        if (c.id) out.push(c);
        walk(c);
      }
    };
    walk(this);
    return out;
  }
}

class FakeInput extends FakeNode {
  constructor(id = "", value = "") {
    super(id);
    this.value = value;
    this.selectionStart = null;
    this.selectionEnd = null;
  }
  focus() { this.doc.setActive(this); }
  setSelectionRange(start, end) { this.selectionStart = start; this.selectionEnd = end; }
}

/**
 * A fake `document`/`window` pair sufficient for `preserveUserState`: an id
 * registry (mimicking `getElementById`), a settable `activeElement`, and a
 * `window.getSelection` that always reports "nothing selected" (selection
 * preservation is a separate concern -- `selectionWithin`/`userIsSelecting`
 * -- not exercised here).
 */
function makeFakeDom() {
  const registry = new Map();
  let active = null;
  const doc = {
    register(node) { if (node.id) registry.set(node.id, node); node.doc = doc; for (const c of node.children) doc.register(c); },
    getElementById(id) { return registry.get(id) || null; },
    get activeElement() { return active; },
    setActive(node) { active = node; },
  };
  const win = { getSelection: () => null };
  return { document: doc, window: win };
}

/**
 * Builds the real `preserveUserState` against the fake DOM above.
 * @returns {{ preserveUserState: Function, document: any }}
 */
export function makePreserveUserState() {
  const { document, window } = makeFakeDom();
  // eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
  const factory = new Function(
    "document",
    "window",
    `${sliceRegion(REGIONS.preserve)}\nreturn { preserveUserState, selectionWithin, userIsSelecting };`,
  );
  const api = factory(document, window);
  return { ...api, document };
}

/**
 * Builds `rerenderOwningPane` wired to the *real* `preserveUserState` (both
 * regions are evaluated together, exactly as they ship), with injectable
 * `sel`/`selectedGuardian` and `renderDetails`/`renderReviewDetail` -- the
 * point is to prove the wiring itself routes through `preserveUserState`
 * rather than calling the raw render functions directly.
 */
export function makeRerenderOwningPane({ sel = { kind: null }, selectedGuardian = null, renderDetails = () => {}, renderReviewDetail = () => {} } = {}) {
  const { document, window } = makeFakeDom();
  // eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
  const factory = new Function(
    "deps",
    `const { document, window, renderDetails, renderReviewDetail } = deps;
     var sel = deps.sel, selectedGuardian = deps.selectedGuardian;
     ${sliceRegion(REGIONS.preserve)}
     ${sliceRegion(REGIONS.rerenderOwningPane)}
     return { rerenderOwningPane, preserveUserState };`,
  );
  const api = factory({ document, window, sel, selectedGuardian, renderDetails, renderReviewDetail });
  return { ...api, document };
}

/**
 * Builds the real `renderAll` (RAL-436) wired to the *real*
 * `preserveUserState`/`selectionWithin` (both regions evaluated together,
 * exactly as they ship), with injectable `editing` and mocked
 * `renderSquads`/`renderGraph`/`renderDetails` -- the point is to prove that
 * `editing` only ever gates the details-pane re-render, never the sidebar or
 * graph.
 * @param {{editing?: boolean}} [opts]
 */
export function makeRenderAll({ editing = false } = {}) {
  const { document, window } = makeFakeDom();
  const calls = { renderSquads: 0, renderGraph: 0, renderDetails: 0 };
  // eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
  const factory = new Function(
    "deps",
    `const { document, window, renderSquads, renderGraph, renderDetails } = deps;
     var editing = deps.editing;
     ${sliceRegion(REGIONS.preserve)}
     ${sliceRegion(REGIONS.renderAll)}
     return { renderAll };`,
  );
  const api = factory({
    document,
    window,
    editing,
    renderSquads: () => { calls.renderSquads++; },
    renderGraph: () => { calls.renderGraph++; },
    renderDetails: () => { calls.renderDetails++; },
  });
  return { ...api, document, calls };
}

export { FakeNode, FakeInput };
