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
      /**
       * Which run the Live tab is walked back to, per branch. Absent (or past
       * the end) means the newest run -- the live one.
       * @type {{[branchId: string]: number}}
       */
      const liveRunIdx = {};
      /**
       * Which Live sub-view is showing, per branch: terminal | prompt | system.
       * @type {{[branchId: string]: string}}
       */
      const liveSub = {};
      /**
       * One thing that ran for a branch -- a rebase pass, a final proof, a
       * feedback revision, a PR submit, a post-merge gate. Derived from the
       * review's Cartographer rows rather than stored anywhere: the daemon
       * records each of these as events, and a run is the span between its
       * start and end row.
       * @typedef {object} BranchRun
       * @property {string} id - Stable within one render, for the picker.
       * @property {string} kind - rebase | proof | feedback.
       * @property {string} label - What the pass was doing.
       * @property {number} atMs - When the run started.
       * @property {string} outcome - resolved / passed / failed / committed …
       * @property {string} who - The agent that ran it.
       * @property {number} elapsedMs - Measured duration, or 0 when the emitter reports none. Never estimated.
       */
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
            : `<div class="empty" style="padding:8px 0">Inherits the daemon environment — nothing overridden for this worktree.</div>`}`;
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
        return `<div class="fb-top">
            <div class="hint" style="margin:0">What you write here is routed into
              <span class="mono">${esc(b.branch)}</span>'s worktree as a change request. The resolver
              agent amends the branch and replies.</div>
            <button class="copy-btn" data-click="showChatCopyMenu"
              data-guardian-id="${esc(g.id)}" data-branch-id="${esc(b.id)}"
              data-tip="Copy this whole thread — every request and reply.\nChoose Markdown for readable text or JSON for raw data.">⧉</button>
          </div>
          ${feedbackThread(g, b)}
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
      /**
       * The branch's feedback thread as a conversation.
       *
       * Consecutive resolver messages are one bubble with a stepper rather than
       * a stack of separate ones: they are that agent's successive replies to
       * the same request, so "did it fail repeatedly before it got there" is a
       * question about one exchange, and the stepper is what answers it.
       * @param {GuardianView} g - The review.
       * @param {GuardianBranch} b - The branch.
       * @returns {string}
       */
      function feedbackThread(g, b) {
        const key = `${g.id}:${b.id}`;
        if (branchMessages[key] === undefined) loadBranchMessages(g.id, b.id);
        const msgs = branchMessages[key] || [];
        if (!msgs.length) {
          return `<div class="absent-run" style="margin-bottom:12px"><span class="ai">○</span>
              <div class="abody"><div class="at">No change requests yet</div>
                <div class="as">Ask for a change below and it is routed straight into this branch's worktree.
                  The same thread is reachable from the CLI:
                  <span class="mono">ralphus review feedback &lt;selector&gt; &lt;text&gt;</span>.</div></div>
            </div>`;
        }
        /** @type {string[]} */
        const out = [];
        for (let i = 0; i < msgs.length;) {
          const m = msgs[i];
          if (m.role === "reviewer") {
            out.push(feedbackBubble(key, m, "you", m.author || "you", feedbackStatusChip(m), g, b));
            i++;
            continue;
          }
          // Collect this agent's run of replies, and show whichever one the
          // reader has stepped to.
          const group = [];
          while (i < msgs.length && msgs[i].role !== "reviewer") { group.push(msgs[i]); i++; }
          const gk = `${key}:g${group[0].seq}`;
          const idx = Math.max(0, Math.min(group.length - 1, feedbackReplyIdx[gk] ?? (group.length - 1)));
          const foot = feedbackReplyFoot(gk, group, idx, g, b);
          out.push(feedbackBubble(key, group[idx], "resolver", "resolver", foot, g, b));
        }
        return `<div class="fb-thread">${out.join("")}</div>`;
      }
      /**
       * One message bubble.
       * @param {string} key - The thread key.
       * @param {ChatMessage} m - The message.
       * @param {string} side - "you" or "resolver".
       * @param {string} who - The label shown on it.
       * @param {string} foot - Trailing status/stepper markup, or "".
       * @param {GuardianView} g - The review.
       * @param {GuardianBranch} b - The branch.
       * @returns {string}
       */
      function feedbackBubble(key, m, side, who, foot, g, b) {
        void g; void b;
        const bubbleKey = `${key}:${m.seq}`;
        const text = stripRoute(m.text);
        const expanded = expandedChatBubbles.has(bubbleKey);
        // Elide on length as well as on line count -- a long single-line
        // paragraph is exactly the case the old first-line rule missed.
        const long = text.includes("\n") || text.length > 180;
        const shown = (!long || expanded) ? text : `${text.slice(0, 180).trimEnd()}…`;
        const t = fmtMsgTime(m.at_ms);
        return `<div class="fb-msg ${side}">
            <div class="fb-head">
              <span class="fb-who">${esc(who)}</span>
              ${t ? `<span class="fb-time" data-tip="${esc(fmtMsgTimeFull(m.at_ms))}, your local time.">${esc(t)}</span>` : ""}
            </div>
            ${long ? `<button class="fb-expand" data-click="toggleChatBubble" data-key="${esc(bubbleKey)}"
              data-tip="${expanded ? "Collapse this message." : "Expand to read the whole message."}">${expanded ? "−" : "+"}</button>` : ""}
            <div class="fb-body">${esc(shown)}</div>
            ${foot}
          </div>`;
      }
      /**
       * A reviewer message's completion chip, straight from the status the
       * daemon records for that one message (RAL-380).
       * @param {ChatMessage} m - The reviewer message.
       * @returns {string}
       */
      function feedbackStatusChip(m) {
        const s = m.action_status;
        if (!s) return "";
        const label = { received: "accepted", done: "applied", failed: "failed", superseded: "superseded" }[s] || s;
        const tip = {
          received: "Accepted — the resolver agent has picked this request up and started applying it.",
          done: "Applied — the resolver agent's pass for this request finished successfully.",
          failed: "Failed — the resolver agent's pass for this request, or a check before it, did not succeed.",
          superseded: "Superseded — a newer request arrived on this branch before this one finished, so its outcome no longer stands.",
        }[s] || "This request's recorded completion status.";
        return `<div class="fb-foot"><span class="fb-chip ${esc(s)}" data-tip="${esc(tip)}">${esc(label)}</span></div>`;
      }
      /**
       * The footer under a resolver bubble: which reply of the run you are on,
       * and the way to its logs.
       * @param {string} gk - The group key.
       * @param {ChatMessage[]} group - Every reply in this run.
       * @param {number} idx - Which one is showing.
       * @param {GuardianView} g - The review.
       * @param {GuardianBranch} b - The branch.
       * @returns {string}
       */
      function feedbackReplyFoot(gk, group, idx, g, b) {
        const stepper = group.length > 1
          ? `<span class="fb-attempt" data-tip="This agent replied ${group.length} times to the same request — step through them to see what it tried before the reply you are reading.">
              <span>reply ${idx + 1} of ${group.length}</span>
              <button class="hnav" data-click="stepFeedbackReply" data-key="${esc(gk)}" data-to="${idx - 1}" ${idx <= 0 ? "disabled" : ""}
                data-tip="The reply before this one.">&#9664;</button>
              <button class="hnav" data-click="stepFeedbackReply" data-key="${esc(gk)}" data-to="${idx + 1}" ${idx >= group.length - 1 ? "disabled" : ""}
                data-tip="The reply after this one.">&#9654;</button>
            </span>`
          : "";
        return `<div class="fb-foot">${stepper}
            <button class="btn fb-logs" style="padding:1px 7px;font-size:10.5px" data-click="scopeReviewDockToBranch"
              data-guardian-id="${esc(g.id)}" data-branch-id="${esc(b.id)}"
              data-tip="Open this branch's log drawer — what the resolver was doing while it wrote this.">logs</button>
          </div>`;
      }
      /** @type {{[groupKey: string]: number}} Which reply of a resolver run is showing. */
      const feedbackReplyIdx = {};
      /**
       * Steps a resolver run's reply selection. The target index is carried on
       * the button rather than derived here, since an unstepped group defaults
       * to its newest reply and this has no way to know how many there are.
       * @param {string} gk - The group key.
       * @param {number} to - The reply index to show.
       * @returns {void}
       */
      function stepFeedbackReply(gk, to) {
        if (!Number.isFinite(to) || to < 0) return;
        feedbackReplyIdx[gk] = to;
        renderReviewInspector();
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
       * Every agent session this branch ran, oldest first.
       *
       * "What was it doing at that time" is not a question about rebase
       * attempts alone -- a branch rebases, proves, and revises against
       * feedback, and any of those is a point someone wants to walk back to.
       * The daemon records each as a Cartographer event, so a run is the span
       * between its start and end rows; this reads rows the board already has
       * for the review rather than asking for anything new.
       *
       * Only passes that ran under a pane are listed. A PR submit goes out over
       * the forge's REST API and a check gate runs as a plain command, so
       * neither ever had a transcript -- an entry for one could only ever say
       * "nothing to show here", which is a walk-back stop that cannot be walked
       * to. Both stay recorded elsewhere: gates in the check-gates section,
       * both of them in the logs drawer.
       * @param {GuardianView} g - The review.
       * @param {GuardianBranch} b - The branch.
       * @returns {BranchRun[]}
       */
      function branchRuns(g, b) {
        const rows = reviewDockEvents[g.id];
        if (!rows) return [];
        /** @type {BranchRun[]} */
        const runs = [];
        const resolver = resolverOf(g);
        // Branch attribution is not uniform across emitters: status transitions
        // key on `payload.ref` (a branch id), the rebase/proof passes on
        // `payload.branch` (a branch *name*), the PR and feedback-posting rows
        // on `payload.branch_id`, and the feedback pass itself on
        // `payload.position` (a stack ordinal -- which a reorder can move, so
        // it is matched last and only when nothing better identifies the row).
        const mine = rows.filter((r) => {
          const p = r.payload || {};
          return p.ref === b.id || p.branch === b.branch || p.branch_id === b.id
            || (p.position !== undefined && p.position === b.position);
        });
        const ordered = mine.slice().sort((x, y) => x.at_ms - y.at_ms);
        /**
         * Closes the most recent still-running run of a kind.
         * @param {string} kind - Which kind to close.
         * @param {string} outcome - The outcome to record.
         * @param {number} [ms] - Measured duration, when the emitter reports one.
         * @returns {void}
         */
        const close = (kind, outcome, ms) => {
          const open = [...runs].reverse().find((x) => x.kind === kind && x.outcome === "running");
          if (!open) return;
          open.outcome = outcome;
          if (ms) open.elapsedMs = ms;
        };
        for (const r of ordered) {
          const msg = r.message || "";
          const p = r.payload || {};
          if (msg.startsWith("conflicts starting")) {
            const found = p.found ? ` · ${p.found} conflict${p.found === 1 ? "" : "s"}` : "";
            runs.push({
              id: `rebase-${r.id}`, kind: "rebase",
              label: `rebase onto ${g.base_branch || "upstream"}${found}`,
              atMs: r.at_ms, outcome: "running", elapsedMs: 0,
              who: String(p.agent || resolver),
            });
          } else if (msg.startsWith("conflicts resolved")) {
            close("rebase", p.committed ? "resolved" : "resolved, nothing to commit");
          } else if (msg.startsWith("conflicts failed")) {
            close("rebase", "failed");
          } else if (msg.startsWith("final proof starting")) {
            runs.push({
              id: `proof-${r.id}`, kind: "proof", label: "final proof",
              atMs: r.at_ms, outcome: "running", elapsedMs: 0, who: resolver,
            });
          } else if (msg.startsWith("final proof done")) {
            close("proof", p.passed ? "passed" : "failed");
          } else if (msg.startsWith("feedback applying")) {
            runs.push({
              id: `fb-${r.id}`, kind: "feedback", label: "feedback revision",
              atMs: r.at_ms, outcome: "running", elapsedMs: 0, who: resolver,
            });
          } else if (msg.startsWith("feedback done")) {
            close("feedback", p.committed ? "committed" : "no change committed");
          }
        }
        // One timeline, in the order things actually happened.
        runs.sort((x, y) => x.atMs - y.atMs);
        return runs;
      }
      /**
       * Formats a run's "05:12:30 · passed" metadata line.
       * @param {BranchRun} run - The run.
       * @returns {string}
       */
      function runMeta(run) {
        const t = run.atMs
          ? new Date(run.atMs).toLocaleTimeString([], { hour12: false, hour: "2-digit", minute: "2-digit", second: "2-digit" })
          : "";
        // Duration only where the daemon measured one -- a gate's elapsed_ms.
        // Nothing here is derived from wall-clock guesses.
        const dur = run.elapsedMs ? ` · ${fmtDurationMs(run.elapsedMs)}` : "";
        return (run.outcome ? `${t} · ${run.outcome}` : t) + dur;
      }
      /**
       * Formats a measured duration the way the rest of the board does.
       * @param {number} ms - Milliseconds.
       * @returns {string}
       */
      function fmtDurationMs(ms) {
        const secs = Math.round(ms / 1000);
        if (secs < 60) return `${secs}s`;
        return `${Math.floor(secs / 60)}m ${secs % 60}s`;
      }
      /**
       * Which run index the Live tab is showing for a branch, clamped to the
       * list it actually has (runs grow as the branch works).
       * @param {BranchRun[]} runs - The branch's runs.
       * @param {string} bid - The branch id.
       * @returns {number}
       */
      function curRunIdx(runs, bid) {
        if (!runs.length) return -1;
        const want = liveRunIdx[bid];
        if (want === undefined) return runs.length - 1;
        return Math.max(0, Math.min(runs.length - 1, want));
      }
      /**
       * Steps the Live tab one run backwards or forwards.
       * @param {string} bid - The branch id.
       * @param {number} dir - -1 for the previous run, 1 for the next.
       * @returns {void}
       */
      function stepBranchRun(bid, dir) {
        const g = guardians.find((x) => x.id === selectedGuardian);
        const b = g && g.branches ? g.branches.find((x) => x.id === bid) : null;
        if (!g || !b) return;
        const runs = branchRuns(g, b);
        if (!runs.length) return;
        liveRunIdx[bid] = Math.max(0, Math.min(runs.length - 1, curRunIdx(runs, bid) + dir));
        renderReviewInspector();
      }
      /**
       * Jumps the Live tab back to the newest run -- the live one.
       * @param {string} bid - The branch id.
       * @returns {void}
       */
      function jumpToLatestRun(bid) {
        delete liveRunIdx[bid];
        renderReviewInspector();
      }
      /**
       * Switches the Live tab's sub-view.
       * @param {string} bid - The branch id.
       * @param {string} sub - terminal | prompt | system.
       * @returns {void}
       */
      function setLiveSub(bid, sub) {
        liveSub[bid] = sub;
        renderReviewInspector();
        if (sub === "system") {
          const g = guardians.find((x) => x.id === selectedGuardian);
          // Fetched on first open and dropped on leaving, like every other
          // lazily-loaded pane -- reopening refetches rather than caching.
          if (g) ensurePeekSystemPrompt(`guardian|${g.id}|${bid}`).then(() => renderReviewInspector());
        }
      }
      /**
       * Live: walk back through every run this branch had, and read whichever
       * one you land on. Nothing attaches until this tab is opened -- a branch
       * row never renders a terminal, so the cost is paid once, on the click.
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
        // The run list is derived from the review's event rows, which the log
        // drawer also reads -- one fetch per review, on the first thing that
        // needs it, rather than one per tab.
        if (reviewDockEvents[g.id] === undefined) loadReviewDockEvents(g.id);
        const runs = branchRuns(g, b);
        const ri = curRunIdx(runs, b.id);
        const run = ri >= 0 ? runs[ri] : null;
        const isLatest = ri === runs.length - 1;
        const sub = liveSub[b.id] || "terminal";
        const ended = !!peekEnded[key];
        // Only the newest pass that used a tmux pane has content the live view
        // still holds: the pane is reused, so an earlier pass's text is no
        // longer in it. "Am I looking at live content" therefore keys on the
        // Whether the pane still holds this run's text at all, separate from
        // whether that text is still moving: the pane is reused between passes,
        // so only the newest one's output is still in it. A finished session's
        // record is not "historical" in the sense the walk-back note means --
        // it is this run's own output, just no longer growing, and the liveness
        // dot and the pane's own banner already say so.
        const onNewestTape = !!run && ri === runs.length - 1;
        const live = onNewestTape && !ended;

        // 1 - who am I looking at, and is it still moving
        const identity = `<div class="peek-head" style="margin-bottom:8px">
            <span class="peek-dot${live ? "" : " ended"}"></span>
            <span class="mono" style="font-size:11px">${esc(resolverOf(g))} · ${esc(b.branch)}</span>
            <span style="flex:1"></span>
            <span class="rg-sub" style="font-size:11px;color:var(--faint)">${live ? "streaming" : (isLatest ? "session ended" : "historical")}</span>
            <button class="copy-btn" data-click="copyPeekText" data-key="${esc(key)}" data-tip="Copy what this tab is currently showing.">⧉</button>
          </div>`;

        // 2 - which run
        const histBar = runs.length && run ? `<div class="histbar${onNewestTape ? "" : " past"}">
            <button class="hnav" data-click="stepBranchRun" data-branch-id="${esc(b.id)}" data-dir="-1" ${ri <= 0 ? "disabled" : ""}
              data-tip="Step back to the previous run on this branch.">&#9664;</button>
            <button class="hpick" data-click="scopeReviewDockToBranch" data-guardian-id="${esc(g.id)}" data-branch-id="${esc(b.id)}"
              data-tip="Every agent session this branch ran — rebases, final proofs, feedback revisions.\nOpens the log drawer, where each run's own rows are listed, along with the PR submits and check gates that never had a session to replay.">
              <span class="hkind ${esc(run.kind)}">${esc(run.kind)}</span>
              <span class="hlabel mono">${esc(run.label)}</span>
              <span class="hmeta">${esc(runMeta(run))}</span>
              <span class="hcount num">${ri + 1}/${runs.length}</span> &#9662;</button>
            <button class="hnav" data-click="stepBranchRun" data-branch-id="${esc(b.id)}" data-dir="1" ${ri >= runs.length - 1 ? "disabled" : ""}
              data-tip="Step forward to the next run on this branch.">&#9654;</button>
            ${isLatest ? "" : `<button class="btn" style="padding:3px 8px;font-size:11px" data-click="jumpToLatestRun" data-branch-id="${esc(b.id)}"
              data-tip="Jump back to the newest run — the one still streaming.">&#8677; Latest</button>`}
          </div>
          ${onNewestTape ? "" : `<div class="histnote">You are walked back to ${esc(run.label)}
            from ${esc(runMeta(run))} — an earlier pass than the one the pane below holds.</div>`}`
          : `<div class="histbar"><span class="hpick" data-tip="Sessions appear here as the daemon records them — a rebase pass, a final proof, a feedback revision.\nThis branch has not run one yet.">
              <span class="hkind">no runs</span><span class="hmeta">no agent session recorded for this branch yet</span></span></div>`;

        // 3 - how it is rendered. Terminal-only controls, but they stay put on
        //     the prompt views rather than vanishing: appearing and disappearing
        //     on every tab swap reflows the pane. aria-disabled, not `disabled`,
        //     so the tooltip explaining why still fires.
        const showDebug = peekShowsDebug(key);
        const showThinking = peekShowsThinking(key);
        const canThink = g.resolver_thinking_capable !== false;
        const dead = sub !== "terminal";
        const deadTip = "Applies to the Terminal view.\nSwitch to Terminal to use it.";
        const ctl = `<div class="peek-ctl">
            <button class="tgl ${showDebug ? "on" : ""}${dead ? " off" : ""}" ${dead ? 'aria-disabled="true"' : ""}
              data-click="toggleShowDebugMessagesBtn" data-key="${esc(key)}"
              data-tip="${dead ? deadTip : "Show ralphus's own diagnostic/telemetry events inline, where they happened.\nOff by default so routine monitoring shows what the agent did.\nThis only changes what is rendered here — the daemon's logs always keep everything."}">
              <span class="bx"></span>Debug messages</button>
            <input class="typefilter${(dead || !showDebug) ? " off" : ""}" placeholder="Filter types (e.g. read glob)"
              value="${esc(peekTypeFilterInput[key] || "")}" ${(dead || !showDebug) ? "disabled" : ""}
              oninput="setPeekTypeFilter('${esc(key)}',this.value)" aria-label="Filter log types"
              data-tip="${dead ? deadTip : (!showDebug ? "Turn on Debug messages to filter by type." : "Show only bracket-tagged lines whose type contains any space-separated term.\nCase-insensitive; 'read glob' shows tool.Read and tool.Glob.")}">
            ${canThink ? `<button class="tgl ${showThinking ? "on" : ""}${dead ? " off" : ""}" ${dead ? 'aria-disabled="true"' : ""}
              data-click="toggleShowThinkingBtn" data-key="${esc(key)}"
              data-tip="${dead ? deadTip : "Show the model's own reasoning expanded inline.\nOff folds each block to a single &lt;thinking…&gt; line.\nPurely a display choice — the reasoning is always captured, so toggling re-renders text already loaded without refetching."}">
              <span class="bx"></span>Thinking</button>` : ""}
          </div>`;

        // 4 - which view, seated directly on what it switches
        const subs = [["terminal", "Terminal"], ["prompt", "Prompt"], ["system", "System Prompt"]];
        const tabs = `<div class="subtabs">${subs.map((s) => `<button class="subtab ${sub === s[0] ? "on" : ""}" `
          + `data-click="setLiveSub" data-branch-id="${esc(b.id)}" data-sub="${s[0]}" `
          + `data-tip="${esc(LIVE_SUB_TIP[s[0]])}">${s[1]}</button>`).join("")}</div>`;

        const top = identity + histBar + ctl + tabs;
        if (sub === "system") return top + liveSystemPromptView(g, key);
        if (sub === "prompt") return top + livePromptView(g, b, run);
        return top + liveTerminalView(g, b, key, run, onNewestTape);
      }
      /** @type {{[sub: string]: string}} What each Live sub-view shows. */
      const LIVE_SUB_TIP = {
        terminal: "The run's captured terminal output.\nThe newest run streams; an earlier one is the daemon's persisted record of it.",
        prompt: "The instruction this run's agent was given — the task it was asked to do, as opposed to the standing rules it works under.",
        system: "The exact system prompt this run's agent received: ralphus's hidden instructions plus the resolver's authored prompt.\nRead-only reference — changing it means changing the resolver settings.\nAdmin-only view.",
      };
      /**
       * The Live tab's Terminal view: the run's captured output, framed, with
       * the controls that act on it underneath.
       * @param {GuardianView} g - The review.
       * @param {GuardianBranch} b - The branch.
       * @param {string} key - The peek key.
       * @param {BranchRun|null} run - The run being shown.
       * @param {boolean} isLatest - Whether that run is the newest one that used
       *   a tmux pane, and so the only one whose text the live view still holds.
       * @returns {string}
       */
      function liveTerminalView(g, b, key, run, isLatest) {
        const foot = `<div class="run-foot">
            ${isLatest ? `<button class="btn" style="padding:3px 8px;font-size:11.5px" data-click="openGuardianBranchTerminalMenuItem" data-key="${esc(key)}" data-gid="${esc(g.id)}" data-bid="${esc(b.id)}" data-mode="open"
              data-tip="Attach a real, interactive terminal to this session.\nShows the runner's own log/event stream, not the agent's conversation.">Open terminal</button>` : ""}
            <button class="btn" style="padding:3px 8px;font-size:11.5px" data-click="scopeReviewDockToBranch" data-guardian-id="${esc(g.id)}" data-branch-id="${esc(b.id)}"
              data-tip="Open this branch's log drawer alongside the transcript.">Logs</button>
            <span class="rg-sub">${run ? esc(run.who) : ""}</span>
          </div>`;
        if (!isLatest) {
          return `<div class="absent-run"><span class="ai">○</span>
              <div class="abody"><div class="at">This run's terminal text is no longer reachable</div>
                <div class="as">The persisted attempt log covers the branch's current pass and its reattaches;
                  an earlier pass — a previous rebase, a proof, a feedback revision — wrote to the same pane and
                  is not addressable on its own. What the run did, when, and how it ended is above and in the
                  logs drawer; only its raw output is gone.</div></div>
            </div>${foot}`;
        }
        const shown = peekContent[key];
        return `<div class="runterm" id="peek-pre-${peekCssKey(key)}" style="height:${peekPaneHeight}px" tabindex="0" data-key="${esc(key)}"
            onscroll="onPeekScroll(this.dataset.key)" onkeydown="handlePeekKeydown(event,this.dataset.key)"
            data-tip="Scroll through this run's output.\nClick here then press Ctrl+End to jump to the latest, or Ctrl+Home for the start.">${shown !== undefined ? esc(shown) : "Loading…"}</div>${foot}
          <div class="hint">Read-only — nothing typed here reaches the agent. Every run's text is
          captured separately, so walking back survives a restart.</div>`;
      }
      /**
       * The Live tab's Prompt view: the instruction this run's agent was given.
       *
       * The daemon composes a resolver's prompt at dispatch and never persists
       * it, so for most runs there is nothing to show and this says so rather
       * than showing the system prompt again under a second label. A feedback
       * revision is the exception -- the reviewer's own text is stored, and it
       * is the substantive half of what that run was told to do.
       * @param {GuardianView} g - The review.
       * @param {GuardianBranch} b - The branch.
       * @param {BranchRun|null} run - The run being shown.
       * @returns {string}
       */
      function livePromptView(g, b, run) {
        if (run && run.kind === "feedback") {
          const msgs = branchMessages[`${g.id}:${b.id}`];
          if (msgs === undefined) { loadBranchMessages(g.id, b.id); return `<div class="promptbox">Loading…</div>`; }
          // The daemon's roles are "reviewer" and "guardian" -- there is no
          // "user" role, so matching one never found anything.
          const last = [...msgs].reverse().find((m) => m.role === "reviewer");
          if (last) {
            return `<div class="promptbox">${esc(last.text)}</div>
              <div class="hint">The reviewer feedback this run was dispatched to act on. ralphus wraps it in
              standing instructions before sending; only the authored half is retained, and this is it.</div>`;
          }
        }
        return `<div class="absent-run"><span class="ai">○</span>
            <div class="abody"><div class="at">This run's prompt was not retained</div>
              <div class="as">A resolver's instruction is composed when the run is dispatched — naming the branch
                and the exact conflicted files — and is not written to the store, so there is nothing to replay.
                The standing half of what it was told is on the System Prompt tab; a feedback revision shows the
                reviewer's own text here.</div></div>
          </div>`;
      }
      /**
       * The Live tab's System Prompt view.
       * @param {GuardianView} g - The review.
       * @param {string} key - The peek key.
       * @returns {string}
       */
      function liveSystemPromptView(g, key) {
        if (!currentUserIsAdmin) {
          return `<div class="absent-run"><span class="ai">○</span>
              <div class="abody"><div class="at">Admin-only view</div>
                <div class="as">The effective system prompt is shown to administrators only.</div></div>
            </div>`;
        }
        const ps = peekSystemPrompt[key];
        const body = ps === undefined || ps === "loading" ? "Loading…" : peekPromptDisplay(ps);
        return `<div class="promptbox">${esc(body)}</div>
          <div class="run-foot"><button class="btn" style="padding:3px 8px;font-size:11.5px" data-click="openEditReviewDetails" data-guardian-id="${esc(g.id)}" data-focus="resolver"
            data-tip="The authored half of this prompt comes from the resolver agent and model. Change those in review setup.">Resolver settings</button></div>
          <div class="hint">Read-only — the effective system prompt actually appended to this agent
          invocation: ralphus's hidden instructions plus the resolver's authored prompt.</div>`;
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
        // The label says what clicking does, not what the drawer is currently
        // tracking -- "following selection" described an internal mode and
        // read as a status, so the one control on the bar looked like a label.
        // Whether it follows the selection is the sticky toggle's business,
        // and that toggle already says so itself.
        const hint = document.getElementById("review-dock-hint");
        if (hint) hint.textContent = reviewDockOpen ? "close" : "open";
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
       * Which env scope each runnable section edits.
       *
       * `edit` is the writable scope. Check gates and test actions share one:
       * the daemon composes the gates' environment from the daemon's own plus
       * the build step's overrides, so both sections edit `build`.
       * @type {{[kind: string]: {edit: string, label: string, tip: string}}}
       */
      const ENV_SCOPE_FOR_SECTION = {
        gates: {
          edit: "build",
          label: "check gates",
          tip: "Edit the environment the check gates run in.\nThe daemon composes it from its own environment plus the build step's overrides, so this edits the build step's — shared with test actions.",
        },
        manual: {
          edit: "manual_checks",
          label: "manual checks",
          tip: "Edit the environment overrides applied when the suggested manual checks run.\nThis scope is the manual-checks step's own — nothing else in the review uses it.",
        },
        actions: {
          edit: "build",
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
        void openEnvOverridesEditor(gid, meta.edit, "");
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
          if (!r.ok) { reviewDockEvents[gid] = []; afterReviewDockEvents(); return; }
          /** @type {{rows: CartographerRow[], total: number}} */
          const data = await r.json();
          reviewDockEvents[gid] = data.rows || [];
        } catch {
          reviewDockEvents[gid] = [];
        }
        afterReviewDockEvents();
      }
      /**
       * Repaints everything derived from the review's event rows once they land.
       *
       * The dock is no longer their only consumer: the Live tab's walk-back is
       * derived from the same rows, and it is usually what triggers the fetch.
       * Repainting only the dock left that tab showing "no runs" until some
       * unrelated event happened to re-render the inspector.
       * @returns {void}
       */
      function afterReviewDockEvents() {
        renderReviewDock();
        renderReviewInspector();
      }
