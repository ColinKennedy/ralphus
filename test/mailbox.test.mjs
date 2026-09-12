// Coverage for RAL-401 "rework review-page toasts into a persistent inbox":
//
// - the widget is absent (hidden) until at least one unread message exists,
//   and stays visible while expanded even after the last unread is dismissed;
// - unread messages always list before read ones, each group newest first;
// - dismissing a message is a per-user drain (`POST
//   /api/mailbox/personal/drain?user=...`), applied optimistically so the
//   card leaves the unread list before the round trip completes;
// - already-read messages only ever show once the user opts in via the
//   "show read history" toggle.
//
// Run with `npm test` (node --test). See ./board-mailbox.mjs for how the
// region is loaded out of the real board chunks.

import test from "node:test";
import assert from "node:assert/strict";
import { makeMailboxWidget, resolveJson } from "./board-mailbox.mjs";

test("mailboxSorted: unread and read are each newest-first, and partitioned correctly", () => {
  const widget = makeMailboxWidget();
  widget.setMessages([
    { id: "m-1", read: false, created_at_ms: 100 },
    { id: "m-2", read: true, created_at_ms: 300 },
    { id: "m-3", read: false, created_at_ms: 200 },
    { id: "m-4", read: true, created_at_ms: 150 },
  ]);
  const { unread, read } = widget.mailboxSorted();
  assert.deepEqual(unread.map((m) => m.id), ["m-3", "m-1"]);
  assert.deepEqual(read.map((m) => m.id), ["m-2", "m-4"]);
});

test("renderMailboxWidget: hidden when there is no unread message and the widget isn't expanded", () => {
  const widget = makeMailboxWidget();
  widget.setMessages([{ id: "m-1", read: true, created_at_ms: 100, priority: "normal" }]);
  widget.renderMailboxWidget();
  assert.equal(widget.els["mailbox-widget"].style.display, "none");
});

test("renderMailboxWidget: visible with a summary count whenever at least one unread message exists", () => {
  const widget = makeMailboxWidget();
  widget.setMessages([
    { id: "m-1", read: false, created_at_ms: 100, priority: "urgent" },
    { id: "m-2", read: false, created_at_ms: 200, priority: "high" },
  ]);
  widget.renderMailboxWidget();
  assert.equal(widget.els["mailbox-widget"].style.display, "");
  assert.equal(widget.els["mailbox-widget-summary"].textContent, "2 unread messages");
  // Collapsed by default -- the list itself stays hidden until expanded.
  assert.equal(widget.els["mailbox-widget-list"].style.display, "none");
});

test("toggleMailboxWidget: expands the list and renders unread before read, with a show-read-history control gating the read ones", () => {
  const widget = makeMailboxWidget();
  widget.setMessages([
    { id: "m-unread", read: false, created_at_ms: 200, priority: "high", message: "still open" },
    { id: "m-read", read: true, created_at_ms: 100, priority: "normal", message: "already seen" },
  ]);
  widget.toggleMailboxWidget();
  assert.equal(widget.state().mailboxExpanded, true);
  assert.equal(widget.els["mailbox-widget-list"].style.display, "");
  let html = widget.els["mailbox-widget-list"].innerHTML;
  assert.match(html, /still open/);
  assert.doesNotMatch(html, /already seen/, "read messages must not show until opted in");
  assert.match(html, /Show read history \(1\)/);

  widget.toggleMailboxShowRead();
  html = widget.els["mailbox-widget-list"].innerHTML;
  assert.match(html, /already seen/, "opting in must reveal the read message");
  // Unread must still be listed first.
  assert.ok(html.indexOf("still open") < html.indexOf("already seen"));
});

test("renderMailboxWidget: stays visible while expanded even once the last unread is gone", () => {
  const widget = makeMailboxWidget();
  widget.setMessages([{ id: "m-1", read: true, created_at_ms: 100, priority: "normal", message: "seen" }]);
  widget.toggleMailboxWidget();
  assert.equal(widget.els["mailbox-widget"].style.display, "", "an expanded widget stays visible even with zero unread");
  assert.equal(widget.els["mailbox-widget-summary"].textContent, "No unread messages");
});

test("mailboxMsgHtml: an unread message gets a dismiss control; a read one does not", () => {
  const widget = makeMailboxWidget();
  const unreadHtml = widget.mailboxMsgHtml({ id: "m-1", read: false, created_at_ms: 0, priority: "urgent", message: "hi" });
  assert.match(unreadHtml, /data-click="dismissMailboxMessage"/);
  assert.match(unreadHtml, /data-id="m-1"/);

  const readHtml = widget.mailboxMsgHtml({ id: "m-2", read: true, created_at_ms: 0, priority: "normal", message: "hi" });
  assert.doesNotMatch(readHtml, /dismissMailboxMessage/);
});

test("dismissMailboxMessage: drains for the acting user only, and applies optimistically before the request resolves", async () => {
  const widget = makeMailboxWidget({ currentUserName: "alice" });
  widget.setMessages([{ id: "m-1", read: false, created_at_ms: 100, priority: "high", message: "x" }]);

  const promise = widget.dismissMailboxMessage("m-1");
  // Optimistic: read flips to true synchronously, before the post() call settles.
  assert.equal(widget.state().mailboxMessages[0].read, true);
  await promise;

  assert.equal(widget.calls.posts.length, 1);
  assert.equal(widget.calls.posts[0].path, "/api/mailbox/personal/drain?user=alice");
  assert.deepEqual(widget.calls.posts[0].body, { message_ids: ["m-1"] });
});

test("dismissMailboxMessage: a no-op with no acting user or no id", async () => {
  const widget = makeMailboxWidget({ currentUserName: "" });
  widget.setMessages([{ id: "m-1", read: false, created_at_ms: 100, priority: "high" }]);
  await widget.dismissMailboxMessage("m-1");
  assert.equal(widget.calls.posts.length, 0);
  assert.equal(widget.state().mailboxMessages[0].read, false);
});

test("pollMailbox: fetches the acting user's personal mailbox, URL-encoding the name", async () => {
  const widget = makeMailboxWidget({ currentUserName: "alice bob" });
  const promise = widget.pollMailbox();
  assert.equal(widget.calls.fetches.length, 1);
  assert.equal(widget.calls.fetches[0], "/api/mailbox/personal/messages?user=alice%20bob");
  resolveJson(widget.pendingFetches[0], [{ id: "m-1", read: false, created_at_ms: 1, priority: "urgent" }]);
  await promise;
  assert.equal(widget.state().mailboxMessages.length, 1);
});

test("pollMailbox: clears messages and does not fetch when there is no acting user yet", async () => {
  const widget = makeMailboxWidget({ currentUserName: null });
  widget.setMessages([{ id: "m-1", read: false, created_at_ms: 1, priority: "urgent" }]);
  await widget.pollMailbox();
  assert.equal(widget.calls.fetches.length, 0);
  assert.equal(widget.state().mailboxMessages.length, 0);
});
