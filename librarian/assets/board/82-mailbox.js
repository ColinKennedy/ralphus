      // ---------- Persistent personal-mailbox inbox widget (RAL-401) ----------
      // A bottom-right fixture that replaces the old fire-and-forget toast
      // pattern for anything the daemon actually queues in the per-user
      // mailbox (RAL-241 broadcast queue, filtered per RAL-320 watches --
      // e.g. `daemon/src/ci_watch.rs`'s "review" category CI-failure
      // notices). Unlike a toast, a message here never disappears on its
      // own: it stays "unread" until the user dismisses it (which drains it
      // for *this* user only -- other watchers of the same entity keep
      // their own independent read state over the same row), or forever if
      // never dismissed. Polled from `tick()` (see `70-sse.js`)
      // unconditionally, like `pollWhoAmI`/`pollHidden`/`pollWatches` --
      // this widget is a global fixture, not scoped to the active tab.

      // RALPHUS-MAILBOX-WIDGET:BEGIN
      /** @type {MailboxMessageView[]} everything last fetched from the acting user's personal mailbox. */
      let mailboxMessages = [];
      /** Whether the widget's message list is currently expanded. */
      let mailboxExpanded = false;
      /** Opt-in: whether already-read messages are shown below the unread ones. Off by default -- unread-only is the normal view. */
      let mailboxShowRead = false;

      /**
       * `mailboxMessages` split into unread/read, each newest-first.
       * @returns {{unread: MailboxMessageView[], read: MailboxMessageView[]}}
       */
      function mailboxSorted() {
        const byNewest = (/** @type {MailboxMessageView} */ a, /** @type {MailboxMessageView} */ b) => b.created_at_ms - a.created_at_ms;
        return {
          unread: mailboxMessages.filter((m) => !m.read).sort(byNewest),
          read: mailboxMessages.filter((m) => m.read).sort(byNewest),
        };
      }

      /**
       * Polls the acting user's personal mailbox (`GET
       * /api/mailbox/personal/messages`) and re-renders the widget. Silent
       * on failure like every other unconditional `tick()` poller -- a
       * transient daemon hiccup shouldn't itself surface as a red toast.
       * @returns {Promise<void>}
       */
      async function pollMailbox() {
        if (!currentUserName) { mailboxMessages = []; renderMailboxWidget(); return; }
        try {
          const res = await fetch(`/api/mailbox/personal/messages?user=${encodeURIComponent(currentUserName)}`);
          if (res.ok) mailboxMessages = await res.json();
        } catch (e) { /* transient -- the next tick retries */ }
        renderMailboxWidget();
      }

      /**
       * Toggles the widget between its collapsed summary line and the full
       * expanded message list.
       * @returns {void}
       */
      function toggleMailboxWidget() {
        mailboxExpanded = !mailboxExpanded;
        renderMailboxWidget();
      }

      /**
       * Toggles the opt-in "show read history" section within the expanded widget.
       * @returns {void}
       */
      function toggleMailboxShowRead() {
        mailboxShowRead = !mailboxShowRead;
        renderMailboxWidget();
      }

      /**
       * Dismisses (drains) one message for the acting user only -- other
       * watchers of the same entity are unaffected. Updates local state
       * optimistically so the card disappears from the unread list
       * immediately, before the daemon round-trip completes.
       * @param {string} id
       * @returns {Promise<void>}
       */
      async function dismissMailboxMessage(id) {
        if (!id || !currentUserName) return;
        const msg = mailboxMessages.find((m) => m.id === id);
        if (msg) msg.read = true;
        renderMailboxWidget();
        try {
          await post(`/api/mailbox/personal/drain?user=${encodeURIComponent(currentUserName)}`, { message_ids: [id] });
        } catch (e) { /* the next poll reconciles either way */ }
      }

      /**
       * Renders one message as a card. Unread cards get a dismiss ("×")
       * button in their upper-right corner; already-read cards (shown only
       * when `mailboxShowRead` is on) don't, since they're already drained.
       * @param {MailboxMessageView} m
       * @returns {string}
       */
      function mailboxMsgHtml(m) {
        const when = new Date(m.created_at_ms).toLocaleString();
        const meta = [m.priority, when, m.category].filter(Boolean).map(esc).join(" · ");
        const dismiss = m.read ? "" : `<button type="button" class="mailbox-msg-dismiss" data-click="dismissMailboxMessage" data-id="${esc(m.id)}" aria-label="Dismiss message" data-tip="Mark this message read for you only.\nOther watchers of the same squad/review keep seeing it as unread until they dismiss it themselves -- this never deletes the message.">×</button>`;
        return `<div class="mailbox-msg p-${esc(m.priority)}${m.read ? " read" : ""}">
            <div class="mailbox-msg-meta">${meta}</div>
            <div class="mailbox-msg-body">${esc(m.message)}</div>
            ${dismiss}
          </div>`;
      }

      /**
       * Renders the widget: hidden entirely when there's nothing unread and
       * it isn't expanded (RAL-401's "absent when there are none"); a single
       * summary line when collapsed; the full unread-first, newest-first
       * list -- plus an opt-in read-history section -- when expanded.
       * @returns {void}
       */
      function renderMailboxWidget() {
        const widget = byId("mailbox-widget");
        const { unread, read } = mailboxSorted();
        if (!unread.length && !mailboxExpanded) { widget.style.display = "none"; return; }
        widget.style.display = "";
        byId("mailbox-widget-summary").textContent = unread.length
          ? `${unread.length} unread message${unread.length === 1 ? "" : "s"}`
          : "No unread messages";
        const toggleBtn = byId("mailbox-widget-toggle-btn");
        toggleBtn.textContent = mailboxExpanded ? "▼" : "▲";
        toggleBtn.setAttribute("aria-label", mailboxExpanded ? "Collapse message inbox" : "Expand message inbox");
        const list = byId("mailbox-widget-list");
        list.style.display = mailboxExpanded ? "" : "none";
        if (!mailboxExpanded) return;
        if (!unread.length && !read.length) {
          list.innerHTML = `<div class="empty">Nothing here.</div>`;
          return;
        }
        const parts = [unread.map(mailboxMsgHtml).join("")];
        if (read.length) {
          parts.push(`<button type="button" class="mailbox-widget-history-toggle" onclick="toggleMailboxShowRead()" data-tip="${mailboxShowRead ? "Hide" : "Show"} messages you've already dismissed, without leaving this widget.\nThe full permanent record is also on the Preferences page's Message history section.">${mailboxShowRead ? "▾ Hide read history" : `▸ Show read history (${read.length})`}</button>`);
          if (mailboxShowRead) parts.push(read.map(mailboxMsgHtml).join(""));
        }
        list.innerHTML = parts.join("");
      }
      // RALPHUS-MAILBOX-WIDGET:END

      // ---------- Preferences tab: full message history (RAL-401) ----------
      // Distinct from the live widget above: shows every message ever
      // delivered (including already-dismissed ones), reachable from the
      // Preferences page rather than floating over every tab.

      /**
       * Polls the full personal-mailbox history (unread and read alike) for
       * `prefsUserName()` -- respects RAL-332's "Edit Profile" view-as, same
       * as the Watches/Hidden-items sections on this page. Does not touch
       * `mailboxMessages`/the live widget, which always reflects the real
       * signed-in user regardless of which profile Preferences is viewing.
       * @returns {Promise<void>}
       */
      async function pollMailboxHistory() {
        const name = prefsUserName();
        if (!name) { mailboxHistory = []; mailboxHistoryError = ""; return; }
        try {
          const res = await fetch(`/api/mailbox/personal/messages?user=${encodeURIComponent(name)}`);
          if (res.ok) { mailboxHistory = await res.json(); mailboxHistoryError = ""; }
          else { mailboxHistory = []; mailboxHistoryError = await responseError(res, "could not load message history"); }
        } catch (e) { mailboxHistory = []; mailboxHistoryError = "daemon unreachable"; }
      }

      /**
       * Renders the Preferences tab's Message history table, newest first.
       * @returns {void}
       */
      function renderMailboxHistory() {
        const root = byId("mailbox-history");
        if (mailboxHistoryError) { root.innerHTML = `<div class="empty">${esc(mailboxHistoryError)}</div>`; return; }
        const rows = [...mailboxHistory].sort((a, b) => b.created_at_ms - a.created_at_ms);
        if (!rows.length) { root.innerHTML = `<div class="empty">No messages yet.</div>`; return; }
        const trs = rows.map((m) => `<tr>
            <td>${m.read ? "Read" : "Unread"}</td>
            <td>${esc(m.priority)}</td>
            <td>${esc(m.category || "—")}</td>
            <td style="color:var(--muted)">${esc(new Date(m.created_at_ms).toLocaleString())}</td>
            <td>${esc(m.message)}</td>
          </tr>`).join("");
        root.innerHTML = `<table class="proj-table mailbox-history-table"><thead><tr><th>Status</th><th>Priority</th><th>Category</th><th>When</th><th>Message</th></tr></thead><tbody>${trs}</tbody></table>`;
      }
