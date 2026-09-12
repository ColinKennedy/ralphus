// Loads the RAL-401 persistent personal-mailbox inbox widget logic out of the
// board chunk files (librarian/assets/board/) so it can be exercised under
// `node --test` with no browser and no build step.
//
// Same slice-the-real-source approach as ./board-tasks-poll.mjs -- the
// RALPHUS-MAILBOX-WIDGET region is the shipped code itself, so these tests
// can't silently drift from what the librarian serves.

import { boardScript } from "./board-source.mjs";

const html = boardScript();

const BEGIN = "// RALPHUS-MAILBOX-WIDGET:BEGIN";
const END = "// RALPHUS-MAILBOX-WIDGET:END";

function sliceRegion() {
  const from = html.indexOf(BEGIN);
  const to = html.indexOf(END);
  if (from === -1 || to === -1 || to < from) {
    throw new Error(
      `board-mailbox: could not find the ${BEGIN} / ${END} markers in the board chunks. ` +
        "If this logic moved, move the markers with it -- these tests are its only coverage.",
    );
  }
  return html.slice(from + BEGIN.length, to);
}

/** A `byId`-style element store: each id lazily gets a DOM-ish stub the sandboxed code can read/write, and the test can inspect afterward. */
function makeElements() {
  const els = {};
  const byId = (id) => (els[id] ||= {
    style: { display: "" },
    textContent: "",
    innerHTML: "",
    _attrs: {},
    setAttribute(name, value) { this._attrs[name] = value; },
  });
  return { els, byId };
}

/**
 * Builds the sandboxed mailbox-widget API (RAL-401), with a controllable
 * fetch (for `pollMailbox`) and a spy `post` (for `dismissMailboxMessage`),
 * mirroring ./board-tasks-poll.mjs's `makeTasksPoll`.
 */
export function makeMailboxWidget({ currentUserName = "alice" } = {}) {
  const calls = { fetches: [], posts: [] };
  /** @type {{url: string, resolve: (r: {ok: boolean, json: () => Promise<any>}) => void, reject: (e: unknown) => void}[]} */
  const pendingFetches = [];
  const fetchImpl = (url) => {
    calls.fetches.push(url);
    return new Promise((resolve, reject) => { pendingFetches.push({ url, resolve, reject }); });
  };
  const postImpl = (path, body) => {
    calls.posts.push({ path, body });
    return Promise.resolve({ ok: true, json: async () => ({ drained: 1 }) });
  };
  const esc = (s) => String(s ?? "").replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" })[c]);
  const { els, byId } = makeElements();
  const deps = { byId, esc, post: postImpl };
  // eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
  const factory = new Function(
    "deps",
    "fetchImpl",
    `const { byId, esc, post } = deps;
     const fetch = fetchImpl;
     var currentUserName = ${JSON.stringify(currentUserName)};
     ${sliceRegion()}
     return {
       pollMailbox, toggleMailboxWidget, toggleMailboxShowRead, dismissMailboxMessage,
       mailboxSorted, mailboxMsgHtml, renderMailboxWidget,
       state: () => ({ mailboxMessages, mailboxExpanded, mailboxShowRead }),
       setMessages: (msgs) => { mailboxMessages = msgs; },
     };`,
  );
  const api = factory(deps, fetchImpl);
  return { ...api, calls, pendingFetches, els };
}

/** Resolves one queued fetch with a JSON payload. */
export function resolveJson(entry, data, { ok = true } = {}) {
  entry.resolve({ ok, json: async () => data });
}
