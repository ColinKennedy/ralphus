// Loads the RAL-406 `post`/`del` squad-mutation invalidation wiring out of
// the board chunk files (librarian/assets/board/) so it can be exercised
// under `node --test` with no browser and no build step.
//
// Same slice-the-real-source approach as ./board-tasks-poll.mjs (which
// covers the `invalidateTasksFetch`/`fetchTasksShared` side of the same
// fix) -- the region is the shipped code itself, so this test can't
// silently drift from what the librarian serves.

import { boardScript } from "./board-source.mjs";

const html = boardScript();

/** Slices one marker-delimited region out of the real board script. */
function sliceRegion([BEGIN, END]) {
  const from = html.indexOf(BEGIN);
  const to = html.indexOf(END);
  if (from === -1 || to === -1 || to < from) {
    throw new Error(
      `board-post-del: could not find the ${BEGIN} / ${END} markers in the board chunks. ` +
        "If this logic moved, move the markers with it -- these tests are its only coverage.",
    );
  }
  return html.slice(from + BEGIN.length, to);
}

const REGION = ["// RALPHUS-POST-DEL:BEGIN", "// RALPHUS-POST-DEL:END"];

/**
 * Builds the sandboxed `post`/`del` pair (RAL-406) with a controllable
 * fetch and a stubbed `invalidateTasksFetch` the test can assert was (or
 * wasn't) called for a given path.
 */
export function makePostDel() {
  const calls = { invalidateTasksFetch: 0, fetches: [] };
  const fetchImpl = (path, init) => {
    calls.fetches.push({ path, init });
    return Promise.resolve({ ok: true, json: async () => ({}) });
  };
  const deps = {
    traceHeaders: () => ({ traceparent: "stub" }),
    invalidateTasksFetch: () => { calls.invalidateTasksFetch++; },
  };
  // eslint-disable-next-line no-new-func -- evaluating the real shipped source is the point; see the header.
  const factory = new Function(
    "deps",
    "fetchImpl",
    `const { traceHeaders, invalidateTasksFetch } = deps;
     const fetch = fetchImpl;
     ${sliceRegion(REGION)}
     return { post, del };`,
  );
  const api = factory(deps, fetchImpl);
  return { ...api, calls };
}
