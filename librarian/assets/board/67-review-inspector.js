      // ---- Reviews tab: branch inspector + log dock ----
      //
      // Two surfaces that used to be buried inside the review's single
      // scrolling column:
      //
      //   * per-branch detail (worktree, PR, feedback, live view) was a
      //     disclosure triangle nested inside a branch row, so reading one
      //     branch pushed every other branch off-screen;
      //   * the audit log was a modal, which meant you could read the log or
      //     the review, never both.
      //
      // They become a context pane bound to the selected branch, and a drawer
      // docked under the review. Both follow the selection, so selecting a
      // branch is the one gesture that drives the whole page.
      //
      // Lazy by construction (see librarian/AGENTS.md "Lazy by default"): only
      // the active tab's renderer runs, the Live tab attaches nothing until it
      // is opened, and every renderer reads state the board already holds --
      // opening the inspector issues no request of its own.

      /** @type {string|null} Branch id the inspector is bound to, or null for none. */
      let inspectorBranchId = null;
      /** @type {string} Which inspector tab is showing: overview | worktree | feedback | live. */
      let inspectorTab = "overview";
      /** @type {{[branchId: string]: boolean}} Branch ids whose Live view has been opened, and so attached. */
      const inspectorLiveAttached = {};
      /** @type {boolean} True while the log dock is expanded. */
      let reviewDockOpen = false;
      /**
       * What the log dock is scoped to: "review", or a branch id. Sticky pins
       * it; off, it follows the selection. Deliberately a plain variable and
       * never persisted -- a refresh always comes back to following.
       * @type {string}
       */
      let reviewDockScope = "review";
      /** @type {string} The command text when the dock is scoped to one command, for its header. */
      let reviewDockCommand = "";
      /** @type {boolean} When true the dock ignores selection changes. */
      let reviewDockSticky = false;

      /**
       * The branch the inspector is showing, or null when nothing is selected
       * or the selection does not belong to the open review.
       * @param {GuardianView} g - The open review.
       * @returns {GuardianBranch|null}
       */
      function inspectorBranch(g) {
        if (!g || !g.branches) return null;
        const b = g.branches.find((x) => x.id === inspectorBranchId);
        return b || null;
      }
      /**
       * Binds the inspector (and, unless pinned, the log dock) to one branch.
       * Called by the branch row's own click handler, so one selection drives
       * every pane.
       * @param {string} branchId - The branch to bind to.
       * @returns {void}
       */
      function selectInspectorBranch(branchId) {
        inspectorBranchId = branchId;
        if (!reviewDockSticky) reviewDockScope = branchId;
        // Switching branches while watching is still watching: the newly
        // selected branch attaches too, rather than showing a collapsed pane
        // that has to be opened again for every branch in the stack.
        if (inspectorTab === "live" && !attachInspectorLive(branchId)) renderReviewInspector();
        else if (inspectorTab !== "live") renderReviewInspector();
        renderReviewDock();
      }
      /**
       * Attaches one branch's live pane and starts its first fetch. Returns
       * whether it did the attaching (and so already re-rendered), so callers
       * don't render twice.
       * @param {string} branchId - The branch to attach.
       * @returns {boolean}
       */
      function attachInspectorLive(branchId) {
        inspectorLiveAttached[branchId] = true;
        const g = guardians.find((x) => x.id === selectedGuardian);
        if (!g) return false;
        const key = `guardian|${g.id}|${branchId}`;
        if (peekOpen[key]) return false;
        peekOpen[key] = true;
        renderReviewInspector();
        fetchPeek(key, true);
        return true;
      }
      /**
       * Switches inspector tabs. Opening Live attaches that branch's stream --
       * the one place this pane costs anything, and only on an explicit open.
       * @param {string} tab - overview | worktree | feedback | live.
       * @returns {void}
       */
      function setInspectorTab(tab) {
        inspectorTab = tab;
        // Opening the Live tab IS the request to watch, so it attaches and
        // shows the pane -- a second "Show Live View" click bought nothing,
        // since nothing had loaded before the tab was opened either way.
        // Opening the Live tab IS the request to watch, so it attaches and
        // shows the pane -- a second "Show Live View" click bought nothing,
        // since nothing had loaded before the tab was opened either way.
        const bid = inspectorBranchId;
        if (tab === "live" && bid && attachInspectorLive(bid)) return;
        renderReviewInspector();
      }
      /**
       * Toggles the log dock open/closed. The first open is what loads its
       * events -- a dock nobody opened issues no request.
       * @returns {void}
       */
      function toggleReviewDock() {
        reviewDockOpen = !reviewDockOpen;
        renderReviewDock();
        if (reviewDockOpen && selectedGuardian && reviewDockEvents[selectedGuardian] === undefined) {
          loadReviewDockEvents(selectedGuardian);
        }
      }
      /** Toggles whether the dock ignores selection changes. @returns {void} */
      function toggleReviewDockSticky() {
        reviewDockSticky = !reviewDockSticky;
        renderReviewDock();
      }

      /**
       * Renders the inspector pane for the open review's selected branch.
       * @returns {void}
       */
      function renderReviewInspector() {
        const host = document.getElementById("review-inspector");
        if (!host) return;
        const g = guardians.find((x) => x.id === selectedGuardian);
        if (!g || !g.branches) {
          host.innerHTML = `<div class="empty">Select a review.</div>`;
          return;
        }
        const b = inspectorBranch(g);
        if (!b) {
          host.innerHTML = `<div class="empty" data-tip="Click a branch in the stack to inspect it here — its worktree, its PR, the feedback thread, and its live resolver session.">Select a branch in the stack.</div>`;
          return;
        }
        const unread = branchHasUnansweredFeedback(g, b);
        const tabs = [
          ["overview", "Overview", "What this branch is, where it came from, and where it sits in the stack."],
          ["worktree", "Worktree", "The on-disk worktree for this branch — its path, its environment layer, and what it retires with."],
          ["feedback", "Feedback", "Your conversation with the resolver agent about this branch.\nWhat you write here is routed into the branch's worktree as a change request."],
          ["live", "Live", "The resolver agent's terminal for this branch.\nNothing attaches until you open this tab."],
        ];
        host.innerHTML = `
          <div class="insp-head">
            <div class="insp-title">
              ${gdot(b.merge_status || "pending")}
              <span class="t mono hc-anchor" data-card="gBranch" data-guardian-id="${esc(g.id)}" data-branch-id="${esc(b.id)}">${esc(b.branch)}</span>
              ${pill(b.merge_status || "pending")}
            </div>
            <div class="insp-tabs">${tabs.map(([k, label, tip]) =>
              `<button class="insp-tab${inspectorTab === k ? " on" : ""}" data-click="setInspectorTab" data-tab="${k}" data-tip="${esc(tip)}">${label}${
                k === "feedback" && unread ? `<span class="insp-badge" data-tip="You left feedback the resolver has not answered yet.">1</span>` : ""}</button>`).join("")}</div>
          </div>
          <div class="insp-body">${inspectorTabBody(g, b)}</div>`;
      }
      /**
       * Body for whichever inspector tab is active. Only this one runs, so a
       * tab nobody opened costs nothing.
       * @param {GuardianView} g - The review.
       * @param {GuardianBranch} b - The selected branch.
       * @returns {string}
       */
      function inspectorTabBody(g, b) {
        if (inspectorTab === "worktree") return inspectorWorktreeTab(g, b);
        if (inspectorTab === "feedback") return inspectorFeedbackTab(g, b);
        if (inspectorTab === "live") return inspectorLiveTab(g, b);
        return inspectorOverviewTab(g, b);
      }
      /**
       * Overview: identity, stack position, source, and PR status.
       * @param {GuardianView} g - The review.
       * @param {GuardianBranch} b - The selected branch.
       * @returns {string}
       */
      function inspectorOverviewTab(g, b) {
        const place = hcStackPlace(g, b);
        const src = b.source_squad_id
          ? `${esc(b.source_squad_id)}${b.source_cell_idx === undefined ? "" : ` · cell ${b.source_cell_idx}`}`
          : "—";
        const prs = (pullRequests[g.id] || []).filter((p) => p.branch_id === b.id);
        return `<dl class="insp-kv">
            <dt>state</dt><dd>${pill(b.merge_status || "pending")}</dd>
            <dt>position</dt><dd>${b.enabled === false ? "not in the stack" : `${place.place} of ${place.total}`}</dd>
            <dt>rebases onto</dt><dd class="mono">${esc(place.onto)}</dd>
            <dt>source</dt><dd class="mono">${src}</dd>
            <dt>detail</dt><dd style="color:var(--muted)">${esc(b.detail || "—")}</dd>
          </dl>
          ${b.is_empty ? `<div class="warn" style="margin-top:10px">Adds no diff over the branch beneath it, which fails the review.</div>` : ""}
          <h3 class="section" style="margin-top:16px">pull request</h3>
          ${prs.length
            ? prs.map((p) => `<div class="insp-pr">
                <div class="insp-pr-head">${branchPrBadge(p, prs.length > 1)}<span class="insp-pr-title">${esc(p.title || "")}</span></div>
                <div class="insp-pr-sub">base <span class="mono">${esc(p.base_ref || "—")}</span> · ${esc(p.state)}</div>
              </div>`).join("")
            : `<div class="empty" style="padding:8px 0" data-tip="Submitting folds this branch into the review's existing PR stack — it is never submitted on its own, which is what keeps the stack's base chain intact.">Not submitted yet.</div>`}`;
      }
      /**
       * Worktree: the on-disk tree for this branch and its own env layer.
       * @param {GuardianView} g - The review.
       * @param {GuardianBranch} b - The selected branch.
       * @returns {string}
       */
      function inspectorWorktreeTab(g, b) {
        if (!b.worktree) {
          return `<div class="empty" data-tip="A per-branch worktree is created when the branch starts rebasing. With 'skip per-branch worktrees' on, the whole stack builds in one shared worktree instead.">No worktree yet — one is created when this branch starts rebasing.</div>`;
        }
        const own = b.env_overrides || {};
        const keys = Object.keys(own);
        return `<div class="hc-path">${esc(b.worktree)}</div>
          <dl class="insp-kv">
            <dt>branch</dt><dd class="mono">${esc(b.branch)}</dd>
            <dt>conflicts</dt><dd>${(b.conflicts_found || 0) > 0 ? `${b.conflicts_fixed || 0} fixed of ${b.conflicts_found} found` : "none"}</dd>
          </dl>
          <div class="btn-row" style="margin-top:10px">
            <button class="btn" data-tip="Copy this worktree's absolute path." data-copy="${esc(b.worktree)}" onclick="copyText(event)">Copy path</button>
            ${worktreeCellBtn(b, `${g.id}:${b.id}`)}
          </div>
          <h3 class="section" style="margin-top:16px">environment</h3>
          ${keys.length
            ? `<dl class="insp-kv">${keys.sort().map((k) => `<dt class="mono">${esc(k)}</dt><dd class="mono">${
                own[k] === null
                  ? `<span style="color:var(--failed);text-decoration:line-through" data-tip="Tombstoned — this key is removed from the effective environment even though it would otherwise be inherited.">removed</span>`
                  : esc(own[k] || "")}</dd>`).join("")}</dl>`
            : `<div class="empty" style="padding:8px 0">Inherits the daemon environment — nothing overridden for this worktree.</div>`}
          <div class="btn-row" style="margin-top:8px">${envViewerBtn(`/api/guardians/${g.id}/branches/${b.id}/env`, `this branch's worktree`)}</div>`;
      }
      /**
       * Feedback: the branch's reviewer thread, promoted out of the nested
       * disclosure it used to live in.
       * @param {GuardianView} g - The review.
       * @param {GuardianBranch} b - The selected branch.
       * @returns {string}
       */
      function inspectorFeedbackTab(g, b) {
        const key = `${g.id}:${b.id}`;
        const sending = feedbackSending.has(key);
        return `<div class="hint" style="margin:0 0 10px">What you write here is routed into
            <span class="mono">${esc(b.branch)}</span>'s worktree as a change request. The resolver
            agent amends the branch and replies.</div>
          ${branchFeedbackSection(g, b)}
          <div class="fb-composer">
            <textarea id="fb-input-${esc(b.id)}" rows="3" ${sending ? "disabled" : ""}
              placeholder="Ask for a change on ${esc(b.branch)}…"
              data-tip="Describe the change you want on this branch. The resolver agent edits it in its own worktree, amends the commit, and replies in this thread.\nCtrl+Enter sends."></textarea>
            <div class="btn-row" style="margin-top:6px">
              <button class="btn primary" ${sending ? "disabled" : ""} data-click="sendBranchFeedback"
                data-guardian-id="${esc(g.id)}" data-branch-id="${esc(b.id)}"
                data-tip="Send this to the resolver agent.\nIt is recorded on the branch's thread immediately; the agent's reply appears here when it finishes.">${sending ? "Sending…" : "Send"}</button>
            </div>
          </div>`;
      }
      /** @type {Set<string>} Branch keys with a feedback post in flight, so Send cannot be double-fired. */
      const feedbackSending = new Set();
      /**
       * Posts a change request onto one branch's feedback thread. Until now the
       * board could only *read* this thread (RAL-272) -- giving feedback meant
       * leaving for the CLI, which is a strange gap on the surface whose whole
       * job is reviewing.
       * @param {string} gid - The review id.
       * @param {string} bid - The branch id.
       * @returns {Promise<void>}
       */
      async function sendBranchFeedback(gid, bid) {
        const el = /** @type {HTMLTextAreaElement|null} */ (document.getElementById(`fb-input-${bid}`));
        const text = (el && el.value || "").trim();
        if (!text) { notify("warn", "Write the change you want before sending."); return; }
        const key = `${gid}:${bid}`;
        if (feedbackSending.has(key)) return;
        feedbackSending.add(key);
        renderReviewInspector();
        try {
          const r = await fetch(`/api/guardians/${gid}/branches/${bid}/feedback`, {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ feedback: text }),
          });
          if (!r.ok) {
            const detail = await r.text();
            notify("error", `Feedback was not sent: ${detail.slice(0, 200)}`);
            return;
          }
          if (el) el.value = "";
          notify("success", "Feedback sent to the resolver agent.");
          // Re-read the thread so the message appears without a manual refresh.
          loadBranchMessages(gid, bid);
        } catch (e) {
          notify("error", `Feedback was not sent: ${e}`);
        } finally {
          feedbackSending.delete(key);
          renderReviewInspector();
        }
      }
      /**
       * Live: the resolver's terminal for this branch. Nothing attaches until
       * this tab is opened -- a branch row never renders a terminal, so the
       * cost is paid once, on the click.
       * @param {GuardianView} g - The review.
       * @param {GuardianBranch} b - The selected branch.
       * @returns {string}
       */
      function inspectorLiveTab(g, b) {
        if (!inspectorLiveAttached[b.id]) {
          return `<div class="empty" data-tip="Opening this tab attaches to the branch's resolver session. Nothing streams until then.">Not attached.</div>`;
        }
        if (!b.worktree) {
          return `<div class="empty">No worktree yet — a resolver session starts when this branch begins rebasing.</div>`;
        }
        const key = `guardian|${g.id}|${b.id}`;
        // The attempt list is what makes walking back possible, and it is only
        // fetched once this tab is open -- see the lazy-by-default rule.
        if (historyAttempts[key] === undefined) fetchHistoryList(key);
        return `${liveRunStepper(key)}
          <div class="btn-row" style="position:relative;gap:0">${resolverTerminalBtns(g, b)}</div>
          ${resolverPeekBox(g, b)}
          <div class="hint">Read-only — nothing typed here reaches the agent. Every attempt's text is
          captured separately, so walking back survives a restart.</div>`;
      }
      /**
       * The walk-back stepper over a branch's persisted resolver attempts.
       *
       * The durable attempt history (RAL-154) was only reachable as a list you
       * opened and closed, which makes "what was it doing two attempts ago" a
       * navigation exercise. Stepping one attempt at a time is the gesture that
       * question actually wants, with the list still there behind the picker.
       * @param {string} key - The peek key, `guardian|<gid>|<branchId>`.
       * @returns {string}
       */
      function liveRunStepper(key) {
        const attempts = historyAttempts[key];
        if (attempts === undefined) {
          return `<div class="run-step"><span class="rs-label">Loading attempts…</span></div>`;
        }
        if (!attempts.length) {
          return `<div class="run-step" data-tip="A durable log is written once this branch's first resolver attempt finishes. Until then there is only the live pane below.">
              <span class="rs-label">Live session only — no earlier attempts yet</span>
            </div>`;
        }
        // Newest last, matching how the attempts are numbered.
        const ordered = attempts.slice().sort((x, y) => x.attempt - y.attempt);
        const viewing = historyViewing[key];
        const curIdx = viewing
          ? Math.max(0, ordered.findIndex((a) => a.attempt === viewing.attempt))
          : ordered.length - 1;
        const cur = ordered[curIdx];
        const atLatest = curIdx === ordered.length - 1 && !viewing;
        const when = cur && cur.modified_ms
          ? new Date(cur.modified_ms).toLocaleTimeString([], { hour: "2-digit", minute: "2-digit" })
          : "";
        return `<div class="run-step${atLatest ? "" : " past"}">
            <button class="rs-nav" data-click="stepLiveAttempt" data-key="${esc(key)}" data-dir="-1" ${curIdx <= 0 ? "disabled" : ""}
              data-tip="Step back to the previous resolver attempt on this branch.">&#9664;</button>
            <button class="rs-pick" data-click="toggleHistory" data-key="${esc(key)}"
              data-tip="Every persisted attempt for this branch, including each reattach.\nPick one to read its full log.">
              <span class="rs-kind">attempt</span>
              <span class="rs-label mono">${cur ? cur.attempt : "?"}${cur && cur.attempt === 0 ? " (initial)" : ""}</span>
              <span class="rs-meta">${esc(when)}</span>
              <span class="rs-count num">${curIdx + 1}/${ordered.length}</span> &#9662;</button>
            <button class="rs-nav" data-click="stepLiveAttempt" data-key="${esc(key)}" data-dir="1" ${curIdx >= ordered.length - 1 ? "disabled" : ""}
              data-tip="Step forward to the next resolver attempt on this branch.">&#9654;</button>
            ${atLatest ? "" : `<button class="btn" data-click="closeHistoryAttempt" data-key="${esc(key)}"
              data-tip="Jump back to the live pane — the newest attempt, still streaming.">&#8677; Latest</button>`}
          </div>
          ${atLatest ? "" : `<div class="histnote">Historical record — read-only. You are reading attempt ${cur ? cur.attempt : "?"}, not the live session.</div>`}`;
      }
      /**
       * Steps the Live tab one resolver attempt backwards or forwards.
       * @param {string} key - The peek key.
       * @param {number} dir - -1 for the previous attempt, 1 for the next.
       * @returns {void}
       */
      function stepLiveAttempt(key, dir) {
        const attempts = (historyAttempts[key] || []).slice().sort((x, y) => x.attempt - y.attempt);
        if (!attempts.length) return;
        const viewing = historyViewing[key];
        const curIdx = viewing
          ? Math.max(0, attempts.findIndex((a) => a.attempt === viewing.attempt))
          : attempts.length - 1;
        const next = Math.min(attempts.length - 1, Math.max(0, curIdx + dir));
        if (next === attempts.length - 1 && dir > 0 && !viewing) return;
        viewHistoryAttempt(key, attempts[next].attempt);
      }

      /**
       * Whether this branch's feedback thread ends on a reviewer message the
       * resolver has not answered -- the signal the inspector's Feedback tab
       * badges, so an open question is visible without opening the tab.
       * @param {GuardianView} g - The review.
       * @param {GuardianBranch} b - The branch.
       * @returns {boolean}
       */
      function branchHasUnansweredFeedback(g, b) {
        const msgs = branchMessages[`${g.id}:${b.id}`];
        if (!msgs || !msgs.length) return false;
        return msgs[msgs.length - 1].role === "reviewer";
      }

      /**
       * Renders the log dock: the open review's audit log, scoped to whatever
       * is selected unless pinned.
       * @returns {void}
       */
      function renderReviewDock() {
        const dock = document.getElementById("review-log-dock");
        if (!dock) return;
        dock.className = `log-dock ${reviewDockOpen ? "open" : "closed"}`;
        const caret = document.getElementById("review-dock-caret");
        if (caret) caret.innerHTML = reviewDockOpen ? "&#9660;" : "&#9650;";
        const sticky = document.getElementById("review-dock-sticky");
        if (sticky) {
          sticky.className = `tgl${reviewDockSticky ? " on" : ""}`;
          sticky.setAttribute("aria-pressed", reviewDockSticky ? "true" : "false");
        }
        const g = guardians.find((x) => x.id === selectedGuardian);
        const b = (g && g.branches ? g.branches.find((x) => x.id === reviewDockScope) : null) || null;
        const scopeEl = document.getElementById("review-dock-scope");
        if (scopeEl) {
          // Three kinds of scope: the whole review, one branch, or one command.
          const cmdScoped = !b && reviewDockScope !== "review" && !!reviewDockCommand;
          scopeEl.textContent = b
            ? b.branch
            : (cmdScoped
              ? (reviewDockCommand.length > 40 ? `${reviewDockCommand.slice(0, 40)}…` : reviewDockCommand)
              : "whole review");
        }
        const hint = document.getElementById("review-dock-hint");
        if (hint) {
          hint.textContent = !reviewDockOpen
            ? "click to open"
            : (reviewDockSticky ? "pinned — ignoring selection" : "following selection");
        }
        const body = document.getElementById("review-dock-body");
        if (!body) return;
        if (!reviewDockOpen) { body.innerHTML = ""; return; }
        if (!g) { body.innerHTML = `<div class="empty">Select a review.</div>`; return; }
        // Reuses the rows the review already polls for its own header, so
        // opening the dock costs no request of its own.
        const rows = (reviewDockRows(g, b) || []);
        body.innerHTML = rows.length
          ? rows.map((r) => `<div class="dock-row ${esc(r.kind)}"><span class="t">${esc(r.at)}</span>`
            + `<span class="s">${esc(r.source)}</span><span class="m">${esc(r.message)}</span></div>`).join("")
          : `<div class="empty">Nothing logged for this scope yet.</div>`;
      }
      /**
       * The dock's rows for one scope, drawn from the Cartographer events the
       * board has already loaded for this review.
       * @param {GuardianView} g - The review.
       * @param {GuardianBranch|null} b - The branch to scope to, or null for the whole review.
       * @returns {{at: string, source: string, kind: string, message: string}[]}
       */
      function reviewDockRows(g, b) {
        const raw = reviewDockEvents[g.id] || [];
        /**
         * @param {CartographerRow} r - One Cartographer event.
         * @returns {{at: string, source: string, kind: string, message: string}}
         */
        const shape = (r) => ({
          at: fmtDockTime(r.at_ms),
          source: r.source || "",
          kind: dockKindFor(r.level || ""),
          message: r.message || "",
        });
        if (!b) return raw.map(shape);
        // Most of a review's events name the review, not the branch, so a
        // branch scope legitimately matches nothing. Showing an empty drawer
        // over 38 unshown rows reads as "no logs"; fall back to the whole
        // review and say that is what happened.
        const mine = raw.filter((r) => (r.message || "").includes(b.branch));
        if (mine.length) return mine.map(shape);
        return [{
          at: "—",
          source: "scope",
          kind: "info",
          message: `No events name ${b.branch} — showing the whole review instead.`,
        }].concat(raw.map(shape));
      }
      /**
       * Maps a Cartographer level onto the dock's row styling.
       * @param {string} level - The event's level.
       * @returns {string}
       */
      function dockKindFor(level) {
        const l = (level || "").toUpperCase();
        if (l === "ERROR") return "err";
        if (l === "WARNING" || l === "WARN") return "warn";
        return "info";
      }
      /**
       * Formats an epoch-ms timestamp as the dock's clock column.
       * @param {number|null|undefined} ms - Epoch milliseconds.
       * @returns {string}
       */
      function fmtDockTime(ms) {
        if (!ms) return "—";
        const d = new Date(ms);
        const p = (/** @type {number} */ n) => String(n).padStart(2, "0");
        return `${p(d.getHours())}:${p(d.getMinutes())}:${p(d.getSeconds())}`;
      }

      // ---- Review list quick filters ----
      //
      // The sidebar opened with five filter controls stacked above the list --
      // a status popover, an agent popover, two origin checkboxes, a PR-status
      // popover and a "show hidden" box -- which is a lot of chrome to read
      // before reaching the reviews themselves. Most of the time the question
      // is simply "what is still live, and what is waiting on me", so those
      // two become one-press presets and the full set folds behind a
      // disclosure. Nothing is removed: the presets write the same
      // `reviewFilters.status` set the popovers do.

      /** @type {{[preset: string]: {label: string, states: string[], tip: string}}} */
      const REVIEW_QUICK_FILTERS = {
        active: {
          label: "Active",
          states: ["collecting", "merging", "merge_failed", "merge_stopped", "in_review"],
          tip: "Reviews you can still act on: collecting, rebasing, stalled, or waiting on review.",
        },
        needs: {
          label: "Needs you",
          states: ["in_review", "merge_failed", "merge_stopped"],
          tip: "Reviews waiting on a human decision — ready to approve, or stalled and needing a call.",
        },
        closed: {
          label: "Closed",
          states: ["merged", "approved", "cancelled", "deployed"],
          tip: "Reviews that are finished one way or another.",
        },
      };
      /** @type {boolean} Whether the full filter set is disclosed. */
      let reviewFiltersExpanded = false;

      /**
       * Which preset the current status set matches exactly, or "" when the
       * selection is a custom one made through the full filter controls.
       * @returns {string}
       */
      function activeReviewQuickFilter() {
        const cur = [...reviewFilters.status].sort().join(",");
        return Object.keys(REVIEW_QUICK_FILTERS).find(
          (k) => REVIEW_QUICK_FILTERS[k].states.slice().sort().join(",") === cur,
        ) || "";
      }
      /**
       * Applies one quick-filter preset to the review list. Pressing the active
       * preset again clears back to everything, so the control is never a trap.
       * @param {string} preset - A key of {@link REVIEW_QUICK_FILTERS}.
       * @returns {void}
       */
      function setReviewQuickFilter(preset) {
        const def = REVIEW_QUICK_FILTERS[preset];
        if (!def) return;
        reviewFilters.status = activeReviewQuickFilter() === preset
          ? new Set(GUARDIAN_STATES)
          : new Set(def.states);
        renderReviewQuickFilters();
        renderReviewStatusFilters();
        renderReviews();
        syncHash();
      }
      /** Shows or hides the full filter controls. @returns {void} */
      function toggleReviewFilters() {
        reviewFiltersExpanded = !reviewFiltersExpanded;
        renderReviewQuickFilters();
      }
      /**
       * Renders the quick-filter segmented control and the disclosure that
       * hides the full filter set.
       * @returns {void}
       */
      function renderReviewQuickFilters() {
        const host = document.getElementById("review-quick-filters");
        if (!host) return;
        const active = activeReviewQuickFilter();
        // Counts come from the already-loaded list, so the control costs no
        // request -- a lean index entry carries the status this keys on.
        const counts = Object.fromEntries(Object.keys(REVIEW_QUICK_FILTERS).map((k) => [
          k,
          guardians.filter((g) => REVIEW_QUICK_FILTERS[k].states.includes(g.status)
            && (reviewFilters.showHidden || !hiddenGuardianIds.has(g.id))).length,
        ]));
        host.innerHTML = `<div class="quick-seg">${Object.keys(REVIEW_QUICK_FILTERS).map((k) =>
            `<button class="${active === k ? "on" : ""}" data-click="setReviewQuickFilter" data-preset="${k}" `
            + `data-tip="${esc(REVIEW_QUICK_FILTERS[k].tip)}\nPress again to clear back to every status.">`
            + `${REVIEW_QUICK_FILTERS[k].label}<span class="n num">${counts[k]}</span></button>`).join("")}</div>
          <button class="filters-more" data-click="toggleReviewFilters" data-tip="The full status, agent, origin and PR-status filters.\nFolded away by default so the sidebar belongs to the review list rather than its controls.">${
            reviewFiltersExpanded ? "− Fewer filters" : "+ More filters"}${
            // "custom" means a selection the presets cannot express -- not
            // simply "everything", which is the default and the state pressing
            // an active preset returns you to.
            (active || reviewFilters.status.size === GUARDIAN_STATES.length)
              ? "" : ` <span class="count-badge num">custom</span>`}</button>`;
        const full = document.getElementById("review-full-filters");
        if (full) full.style.display = reviewFiltersExpanded ? "" : "none";
      }

      // ---- Section menus ----
      //
      // Every section of a review carries the same ⋯ menu, so "show me the log
      // for this" is one gesture in a consistent place rather than a chip
      // repeated beside each heading. The log drawer is the shared destination.

      /** @type {{[kind: string]: {label: string, note: string}}} What each section's menu is about. */
      const REVIEW_SECTION_MENUS = {
        summary: { label: "change summary", note: "Written from git log as branches become ready, then replaced by an agent-written summary." },
        gates: { label: "check gates", note: "Run after each merge commit and on the combined worktree. All must pass before approval." },
        branches: { label: "branch stack", note: "The rebase stack, in order. Each branch rebases onto the one above it." },
        actions: { label: "test actions", note: "Declared by the task author in [[review.action]] blocks." },
        manual: { label: "manual checks", note: "Written by the resolver agent against this stack's changes. Advisory — they never block approval." },
      };
      /**
       * Which env scope each runnable section edits, and which resolved-env
       * endpoint describes what that section actually ends up running in.
       *
       * `edit` is the writable scope; `resolvedUrl` is the read-only resolution
       * of it for this section. They differ for check gates: the daemon composes
       * the gates' environment from the daemon's own plus the build step's
       * overrides, so the thing you edit is `build` while the thing you read
       * back is `tests-env`.
       * @type {{[kind: string]: {edit: string, resolvedUrl: string, label: string, tip: string}}}
       */
      const ENV_SCOPE_FOR_SECTION = {
        gates: {
          edit: "build",
          resolvedUrl: "tests-env",
          label: "check gates",
          tip: "Edit the environment the check gates run in.\nThe daemon composes it from its own environment plus the build step's overrides, so this edits the build step's — shared with test actions.",
        },
        manual: {
          edit: "manual_checks",
          resolvedUrl: "manual-checks-env",
          label: "manual checks",
          tip: "Edit the environment overrides applied when the suggested manual checks run.\nThis scope is the manual-checks step's own — nothing else in the review uses it.",
        },
        actions: {
          edit: "build",
          resolvedUrl: "build-env",
          label: "test actions",
          tip: "Edit the environment overrides for the build step, which is what test actions run against.\nShared with the check gates, which resolve from the same layer.",
        },
      };
      /**
       * Opens the environment-override editor for one section's writable scope.
       * @param {string} gid - The review id.
       * @param {string} kind - Which section's scope to open.
       * @returns {void}
       */
      function openSectionEnv(gid, kind) {
        closeSquadMenu();
        const meta = ENV_SCOPE_FOR_SECTION[kind];
        if (!meta) return;
        void openEnvOverridesEditor(gid, meta.edit, "", `/api/guardians/${gid}/${meta.resolvedUrl}`);
      }
      /**
       * The ⋯ button for one section heading.
       * @param {string} gid - The review id.
       * @param {string} kind - Which section, keyed into {@link REVIEW_SECTION_MENUS}.
       * @returns {string}
       */
      function sectionMenuBtn(gid, kind) {
        const meta = REVIEW_SECTION_MENUS[kind];
        if (!meta) return "";
        return `<button class="section-menu" data-click="openReviewSectionMenu" data-guardian-id="${esc(gid)}" `
          + `data-kind="${esc(kind)}" data-tip="Actions for the ${esc(meta.label)} section — including its logs.">⋯</button>`;
      }
      /**
       * Opens one section's ⋯ menu.
       * @param {MouseEvent} e - The click that opened it.
       * @param {string} gid - The review id.
       * @param {string} kind - Which section.
       * @returns {void}
       */
      function openReviewSectionMenu(e, gid, kind) {
        e.preventDefault(); e.stopPropagation(); closeSquadMenu();
        const meta = REVIEW_SECTION_MENUS[kind];
        if (!meta) return;
        const menu = document.createElement("div");
        menu.className = "ctx-menu"; menu.id = "squad-menu";
        const items = [
          `<div data-click="scopeReviewDockToSection" data-guardian-id="${esc(gid)}" data-kind="${esc(kind)}" data-tip="Open the log drawer for this review.\n${esc(meta.note)}">☰ Logs</div>`,
        ];
        // Environment lives with the thing it applies to. Each runnable
        // section edits its own scope from here, rather than every scope being
        // piled into the review's settings modal where you had to work out
        // which one governed what you were looking at.
        if (ENV_SCOPE_FOR_SECTION[kind]) {
          items.push(`<div data-click="openSectionEnv" data-guardian-id="${esc(gid)}" data-kind="${esc(kind)}" data-tip="${esc(ENV_SCOPE_FOR_SECTION[kind].tip)}">⚙ Environment overrides…</div>`);
        }
        if (kind === "manual") {
          items.push(`<div data-click="runAllManualChecks" data-guardian-id="${esc(gid)}" data-tip="Run every suggested manual check, each in the built review worktree.">▶ Run all</div>`);
          items.push(`<div data-click="regenManualChecks" data-guardian-id="${esc(gid)}" data-tip="Ask the resolver agent to write these checks again against the stack's current changes.\nRuns in the background and never blocks Approve or Merge / rebase.">↻ Regenerate</div>`);
        }
        if (kind === "gates") {
          items.push(`<div data-click="openEditReviewDetails" data-guardian-id="${esc(gid)}" data-focus="gates" data-tip="Open review setup on the build settings that decide whether gates run at all.\nThe gate commands themselves come from the project's review settings, not from here.">✎ Build settings…</div>`);
        }
        menu.innerHTML = items.join("");
        document.body.appendChild(menu);
        menu.style.left = Math.min(e.clientX, window.innerWidth - 200) + "px";
        menu.style.top = Math.min(e.clientY, window.innerHeight - 120) + "px";
      }
      /**
       * Opens one branch's ⋯ menu. Gathers everything that acts on a single
       * branch or its worktree, which previously sat on the row as bare glyphs
       * (⇄ to move it) or not at all (its env layer, its logs).
       * @param {MouseEvent} e - The click that opened it.
       * @param {string} gid - The review id.
       * @param {string} bid - The branch id.
       * @param {boolean} canMove - Whether moving this branch to another review is currently allowed.
       * @returns {void}
       */
      function openReviewBranchMenu(e, gid, bid, canMove) {
        e.preventDefault(); e.stopPropagation(); closeSquadMenu();
        const g = guardians.find((x) => x.id === gid);
        const b = g && g.branches ? g.branches.find((x) => x.id === bid) : null;
        if (!b) return;
        const menu = document.createElement("div");
        menu.className = "ctx-menu"; menu.id = "squad-menu";
        /** @type {string[]} */
        const items = [];
        items.push(`<div data-click="inspectBranchFromMenu" data-branch-id="${esc(bid)}" data-tab="overview" data-tip="Open this branch in the inspector pane.">◉ Inspect branch</div>`);
        items.push(`<div data-click="scopeReviewDockToBranch" data-guardian-id="${esc(gid)}" data-branch-id="${esc(bid)}" data-tip="Open the log drawer scoped to this branch.">☰ Logs</div>`);
        if (b.worktree) {
          items.push(`<div class="sep"></div>`);
          items.push(`<div data-click="inspectBranchFromMenu" data-branch-id="${esc(bid)}" data-tab="worktree" data-tip="Show this branch's worktree — its path, its head, and what changed in it.">🗂 Worktree</div>`);
          items.push(`<div data-click="openEnvOverridesEditor" data-guardian-id="${esc(gid)}" data-scope="branch" data-branch-id="${esc(bid)}" data-tip="Edit the environment overrides that apply to this one worktree.\nEvery other branch in the stack keeps its own; this is the layer on top of what the review already inherits.">⚙ Environment overrides…</div>`);
          items.push(`<div data-copy="${esc(b.worktree)}" onclick="copyText(event)" data-tip="Copy this worktree's absolute path.">⧉ Copy worktree path</div>`);
          items.push(`<div data-click="inspectBranchFromMenu" data-branch-id="${esc(bid)}" data-tab="live" data-tip="Watch this branch's resolver session. Nothing attaches until you open it.">▶ Live view</div>`);
        }
        if (canMove) {
          items.push(`<div class="sep"></div>`);
          items.push(`<div data-click="openMoveBranchMenu" data-guardian-id="${esc(gid)}" data-branch-id="${esc(bid)}" data-tip="Move this branch to a different review.\nWho/when: a change needs to ship independently of the review it started in — this review is stalled but this branch is ready, or another review needs just this branch.\nBoth reviews rebuild afterward.\nThis cannot be undone.">⇄ Move to another review…</div>`);
        }
        menu.innerHTML = items.join("");
        document.body.appendChild(menu);
        menu.style.left = Math.min(e.clientX, window.innerWidth - 230) + "px";
        menu.style.top = Math.min(e.clientY, window.innerHeight - 190) + "px";
      }
      /**
       * Selects a branch into the inspector on a chosen tab, from its ⋯ menu.
       * @param {string} bid - The branch id.
       * @param {string} tab - Which inspector tab to open.
       * @returns {void}
       */
      function inspectBranchFromMenu(bid, tab) {
        closeSquadMenu();
        inspectorBranchId = bid;
        if (!reviewDockSticky) reviewDockScope = bid;
        setInspectorTab(tab);
        renderReviewDock();
      }
      /**
       * Opens the log drawer scoped to one branch, from its ⋯ menu. An explicit
       * ask beats the pin, which exists to stop *incidental* scope changes.
       * @param {string} gid - The review id.
       * @param {string} bid - The branch id.
       * @returns {void}
       */
      function scopeReviewDockToBranch(gid, bid) {
        closeSquadMenu();
        reviewDockScope = bid;
        if (!reviewDockOpen) toggleReviewDock();
        else renderReviewDock();
        if (reviewDockEvents[gid] === undefined) loadReviewDockEvents(gid);
      }

      // ---- Command rows as selections ----
      //
      // A command row is a selection in the same sense a branch row is: picking
      // one scopes the log drawer to it. That makes "what did this command
      // actually do" the same gesture as "what did this branch actually do",
      // instead of a different control in a different place.

      /**
       * Per-command run state, keyed by `<gid>:<kind>:<index>`.
       *
       * The daemon launches a command into a terminal and reports nothing back
       * about how it ended -- `GuardianCheck` carries no exit code, status or
       * duration, and there is no per-command result endpoint. So this tracks
       * what the board genuinely knows: that you asked for it, and how long ago.
       * It deliberately never claims "pass": inventing an outcome the backend
       * never sent would be worse than admitting the gap. Surfacing a real
       * pass/fail needs the daemon to record the tmux pane's RALPHUS_TMUX_DONE
       * result per command.
       * @type {{[key: string]: {state: string, startedMs: number, endedMs: number}}}
       */
      const commandRuns = {};
      /**
       * Marks one command as launched, so its row can show it is running and
       * for how long.
       * @param {string} key - The command's key.
       * @returns {void}
       */
      function markCommandRunning(key) {
        commandRuns[key] = { state: "running", startedMs: Date.now(), endedMs: 0 };
        renderReviewDetail();
      }
      /**
       * One command row's status chip and elapsed time.
       * @param {string} key - The command's key.
       * @returns {string}
       */
      function commandRunStatus(key) {
        const r = commandRuns[key];
        if (!r) {
          return `<span class="cmd-status idle" data-tip="Not run from the board this session.\nThe daemon does not report a per-command result, so this only reflects runs you started here.">idle</span>`;
        }
        const secs = Math.max(0, Math.round(((r.endedMs || Date.now()) - r.startedMs) / 1000));
        const elapsed = secs >= 60 ? `${Math.floor(secs / 60)}m ${secs % 60}s` : `${secs}s`;
        return `<span class="cmd-dur num" data-tip="How long ago this run was launched from the board.">${elapsed}</span>`
          + `<span class="cmd-status running" data-tip="Launched in a terminal from the board.\nThe daemon reports no pass/fail for an individual command, so this cannot turn green on its own — open the command's logs to see how it ended.">running</span>`;
      }

      /** @type {string} The selected command's key, or "" for none. */
      let selectedCommandKey = "";
      /** @type {{[key: string]: boolean}} Command keys whose full text is expanded under the row. */
      const commandFullOpen = {};

      /**
       * Selects one command row, scoping the log drawer to it unless pinned.
       * @param {string} gid - The review id.
       * @param {string} key - The command's key, `<gid>:<kind>:<index>`.
       * @param {string} cmd - The command text, for the drawer's header.
       * @returns {void}
       */
      function selectReviewCommandRow(gid, key, cmd) {
        selectedCommandKey = selectedCommandKey === key ? "" : key;
        if (!reviewDockSticky) {
          reviewDockScope = selectedCommandKey ? key : "review";
          reviewDockCommand = selectedCommandKey ? cmd : "";
        }
        renderReviewDetail();
        renderReviewDock();
      }
      /**
       * Scopes the log drawer to one command and opens it. An explicit ask
       * beats the sticky pin, which guards against incidental scope changes.
       * @param {string} gid - The review id.
       * @param {string} key - The command's key.
       * @param {string} cmd - The command text.
       * @returns {void}
       */
      function scopeReviewDockToCommand(gid, key, cmd) {
        closeSquadMenu();
        selectedCommandKey = key;
        reviewDockScope = key;
        reviewDockCommand = cmd;
        if (!reviewDockOpen) toggleReviewDock();
        else renderReviewDock();
        if (reviewDockEvents[gid] === undefined) loadReviewDockEvents(gid);
        renderReviewDetail();
      }
      /**
       * Expands or collapses one command's full text beneath its row. The row
       * elides so the section stays scannable; this is how you read the rest
       * without leaving the page for a popup.
       * @param {string} key - The command's key.
       * @returns {void}
       */
      function toggleReviewCommandFull(key) {
        closeSquadMenu();
        commandFullOpen[key] = !commandFullOpen[key];
        renderReviewDetail();
      }
      /**
       * Whether one command row is the selected one.
       * @param {string} key - The command's key.
       * @returns {boolean}
       */
      function isCommandRowSelected(key) {
        return selectedCommandKey === key;
      }
      /**
       * The expanded full-text block under a command row, or "" when collapsed.
       * @param {string} key - The command's key.
       * @param {string} cmd - The command text.
       * @returns {string}
       */
      function commandFullBlock(key, cmd) {
        if (!commandFullOpen[key]) return "";
        return `<div class="cmd-full mono">${esc(cmd)}
            <div class="btn-row" style="margin-top:7px">
              <button class="btn" data-copy="${esc(cmd)}" onclick="copyText(event)" data-tip="Copy the whole command.">⧉ Copy</button>
              <button class="btn" data-click="toggleReviewCommandFull" data-key="${esc(key)}" data-tip="Collapse this command back to one line.">Collapse</button>
            </div>
          </div>`;
      }

      /**
       * Opens one command's ⋯ menu -- a check gate, for now. Commands are long
       * and elided in their row, so "see the whole thing" and "copy it" need a
       * home that is not the row itself.
       * @param {MouseEvent} e - The click that opened it.
       * @param {string} gid - The review id.
       * @param {string} cmd - The command text.
       * @param {string} anchorKey - The command's key, `<gid>:<kind>:<index>`.
       * @returns {void}
       */
      function openReviewCommandMenu(e, gid, cmd, anchorKey) {
        e.preventDefault(); e.stopPropagation(); closeSquadMenu();
        const menu = document.createElement("div");
        menu.className = "ctx-menu"; menu.id = "squad-menu";
        const key = anchorKey;
        menu.innerHTML = [
          `<div data-click="toggleReviewCommandFull" data-key="${esc(key)}" data-tip="Show the whole command under its row — the row elides it to keep the section scannable.">🔍 ${commandFullOpen[key] ? "Collapse" : "Show full command"}</div>`,
          `<div data-copy="${esc(cmd)}" onclick="copyText(event)" data-tip="Copy this command to the clipboard.">⧉ Copy command</div>`,
          `<div data-click="scopeReviewDockToCommand" data-guardian-id="${esc(gid)}" data-key="${esc(key)}" data-cmd="${esc(cmd)}" data-tip="Open the log drawer scoped to this command.">☰ Logs for this command</div>`,
        ].join("");
        document.body.appendChild(menu);
        menu.style.left = Math.min(e.clientX, window.innerWidth - 220) + "px";
        menu.style.top = Math.min(e.clientY, window.innerHeight - 110) + "px";
      }
      /**
       * Opens the log drawer from a section's menu.
       * @param {string} gid - The review id.
       * @returns {void}
       */
      function scopeReviewDockToSection(gid) {
        closeSquadMenu();
        if (!reviewDockSticky) reviewDockScope = "review";
        if (!reviewDockOpen) toggleReviewDock();
        else renderReviewDock();
        if (reviewDockEvents[gid] === undefined) loadReviewDockEvents(gid);
      }

      /**
       * Cartographer rows per review, loaded on the dock's first open and
       * refreshed only while it is open -- a closed dock issues no request.
       * @type {{[guardianId: string]: CartographerRow[]}}
       */
      const reviewDockEvents = {};
      /**
       * Loads this review's Cartographer events for the dock. Called only from
       * the dock's own open path, never from the review's render.
       * @param {string} gid - The review id.
       * @returns {Promise<void>}
       */
      async function loadReviewDockEvents(gid) {
        try {
          const r = await fetch(`/api/cartographer?guardian_id=${encodeURIComponent(gid)}&limit=200`);
          if (!r.ok) { reviewDockEvents[gid] = []; renderReviewDock(); return; }
          /** @type {{rows: CartographerRow[], total: number}} */
          const data = await r.json();
          reviewDockEvents[gid] = data.rows || [];
        } catch {
          reviewDockEvents[gid] = [];
        }
        renderReviewDock();
      }
