      // ---------- Unified board-action notifications (RAL-433) ----------
      // One notification system for every user-visible board action (create
      // squad, cancel/restart/delete/retry, merge/rebase, bulk operations,
      // ...), replacing the three single-purpose toast helpers RAL-108
      // introduced only for review/guardian actions (showReviewError /
      // showInfoToast / showWarningToast, formerly in 65-reviews.js). Two
      // halves, both driven by one call -- notify(kind, message, opts):
      //   - a transient toast (bottom-right, auto-dismisses) for immediate
      //     feedback, same #toast-root fixture the old helpers already used
      //   - a persistent notification-center history (the header's 🔔), so a
      //     toast that appeared while you were looking elsewhere isn't lost
      // "Debounced" per RAL-433 means both of:
      //   - an identical (kind, message) repeated within NOTIFY_DEDUPE_COOLDOWN_MS
      //     bumps the existing history entry's count instead of spawning a
      //     new toast (generalizes RAL-108's showReviewError cooldown map to
      //     every kind, not just review errors)
      //   - a burst of *distinct* messages landing within NOTIFY_BATCH_WINDOW_MS
      //     of each other is flushed as one grouped toast instead of stacking
      //     one toast per action
      // RALPHUS-NOTIFICATIONS:BEGIN

      /**
       * @typedef {"success"|"error"|"warn"|"info"} NotifyKind
       */
      /**
       * @typedef {object} BoardNotification
       * @property {string} id
       * @property {NotifyKind} kind
       * @property {string} message
       * @property {number} at_ms
       * @property {boolean} read
       * @property {boolean} dismissed
       * @property {number} count
       */
      /**
       * @typedef {object} NotifyAction
       * @property {string} label
       * @property {() => Promise<void>} run
       */
      /**
       * @typedef {object} NotifyOpts
       * @property {string} [success] - message shown via notify("success", ...) when the response is ok
       * @property {string} [errorLabel] - short label (e.g. "cancel squad") used to build the notify("error", ...) message on a non-ok response or network failure; omit to opt out of automatic error notification
       */

      /** @type {BoardNotification[]} newest first; capped at NOTIFY_HISTORY_LIMIT. */
      let boardNotifications = [];
      let _notifySeq = 0;
      /** Whether the notification-center dropdown is currently open. */
      let notifCenterOpen = false;
      /** RAL-465: opt-in "show read messages" toggle -- off by default, so dismissed notifications stay out of the way until asked for. */
      let notifShowRead = false;
      /** Oldest history kept; older entries are dropped so this never grows unbounded across a long-lived tab. */
      const NOTIFY_HISTORY_LIMIT = 200;
      /** An identical (kind, message) pair repeated within this window bumps the existing entry instead of spawning a new toast. */
      const NOTIFY_DEDUPE_COOLDOWN_MS = 8000;
      /** Distinct messages arriving within this window of each other are flushed as one grouped toast. */
      const NOTIFY_BATCH_WINDOW_MS = 350;
      /** How long each toast kind stays on screen before auto-removing itself. */
      const NOTIFY_TOAST_MS = { success: 4000, info: 4000, warn: 8000, error: 6000 };
      /** Per-kind tooltip text (RAL-40's tooltip rule) shared by every toast/notification-row rendering of that kind. */
      const NOTIFY_TIP = {
        success: "Confirms the action completed successfully.\nDisappears automatically after a few seconds; check the notification center (🔔) if you missed it.",
        error: "The action was rejected by the daemon, or failed over the network -- this is the real error, not a silent no-op.\nRepeated identical failures within a short window are suppressed rather than piling up more toasts.",
        warn: "Warns about a side effect of the action you just took.\nUse the button, if shown, to act on it now; otherwise no action is needed.",
        info: "Confirms the action was accepted and is running in the background.\nIts real outcome, once known, shows up as a separate notification.",
      };
      /** @type {{kind: NotifyKind, message: string, action?: NotifyAction}[]} */
      let _notifyBatchQueue = [];
      /** @type {ReturnType<typeof setTimeout>|null} */
      let _notifyBatchTimer = null;
      /** @type {Map<string, number>} "kind|message" -> ms timestamp last shown */
      const _notifyLastShown = new Map();

      /**
       * Central entry point for reporting a board action's outcome: records
       * it in the notification-center history and queues a toast, deduping
       * an identical (kind, message) repeat within the cooldown window into
       * the existing history entry (count++) instead of piling up another
       * toast for it.
       * @param {NotifyKind} kind
       * @param {string} message
       * @param {{action?: NotifyAction}} [opts]
       * @returns {void}
       */
      function notify(kind, message, opts) {
        const action = opts && opts.action;
        const key = `${kind}|${message}`;
        const now = Date.now();
        const last = _notifyLastShown.get(key) || 0;
        _notifyLastShown.set(key, now);
        if (now - last < NOTIFY_DEDUPE_COOLDOWN_MS) {
          const existing = boardNotifications.find((n) => n.kind === kind && n.message === message && !n.dismissed);
          if (existing) {
            existing.count++; existing.at_ms = now; existing.read = false;
            renderNotificationCenter();
            return;
          }
        }
        const entry = { id: `notif-${++_notifySeq}`, kind, message, at_ms: now, read: false, dismissed: false, count: 1 };
        boardNotifications.unshift(entry);
        if (boardNotifications.length > NOTIFY_HISTORY_LIMIT) boardNotifications.length = NOTIFY_HISTORY_LIMIT;
        renderNotificationCenter();
        _notifyBatchQueue.push({ kind, message, action });
        if (_notifyBatchTimer) clearTimeout(_notifyBatchTimer);
        _notifyBatchTimer = setTimeout(flushNotifyBatch, NOTIFY_BATCH_WINDOW_MS);
      }

      /**
       * Flushes the pending batch queue as either a single toast (one action
       * fired in isolation) or one grouped toast (a burst of distinct
       * actions that landed within NOTIFY_BATCH_WINDOW_MS of each other) --
       * see the module doc comment above for why this beats stacking one
       * toast per action.
       * @returns {void}
       */
      function flushNotifyBatch() {
        const batch = _notifyBatchQueue;
        _notifyBatchQueue = [];
        _notifyBatchTimer = null;
        const root = document.getElementById("toast-root");
        if (!root || !batch.length) return;
        root.appendChild(batch.length === 1 ? notifyToastEl(batch[0].kind, batch[0].message, batch[0].action) : notifyGroupToastEl(batch));
      }

      /**
       * Builds one toast element for a single notification.
       * @param {NotifyKind} kind
       * @param {string} message
       * @param {NotifyAction} [action]
       * @returns {HTMLElement}
       */
      function notifyToastEl(kind, message, action) {
        const el = document.createElement("div");
        el.className = `toast${kind === "error" ? "" : ` ${kind}`}`;
        el.setAttribute("data-tip", NOTIFY_TIP[kind]);
        const msg = document.createElement("div");
        msg.textContent = message;
        el.appendChild(msg);
        if (action) {
          const actions = document.createElement("div");
          actions.className = "toast-actions";
          const btn = document.createElement("button");
          btn.className = "btn";
          btn.textContent = action.label;
          btn.onclick = async () => {
            btn.disabled = true;
            try { await action.run(); } finally { el.remove(); }
          };
          actions.appendChild(btn);
          el.appendChild(actions);
        }
        setTimeout(() => el.remove(), NOTIFY_TOAST_MS[kind] || 6000);
        return el;
      }

      /**
       * Builds one grouped toast covering a burst of distinct notifications
       * that landed inside the same debounce window -- one row per message,
       * each left-bordered by that message's own kind, instead of several
       * separate toasts appearing at once.
       * @param {{kind: NotifyKind, message: string}[]} batch
       * @returns {HTMLElement}
       */
      function notifyGroupToastEl(batch) {
        const el = document.createElement("div");
        el.className = "toast group";
        el.setAttribute("data-tip", "Several board actions completed within the same moment, grouped into one popup instead of stacking one toast per action.\nEach row is colored by its own outcome; check the notification center (🔔) for the full history.");
        const head = document.createElement("div");
        head.style.fontWeight = "600";
        head.textContent = `${batch.length} updates`;
        el.appendChild(head);
        for (const b of batch) {
          const row = document.createElement("div");
          row.className = `toast-group-row ${b.kind}`;
          row.textContent = b.message;
          el.appendChild(row);
        }
        const longestMs = Math.max(...batch.map((b) => NOTIFY_TOAST_MS[b.kind] || 6000));
        setTimeout(() => el.remove(), longestMs);
        return el;
      }

      /**
       * Opens/closes the notification-center dropdown. Opening marks every
       * currently-listed notification read -- same "seeing it clears the
       * badge" convention the mailbox widget elsewhere on the board uses --
       * but dismissal is a separate, explicit action (see
       * dismissNotification): a dismissed entry stays in the list, shown in
       * red, rather than disappearing, so closing/opening this panel can
       * never lose the record of what happened.
       * @param {MouseEvent} [e]
       * @returns {void}
       */
      function toggleNotificationCenter(e) {
        if (e) e.stopPropagation();
        notifCenterOpen = !notifCenterOpen;
        if (notifCenterOpen) for (const n of boardNotifications) n.read = true;
        renderNotificationCenter();
      }
      document.addEventListener("click", () => {
        if (!notifCenterOpen) return;
        notifCenterOpen = false;
        renderNotificationCenter();
      });

      /**
       * Marks one notification dismissed (rendered in red, retained) rather
       * than removing it from the history. Once dismissed and hidden by the
       * default "show read messages" toggle, the next-newest notification
       * becomes the first row shown, so dismissing effectively promotes it.
       * @param {string} id
       * @returns {void}
       */
      function dismissNotification(id) {
        const n = boardNotifications.find((x) => x.id === id);
        if (n) { n.dismissed = true; n.read = true; }
        renderNotificationCenter();
      }

      /**
       * Reverts one dismissed notification back to unread (RAL-465) --
       * the inverse of dismissNotification. Removes it from the read
       * list and returns it to the main (unread) view.
       * @param {string} id
       * @returns {void}
       */
      function undismissNotification(id) {
        const n = boardNotifications.find((x) => x.id === id);
        if (n) { n.dismissed = false; n.read = false; }
        renderNotificationCenter();
      }

      /**
       * Toggles whether already-dismissed ("read") notifications are shown
       * in the notification-center list, mirroring the mailbox widget's
       * toggleMailboxShowRead.
       * @returns {void}
       */
      function toggleNotifShowRead() {
        notifShowRead = !notifShowRead;
        renderNotificationCenter();
      }

      /**
       * Renders one notification-center row. Dismissed rows get a "mark
       * unread" action instead of a dismiss button, so they can be moved
       * back to the main view.
       * @param {BoardNotification} n
       * @returns {string}
       */
      function notifRowHtml(n) {
        const when = new Date(n.at_ms).toLocaleTimeString();
        const countSuffix = n.count > 1 ? ` ×${n.count}` : "";
        const cls = `notif-row ${n.kind}${n.dismissed ? " dismissed" : ""}${n.read ? "" : " unread"}`;
        const actionBtn = n.dismissed
          ? `<button type="button" class="notif-undismiss" data-click="undismissNotification" data-id="${esc(n.id)}" aria-label="Mark notification unread" data-tip="Move this notification back to unread.\nIt leaves the read list and reappears in the main view.">↺</button>`
          : `<button type="button" class="notif-dismiss" data-click="dismissNotification" data-id="${esc(n.id)}" aria-label="Dismiss notification" data-tip="Mark this notification dismissed.\nIt stays in this history, shown in red, instead of being removed -- so you can still see what happened.">×</button>`;
        return `<div class="${cls}" data-tip="${esc(NOTIFY_TIP[n.kind])}">
            <div class="notif-row-meta">${esc(when)}</div>
            <div class="notif-row-msg">${esc(n.message)}${countSuffix}</div>
            ${actionBtn}
          </div>`;
      }

      /**
       * Renders the header bell's unread badge and, when open, the
       * notification-center dropdown (newest first). Dismissed ("read")
       * notifications are hidden by default; the "show read messages"
       * checkbox in the panel head opts back into seeing them (RAL-465).
       * @returns {void}
       */
      function renderNotificationCenter() {
        const badge = document.getElementById("notif-badge");
        if (badge) {
          const unread = boardNotifications.filter((n) => !n.read).length;
          badge.textContent = String(unread);
          badge.style.display = unread ? "" : "none";
        }
        const panel = document.getElementById("notif-panel");
        if (!panel) return;
        panel.classList.toggle("hidden", !notifCenterOpen);
        if (!notifCenterOpen) return;
        preserveUserState(panel, () => {
          const visible = notifShowRead
            ? boardNotifications
            : boardNotifications.filter((n) => !n.dismissed);
          const list = visible.length
            ? visible.map(notifRowHtml).join("")
            : `<div class="empty">No notifications yet.</div>`;
          panel.innerHTML = `<div class="notif-panel-inner" onclick="event.stopPropagation()">
              <div class="notif-panel-head">
                <span>Notifications</span>
                <label class="notif-show-read" data-tip="Show notifications you've already dismissed, alongside the current ones.">
                  <input type="checkbox" ${notifShowRead ? "checked" : ""} onchange="toggleNotifShowRead()">
                  Show read
                </label>
              </div>
              <div id="notif-panel-list" class="notif-panel-list">${list}</div>
            </div>`;
        });
      }

      /**
       * Wraps a post()/del() fetch so its outcome also produces a board
       * notification (RAL-433), without disturbing the Response returned to
       * the caller. The success/failure text is read off a *cloned* response
       * -- the real body stream can only be consumed once, and most callers
       * still read it themselves (e.g. to pull a newly created id out of a
       * 200, or a structured error out of a non-2xx) -- so this never steals
       * the body out from under them. A network failure (the fetch itself
       * rejecting) notifies then rethrows, so a caller with no explicit
       * catch of its own still behaves exactly as it did before this hook
       * existed.
       * @param {Promise<Response>} respPromise
       * @param {NotifyOpts} notifyOpts
       * @returns {Promise<Response>}
       */
      function withActionNotify(respPromise, notifyOpts) {
        return respPromise.then(
          async (resp) => {
            if (resp.ok) {
              if (notifyOpts.success) notify("success", notifyOpts.success);
            } else if (notifyOpts.errorLabel) {
              notify("error", await responseError(resp.clone(), `${notifyOpts.errorLabel} failed`));
            }
            return resp;
          },
          (err) => {
            if (notifyOpts.errorLabel) notify("error", `${notifyOpts.errorLabel} failed: network error`);
            throw err;
          },
        );
      }
      // RALPHUS-NOTIFICATIONS:END
