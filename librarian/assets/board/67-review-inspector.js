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
        renderReviewInspector();
        renderReviewDock();
      }
      /**
       * Switches inspector tabs. Opening Live attaches that branch's stream --
       * the one place this pane costs anything, and only on an explicit open.
       * @param {string} tab - overview | worktree | feedback | live.
       * @returns {void}
       */
      function setInspectorTab(tab) {
        inspectorTab = tab;
        if (tab === "live" && inspectorBranchId) inspectorLiveAttached[inspectorBranchId] = true;
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
        return `<div class="hint" style="margin:0 0 10px">What you write here is routed into
            <span class="mono">${esc(b.branch)}</span>'s worktree as a change request. The resolver
            agent amends the branch and replies.</div>
          ${branchFeedbackSection(g, b)}`;
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
        return `<div class="btn-row" style="position:relative;gap:0">${resolverTerminalBtns(g, b)}</div>
          ${resolverPeekBox(g, b)}
          <div class="hint">Read-only — nothing typed here reaches the agent. The transcript is
          captured per attempt, so it survives a restart.</div>`;
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
        if (scopeEl) scopeEl.textContent = b ? b.branch : "whole review";
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
        if (kind === "manual") {
          items.push(`<div data-click="runAllManualChecks" data-guardian-id="${esc(gid)}" data-tip="Run every suggested manual check, each in the built review worktree.">▶ Run all</div>`);
          items.push(`<div data-click="regenManualChecks" data-guardian-id="${esc(gid)}" data-tip="Ask the resolver agent to write these checks again against the stack's current changes.\nRuns in the background and never blocks Approve or Merge / rebase.">↻ Regenerate</div>`);
        }
        if (kind === "gates") {
          items.push(`<div data-click="openEditReviewDetails" data-guardian-id="${esc(gid)}" data-tip="Check gates are edited in the review's settings.">✎ Edit gates…</div>`);
        }
        menu.innerHTML = items.join("");
        document.body.appendChild(menu);
        menu.style.left = Math.min(e.clientX, window.innerWidth - 200) + "px";
        menu.style.top = Math.min(e.clientY, window.innerHeight - 120) + "px";
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
