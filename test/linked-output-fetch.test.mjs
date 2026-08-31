// Coverage for `fetchLinkedOutputText` (RAL-295), the function behind the
// cells/proofs-tab output button's "linking to pane/terminal-log-attempts"
// behaviour: it prefers a durably-persisted terminal-log attempt (the
// fuller, final record) and only falls back to the live/last tmux pane
// snapshot when no attempt is available, returning `null` when neither
// backend has ever captured anything for the key.
//
// Run with `npm test` (node --test). See ./board-linked-output-fetch.mjs for
// how the function is loaded out of the real board.html with its URL-lookup
// helpers injected as stubs.

import test from "node:test";
import assert from "node:assert/strict";
import { readFileSync } from "node:fs";
import { makeFetchLinkedOutputText, boardPath } from "./board-linked-output-fetch.mjs";

const originalFetch = globalThis.fetch;
test.afterEach(() => {
  globalThis.fetch = originalFetch;
});

/**
 * A `fetch` stub that answers per exact URL, throwing if an unlisted URL is
 * requested (so an unexpected extra call fails loudly instead of hanging).
 * @param {Record<string, {ok: boolean, json?: () => Promise<unknown>} | "throw">} byUrl
 * @returns {(url: string) => Promise<object>}
 */
function stubFetch(byUrl) {
  return async (url) => {
    const answer = byUrl[url];
    if (answer === undefined) throw new Error(`unexpected fetch: ${url}`);
    if (answer === "throw") throw new Error("network error");
    return { ok: answer.ok, json: answer.json };
  };
}

test("a persisted terminal-log attempt is preferred over the pane snapshot", async () => {
  globalThis.fetch = stubFetch({
    "/attempts": { ok: true, json: async () => ({ attempts: [{ attempt: 1 }, { attempt: 2 }] }) },
    "/attempts/2": { ok: true, json: async () => ({ content: "final attempt output" }) },
  });
  const fetchLinkedOutputText = makeFetchLinkedOutputText({
    terminalLogAttemptsUrlFor: (key, attempt) => (attempt === undefined ? "/attempts" : `/attempts/${attempt}`),
    peekUrlFor: () => { throw new Error("must not fall back to the pane when an attempt exists"); },
  });
  assert.equal(await fetchLinkedOutputText("cell|squad-abc|0|1"), "final attempt output");
});

test("it fetches the most recent attempt, not the first", async () => {
  globalThis.fetch = stubFetch({
    "/attempts": { ok: true, json: async () => ({ attempts: [{ attempt: 1 }, { attempt: 2 }, { attempt: 3 }] }) },
    "/attempts/3": { ok: true, json: async () => ({ content: "attempt three" }) },
  });
  const fetchLinkedOutputText = makeFetchLinkedOutputText({
    terminalLogAttemptsUrlFor: (key, attempt) => (attempt === undefined ? "/attempts" : `/attempts/${attempt}`),
  });
  assert.equal(await fetchLinkedOutputText("cell|squad-abc|0|1"), "attempt three");
});

test("no persisted attempts yet falls back to the pane snapshot", async () => {
  globalThis.fetch = stubFetch({
    "/attempts": { ok: true, json: async () => ({ attempts: [] }) },
    "/pane": { ok: true, json: async () => ({ content: "live pane output" }) },
  });
  const fetchLinkedOutputText = makeFetchLinkedOutputText({
    terminalLogAttemptsUrlFor: () => "/attempts",
    peekUrlFor: () => "/pane",
  });
  assert.equal(await fetchLinkedOutputText("cell|squad-abc|0|1"), "live pane output");
});

test("a failed attempts-list request falls back to the pane snapshot", async () => {
  globalThis.fetch = stubFetch({
    "/attempts": { ok: false },
    "/pane": { ok: true, json: async () => ({ content: "live pane output" }) },
  });
  const fetchLinkedOutputText = makeFetchLinkedOutputText({
    terminalLogAttemptsUrlFor: () => "/attempts",
    peekUrlFor: () => "/pane",
  });
  assert.equal(await fetchLinkedOutputText("cell|squad-abc|0|1"), "live pane output");
});

test("a network error listing attempts falls back to the pane snapshot", async () => {
  globalThis.fetch = stubFetch({
    "/attempts": "throw",
    "/pane": { ok: true, json: async () => ({ content: "live pane output" }) },
  });
  const fetchLinkedOutputText = makeFetchLinkedOutputText({
    terminalLogAttemptsUrlFor: () => "/attempts",
    peekUrlFor: () => "/pane",
  });
  assert.equal(await fetchLinkedOutputText("cell|squad-abc|0|1"), "live pane output");
});

