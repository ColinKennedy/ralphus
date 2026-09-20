// Loads the shared Status dropdown component (RAL-475) straight out of
// librarian/assets/board/06-status-dropdown.js, sliced by the
// RALPHUS-STATUS-DROPDOWN markers. Same slice-the-real-source approach as
// ./board-project-filter-menu.mjs -- this logic touches `document` directly
// (menu creation/removal), which is why it needs its own DOM stubs here.

import { boardScript } from "./board-source.mjs";

const REGION = ["// RALPHUS-STATUS-DROPDOWN:BEGIN", "// RALPHUS-STATUS-DROPDOWN:END"];

function sliceRegion([BEGIN, END]) {
  const html = boardScript();
  const from = html.indexOf(BEGIN);
  const to = html.indexOf(END);
  if (from === -1 || to === -1 || to < from) {
    throw new Error(
      `board-status-dropdown: could not find the ${BEGIN} / ${END} markers in librarian/assets/board/. ` +
        "If this logic moved, move the markers with it -- these tests are its only coverage.",
    );
  }
  return html.slice(from + BEGIN.length, to);
}

/** A minimal fake element: just enough for setAttribute/getAttribute/focus/remove/querySelector bookkeeping. */
function makeFakeDocument() {
  const byIdMap = new Map();
  const clickListeners = [];

  function makeElement() {
    const attrs = new Map();
    const el = {
      id: "",
      className: "",
      innerHTML: "",
      style: {},
      _focused: false,
      setAttribute(name, value) { attrs.set(name, String(value)); },
      getAttribute(name) { return attrs.has(name) ? attrs.get(name) : null; },
      focus() { el._focused = true; },
      addEventListener() {},
      querySelector() { return makeElement(); },
      getBoundingClientRect() { return { left: 0, right: 0, top: 0, bottom: 0 }; },
      remove() { if (el.id) byIdMap.delete(el.id); },
    };
    return el;
  }

  function selectorMatches(selector, el) {
    const classMatch = selector.match(/^\.([\w-]+)/);
    if (classMatch && el.className !== classMatch[1]) return false;
    const idPrefixMatch = selector.match(/\[id\^='([^']+)'\]/);
    if (idPrefixMatch && !el.id.startsWith(idPrefixMatch[1])) return false;
    return true;
  }

  const doc = {
    getElementById: (id) => byIdMap.get(id) || null,
    createElement: () => makeElement(),
    querySelectorAll: (selector) => [...byIdMap.values()].filter((el) => selectorMatches(selector, el)),
    body: {
      appendChild: (el) => { if (el.id) byIdMap.set(el.id, el); },
    },
    addEventListener: (type, fn) => { if (type === "click") clickListeners.push(fn); },
  };
  return { doc, byIdMap, clickListeners, makeElement };
}

/**
 * Builds one Status dropdown instance (renderStatusDropdown/toggle/All/None/
 * close-all/keydown functions) with injectable collaborator stubs, plus a
 * `config` object whose `onToggle`/`onAll`/`onNone` mutate `selected`
 * in place -- the same object-reference-sharing contract every real calling
 * view relies on.
 */
export function makeStatusDropdown({
  id = "test",
  label = "Status",
  mode = "multi",
  options = [
    { value: "zzz_first_raw", label: "Apple", color: "--done" },
    { value: "aaa_second_raw", label: "Banana", color: "--pending" },
  ],
  selected = new Set(["zzz_first_raw", "aaa_second_raw"]),
  selectedValue = null,
  containerId = "status-filters",
} = {}) {
  const { doc, byIdMap, clickListeners, makeElement } = makeFakeDocument();
  const container = makeElement();
  container.id = containerId;
  byIdMap.set(containerId, container);
  const calls = { onToggle: [], onAll: 0, onNone: 0, onSelect: [] };
  const deps = {
    document: doc,
    byId: (elId) => byIdMap.get(elId) || null,
    esc: (s) => String(s).replace(/&/g, "&amp;").replace(/</g, "&lt;").replace(/>/g, "&gt;").replace(/"/g, "&quot;"),
    cvar: (name) => `var(${name})`,
    window: { innerWidth: 1024, innerHeight: 768 },
  };
  const config = {
    id,
    label,
    mode,
    options,
    selected,
    selectedValue,
    onToggle: (value, on) => { calls.onToggle.push([value, on]); on ? selected.add(value) : selected.delete(value); },
    onAll: () => { calls.onAll++; },
    onNone: () => { calls.onNone++; },
    onSelect: (value) => { calls.onSelect.push(value); },
  };
  // eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
  const factory = new Function(
    "deps",
    `const { document, byId, esc, cvar, window } = deps;
     ${sliceRegion(REGION)}
     return {
       statusDropdownLabel, statusDropdownSortedOptions, statusDropdownDot,
       statusDropdownTriggerLabelHtml, statusDropdownTriggerTip, statusDropdownTriggerHtml,
       statusDropdownMultiRowsHtml, statusDropdownSingleRowsHtml,
       renderStatusDropdown, statusDropdownRefreshMenu, statusDropdownRefreshTrigger,
       statusDropdownToggleMenu, statusDropdownCloseAll, statusDropdownMenuKeydown,
       statusDropdownToggleOption, statusDropdownSelectAll, statusDropdownSelectNone,
       statusDropdownSelectSingle, statusDropdownMenuId, statusDropdownTriggerId,
       statusDropdownRegistry,
     };`,
  );
  const api = factory(deps);
  return { ...api, calls, config, container, containerId, byIdMap, clickListeners, selected, makeElement };
}

/**
 * Builds a fake click event whose `currentTarget` is a real (fake) trigger
 * element registered in `byIdMap` under the dropdown's trigger id -- mirrors
 * what the browser gives `statusDropdownToggleMenu` for a real click, since
 * this harness's fake `document` doesn't parse `innerHTML` strings into
 * queryable elements the way a real DOM does.
 * @param {ReturnType<typeof makeStatusDropdown>} d
 * @returns {{preventDefault: () => void, stopPropagation: () => void, currentTarget: object}}
 */
export function fakeTriggerClick(d) {
  const triggerId = d.statusDropdownTriggerId(d.config.id);
  let trigger = d.byIdMap.get(triggerId);
  if (!trigger) {
    trigger = d.makeElement();
    trigger.id = triggerId;
    d.byIdMap.set(triggerId, trigger);
  }
  return { preventDefault() {}, stopPropagation() {}, currentTarget: trigger };
}