test("a key with no terminal-log-attempts endpoint (e.g. an unsupported kind) goes straight to the pane", async () => {
  globalThis.fetch = stubFetch({
    "/pane": { ok: true, json: async () => ({ content: "live pane output" }) },
  });
  const fetchLinkedOutputText = makeFetchLinkedOutputText({
    terminalLogAttemptsUrlFor: () => null,
    peekUrlFor: () => "/pane",
  });
  assert.equal(await fetchLinkedOutputText("nonsense|whatever"), "live pane output");
});

test("neither backend has ever captured output for this key: null, not an empty string or a thrown error", async () => {
  const fetchLinkedOutputText = makeFetchLinkedOutputText({
    terminalLogAttemptsUrlFor: () => null,
    peekUrlFor: () => null,
  });
  assert.equal(await fetchLinkedOutputText("cell|squad-abc|0|1"), null);
});

test("the pane responding but with empty content is treated the same as no content", async () => {
  globalThis.fetch = stubFetch({
    "/pane": { ok: true, json: async () => ({ content: "" }) },
  });
  const fetchLinkedOutputText = makeFetchLinkedOutputText({
    terminalLogAttemptsUrlFor: () => null,
    peekUrlFor: () => "/pane",
  });
  assert.equal(await fetchLinkedOutputText("cell|squad-abc|0|1"), null);
});

test("a failed pane request after no attempts still resolves to null rather than throwing", async () => {
  globalThis.fetch = stubFetch({
    "/attempts": { ok: true, json: async () => ({ attempts: [] }) },
    "/pane": { ok: false },
  });
  const fetchLinkedOutputText = makeFetchLinkedOutputText({
    terminalLogAttemptsUrlFor: () => "/attempts",
    peekUrlFor: () => "/pane",
  });
  assert.equal(await fetchLinkedOutputText("cell|squad-abc|0|1"), null);
});

test("both the attempt content and the pane fallback content are scrubbed for secrets", async () => {
  const seen = [];
  const scrubSecrets = (text) => { seen.push(text); return `SCRUBBED(${text})`; };

  globalThis.fetch = stubFetch({
    "/attempts": { ok: true, json: async () => ({ attempts: [{ attempt: 1 }] }) },
    "/attempts/1": { ok: true, json: async () => ({ content: "sk-secret-token" }) },
  });
  const withAttempt = makeFetchLinkedOutputText({
    terminalLogAttemptsUrlFor: (key, attempt) => (attempt === undefined ? "/attempts" : `/attempts/${attempt}`),
    scrubSecrets,
  });
  assert.equal(await withAttempt("cell|squad-abc|0|1"), "SCRUBBED(sk-secret-token)");

  globalThis.fetch = stubFetch({
    "/pane": { ok: true, json: async () => ({ content: "sk-another-secret" }) },
  });
  const withPane = makeFetchLinkedOutputText({
    terminalLogAttemptsUrlFor: () => null,
    peekUrlFor: () => "/pane",
    scrubSecrets,
  });
  assert.equal(await withPane("cell|squad-abc|0|1"), "SCRUBBED(sk-another-secret)");

  assert.deepEqual(seen, ["sk-secret-token", "sk-another-secret"]);
});

// ---- Wiring: openLinkedOutputPopup's loading/empty-state text ----
//
// openLinkedOutputPopup itself needs a live document (byId/modal-root), so
// it stays out of the marker region above and is asserted from the shipped
// source directly, the same way merge-button.test.mjs checks mergeReview's
// wiring.

test("openLinkedOutputPopup shows a Loading state immediately, before the fetch resolves", () => {
  const boardSource = readFileSync(boardPath, "utf8");
  const fnStart = boardSource.indexOf("async function openLinkedOutputPopup(key)");
  const fnBody = boardSource.slice(fnStart, boardSource.indexOf("\n      }\n", fnStart));
  const loadingAt = fnBody.indexOf('showTextPopup("Output", "Loading…")');
  const awaitAt = fnBody.indexOf("await fetchLinkedOutputText(key)");
  assert.ok(loadingAt > -1 && awaitAt > -1, "openLinkedOutputPopup shape changed");
  assert.ok(loadingAt < awaitAt, "the Loading state must be shown before awaiting the fetch");
});

test("openLinkedOutputPopup falls back to an explicit empty-state message, not a blank popup", () => {
  const boardSource = readFileSync(boardPath, "utf8");
  const fnStart = boardSource.indexOf("async function openLinkedOutputPopup(key)");
  const fnBody = boardSource.slice(fnStart, boardSource.indexOf("\n      }\n", fnStart));
  assert.match(fnBody, /text \|\| "No output recorded yet\."/);
});
