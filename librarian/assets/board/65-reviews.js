      // ---------- structured check inputs (RAL-164) ----------
      // A GuardianCheck (either an AI-synthesized manual check or a
      // user-declared [[review.action]] hint) may declare named `inputs`
      // (e.g. a port number) referenced in its command as `{name}`
      // placeholders. A check with no inputs still runs immediately, exactly
      // as before; a check that declares any renders as a toggle that
      // expands an inline form instead.
      /**
       * Renders the inline input form for a check that declares CheckInputs:
       * one labelled field per input (pre-filled from the guardian's stored
       * input_values, falling back to the input's own default), a "set it
       * for me" button per field that delegates to the resolver agent, an
       * optional cleanup checkbox when the check declares a cleanup_command,
       * and Run/Cancel buttons.
       * @param {GuardianView} g
       * @param {"manual"|"action"} kind
       * @param {number} i
       * @param {GuardianCheck} check
       * @returns {string}
       */
      function renderCheckInputFields(g, kind, i, check) {
        const key = `${g.id}:${kind}:${i}`;
        const values = g.input_values || {};
        const resolutions = g.input_resolutions || {};
        const fields = (check.inputs || []).map((inp) => {
          const fieldId = `check-input:${key}:${inp.name}`;
          const current = Object.prototype.hasOwnProperty.call(values, inp.name) ? values[inp.name] : inp.default;
          const res = resolutions[inp.name];
          const resolving = !!res && res.status === "resolving";
          const setTip = "Ask the resolver agent to pick a value for this input.\nWho/when: you don't know (or don't care) what value to use here — let the AI decide.\nFills the field with its answer; the command above updates to match, and ▶ runs it.";
          const setBtn = `<button class="btn" ${resolving ? "disabled" : ""} data-click="resolveCheckInput" data-guardian-id="${esc(g.id)}" data-input-name="${esc(inp.name)}" data-tip="${setTip}">${resolving ? "Resolving…" : "Set it for me"}</button>`;
          return `<div style="margin:6px 0 0">
            <label for="${fieldId}" style="display:block;font-size:11px;color:var(--muted);margin-bottom:2px" data-tip="${esc(inp.message)}">${esc(inp.message)}</label>
            <div style="display:flex;gap:4px">
              <input id="${fieldId}" type="text" value="${esc(current)}" class="mono"
                oninput="onCheckInputChange('${esc(key)}')"
                style="flex:1;font-size:12px;background:var(--bg);color:var(--text);border:1px solid var(--border);border-radius:4px;padding:3px 6px"
                data-tip="Substituted for {${esc(inp.name)}} in the command above, which updates as you type.\nStarts from the last value used on this review; ▶ runs with whatever is here.">
              ${setBtn}
            </div>
          </div>`;
        }).join("");
        const cleanupField = check.cleanup_command
          ? `<label style="display:flex;align-items:center;gap:5px;font-size:11px;margin:7px 0 0;color:var(--muted)" data-tip="Runs '${esc(check.cleanup_command)}' immediately before the main command — e.g. to stop a stale process from a previous run.\nWho/when: the command binds a port or leaves something running that a rerun would collide with.\nThis cannot be undone once the cleanup command executes.">
              <input type="checkbox" id="check-cleanup:${key}"> Stop stale process first
            </label>`
          : "";
        return `<div class="cmd-params">${fields}${cleanupField}</div>`;
      }
      /**
       * The values a parameterised check would run with right now: whatever is
       * typed into its fields if they are on screen, otherwise the review's
       * stored value for that input, otherwise the input's own default.
       *
       * Both the preview and the run path read this, so what the expanded panel
       * shows is exactly what ▶ executes -- including when the panel is closed
       * and there are no fields to read.
       * @param {GuardianView} g - The review.
       * @param {GuardianCheck} check - The check.
       * @param {string} key - The command key, `<gid>:<kind>:<index>`.
       * @returns {{[name: string]: string}}
       */
      function checkInputValues(g, check, key) {
        const stored = g.input_values || {};
        /** @type {{[name: string]: string}} */
        const out = {};
        for (const inp of check.inputs || []) {
          const el = /** @type {HTMLInputElement|null} */ (document.getElementById(`check-input:${key}:${inp.name}`));
          out[inp.name] = el
            ? el.value
            : String(Object.prototype.hasOwnProperty.call(stored, inp.name) ? stored[inp.name] : (inp.default ?? ""));
        }
        return out;
      }
      /**
       * Substitutes `{name}` placeholders in a command with the given values.
       * @param {string} cmd - The command template.
       * @param {{[name: string]: string}} values - Value per input name.
       * @returns {string}
       */
      function substituteCheckInputs(cmd, values) {
        let out = cmd;
        for (const [name, value] of Object.entries(values)) {
          out = out.split(`{${name}}`).join(value);
        }
        return out;
      }
      /**
       * Repaints a parameterised check's command preview as its fields change.
       *
       * Patches the one element rather than re-rendering the pane, so typing
       * doesn't cost the caret its position.
       * @param {string} key - The command key, `<gid>:<kind>:<index>`.
       * @returns {void}
       */
      function onCheckInputChange(key) {
        const el = document.getElementById(`cmd-preview-${key}`);
        if (!el) return;
        const [gid, kind, idx] = key.split(":");
        const g = guardians.find((x) => x.id === gid);
        if (!g) return;
        const check = (kind === "manual" ? (g.manual_commands || []) : (g.action_hints || []))[Number(idx)];
        if (!check) return;
        el.textContent = substituteCheckInputs(check.command || "", checkInputValues(g, check, key));
      }
      // Render the inline cell-link button for a review branch row.
      // Prefers direct squad/task/cell indices from the API; falls back to cwd path matching.
      /**
       * Renders a review branch row's linked-cell button (direct index, path-linked, or none).
       * @param {GuardianBranch} branch
       * @param {string} menuKey
       * @returns {string}
       */
      function worktreeCellBtn(branch, menuKey) {
        if (branch.source_squad_id != null && branch.source_task_idx != null && branch.source_cell_idx != null) {
          const ti = branch.source_task_idx, si = branch.source_cell_idx;
          const squad = squads.find(r => r.id === branch.source_squad_id);
          const t = squad && squad.tasks[ti];
          const s = t && t.cells[si];
          const state = s ? s.state : (branch.source_cell_state || "pending");
          const name = s ? (s.name ?? s.id) : branch.branch;
          const taskName = t ? t.name : "";
          const readyDot = state === "done" ? ` <span class="dot" style="background:var(--done)"></span>` : "";
          const tip = `Open the task cell that worked on this review branch.\nCell: ${esc(name)} · Task: ${esc(taskName)} · State: ${esc(state)}\nSwitches to the Squads tab and opens this cell in the detail pane.`;
          return `<button class="btn" data-click="gotoSquadItem" data-squad-id="${esc(branch.source_squad_id)}" data-kind="cell" data-ti="${ti}" data-si="${si}" data-vi="-1" data-tip="${tip}">${sdot(state)} ${esc(name)}${readyDot}</button>`;
        }
        // Fallback: path-based linking (manually added branches with no submitted cell).
        const cells = branch.worktree ? findLinkedCells(branch.worktree) : [];
        if (!cells.length) {
          return `<span data-tip="No task cells are linked to this review branch.\nThe squad may not have started yet, or the cell may not have a review_branch set."><button class="btn" disabled style="opacity:0.5;cursor:not-allowed">◉ No cells</button></span>`;
        }
        const allDone = cells.every(s => s.state === "done");
        const readyDot = allDone ? ` <span class="dot" style="background:var(--done)"></span>` : "";
        if (cells.length === 1) {
          const s = cells[0];
          const tip = `Open the task cell that worked on this review branch.\nCell: ${esc(s.name)} · Task: ${esc(s.taskName)} · State: ${esc(s.state)}\nSwitches to the Squads tab and opens this cell in the detail pane.`;
          return `<button class="btn" data-click="gotoSquadItem" data-squad-id="${esc(s.squadId)}" data-kind="cell" data-ti="${s.taskIdx}" data-si="${s.cellIdx}" data-vi="-1" data-tip="${tip}">${sdot(s.state)} ${esc(s.name)}${readyDot}</button>`;
        }
        const worst = CELL_STATE_RANK.find(r => cells.some(s => s.state === r)) || cells[0].state;
        const isOpen = !!worktreeMenuOpen[menuKey];
        const tip = `${cells.length} task cells are linked to this review branch — click to pick one.\nEach entry shows the cell name and its current state.`;
        const items = cells.map(s =>
          `<div data-click="gotoWorktreeCell" data-squad-id="${esc(s.squadId)}" data-ti="${s.taskIdx}" data-si="${s.cellIdx}" data-tip="${esc(s.name)} · ${esc(s.taskName)} · state: ${esc(s.state)}" style="padding:5px 10px;cursor:pointer;display:flex;align-items:center;gap:6px;font-size:12px" onmouseover="this.style.background='var(--panel-2)'" onmouseout="this.style.background=''">
            ${sdot(s.state)}<span style="flex:1">${esc(s.name)}</span><span style="color:var(--muted)">${esc(s.state)}</span>
          </div>`
        ).join("");
        return `<div style="position:relative;display:inline-block">
          <button class="btn" data-click="toggleWorktreeMenu" data-key="${esc(menuKey)}" data-tip="${tip}">${sdot(worst)} ${cells.length} cells ▾${readyDot}</button>
          ${isOpen ? `<div style="position:absolute;left:0;top:100%;background:var(--panel);border:1px solid var(--border);border-radius:6px;min-width:220px;z-index:50;box-shadow:0 4px 12px rgba(0,0,0,.4);padding:4px 0;margin-top:2px">${items}</div>` : ""}
        </div>`;
      }

      // ---------- reviews (list + detail, read-only for now) ----------
      /** @type {{[key: string]: string}} */
      const G_COLORS = { collecting:"--muted", merging:"--running", merge_failed:"--failed", merge_stopped:"--pending", in_review:"--accent", merged:"--done", approved:"--done", cancelled:"--cancelled", deployed:"--done", pending:"--pending", ready:"--teal", in_progress:"--running", actioning:"--running", done:"--done", proof_pending:"--running", conflict_resolved:"--queued", failed:"--failed", closed:"--cancelled" };
      // States in which a review may be cancelled — mirrors the backend's
      // cancel_guardian() (daemon/src/guardian.rs). Both the left-hand review
      // list menu and the detail pane's upper-right ⋯ menu use this one set so
      // "Cancel review" appears in the same states everywhere.
      const G_CANCELLABLE = ["collecting", "merging", "merge_failed", "merge_stopped", "in_review", "merged"];
      /**
       * Renders a colored status dot for a guardian/branch/merge state.
       * @param {string} s
       * @returns {string}
       */
      const gdot = (s) => {
        const safe = safeState(s);
        const color = Object.prototype.hasOwnProperty.call(G_COLORS, safe) ? G_COLORS[safe] : "--muted";
        return `<span class="dot" style="background:${cvar(color)}"></span>`;
      };
      /**
       * Renders the small provenance badge for a review origin string
       * ("explicit" or "arbiter"), or an empty string for "explicit" (the
       * default -- no badge needed). Shared by `arbiterBadge()` (which has a
       * full `GuardianView`) and any caller that only has the plain origin
       * string, e.g. a cell's `SquadReviewRef`.
       * @param {string} origin
       * @returns {string}
       */
      function originBadge(origin) {
        return origin === "arbiter"
          ? `<span class="badge" style="color:var(--arbiter);border-color:var(--arbiter);font-size:11px" data-tip="This review was created automatically by the Arbiter/Triage subsystem when a pooled cell count threshold or cron schedule fired, draining cells from possibly-multiple past submissions into one fresh review -- not from an authored [[review]] block.">⚙ Arbiter</span>`
          : "";
      }
      /**
       * Renders the small provenance badge for a review the Arbiter/Triage
       * subsystem created automatically (RAL-318), or an empty string for an
       * explicitly-authored review (the default -- no badge needed).
       * @param {GuardianView} g
       * @returns {string}
       */
      function arbiterBadge(g) {
        return originBadge(originOf(g));
      }
      /**
       * Selects a review and shows its detail pane.
       * @param {string} id
       * @returns {void}
       */
      function selectGuardian(id) { selectedGuardian = id; revealedGuardianId = id; syncHash(true); renderReviews(); renderReviewDetail(); ensureGuardianDetailLoaded(id); }
      /**
       * Renders the review list in the Reviews tab's sidebar.
       * @returns {void}
       */
      function renderReviews() {
        const el = byId("reviews");
        renderReviewQuickFilters();
        renderReviewStatusFilters();
        renderReviewResolverFilters();
        renderReviewOriginFilters();
        renderReviewPrStatusFilter();
        if (!guardians.length) { el.innerHTML = `<div class="empty">No reviews.</div>`; return; }
        const list = visibleGuardians();
        if (!list.length) { el.innerHTML = `<div class="empty">No matching reviews.</div>`; return; }
        const bulkBar = guardianMultiSel.size > 1 ? reviewSelectionBar() : "";
        // The ⋯ shares the title's line (the `.squad-row`/`.squad-actions` frame
        // the Squads list uses) rather than claiming one of its own -- a third
        // line per row costs a third of the list you can see at once.
        el.innerHTML = bulkBar + list.map((g) => `<div class="squad-item ${(g.id===selectedGuardian || guardianMultiSel.has(g.id))?"selected":""}" data-click="onReviewClick" data-ctx="openReviewMenu" data-guardian-id="${esc(g.id)}">
          <div class="squad-row">
            ${gdot(g.status)}<div class="rid">${hiddenGuardianIds.has(g.id) ? `<span data-tip="You've hidden this review from your own view.\nIt's shown now because \"show hidden\" is on, or you navigated to it directly.\nA personal preference — it does not affect what other users see.">🙈</span> ` : ""}${esc(g.name)} ${arbiterBadge(g)}</div>
            <div class="squad-actions"><button class="btn squadbtn" data-click="openReviewMenu" data-guardian-id="${esc(g.id)}" data-tip="Review actions — rename, hide, cancel, or delete this review.">⋯</button></div>
          </div>
          <div class="meta">${pill(g.status)}${reviewSparkbar(g)}</div></div>`).join("");
      }
      /**
       * A review's branch stack as a row of segments, one per branch, coloured
       * by that branch's merge state -- so the list says how far along each
       * review is, not just what status it carries. A lean index entry has no
       * `branches` yet (and must not be made to fetch them just for this), so
       * it falls back to the plain count it does carry.
       * @param {GuardianView} g - The review, lean or full.
       * @returns {string}
       */
      function reviewSparkbar(g) {
        const branches = g.branches || null;
        const total = branches ? branches.length : (g.branch_count || 0);
        if (!total) return `<span class="spark-count">no branches</span>`;
        if (!branches) {
          return `<span class="spark-count" data-tip="This review's branch detail loads when you open it.">${total} branches</span>`;
        }
        const done = branches.filter((b) => b.enabled !== false
          && ["done", "merged", "conflict_resolved"].includes(b.merge_status || "")).length;
        const enabled = branches.filter((b) => b.enabled !== false).length;
        const segs = branches.map((b) => {
          const cls = b.enabled === false ? "off" : `s-${esc(b.merge_status || "pending")}`;
          return `<i class="${cls}"></i>`;
        }).join("");
        return `<span class="spark" data-tip="${esc(`${done} of ${enabled} enabled branch(es) merged. Each segment is one branch, coloured by its state.`)}">${segs}</span>`
          + `<span class="spark-count">${done}/${enabled}</span>`;
      }
      /**
       * Handles a click on a review row: plain select, ctrl/cmd toggle, or
       * shift range-select (RAL-331, mirrors onSquadClick).
       * @param {MouseEvent} e
       * @param {string} id
       * @returns {void}
       */
      function onReviewClick(e, id) {
        if (e.shiftKey && guardianAnchorId) {
          const vis = visibleGuardians().map((g) => g.id);
          const a = vis.indexOf(guardianAnchorId), b = vis.indexOf(id);
          if (a >= 0 && b >= 0) { const lo = Math.min(a, b), hi = Math.max(a, b); guardianMultiSel = new Set(vis.slice(lo, hi + 1)); }
        } else if (e.ctrlKey || e.metaKey) {
          if (guardianMultiSel.has(id)) guardianMultiSel.delete(id); else guardianMultiSel.add(id);
          guardianAnchorId = id;
        } else {
          guardianMultiSel = new Set([id]); guardianAnchorId = id;
        }
        selectGuardian(id);
      }
      /**
       * Renders the Reviews sidebar's multi-select bulk hide/unhide bar (RAL-331).
       * @returns {string}
       */
      function reviewSelectionBar() {
        return `<div class="btn-row" style="margin:0 0 8px">
          <span style="color:var(--muted);font-size:12px;align-self:center">${guardianMultiSel.size} selected</span>
          <button class="btn" onclick="bulkHideReviews()" data-tip="Hide all ${guardianMultiSel.size} selected reviews from your own view.\nWho/when: use this after shift-selecting a range of reviews you want to declutter at once.\nA personal preference — it never affects what other users see or any review's status.">🙈 Hide ${guardianMultiSel.size}</button>
          <button class="btn" onclick="bulkUnhideReviews()" data-tip="Re-enable all ${guardianMultiSel.size} selected reviews in your own view, if hidden.">👁 Unhide ${guardianMultiSel.size}</button>
        </div>`;
      }
      // CCTL-100: right-click a review for Rename / Delete (mirrors the squad menu).
      // RAL-508: when the clicked review is part of an active multi-selection,
      // every applicable action targets the whole selection: single-item-only
      // actions (Edit Details, Watch) are disabled rather than silently
      // dropping the rest of the selection, and everything else applies once
      // per selected review.
      /**
       * Opens the review right-click context menu.
       * @param {MouseEvent} e
       * @param {string} id
       * @returns {void}
       */
      // RALPHUS-REVIEW-MENU:BEGIN
      function openReviewMenu(e, id) {
        e.preventDefault(); e.stopPropagation(); closeSquadMenu();
        const g = guardians.find((x) => x.id === id); if (!g) return;
        const menu = document.createElement("div");
        menu.className = "ctx-menu"; menu.id = "squad-menu";
        const menuBatchSize = menuActionTargets(id, guardianMultiSel).length;
        /**
         * @param {string} what
         * @returns {string}
         */
        const singleOnlyTip = (what) => esc(`${what} works on a single review, so it is disabled while multiple reviews are selected.\nClick the review on its own (plain click, no Ctrl/Shift) to narrow the selection to it, then right-click it again.`);
        const batchTip = menuBatchSize > 1 ? `\nApplies to all ${menuBatchSize} selected reviews; any that aren't eligible for this action are skipped and reported.` : "";
        const canCancel = G_CANCELLABLE.includes(g.status);
        const items = [menuBatchSize > 1
          ? `<div class="ctx-disabled" data-tip="${singleOnlyTip("Edit Details")}">✎ Edit Details</div>`
          : `<div data-click="openEditReviewDetailsFromMenu" data-guardian-id="${esc(id)}" data-tip="Edit this review's name and other settings.">✎ Edit Details</div>`];
        const reviewUri = `guardian:${id}`;
        items.push(menuBatchSize > 1
          ? `<div class="ctx-disabled" data-tip="${singleOnlyTip("Watch")}">${isWatching(reviewUri) ? "◉ Unwatch" : "◎ Watch…"}</div>`
          : `<div data-click="toggleWatch" data-entity-uri="${esc(reviewUri)}" data-tip="${isWatching(reviewUri) ? "Stop receiving watcher notifications for this review." : "Watch this whole review and choose which mailbox priority tiers should notify you."}">${isWatching(reviewUri) ? "◉ Unwatch" : "◎ Watch…"}</div>`);
        items.push(menuBatchSize > 1
          ? `<div class="ctx-disabled" data-tip="${singleOnlyTip("Add to waypoint")}">📍 Add to waypoint…</div>`
          : `<div onclick="openAddToWaypointMenu(event,'review','${esc(id)}')" data-tip="Add this review to a cross-squad waypoint's affected list, or create a new waypoint from it.">📍 Add to waypoint…</div>`);
        if (menuBatchSize > 1) {
          items.push(`<div data-click="hideReviewMenuItem" data-guardian-id="${esc(id)}" data-tip="Hide all ${menuBatchSize} selected reviews from your own view — they stay fully intact and keep running/counting normally.\nWho/when: use this to declutter your list of reviews you don't need to watch right now.\nA personal preference — it never affects what other users see, and can be undone any time via \"show hidden\".">🙈 Hide ${menuBatchSize}</div>`);
          items.push(`<div data-click="unhideReviewMenuItem" data-guardian-id="${esc(id)}" data-tip="Show all ${menuBatchSize} selected reviews again in your own view, if hidden.\nWho/when: use this to undo an earlier hide across a whole selection.\nA personal preference — it never affects what other users see.">👁 Unhide ${menuBatchSize}</div>`);
        } else {
          items.push(hiddenGuardianIds.has(id)
            ? `<div data-click="unhideReviewMenuItem" data-guardian-id="${esc(id)}" data-tip="Show this review again in your own view.\nWho/when: use this to undo an earlier hide.\nA personal preference — it never affects what other users see.">👁 Unhide</div>`
            : `<div data-click="hideReviewMenuItem" data-guardian-id="${esc(id)}" data-tip="Hide this review from your own view — it stays fully intact and keeps running/counting normally.\nWho/when: use this to declutter your list of reviews you don't need to watch right now.\nA personal preference — it never affects what other users see, and can be undone any time via \"show hidden\".">🙈 Hide</div>`);
        }
        items.push(`<div data-click="mergeReviewFromMenu" data-guardian-id="${esc(id)}" data-tip="Start (or resume) a fresh merge/rebase — rebases each enabled branch onto the base, resolving conflicts with the AI agent, then runs check gates.\nWho/when: use this to force a rebase right now instead of waiting for the automatic one, e.g. right after enabling/disabling branches, or on a review with automatic rebasing turned off.\nRuns regardless of this review's 'Skip automatic rebasing' setting -- that setting only gates the automatic sweep, not this manual trigger.\nReviews not currently eligible (already merging, merged, cancelled, or deployed) are skipped and reported.${esc(batchTip)}">⇄ Merge / rebase</div>`);
        if (canCancel) items.push(`<div class="danger" data-click="cancelReview" data-guardian-id="${esc(id)}" data-tip="Cancel this review — stops the current merge and discards its result.\nThe review can be restarted afterward.\nThis cannot be undone.${esc(batchTip)}">⊘ Cancel review</div>`);
        if (g.status === "cancelled") items.push(`<div data-click="reopenReview" data-guardian-id="${esc(id)}" data-tip="Reopen this cancelled review and immediately stage in whatever branches are already ready, without waiting for the rest.\nUse this when a review was cancelled by mistake, or you want to retry it without recreating it from scratch.\nAny branch still waiting on its task keeps the review in collecting until it finishes.${esc(batchTip)}">↺ Reopen review</div>`);
        items.push(`<div class="danger" data-click="deleteReview" data-guardian-id="${esc(id)}" data-tip="Delete this review and remove all review worktrees permanently.\nThis cannot be undone.${esc(batchTip)}">🗑 Delete</div>`);
        menu.innerHTML = items.join("");
        document.body.appendChild(menu);
        menu.style.left = Math.min(e.clientX, window.innerWidth - 180) + "px";
        menu.style.top = Math.min(e.clientY, window.innerHeight - 90) + "px";
      }
      // RALPHUS-REVIEW-MENU:END
      /**
       * Resolves the display label a bulk action's report/confirmation names a review by.
       * @param {string} id
       * @returns {string}
       */
      const reviewLabelOf = (id) => guardians.find((x) => x.id === id)?.name || id;
      /**
       * Starts merge/rebase from the review context menu (RAL-514). Applies
       * to every selected review when the clicked one is part of a
       * multi-selection via bulkMergeReviews instead; otherwise defers to
       * the single-review `mergeReview`, which already knows how to read
       * that review's own current status (e.g. resuming vs. restarting).
       * @param {string} id
       * @returns {Promise<void>}
       */
      async function mergeReviewFromMenu(id) {
        closeSquadMenu();
        const ids = menuActionTargets(id, guardianMultiSel);
        if (ids.length > 1) { await bulkMergeReviews(ids); return; }
        const g = guardians.find((x) => x.id === id);
        await mergeReview(id, g ? g.status : "");
      }
      /**
       * Starts merge/rebase for every selected review whose status currently
       * allows it (RAL-514) -- mirrors the single-review "Merge / rebase"
       * button's own eligibility (`MERGE_STARTABLE`), so a review that's
       * e.g. already merging, already merged, cancelled, or deployed is
       * skipped client-side and reported rather than attempted. Runs
       * unconditionally regardless of each review's own "skip automatic
       * rebasing" setting -- that setting only gates the automatic
       * base-shift sweep, not this explicit manual trigger. Sends every
       * remaining id in one request to `POST /api/guardians/merge-batch`
       * rather than looping a per-review call, and reports the server's
       * three-way started/not_applicable/failed outcome per review -- a
       * review the daemon finds already merged or already mid-rebase had
       * nothing to do, which is distinct from a real failure.
       * @param {string[]} ids
       * @returns {Promise<void>}
       */
      // RALPHUS-REVIEW-BULK-MERGE:BEGIN
      async function bulkMergeReviews(ids) {
        const { eligible, skipped } = bulkEligibleSplit(ids, (gid) => MERGE_STARTABLE.includes(guardians.find((x) => x.id === gid)?.status || ""));
        if (!eligible.length) return;
        if (!confirm(`Start merge/rebase for ${eligible.length} review(s)? Each rebases onto its base branch, resolving conflicts with the AI agent.\n\n${bulkNameList(eligible.map(reviewLabelOf))}`)) return;
        let resp;
        try {
          resp = await post("/api/guardians/merge-batch", { ids: eligible });
        } catch (e) { notify("error", "daemon unreachable"); return; }
        if (!resp.ok) { notify("error", await responseError(resp, "merge/rebase failed")); return; }
        /** @type {GuardianMergeBatchResponse} */
        const result = await resp.json();
        const started = result.results.filter((r) => r.outcome === "started");
        const notApplicable = result.results.filter((r) => r.outcome === "not_applicable");
        const failed = result.results.filter((r) => r.outcome === "failed");
        if (skipped > 0) notify("error", `Skipped ${skipped} of the selected review(s): not in a mergeable/rebaseable status.`);
        if (notApplicable.length) notify("error", `${notApplicable.length} review(s) had nothing to do: ${notApplicable.map((r) => `${reviewLabelOf(r.id)} (${r.message})`).join("; ")}.`);
        if (failed.length) notify("error", failed.map((r) => `${reviewLabelOf(r.id)}: ${r.message}`).join("; "));
        else if (started.length > 0 && skipped === 0 && notApplicable.length === 0) notify("success", `Started merge/rebase for ${started.length} review(s).`);
        tick();
      }
      // RALPHUS-REVIEW-BULK-MERGE:END
      /**
       * Deletes a review and its worktrees after confirmation. Applies to
       * every selected review when the clicked one is part of a
       * multi-selection (RAL-508) via bulkDeleteReviews instead.
       * @param {string} id
       * @returns {Promise<void>}
       */
      async function deleteReview(id) {
        closeSquadMenu();
        const ids = menuActionTargets(id, guardianMultiSel);
        if (ids.length > 1) { await bulkDeleteReviews(ids); return; }
        const g = guardians.find((x) => x.id === id);
        if (!confirm(`Delete review "${g ? g.name : id}"? This removes its review worktrees and cannot be undone.`)) return;
        await del(`/api/guardians/${id}`, { success: `Review "${g ? g.name : id}" deleted.`, errorLabel: "delete review" });
        if (selectedGuardian === id) selectedGuardian = null;
        guardianMultiSel.delete(id);
        tick();
      }
      /**
       * Deletes every selected review after one shared confirmation naming
       * them all (RAL-508). Sends every id in one request to
       * `POST /api/guardians/delete-batch` (RAL-549) rather than looping a
       * per-review DELETE call -- worktree/branch cleanup for a review
       * whose worktree is held by an in-progress merge can be slow, and a
       * sequential per-id loop let one stuck review stall the whole batch.
       * Reports any review the daemon refused to delete via notify() --
       * one review failing does not block the rest of the batch; deleted
       * reviews leave the selection either way.
       * @param {string[]} ids
       * @returns {Promise<void>}
       */
      // RALPHUS-REVIEW-BULK-DELETE:BEGIN
      async function bulkDeleteReviews(ids) {
        if (!ids.length) return;
        if (!confirm(`Delete ${ids.length} review(s)? This removes their review worktrees and cannot be undone.\n\n${bulkNameList(ids.map(reviewLabelOf))}`)) return;
        let resp;
        try {
          resp = await post("/api/guardians/delete-batch", { ids });
        } catch (e) { notify("error", "daemon unreachable"); return; }
        if (!resp.ok) { notify("error", await responseError(resp, "delete review failed")); return; }
        /** @type {GuardianDeleteBatchResponse} */
        const result = await resp.json();
        const deleted = result.results.filter((r) => r.outcome === "deleted");
        const failed = result.results.filter((r) => r.outcome === "failed");
        if (failed.length) notify("error", failed.map((r) => `${reviewLabelOf(r.id)}: ${r.message}`).join("; "));
        else notify("success", `Deleted ${deleted.length} review(s).`);
        for (const gid of ids) {
          if (selectedGuardian === gid) selectedGuardian = null;
          guardianMultiSel.delete(gid);
        }
        tick();
      }
      // RALPHUS-REVIEW-BULK-DELETE:END
      /**
       * Hides or unhides a review from the current user's own view (RAL-331) --
       * a personal preference that never changes the review itself or what any
       * other user sees. Only updates local state once the daemon confirms
       * the change (e.g. an unresolved current user 400s the request) --
       * an unconditional optimistic update would make a failed hide look
       * like it worked until the next full reload silently reverted it.
       * No confirmation prompt, since it's reversible via "show hidden".
       * @param {string} id
       * @param {boolean} hide
       * @returns {Promise<void>}
       */
      async function setReviewHidden(id, hide) {
        closeSquadMenu();
        let resp;
        try {
          resp = await (hide ? post(`/api/hidden/reviews/${id}`) : del(`/api/hidden/reviews/${id}`));
        } catch (e) { notify("error", "daemon unreachable"); return; }
        if (!resp.ok) { notify("error", await responseError(resp, hide ? "hide failed" : "unhide failed")); return; }
        if (hide) hiddenGuardianIds.add(id); else hiddenGuardianIds.delete(id);
        renderReviews();
      }
      /**
       * Hides or unhides from the review context menu (RAL-508) -- applies to
       * every multi-selected review when the clicked review is part of an
       * active multi-selection (size > 1), otherwise just the clicked review.
       * @param {string} id
       * @param {boolean} hide
       * @returns {Promise<void>}
       */
      // RALPHUS-REVIEW-HIDE:BEGIN
      function setReviewHiddenFromMenu(id, hide) {
        if (guardianMultiSel.has(id) && guardianMultiSel.size > 1) return (hide ? bulkHideReviews() : bulkUnhideReviews());
        return setReviewHidden(id, hide);
      }
      /**
       * Hides every multi-selected review from the current user's own view
       * (RAL-331). Reports any review the daemon refused to hide via notify().
       * @returns {Promise<void>}
       */
      async function bulkHideReviews() {
        const failed = [];
        for (const id of [...guardianMultiSel]) {
          try {
            const r = await post(`/api/hidden/reviews/${id}`);
            if (r.ok) hiddenGuardianIds.add(id); else failed.push(`${id}: ${await responseError(r, "hide failed")}`);
          } catch (e) { failed.push(`${id}: daemon unreachable`); }
        }
        renderReviews();
        if (failed.length) notify("error", failed.join("; "));
        else notify("success", `${guardianMultiSel.size} review(s) hidden.`);
      }
      /**
       * Re-enables every multi-selected review in the current user's own view
       * (RAL-331). Reports any review the daemon refused to unhide via notify().
       * @returns {Promise<void>}
       */
      async function bulkUnhideReviews() {
        const failed = [];
        for (const id of [...guardianMultiSel]) {
          try {
            const r = await del(`/api/hidden/reviews/${id}`);
            if (r.ok) hiddenGuardianIds.delete(id); else failed.push(`${id}: ${await responseError(r, "unhide failed")}`);
          } catch (e) { failed.push(`${id}: daemon unreachable`); }
        }
        renderReviews();
        if (failed.length) notify("error", failed.join("; "));
        else notify("success", `${guardianMultiSel.size} review(s) unhidden.`);
      }
      // RALPHUS-REVIEW-HIDE:END
      /**
       * The review's primary action, as a split button: the thing you came to
       * press, with everything else behind its ▾.
       *
       * The actions used to sit in a row at the foot of the pane, below the
       * branch stack -- so the one control a reviewer reaches for most was
       * wherever the stack happened to end, and scrolled off on a long review.
       * Rebasing is the primary slot rather than Approve: it is what gets
       * pressed repeatedly while a review is being worked, where Approve is
       * pressed once at the end and is a decision, not a step.
       * @param {GuardianView} g - The review.
       * @returns {string}
       */
      function reviewPrimaryAction(g) {
        // Reopen takes the primary slot once rebasing is a dead end, so the
        // button is never a permanently greyed-out stub.
        const view = REOPEN_ELIGIBLE.includes(g.status)
          ? { ...reopenButtonView(g.status, pendingMergeActions.has(g.id)), action: "reopenReview" }
          : { ...mergeButtonView(g.status, pendingMergeActions.has(g.id)), action: "mergeReview" };
        const tip = esc(view.tip);
        const btn = `<button class="btn primary split-main" data-click="${view.action}" data-guardian-id="${esc(g.id)}" `
          + `data-status="${esc(g.status)}" ${view.enabled ? "" : "disabled"} data-tip="${tip}">${esc(view.label)}</button>`;
        // A disabled <button> swallows mouseover, so its tooltip lives on a
        // wrapper -- the same reason every other gated button here does this.
        return `<span class="split">${view.enabled ? btn : `<span data-tip="${tip}">${btn}</span>`}`
          + `<button class="btn primary split-caret" data-click="openReviewTitleMenu" data-guardian-id="${esc(g.id)}" `
          + `data-tip="Every other action for this review — approving it, its PR stack, and the destructive ones.">▾</button></span>`;
      }
      /**
       * Opens the review's action menu, grouped by what each action acts on:
       * the review as a whole, or its branch stack.
       * @param {MouseEvent} e
       * @param {string} id
       * @returns {void}
       */
      function openReviewTitleMenu(e, id) {
        e.preventDefault(); e.stopPropagation(); closeSquadMenu();
        const g = guardians.find((x) => x.id === id); if (!g) return;
        const canCancel = G_CANCELLABLE.includes(g.status);
        const menu = document.createElement("div");
        menu.className = "ctx-menu"; menu.id = "squad-menu";
        const cancelItem = canCancel
          ? `<div class="danger" data-click="cancelReview" data-guardian-id="${esc(id)}" data-tip="Cancel this review — stops the current merge and discards its result.\nThe review can be restarted afterward.\nThis cannot be undone.">⊘ Cancel review</div>`
          : g.status === "cancelled"
            ? `<div data-click="reopenReview" data-guardian-id="${esc(id)}" data-tip="Reopen this cancelled review and immediately stage in whatever branches are already ready, without waiting for the rest.\nUse this when a review was cancelled by mistake, or you want to retry it without recreating it from scratch.\nAny branch still waiting on its task keeps the review in collecting until it finishes.">↺ Reopen review</div>`
            : `<div style="color:var(--muted);padding:6px 10px;font-size:12px" data-tip="No actions are available because this review's status is '${g.status}'.\nActions like Cancel are only available while the review is collecting, merging, merge_failed, in_review, or merged.">No actions available</div>`;
        const stacksItem = `<div data-click="openReviewPrStacks" data-guardian-id="${esc(id)}" data-tip="View every PR stack previously submitted for this review, in any state -- including ones dropped because their linked PR merged on the forge while the review was still mid-flight (RAL-300).\nWho/when: use this to see what was submitted before deciding whether/how to resubmit.\nRead-only -- does not resubmit or replay anything.">📜 View past PR stacks</div>`;
        // PR submission used to be its own section in the pane. A whole section
        // for one button and a read-only settings summary was more weight than
        // it earned -- each branch's PR state is on its own row and in the
        // inspector, which is where you read it, so the action lives here with
        // the review's other actions instead.
        const submitItem = g.branches && g.branches.some((b) => b.enabled && b.worktree)
          ? `<div data-click="submitPrStack" data-guardian-id="${esc(id)}" data-tip="Push every enabled branch in this review as its own PR, each based on the branch below it -- never one squashed PR containing everything.\nOn GitHub, also registers/grows a native PR stack so GitHub's own UI shows them as a linked stack.\nSafe to press again after adding a branch on top: only the new branch gets its own PR.\nRuns in the background; each branch's PR chip updates as its forge call completes.${esc(g.effective_auto_submit_pr_stack ? "\nAuto-submit is on for this review, so this normally happens on its own." : "")}">⇧ Submit PR stack</div>`
          : "";
        // Approve moved here from the foot of the pane. It is a decision made
        // once, not a step repeated while working, so it belongs with the other
        // one-off actions rather than beside the button you press all day.
        const approveDecided = g.status === "merged" || g.status === "approved";
        const approvePending = pendingGuardianActions.has(id);
        const approveItem = (approveDecided || approvePending)
          ? `<div class="ctx-disabled" data-tip="${esc(approvePending
            ? "Approval is in flight — waiting for the daemon to confirm."
            : `This review is already ${g.status}.\nReopen it to make further changes, then approve again.`)}">✓ ${approvePending ? "Approving…" : "Approve"}</div>`
          : `<div data-click="approveReview" data-guardian-id="${esc(id)}" data-tip="Approve this review — marks it as approved, whether or not its PR/MR has merged.\nCan be pressed from in_review or merge_stopped; the daemon rejects it if the review isn't in a state that can be approved yet.">✓ Approve</div>`;
        const stopItem = g.status === "merging"
          ? `<div data-click="stopMerge" data-guardian-id="${esc(id)}" data-tip="Stop this rebase mid-flight — halts at the next checkpoint and pauses the review.\nThe review and its branches are kept, so you can resume the rebase afterward.\nThis is not a cancel: nothing is discarded.">⏸ Stop rebase</div>`
          : "";
        const canSync = ["in_review", "merging"].includes(g.status);
        const syncItem = canSync
          ? `<div data-click="syncPrReview" data-guardian-id="${esc(id)}" data-tip="Check GitHub/GitLab for a stack reorder made outside ralphus (e.g. dragging PRs into a new order) and apply it here, retriggering a rebase.\nRuns in the background; watch this review's branch order/status for the result.\nAlso happens automatically every 5 minutes for reviews with an active stack.">⇅ Sync PR order</div>`
          : `<div class="ctx-disabled" data-tip="Not available — a stack reorder can only be detected once this review has an open PR stack (status in_review or merging).">⇅ Sync PR order</div>`;
        menu.innerHTML = `<div class="ctx-group">review</div>`
          + approveItem + submitItem + syncItem + stopItem
          + `<div class="ctx-sep"></div><div class="ctx-group">stack</div>`
          + stacksItem + cancelItem;
        document.body.appendChild(menu);
        // Opened from a button at the pane's top-right, so it is anchored to
        // that button rather than to the pointer -- a menu that lands under the
        // cursor would cover the control it belongs to.
        const btn = /** @type {HTMLElement|null} */ (
          /** @type {HTMLElement} */ (e.target).closest("[data-click]"));
        const r = (btn || /** @type {HTMLElement} */ (e.target)).getBoundingClientRect();
        menu.style.left = Math.max(8, Math.min(r.right - menu.offsetWidth, window.innerWidth - menu.offsetWidth - 8)) + "px";
        menu.style.top = Math.min(r.bottom + 4, window.innerHeight - menu.offsetHeight - 8) + "px";
      }
      /**
       * Cancels a review's in-progress merge after confirmation. Applies to
       * every selected review when the clicked one is part of a
       * multi-selection (RAL-508) via bulkCancelReviews instead.
       * @param {string} id
       * @returns {Promise<void>}
       */
      async function cancelReview(id) {
        closeSquadMenu();
        const ids = menuActionTargets(id, guardianMultiSel);
        if (ids.length > 1) { await bulkCancelReviews(ids); return; }
        const g = guardians.find((x) => x.id === id);
        if (!confirm(`Cancel review "${g ? g.name : id}"? The current merge will be discarded. This cannot be undone.`)) return;
        await guardianAction(`/api/guardians/${id}/cancel`);
        tick();
      }
      /**
       * Cancels every selected cancellable review after one shared
       * confirmation naming them all (RAL-508). Reviews whose status can't be
       * cancelled are skipped and reported; per-item failures are reported
       * without blocking the rest of the batch.
       * @param {string[]} ids
       * @returns {Promise<void>}
       */
      // RALPHUS-REVIEW-BULK-CANCEL:BEGIN
      async function bulkCancelReviews(ids) {
        const { eligible, skipped } = bulkEligibleSplit(ids, (gid) => G_CANCELLABLE.includes(guardians.find((x) => x.id === gid)?.status || ""));
        if (!eligible.length) return;
        if (!confirm(`Cancel ${eligible.length} review(s)? The current merge of each will be discarded. This cannot be undone.\n\n${bulkNameList(eligible.map(reviewLabelOf))}`)) return;
        const failed = await bulkActEach(eligible, reviewLabelOf, (gid) => post(`/api/guardians/${gid}/cancel`), "cancel review failed");
        reportBulkOutcome("Cancelled", "review", eligible.length, skipped, "not in a cancellable status", failed);
        tick();
      }
      // RALPHUS-REVIEW-BULK-CANCEL:END
      /**
       * Reopens a cancelled or merged review, immediately trying a fresh
       * merge pass. Shares its pending/disabled tracking with the
       * "Merge / rebase" button (`pendingMergeActions`) since "Reopen"
       * occupies that same button slot once a review is cancelled or
       * merged (see `REOPEN_ELIGIBLE`).
       * Applies to every selected review when the clicked one is part of a
       * multi-selection (RAL-508) via bulkReopenReviews instead.
       * @param {string} id
       * @returns {Promise<void>}
       */
      async function reopenReview(id) {
        const ids = menuActionTargets(id, guardianMultiSel);
        if (ids.length > 1) { closeSquadMenu(); await bulkReopenReviews(ids); return; }
        if (pendingMergeActions.has(id)) return;
        closeSquadMenu();
        pendingMergeActions.add(id);
        if (!userIsSelecting()) renderReviewDetail();
        try {
          await guardianAction(`/api/guardians/${id}/reopen`);
        } finally {
          pendingMergeActions.delete(id);
          if (!userIsSelecting()) renderReviewDetail();
        }
        tick();
      }
      /**
       * Reopens every selected cancelled review after one shared confirmation
       * naming them all (RAL-508). Reviews that aren't cancelled are skipped
       * and reported; per-item failures are reported without blocking the
       * rest of the batch.
       * @param {string[]} ids
       * @returns {Promise<void>}
       */
      // RALPHUS-REVIEW-REOPEN:BEGIN
      async function bulkReopenReviews(ids) {
        const { eligible, skipped } = bulkEligibleSplit(ids, (gid) => guardians.find((x) => x.id === gid)?.status === "cancelled");
        if (!eligible.length) return;
        if (!confirm(`Reopen ${eligible.length} cancelled review(s)? Each will immediately try a fresh merge pass, staging in whatever branches are already ready.\n\n${bulkNameList(eligible.map(reviewLabelOf))}`)) return;
        const failed = await bulkActEach(eligible, reviewLabelOf, (gid) => post(`/api/guardians/${gid}/reopen`), "reopen review failed");
        reportBulkOutcome("Reopened", "review", eligible.length, skipped, "not cancelled (only cancelled reviews can be reopened)", failed);
        tick();
      }
      // RALPHUS-REVIEW-REOPEN:END
      /**
       * Renders a review's merge-progress bar and done/total summary.
       * @param {GuardianView} g
       * @returns {string}
       */
      function mergeProgress(g) {
        const total = g.branches.length;
        if (!total) return "";
        /**
         * @param {(b: GuardianBranch) => boolean} f
         * @returns {number}
         */
        const cnt = (f) => g.branches.filter(f).length;
        // RAL-480: a branch already merged upstream is a clean-merge outcome
        // like "done" -- just one whose PR/MR automation stopped early --
        // so it counts in the same green slice rather than falling out of
        // every bucket and leaving the bar short of `total`. RAL-<new>:
        // `closed` is the same situation -- the branch's own rebase already
        // succeeded (that's how it got a PR/MR to close in the first place);
        // only what happened to its PR/MR afterward differs from `merged`.
        const done = cnt((b) => b.merge_status === "done" || b.merge_status === "merged" || b.merge_status === "closed");
        const resolved = cnt((b) => b.merge_status === "conflict_resolved");
        // RAL-149/<new>: proof_pending and actioning are transient sub-states
        // of "in progress" — conflicts are resolved/committed but the
        // dedicated final-proof call hasn't finished, or reviewer feedback is
        // still being applied, so they count toward the same blue "in
        // progress" slice rather than getting their own bar segment.
        const prog = cnt((b) => b.merge_status === "in_progress" || b.merge_status === "proof_pending" || b.merge_status === "actioning");
        const failed = cnt((b) => b.merge_status === "failed");
        /**
         * @param {number} n
         * @returns {string}
         */
        const pct = (n) => (100 * n / total).toFixed(2) + "%";
        return `<div class="kv-row"><span class="k">merged</span><span>${done + resolved}/${total}${failed ? ` · <span style="color:var(--failed)">${failed} failed</span>` : ""}</span></div>
          <div class="progress" data-tip="Merge progress bar — green: clean merge, purple: conflict resolved by agent, blue: in progress, red: failed.">
            <span style="width:${pct(done)};background:var(--done)"></span>
            <span style="width:${pct(resolved)};background:var(--queued)"></span>
            <span style="width:${pct(prog)};background:var(--running)"></span>
            <span style="width:${pct(failed)};background:var(--failed)"></span>
          </div>`;
      }
      // RAL-72: live conflict-resolution progress — three distinct metrics.
      /**
       * Renders a review's live conflict-resolution progress (found/fixed/committed).
       * @param {GuardianView} g
       * @returns {string}
       */
      function conflictProgress(g) {
        const found = g.conflicts_found;
        if (found === null || found === undefined || found <= 0) return "";
        const fixed = g.conflicts_fixed ?? 0;
        const committed = g.conflicts_committed ?? 0;
        const active = g.status === "merging";
        const resolving = active ? " · resolving…" : "";
        const tip = "Found: conflict blocks detected in the worktree\nFixed: files resolved in the working tree, not yet staged\nCommitted: files whose conflict resolutions are staged and committed to the branch";
        return `<div class="kv-row" data-tip="${tip}"><span class="k">conflicts</span><span>${found} found · ${fixed} fixed · ${committed} committed${resolving}</span></div>`;
      }
      // RAL-193: this review's own agent cost -- conflict-resolution and
      // proof LLM calls made by the guardian merge machinery -- scoped to
      // the current merge attempt and cumulatively across every
      // rebase/re-merge attempt. Deliberately excludes the cost of the
      // tasks/cells that fed into the review.
      /**
       * The review's spend as one chip for the command bar. Cost is a property
       * of the review in the same way its id is, and only means anything next
       * to it -- as two kv-rows further down the pane it was something you had
       * to go looking for. The full attempt/cumulative breakdown moves into
       * the chip's tooltip rather than being lost.
       * @param {GuardianView} g - The review.
       * @returns {string}
       */
      function reviewCostChip(g) {
        const hasAttempt = (g.attempt_tokens_in ?? 0) > 0 || (g.attempt_tokens_out ?? 0) > 0 || (g.attempt_cost_usd ?? 0) > 0;
        const hasCumulative = (g.cumulative_tokens_in ?? 0) > 0 || (g.cumulative_tokens_out ?? 0) > 0 || (g.cumulative_cost_usd ?? 0) > 0;
        if (!hasAttempt && !hasCumulative) return "";
        const cap = g.maximum_budget_usd;
        const overCap = cap != null && (g.cumulative_cost_usd ?? 0) > cap;
        // Always the money, never a token count: tokens are not comparable
        // across models, so a token figure sitting where a reader scans for
        // "what has this cost" is actively misleading. Cumulative, so it covers
        // every rebase attempt rather than only the latest.
        const priced = (g.cumulative_cost_usd ?? 0) > 0;
        const label = fmtCostUsd(g.cumulative_cost_usd);
        const tip = "This review's own conflict-resolution and proof agent spend, summed across every rebase attempt."
          + " It excludes the tasks/cells that fed into the review."
          + `\nThis attempt: input ${g.attempt_tokens_in ?? 0} · output ${g.attempt_tokens_out ?? 0} · ${fmtCostUsd(g.attempt_cost_usd)}`
          + `\nAll attempts: input ${g.cumulative_tokens_in ?? 0} · output ${g.cumulative_tokens_out ?? 0} · ${fmtCostUsd(g.cumulative_cost_usd)}`
          + (cap ? `\nBudget cap: $${cap.toFixed(4)} — once exceeded the daemon stops making resolver/proof calls and fails the review.` : "")
          + (priced ? "" : "\nN/A means this review's agent backend reported no dollar figure at all (ollama, codex and the native runner never do) — it does not mean the work was free.");
        return `<span class="cost-chip${overCap ? " over" : ""}" data-tip="${esc(tip)}">`
          + `${esc(label)}${overCap ? " ⚠" : ""}</span>`;
      }
      /**
       * The review's identity -- what it *is*, as opposed to how it is
       * configured (which the setup strip owns). Kept as kv-rows because these
       * are read, not acted on.
       * @param {GuardianView} g - The review.
       * @param {boolean} isMultiProject - Whether the review spans several git projects.
       * @returns {string}
       */
      function reviewIdentityRow(g, isMultiProject) {
        void isMultiProject;
        // Only the collecting gate survives as a row: it is the one piece of
        // identity that tells you what will happen next rather than what the
        // review is. The rest (squad, type, project, review branch, combined
        // worktree) moved into the setup strip's chips and the review-id
        // hovercard -- a column of five read-only rows between the banner and
        // the branch stack pushed the stack, the thing a reviewer opens a
        // review to read, below the fold.
        if (g.status !== "collecting" || !g.squad_id) return "";
        return `<div class="kv-row"><span class="k" style="text-transform:none;letter-spacing:0">gate</span><span class="v">${
          g.branches.some((b) => b.merge_status === "ready")
            ? "tasks complete — rebase will start automatically"
            : "starts automatically when its squad finishes — or start it now below"}</span></div>`;
      }
      /**
       * Renders the RAL-480 "already merged upstream" badge shared by
       * `branchBadge` (the branch row) and the review-worktree detail row --
       * kept as one function so both spots show identical wording.
       * @returns {string}
       */
      function mergedBranchBadge() {
        return `<span class="badge done" data-tip="This branch's commits are already integrated upstream.\nWho/when: a partial or serial stack merge landed this branch's PR/MR (or its base already absorbed its commits) before the rest of the review finished.\nRalphus will not create, update, or otherwise touch this branch's PR/MR again -- it stays enabled and keeps rebasing normally with the rest of the stack.">✓ merged</span>`;
      }
      /**
       * Renders the "PR closed externally" badge for a branch whose linked
       * PR/MR was observed closed on the forge without merging -- a human's
       * deliberate rejection, made directly on GitHub/GitLab rather than
       * through ralphus. Mirrors `mergedBranchBadge()`'s placement and
       * "ralphus will not touch this again" wording, but in the existing
       * `--cancelled` PR-lifecycle color (already used for a closed PR's own
       * chip, `PR_STATE_COLORS.closed`) rather than `--done`, since the
       * outcome here is a rejection, not a success.
       * @returns {string}
       */
      function closedExternallyBadge() {
        return `<span class="badge cancelled" data-tip="This branch's linked PR/MR was closed on the forge without merging -- most likely a human closed it directly on GitHub/GitLab.\nWho/when: you expected auto-submit or a 'submit PR stack' click to keep this branch's PR open, but it stays closed instead.\nRalphus will not create, update, or otherwise resubmit a PR for this branch again -- it stays enabled and keeps rebasing normally with the rest of the stack. Submit this one branch again manually (its own 'submit PR' action) to open a fresh PR and clear this badge.">✕ closed</span>`;
      }
      /**
       * Renders a review branch's merge-status badge (ready / conflict / resolved).
       * @param {GuardianBranch} b
       * @returns {string}
       */
      function branchBadge(b) {
        if (b.delayed_until_ms) return delayedBadge(b.delayed_until_ms, b.delayed_reason || "");
        // Checked before every status badge — including "failed": an empty
        // branch fails the review (RAL-190), and the generic red "⚠ conflict"
        // badge below would say only that something broke. This says *which*
        // thing, so the reviewer knows to go look at the task's cell rather
        // than at a diff or a rebase conflict.
        if (b.is_empty) return `<span class="badge empty" data-tip="This branch adds no changes over the branch beneath it in the stack, so it failed the review.
Almost always means its task never committed its work — the review would otherwise have marked a stack merged containing none of that task's changes.
Who/when: you are looking at a failed review and need to know why this branch stopped it.
Check the task's cell output and re-run it — or, if this branch is meant to be empty, disable it to drop it from the stack.">⌀ empty</span>`;
        // RAL-317: a one-shot marker for the most recent auto-submit-PR-stack
        // attempt on this branch — checked before the merge-status badges
        // below (same priority as is_empty) since it's a separate concern
        // from the branch's own merge outcome and can be true on an
        // otherwise-healthy "done"/"conflict_resolved" branch. Clears itself
        // (server-side) the next time the branch's state is covered by an
        // open PR again, whether via a fresh auto-submit or a manual one.
        // RAL-389: an auto-submit-PR-stack request for this branch is
        // durably queued or actively running on its own worker thread,
        // decoupled from the merge worker. Checked before `auto_submit_error`
        // -- a fresh attempt in flight is more relevant than a stale failure
        // from a previous one, which the in-flight attempt is about to
        // either clear or replace.
        if (b.pr_submission_pending) return `<span class="badge live" data-tip="Auto-submitting this branch's pull request is in progress on its own background worker.\nWho/when: you enabled auto-submit for this review's PR stack and this branch just reached a terminal state.\nClears automatically once the attempt completes (success clears it silently; failure leaves the ⚠ auto-submit failed badge instead).">⏳ submitting PR</span>`;
        if (b.auto_submit_error) return `<span class="badge bad" data-full="${esc(b.auto_submit_error)}" onclick="event.stopPropagation();openErrPopup(event)" data-tip="Auto-submitting this branch's pull request failed: ${esc(b.auto_submit_error)}\nWho/when: you enabled auto-submit for this review's PR stack and this branch's PR wasn't opened/updated as a result.\nCheck forge credentials/connectivity, then resubmit manually (review pr submit) or wait for the next auto-submit attempt.\nClick to open the full failure text in a copyable popup.">⚠ auto-submit failed</span>`;
        if (b.merge_status === "ready") return `<span class="badge ready" data-tip="All tasks are done — this branch is queued for the automatic rebase.\nThe scheduler will start rebasing it into the review stack shortly.">⚡ ready</span>`;
        if (b.merge_status === "failed") {
          const failDetail = b.detail || "conflict during rebase";
          return `<span class="badge bad" data-tip="Merge failed — ${esc(failDetail)}">⚠ conflict</span> ${detailSummary(failDetail, "Failure log", "fail")}`;
        }
        // RAL-149: all conflict markers for this branch are resolved and committed,
        // but the dedicated final-proof agent call (a separate LLM call from
        // the fix pass) hasn't finished yet. Clears automatically once that call
        // completes — the branch then lands on "conflict_resolved" (pass or fail
        // reflected in its detail text).
        if (b.merge_status === "proof_pending") return `<span class="badge live" data-tip="Conflicts are resolved and committed — the dedicated final-proof call is running now to confirm the fix meets the quality bar.\nWho/when: reviewers checking whether 'conflicts resolved' really means done, or still needs confirming.\nClears automatically once verification finishes (pass or fail reflected normally).">⏳ proofing</span>`;
        // RAL-<new>: the resolver agent is editing this branch's worktree in
        // response to `review feedback` (and, once it finishes, a proof pass
        // and a commit/push may follow). Clears automatically to "done" or a
        // conflict/failure badge once the whole feedback pass finishes.
        if (b.merge_status === "actioning") return `<span class="badge live" data-tip="Reviewer feedback is being applied — the resolver agent is revising this branch now.\nWho/when: you submitted feedback and want confirmation it's actually being worked.\nClears automatically once the revision is committed (and pushed, if applicable).">✎ actioning</span>`;
        if (b.merge_status === "conflict_resolved") return `<span class="badge warn2" data-tip="Conflict was resolved by the guardian agent — ${esc(b.detail || "resolved")}">✓ resolved</span>`;
        // RAL-480: this branch's own commits are already integrated upstream
        // (detected by git ancestry during a rebase, or by the forge
        // reporting its linked PR/MR as merged) -- shown in place of the
        // ordinary "done" pill so a reviewer can tell at a glance that this
        // branch's PR/MR is frozen: ralphus will never create, update, or
        // otherwise touch it again, even though the branch stays enabled and
        // keeps participating in the stack's rebases. One review can carry a
        // mix of `merged` and not-yet-merged branches (a partial stack
        // merge) without the review itself leaving `in_review`.
        if (b.merge_status === "merged") return mergedBranchBadge();
        // RAL-<new>: mirrors the `merged` check just above -- a `closed`
        // branch is just as terminal for PR/MR automation, but the outcome
        // is the opposite one, so it gets its own badge/color rather than
        // reusing `mergedBranchBadge()`'s "success" wording and green.
        if (b.merge_status === "closed") return closedExternallyBadge();
        return "";
      }
      // RAL-146: per-branch live progress bars (rebase position + conflict
      // resolution), shown only while this branch is the one actively
      // rebasing. Reuses the existing `.progress` CSS pattern also used by
      // `mergeProgress`.
      /**
       * Renders an in-progress branch's live rebase-position and conflict-resolution progress bars.
       * @param {GuardianBranch} b
       * @returns {string}
       */
      function branchConflictBar(b) {
        if (b.merge_status !== "in_progress") return "";
        const rDone = b.rebase_commands_done;
        const rTotal = b.rebase_commands_total;
        let rebaseBar = "";
        if (rDone !== null && rDone !== undefined && rTotal !== null && rTotal !== undefined && rTotal > 0) {
          const pct = (100 * Math.min(rDone, rTotal) / rTotal).toFixed(2) + "%";
          rebaseBar = `<div class="progress" data-tip="Rebase position — commits processed so far out of the total commits in this branch's interactive rebase.\nRead live from git's own rebase-todo bookkeeping (rebase-merge/done and rebase-merge/git-rebase-todo) in the branch's worktree.\nOnly populated while a rebase is actively paused or running here — briefly disappears between --continue/--skip steps.">
            <span style="width:${pct};background:var(--running)"></span>
          </div>`;
        }
        const found = b.conflicts_found ?? 0;
        let conflictBar = "";
        if (found > 0) {
          const fixed = b.conflicts_fixed ?? 0;
          const committed = b.conflicts_committed ?? 0;
          const tip = "Found: conflict blocks detected in the worktree\nFixed: files resolved in the working tree, not yet staged\nCommitted: files whose conflict resolutions are staged and committed to the branch";
          const committedPct = (100 * Math.min(committed, found) / found).toFixed(2) + "%";
          const fixedPct = (100 * Math.min(fixed, found) / found).toFixed(2) + "%";
          conflictBar = `<div class="progress" data-tip="${tip}">
            <span style="width:${committedPct};background:var(--done)"></span>
            <span style="width:${fixedPct};background:var(--queued)"></span>
          </div>
          <div class="branch-conflict-label"><span>${found} found · ${fixed} fixed · ${committed} committed</span></div>`;
        }
        if (!rebaseBar && !conflictBar) return "";
        return `<div class="branch-conflict-bar">${rebaseBar}${conflictBar}</div>`;
      }
      // RAL-148: live, polled list of the files still carrying unresolved
      // conflict markers in a branch's worktree, so a reviewer can watch files
      // disappear from the list as the resolver (or a human, via the worktree
      // terminal) fixes them -- no need to leave the board and run `git status`
      // by hand. Distinct from `branchConflictBar` above: that one shows
      // static found/fixed/committed *counts* recorded at resolve time; this
      // shows the *current* file list, polled live from the daemon.
      /**
       * Renders a branch's live conflicting-files list (RAL-148), reading from
       * the `branchConflicts` cache `pollBranchConflicts` keeps warm. Renders
       * nothing once the branch isn't in a failed merge state, even if a stale
       * cache entry still exists (e.g. right after the conflict was resolved,
       * before the next poll clears it).
       * @param {string} gid
       * @param {GuardianBranch} b
       * @returns {string}
       */
      function branchConflictFiles(gid, b) {
        if (b.merge_status !== "failed") return "";
        const c = branchConflicts[`${gid}:${b.id}`];
        const tip = "Files in this branch's worktree that still have unresolved <<<<<<< conflict markers.\nWho/when: watch this list while the guardian agent (or you, via the branch's worktree terminal) resolves conflicts one file at a time -- it updates live as files are fixed.\nRead-only -- resolve conflicts in the worktree itself; this list just reflects what git sees there.";
        if (!c) {
          return `<div class="branch-conflict-files" data-tip="${tip}"><div class="branch-conflict-files-empty">loading conflicting files…</div></div>`;
        }
        if (c.files.length === 0) {
          return `<div class="branch-conflict-files" data-tip="${tip}"><div class="branch-conflict-files-empty">${c.rebase_in_progress ? "rebase in progress…" : "no unresolved files right now"}</div></div>`;
        }
        const items = c.files.map((f) => `<div class="branch-conflict-file mono">${esc(f)}</div>`).join("");
        return `<div class="branch-conflict-files" data-tip="${tip}">${items}</div>`;
      }
      /**
       * Renders a review's change-summary section (git-log summary or LLM-authored final one).
       * @param {GuardianView} g
       * @returns {string}
       */
      function renderChangeSummary(g) {
        // RAL-103: summary_state is "ready" (preliminary git-log-only summary,
        // or the LLM-authored final one once a branch's review worktree
        // exists) or "waiting" (no branch has reached Ready yet). RAL-121:
        // the preliminary summary is now computed off-request by a
        // background priority worker rather than inline when a branch
        // becomes ready, so "waiting" can persist a little longer for a
        // review that isn't the one currently open — the periodic board poll
        // picks up `change_summary` once that worker gets to it. There is
        // still no "generating" gap to report: from the client's view a
        // summary is either present or not yet computed.
        if (g.summary_state === "ready" && g.change_summary) {
          return `<h3 class="section" data-tip="Computed from git log as each branch becomes ready — a plain commit-subject listing at first, replaced by an agent-written summary once that branch's review worktree is built.\nRecomputed whenever another branch becomes ready or the review is rebuilt.">what changed${sectionMenuBtn(g.id, "summary")}</h3>
            <div class="summary-box">${esc(g.change_summary)}</div>`;
        }
        return `<h3 class="section" data-tip="A summary appears here as soon as one branch's source task cell finishes — no need to wait for merging/rebasing.">what changed${sectionMenuBtn(g.id, "summary")}</h3><div class="empty">waiting for a branch to be ready…</div>`;
      }
      // ---------- PR submission + sync (RAL-117/RAL-190) ----------
      /** RAL-395: PR lifecycle-state color roles, reusing existing status hues (docs/colors.md) rather than inventing new ones -- mirrors the Tasks tab's `TT_PR_COLORS` (10-tab-registry.js) so the two surfaces never drift apart on what a PR state means visually. */
      /** @type {{[state: string]: string}} */
      const PR_STATE_COLORS = { open: "--accent", merged: "--done", closed: "--cancelled", dropped: "--failed" };
      /** RAL-395: CI/CD status color roles for an *open* PR, reusing the same status hues -- see docs/colors.md's "PR CI/CD status" subsection. Mirrors the Tasks tab's `TT_PR_CI_COLORS`. */
      /** @type {{[status: string]: string}} */
      const PR_CI_COLORS = { passing: "--done", failing: "--failed", pending: "--pending" };
      /**
       * The color role for a PR chip/badge (RAL-395): once a PR is no longer
       * `open` (merged/closed/dropped), its CI status is moot -- use
       * `PR_STATE_COLORS`' lifecycle coloring. While `open`, prefer the
       * polled CI status (a distinct color for failing vs. passing vs.
       * not-yet-known) over the flat "in-flight" accent color, so a reviewer
       * sees red/green without opening the PR.
       * @param {PullRequestView} p
       * @returns {string} a `var(--name)` CSS value, ready to drop into a `style` attribute.
       */
      function prColorVar(p) {
        const role = p.state === "open" && p.ci_status
          ? (PR_CI_COLORS[p.ci_status] || PR_STATE_COLORS.open)
          : (PR_STATE_COLORS[p.state] || "--muted");
        return `var(${role})`;
      }
      // RALPHUS-PR-VISIBLE-STATES:BEGIN
      /**
       * RAL-478: PR/MR states a review's branch link/card still shows once no
       * longer open -- a PR closed without merging stays visible alongside a
       * merged one, distinct from the flat "gone" behavior before this ticket
       * (both were filtered out with `open`-only checks). Excludes `dropped`
       * (RAL-302's soft-delete marker for a row that was never a real,
       * currently-relevant PR).
       * @type {Set<string>}
       */
      const BRANCH_PR_VISIBLE_STATES = new Set(["open", "merged", "closed"]);
      /**
       * Whether a branch's PR row still belongs on the review detail view
       * (RAL-478): its state must be one a reviewer still cares about
       * (`BRANCH_PR_VISIBLE_STATES`), and it must not have been superseded by
       * a fresher replacement row (RAL-338's fork-promotion reconcile-first) --
       * the replacement is the current one to show, not this one.
       * @param {{state: string, superseded_by?: string|null}} p
       * @returns {boolean}
       */
      function isVisibleBranchPr(p) {
        return BRANCH_PR_VISIBLE_STATES.has(p.state) && !p.superseded_by;
      }
      /** Lower ranks first -- an open PR outranks a merged one, which outranks a closed one, when a branch has more than one visible row to pick a single link from. */
      /** @type {{[state: string]: number}} */
      const BRANCH_PR_STATE_RANK = { open: 0, merged: 1, closed: 2 };
      /**
       * Picks the single most relevant visible PR within one `pr_kind` group
       * (RAL-478): prefers a still-open PR, falling back to merged then
       * closed, so a stale closed/merged link never shadows a genuinely
       * active one when a branch somehow carries more than one visible row
       * of the same kind.
       * @param {PullRequestView[]} prs
       * @returns {PullRequestView|null}
       */
      function pickBranchPr(prs) {
        const visible = prs.filter(isVisibleBranchPr);
        if (!visible.length) return null;
        return visible.reduce((best, p) =>
          (BRANCH_PR_STATE_RANK[p.state] ?? 99) < (BRANCH_PR_STATE_RANK[best.state] ?? 99) ? p : best);
      }
      /**
       * Picks up to one visible PR per `pr_kind` ("parent" first, then
       * "stack") for a branch's compact link (RAL-<new>): dual_root_pr mode
       * gives a fork-routed root branch two concurrently-open PRs -- the real
       * "parent" merge target and a "stack" PR that only exists so the
       * branch visually chains into the rest of the review's PR stack -- so
       * a branch can have up to one visible link *per kind*, not just one
       * overall. A row with no `pr_kind` (impossible for any row created
       * after this ticket, but tolerated for defense in depth) is treated as
       * "parent".
       * @param {PullRequestView[]} prs
       * @returns {PullRequestView[]}
       */
      function pickBranchPrsByKind(prs) {
        const picked = ["parent", "stack"].map((kind) =>
          pickBranchPr(prs.filter((p) => (p.pr_kind || "parent") === kind)));
        return /** @type {PullRequestView[]} */ (picked.filter((p) => p !== null));
      }
      // RALPHUS-PR-VISIBLE-STATES:END
      /**
       * Renders one existing PR's status row: forge/number/state, and a drift
       * banner (RAL-190) offering "pull PR commits" when the PR branch has
       * commits the review worktree does not yet.
       * @param {PullRequestView} p
       * @returns {string}
       */
      function prCard(p) {
        const link = p.pr_url
          ? `<a href="${esc(p.pr_url)}" target="_blank" rel="noopener" class="mono" style="color:var(--accent)" data-tip="Open this pull/merge request on ${esc(p.forge)}.">#${p.pr_number ?? "?"}</a>`
          : `<span class="mono">${esc(p.branch_alias)}</span>`;
        const sync = prSyncStatus[p.id];
        let drift = "";
        if (sync && sync.pr_ahead) {
          drift = `<div class="row" style="margin-top:4px;gap:6px">
              <span class="badge" style="color:var(--drift);border-color:var(--drift);font-size:11px" data-tip="A reviewer pushed commit(s) directly to this PR branch that this review worktree does not have yet.\nWho/when: pull them in before this branch is pushed again, so they aren't silently overwritten.">⇅ PR branch has new commits</span>
              <button class="btn" style="padding:1px 8px;font-size:11px" data-click="pullPrCommits" data-pr-id="${esc(p.id)}" data-tip="Fetch the PR branch's new commits and rebase them into this review worktree.\nConflicts are resolved the same way a normal stacked rebase resolves them, then everything downstream of this branch is restacked on top.\nThe merged result is pushed back to the PR branch afterward.">Pull PR commits</button>
            </div>`;
        } else if (sync && sync.worktree_ahead) {
          drift = `<div class="row" style="margin-top:4px">
              <span class="badge" style="color:var(--muted);border-color:var(--border);font-size:11px" data-tip="This review worktree has commits not yet reflected on the PR branch -- e.g. feedback was just resolved.\nInformational only: submitting again, or resolving PR feedback, pushes the latest worktree state to the PR branch automatically.">worktree ahead of PR — syncs on next push</span>
            </div>`;
        }
        const badgeColor = prColorVar(p);
        const canQueryForge = p.state === "open" && p.pr_number != null;
        const autoFixError = p.auto_fix_error
          ? `<div class="row" style="margin-top:4px"><span class="badge" style="color:var(--failed);border-color:var(--failed);font-size:11px" data-tip="Automatic CI fixing has stopped for this PR/MR to prevent repeated pushes and CI runs.\nWho/when: use Action Feedback when a person has reviewed the failure and wants an explicit retry.">${esc(p.auto_fix_error)}</span></div>`
          : "";
        // RAL-509: surface the latest auto-fix outcome even when it isn't an
        // error -- e.g. "deferred_no_worktree"/"deferred_backoff" explain why
        // a failing PR got no auto-fix attempt yet, which `auto_fix_error`
        // (only set once the campaign has fully stopped) does not cover.
        // Suppressed once `auto_fix_error` is already shown so a permanently
        // stopped PR doesn't show two overlapping badges.
        const autoFixOutcome = p.state === "open" && p.auto_fix_last_outcome && !p.auto_fix_error
          ? `<div class="row" style="margin-top:4px"><span class="badge" style="color:var(--muted);border-color:var(--border);font-size:11px" data-tip="Latest unattended CI auto-fix outcome for this PR/MR.\nWhy: explains whether/why an automatic fix attempt ran for the most recent CI failure, even when nothing else here indicates a reason.">auto-fix: ${esc(p.auto_fix_last_outcome)}</span></div>`
          : "";
        const ciTip = p.state === "open" && p.ci_status
          ? ` data-tip="CI/CD status: ${esc(p.ci_status)}. Right-click to refresh its status or pull in feedback."`
          : ` data-tip="Right-click to refresh its status or pull in feedback."`;
        const badgeLabel = p.state === "open" && p.ci_status ? `${esc(p.state)} · ${esc(p.ci_status)}` : esc(p.state);
        return `<div class="pr-card" style="border:1px solid var(--border);border-radius:6px;padding:6px 8px;margin-bottom:4px" data-tip="Pull/merge request submitted via ${esc(p.forge)}.">
            <div class="row" style="justify-content:space-between;gap:6px">
              <span>${esc(p.forge)} ${link} <span class="mono" style="color:var(--muted);font-size:11px">${esc(p.branch_alias)} → ${esc(p.base_ref)}</span></span>
              <span class="badge" style="font-size:11px;color:${badgeColor};border-color:${badgeColor}" data-ctx="openPrMenu" data-pr-id="${esc(p.id)}" data-pr-open="${canQueryForge ? "1" : "0"}"${ciTip}>${badgeLabel}</span>
            </div>
            ${drift}
            ${autoFixError}
            ${autoFixOutcome}
          </div>`;
      }
      // RALPHUS-AUTO-FIX-EXHAUSTED-BADGE:BEGIN
      /**
       * Renders a badge for a worktree chip when unattended CI auto-fix has
       * exhausted its attempt budget (RAL-537) on one of this branch's open
       * PR(s) -- a per-branch/per-PR concern, unlike the review-wide
       * base-shift rebuild exhaustion notice (`rebaseExhaustedNotice`
       * below), which is a single counter shared across every branch in the
       * review. A branch can carry up to one open PR per `pr_kind`
       * ("parent" and "stack" -- dual_root_pr mode), so both are checked;
       * if both are exhausted the tooltip names both, using the approved
       * copy built from the live PR number/attempt count per PR (the
       * backend's own `auto_fix_error` string says the same thing but isn't
       * worded to match that approved copy, so it isn't substituted in
       * here). Appended as its own element beside `branchBadge()`'s output
       * rather than folded into it -- `branchBadge()` is a strict one-badge-at-a-time
       * priority chain and this can be true independently of whatever it
       * shows. Clears within one poll cycle of "Merge / rebase" being
       * pressed, since that resets `auto_fix_attempt_count` and
       * `auto_fix_last_outcome` server-side (`reset_open_pr_auto_fix_attempts`).
       * Takes `prs` explicitly (rather than reading the `pullRequests`
       * module-level cache itself) so this stays pure and slice-testable,
       * matching the `rebaseExhaustedNotice` convention below.
       * @param {PullRequestView[]} prs
       * @param {GuardianBranch} b
       * @returns {string}
       */
      function autoFixExhaustedBadge(prs, b) {
        const exhausted = prs
          .filter((p) => p.branch_id === b.id && p.state === "open" && p.auto_fix_last_outcome === "exhausted");
        if (!exhausted.length) return "";
        const tip = exhausted.map((p) =>
          `Auto-fix gave up on PR #${p.pr_number ?? "?"}: tried ${p.auto_fix_attempt_count} time(s) to fix CI failures automatically and stopped, so it doesn't keep pushing broken fixes and burning CI runs. Press 'Merge / rebase' to reset this and let auto-fix try again — or fix the CI failure yourself first.`
        ).join("\n\n");
        return `<span class="badge" style="color:var(--muted);border-color:var(--border);font-size:11px" data-tip="${esc(tip)}">⚠ auto-fix exhausted</span>`;
      }
      // RALPHUS-AUTO-FIX-EXHAUSTED-BADGE:END
      // RALPHUS-REBASE-EXHAUSTED-NOTICE:BEGIN
      /**
       * Renders the review-level notice (RAL-537) for when the base-shift
       * rebuild campaign (RAL-507) has exhausted its attempt budget --
       * unlike `autoFixExhaustedBadge` above, `base_shift_rebuild_attempts`
       * is a single counter shared across every branch in the review (one
       * rebuild pass rebases every affected branch together), not something
       * attributable to any one worktree chip, so this renders once above
       * the worktree list rather than as a per-branch badge (RAL-542 tracks
       * the gap that one bad worktree can exhaust the budget for the whole
       * review). `canReorder` is the same terminal-status check already
       * computed by the caller for the branch-reorder affordance -- reused
       * here so the notice doesn't linger once the review can no longer be
       * acted on (merged/cancelled/deployed/approved). Clears within one
       * poll cycle of "Merge / rebase" being pressed, since that resets
       * `base_shift_rebuild_attempts` server-side
       * (`clear_guardian_base_shift_campaign`).
       * @param {GuardianView} g
       * @param {boolean} canReorder
       * @returns {string}
       */
      function rebaseExhaustedNotice(g, canReorder) {
        if (!canReorder) return "";
        const attempts = g.base_shift_rebuild_attempts || 0;
        const max = g.effective_base_shift_maximum_rebuilds || 0;
        if (max <= 0 || attempts < max) return "";
        const tip = `The base branch moved, and automatic rebasing stopped after ${attempts} failed attempt(s) against the same new base (cap: ${max}). This applies to the whole review — one bad worktree can use up the budget for all of them. The review is left as-is awaiting human action. Press 'Merge / rebase' to reset and start a fresh automatic attempt.`;
        return `<div style="color:var(--muted);border:1px solid var(--border);border-radius:6px;padding:8px;font-size:12px;margin:8px 0" data-tip="${esc(tip)}">⚠ Automatic rebasing stopped after ${attempts}/${max} failed attempt(s) against the new base branch — press "Merge / rebase" to reset and try again.</div>`;
      }
      // RALPHUS-REBASE-EXHAUSTED-NOTICE:END
      /**
       * Renders one compact clickable PR/MR badge (RAL-478, split out of
       * `branchPrLink` by RAL-<new> so a dual_root_pr mode root branch can
       * show one badge per `pr_kind` instead of collapsing to a single
       * winner). `dual` is whether a sibling badge of the other kind is also
       * being shown alongside this one, which changes what the tooltip needs
       * to clarify.
       * @param {PullRequestView} pr
       * @param {boolean} dual
       * @returns {string}
       */
      function branchPrBadge(pr, dual) {
        if (!pr.pr_url) return "";
        const color = prColorVar(pr);
        const ciNote = pr.state === "open" && pr.ci_status ? ` CI/CD: ${esc(pr.ci_status)}.` : "";
        const canQueryForge = pr.state === "open" && pr.pr_number != null;
        const kindNote = pr.pr_kind === "stack"
          ? " This is the stack PR dual-root-PR mode uses so this branch visually joins the rest of the review's PR stack -- it is never actually merged, and closes automatically once the parent PR beside it merges."
          : dual
            ? " This is the PR that actually gets merged; the other badge alongside it is a stack-only PR kept just for visual chaining."
            : "";
        return `<a href="${esc(pr.pr_url)}" target="_blank" rel="noopener" class="badge mono" style="color:${color};border-color:${color}" onclick="event.stopPropagation()" data-ctx="openPrMenu" data-pr-id="${esc(pr.id)}" data-pr-open="${canQueryForge ? "1" : "0"}" data-tip="Open this branch's pull/merge request on ${esc(pr.forge)}.${ciNote}${kindNote} Right-click to refresh its status or pull in feedback.">${esc(pr.forge)} #${pr.pr_number ?? "?"}</a>`;
      }
      /**
       * Renders compact clickable link(s) to this branch's most relevant
       * PR/MR(s) (RAL-478: open, or -- once no longer open -- merged/closed),
       * if any exist, directly in the branch row (RAL-190) -- so a branch
       * that already has a PR stays one click away from GitHub/GitLab
       * without expanding its detail. RAL-<new>: a fork-routed root branch
       * in dual_root_pr mode carries two concurrently-open PRs (its real
       * "parent" merge target and a same-repo "stack" PR for visual
       * chaining), so this can render up to two badges, not just one.
       * `branchPrSection` below shows the fuller card(s) (with drift info)
       * once expanded; this is just the always-visible shortcut the
       * per-branch submit button used to give for free.
       * @param {GuardianView} g
       * @param {GuardianBranch} b
       * @returns {string}
       */
      function branchPrLink(g, b) {
        const prs = pickBranchPrsByKind((pullRequests[g.id] || []).filter((p) => p.branch_id === b.id));
        if (!prs.length) return "";
        const dual = prs.length > 1;
        return prs.map((pr) => branchPrBadge(pr, dual)).join(" ");
      }
      /**
       * Renders the PR status section for one stacked branch (RAL-190+): its
       * existing PR(s), if any -- open, or (RAL-478) merged/closed once no
       * longer open, so a PR abandoned without merging stays visible here
       * instead of silently vanishing. Submission itself is triggered once,
       * for the whole stack, via `combinedPrSection`'s button -- not per
       * branch (a stray manual "submit PR" per branch made it too easy to
       * submit branches out of order and break the base chain).
       * @param {GuardianView} g
       * @param {GuardianBranch} b
       * @returns {string}
       */
      function branchPrSection(g, b) {
        if (!b.worktree) return "";
        const prs = (pullRequests[g.id] || []).filter((p) => p.branch_id === b.id).filter(isVisibleBranchPr);
        if (!prs.length) return "";
        return `<div class="pr-section" style="margin-top:6px" onclick="event.stopPropagation()">${prs.map(prCard).join("")}</div>`;
      }
      // RALPHUS-MERGE-BUTTON:BEGIN
      // How the "Merge / rebase" button reads, and what the click acknowledges.
      //
      // Deliberately pure -- no DOM, no fetch, no module-level state -- so
      // test/board-merge-button.mjs can slice this region straight out of the
      // shipped page and exercise it under `node --test`. Keep it that way:
      // anything reaching for `document` belongs in renderReviewDetail.

      /** @type {Record<string, string>} Why the button is unavailable, keyed by the review's status. */
      const MERGE_DISABLED_REASON = {
        merging: "A rebase is already in progress — wait for it to finish, or press Stop to halt it mid-rebase.\nTo restart from scratch, stop then merge, or cancel the running merge first.",
        merged: "This review has already been marked merged — merge/rebase locks once a review is merged.\nOnly available when status is collecting, in_review, merge_stopped, or merge_failed.",
        cancelled: "This review was cancelled — merge/rebase is not available for a cancelled review.\nOnly available when status is collecting, in_review, merge_stopped, or merge_failed.",
        deployed: "This review has already been deployed — merge/rebase is not available once deployed.\nOnly available when status is collecting, in_review, merge_stopped, or merge_failed.",
      };

      /** Review statuses a rebase may be started or resumed from. */
      const MERGE_STARTABLE = ["collecting", "merge_failed", "merge_stopped", "in_review"];

      /**
       * How the "Merge / rebase" button should read for a review.
       *
       * `pending` is true from the moment the button is clicked until the
       * daemon has answered and the board has reloaded. Starting a rebase is
       * only a DB state transition plus a thread spawn, but the reload behind
       * it is a further network round-trip or two; with no pending state the
       * button sits looking untouched for that whole window, which is what
       * trains people to click it twice.
       * @param {string} status the review's current status
       * @param {boolean} pending whether a kickoff for it is already in flight
       * @returns {{label: string, enabled: boolean, tip: string}}
       */
      function mergeButtonView(status, pending) {
        const resuming = status === "merge_stopped";
        if (pending) {
          return {
            label: resuming ? "Resuming…" : "Starting…",
            enabled: false,
            tip: "The request has been sent — waiting for the daemon to confirm the rebase started.\nThe rebase itself runs in the background and takes far longer than this; watch the review's status for its progress.\nDisabled so the same rebase cannot be submitted twice.",
          };
        }
        if (!MERGE_STARTABLE.includes(status)) {
          return {
            label: "Merge / rebase",
            enabled: false,
            tip: MERGE_DISABLED_REASON[status] || `Not available — this review's status is currently "${status}".\nOnly available when status is collecting, in_review, merge_stopped, or merge_failed.`,
          };
        }
        return {
          label: resuming ? "Resume rebase" : "Merge / rebase",
          enabled: true,
          tip: resuming
            ? "Resume the stopped rebase from where it left off — rebuilds the review stack from the first remaining worktree.\nA review left stopped mid-rebase is paused, not cancelled: branches and worktrees are kept.\nThis also gives each open PR one fresh automatic CI-fix attempt."
            : "Start the Guardian: rebase each branch onto the prior in the stack, resolve conflicts with the AI agent, and run check gates.\nThis also gives each open PR one fresh automatic CI-fix attempt.\nOnly available when status is collecting, in_review, merge_stopped, or merge_failed.",
        };
      }

      /**
       * The acknowledgement shown the instant the button is clicked. It
       * confirms the *request* only — the rebase runs in the background and
       * finishes long after this toast is gone.
       * @param {string} status the review's status at the moment of the click
       * @returns {string}
       */
      function mergeRequestedToast(status) {
        if (status === "merging") return "Restart requested — stopping the running rebase, then starting a fresh one in the background.";
        if (status === "merge_stopped") return "Resume requested — the rebase is picking up where it stopped, in the background.";
        return "Rebase requested — the review is starting to rebase in the background.";
      }
      // RALPHUS-MERGE-BUTTON:END

      /** Review statuses whose primary action button is "Reopen" instead of "Merge / rebase" -- mirrors the backend's `Store::reopen_guardian` (daemon/src/guardian.rs). */
      const REOPEN_ELIGIBLE = ["cancelled", "merged", "approved"];

      /**
       * How the "Reopen" button should read, for a review that occupies the
       * "Merge / rebase" button's slot once it is cancelled, merged, or
       * approved -- all three are otherwise dead ends for that button (see
       * `MERGE_DISABLED_REASON`), so reopening back into `collecting` is the
       * only way to continue one instead of starting over from scratch.
       * @param {string} status the review's current status ("cancelled", "merged", or "approved")
       * @param {boolean} pending whether a reopen kickoff is already in flight
       * @returns {{label: string, enabled: boolean, tip: string}}
       */
      function reopenButtonView(status, pending) {
        if (pending) {
          return {
            label: "Reopening…",
            enabled: false,
            tip: "The request has been sent — waiting for the daemon to confirm the review reopened.\nReopening also immediately stages whatever branches are already ready.\nDisabled so the same reopen cannot be submitted twice.",
          };
        }
        let tip;
        if (status === "merged") {
          tip = "Reopen this merged review back into collecting and immediately stage in whatever branches are already ready, without waiting for the rest.\nUse this to make further changes to an already-merged review instead of starting a new one from scratch.";
        } else if (status === "approved") {
          tip = "Reopen this approved review back into collecting and immediately stage in whatever branches are already ready, without waiting for the rest.\nUse this to make further changes to an already-approved review instead of starting a new one from scratch.";
        } else {
          tip = "Reopen this cancelled review and immediately stage in whatever branches are already ready, without waiting for the rest.\nUse this when a review was cancelled by mistake, or you want to retry it without recreating it from scratch.";
        }
        return {
          label: "↺ Reopen review",
          enabled: true,
          tip,
        };
      }

      /**
       * Resolves the actual scrollable element behind the review-detail pane
       * (RAL-481). `#review-detail`'s own contents are swapped wholesale by
       * `renderReviewDetail()`, but the scrollbar itself lives one level up,
       * on its `.col center` wrapper (`board.html`) -- the same `.col`
       * convention `#graph`/`#details` use on the Squads page, just without
       * the id sitting on that element here.
       * @returns {HTMLElement|null}
       */
      function reviewDetailScrollEl() {
        const inner = document.getElementById("review-detail");
        return /** @type {HTMLElement|null} */ (inner ? inner.parentElement : null);
      }
      // ---- Entity hovercards for the Reviews tab ----
      //
      // A review's most-inspected values -- the worktree paths, the branch
      // names, the review id -- were plain text. Knowing what a worktree
      // actually *was* (which branch, whether it is clean, what overrides it
      // carries, when it retires) meant reading the change notes or the CLI.
      // These put that where the value already is. See board/22-hovercards.js
      // for the engine; the renderers live here because only this chunk knows
      // what a guardian branch is.

      /**
       * Resolves a hovercard anchor's `data-guardian-id` to the loaded review,
       * or null when that review's full detail is not in `guardians` yet.
       * @param {DOMStringMap} ds - The anchor's dataset.
       * @returns {GuardianView|null}
       */
      function hcGuardian(ds) {
        const g = guardians.find((x) => x.id === (ds.guardianId || ""));
        return g && g.branches ? g : null;
      }
      /**
       * Resolves a hovercard anchor's `data-guardian-id`/`data-branch-id` pair
       * to one branch of a loaded review.
       * @param {DOMStringMap} ds - The anchor's dataset.
       * @returns {{g: GuardianView, b: GuardianBranch}|null}
       */
      function hcBranch(ds) {
        const g = hcGuardian(ds);
        if (!g) return null;
        const b = (g.branches || []).find((x) => x.id === (ds.branchId || ""));
        return b ? { g: g, b: b } : null;
      }
      /**
       * Counts a branch's own env-override layer, split into values it sets and
       * keys it tombstones (a `null` value removes an inherited variable).
       * @param {GuardianBranch} b - The branch.
       * @returns {{set: number, unset: number}}
       */
      function hcEnvCounts(b) {
        const own = b.env_overrides || {};
        let set = 0, unset = 0;
        Object.keys(own).forEach((k) => { if (own[k] === null) unset++; else set++; });
        return { set: set, unset: unset };
      }
      /**
       * A branch's 1-based place in the enabled rebase stack, and what it
       * rebases onto (the branch beneath it, or the review's upstream when it
       * is first). Derived from the ordered list rather than read off
       * `GuardianBranch.position`, which is a 0-based internal ordinal -- using
       * it directly rendered the first branch as "0 of 3".
       * @param {GuardianView} g - The review.
       * @param {GuardianBranch} b - The branch.
       * @returns {{place: number, total: number, onto: string}}
       */
      function hcStackPlace(g, b) {
        const ordered = (g.branches || []).filter((x) => x.enabled !== false);
        const i = ordered.findIndex((x) => x.id === b.id);
        return {
          place: i < 0 ? 0 : i + 1,
          total: ordered.length,
          onto: i > 0 ? ordered[i - 1].branch : (g.base_branch || "—"),
        };
      }

      registerHoverCard("gBranchWorktree", (ds) => {
        const hit = hcBranch(ds);
        if (!hit) return null;
        const b = hit.b;
        if (!b.worktree) {
          return {
            title: "Branch worktree",
            body: hcNote("Not built yet. A per-branch worktree is created when this branch starts rebasing. "
              + "With <b>skip per-branch worktrees</b> on, the whole stack builds in one shared worktree instead."),
          };
        }
        const env = hcEnvCounts(b);
        const envText = env.set || env.unset
          ? `${env.set} set${env.unset ? `, ${env.unset} removed` : ""}`
          : "inherited";
        const conflicts = (b.conflicts_found || 0) > 0
          ? `${b.conflicts_fixed || 0} fixed of ${b.conflicts_found} found`
          : "none";
        return {
          title: "Branch worktree",
          badge: pill(b.merge_status || "pending"),
          body: `<div class="hc-path">${esc(b.worktree)}</div>`
            + hcKv(
              hcRow("branch", esc(b.branch), "mono")
              + hcRow("rebases onto", esc(hcStackPlace(hit.g, b).onto), "mono")
              + hcRow("conflicts", esc(conflicts))
              + hcRow("env", esc(envText))
              + hcRow("status", esc(b.detail || "—")),
            )
            + hcNote("Retired with the review once it is approved — see the Worktree Retirement tab for when."),
          foot: `<button class="btn" data-tip="Copy this worktree's absolute path." data-copy="${esc(b.worktree)}" onclick="copyText(event)">Copy path</button>`,
        };
      });

      registerHoverCard("gCombinedWorktree", (ds) => {
        const g = hcGuardian(ds);
        if (!g || !g.combined_worktree) return null;
        return {
          title: "Combined worktree",
          badge: pill(g.status),
          body: `<div class="hc-path">${esc(g.combined_worktree)}</div>`
            + hcKv(
              hcRow("review branch", esc(g.review_branch || "—"), "mono")
              + hcRow("upstream", esc(g.base_branch || "—"), "mono")
              + hcRow("branches", `${(g.branches || []).filter((b) => b.enabled !== false).length} enabled of ${(g.branches || []).length}`),
            )
            + hcNote("The whole stack rebased into one tree — what the check gates actually run against."),
          foot: `<button class="btn" data-tip="Copy this worktree's absolute path." data-copy="${esc(g.combined_worktree)}" onclick="copyText(event)">Copy path</button>`,
        };
      });

      registerHoverCard("gReviewId", (ds) => {
        const g = hcGuardian(ds);
        if (!g) return null;
        return {
          title: "Review id",
          badge: pill(g.status),
          body: `<div class="hc-path">${esc(g.id)}</div>`
            + hcKv(
              hcRow("from squad", esc(g.squad_id || "—"), "mono")
              + hcRow("type", esc(g.review_type || "git"))
              + hcRow("upstream", esc(g.base_branch || "—"), "mono"),
            )
            + hcNote("Use this id with <span class=\"mono\">ralphus review show</span>, in daemon logs, "
              + "or to find the review worktree on disk."),
          foot: `<button class="btn" data-tip="Copy the full review id." data-copy="${esc(g.id)}" onclick="copyText(event)">Copy id</button>`
            + `<button class="btn" data-tip="View the audit log for this review." data-click="openReviewLogs" data-guardian-id="${esc(g.id)}">Logs</button>`,
        };
      });

      // ---- Command bar ----
      //
      // The review's lifecycle, rendered as the rail it actually is, plus the
      // one action its current state calls for. Previously the banner carried
      // only a name and a status pill, and every action lived in a button row
      // two thirds down the pane -- so "where is this review, and what do I do
      // about it" took a scroll and some inference.

      /** The Guardian lifecycle in the order a review passes through it, for the command bar's rail. */
      const REVIEW_PIPELINE = ["collecting", "merging", "in_review", "approved", "deployed"];
      /** @type {{[status: string]: string}} Short label for each pipeline stop. */
      const REVIEW_PIPELINE_LABELS = {
        collecting: "Collect",
        merging: "Rebase",
        in_review: "Review",
        approved: "Approve",
        deployed: "Deploy",
      };
      /**
       * Each pipeline stage's own state, computed independently.
       *
       * A review is not a single point on a line. Branches rebase concurrently,
       * post-merge gates run while the stack is already readable, and auto-fix
       * can be pushing commits to a PR while a later branch is still merging --
       * so "collect", "rebase" and "review" are routinely live at the same
       * time. Reading one status enum and lighting a single dot misreported all
       * of that; every stage now answers for itself, and more than one can be
       * active at once.
       * @param {GuardianView} g - The review.
       * @returns {{[stop: string]: string}} Per-stage state: "done" | "now" | "stalled" | "idle".
       */
      function reviewPipelineStates(g) {
        const branches = (g.branches || []).filter((b) => b.enabled !== false);
        const anyBranch = (/** @type {(b: GuardianBranch) => boolean} */ f) => branches.some(f);
        const terminal = (/** @type {GuardianBranch} */ b) =>
          ["done", "merged", "closed", "conflict_resolved", "failed"].includes(b.merge_status || "");

        // Collect: any contributing cell still producing its branch.
        const collecting = g.status === "collecting"
          || anyBranch((b) => b.source_cell_state !== undefined && b.source_cell_state !== "done");
        // Rebase: any branch actively mid-rebase, or the review as a whole is.
        const rebasing = g.status === "merging"
          || anyBranch((b) => b.merge_status === "in_progress"
            || (b.rebase_commands_total !== null && b.rebase_commands_total !== undefined));
        const rebaseFailed = g.status === "merge_failed" || g.status === "merge_stopped"
          || anyBranch((b) => b.merge_status === "failed");
        const rebaseDone = branches.length > 0 && branches.every(terminal);
        // Review: the stack is readable. Post-merge work (check gates,
        // manual-checks generation) runs here and never blocks it.
        const reviewing = g.status === "in_review" || g.post_merge_status === "running";
        const approved = ["approved", "merged", "deployed"].includes(g.status);
        const deployed = g.status === "deployed";

        /** @type {{[stop: string]: string}} */
        const st = {
          collecting: collecting ? "now" : (branches.length ? "done" : "idle"),
          merging: rebaseFailed ? "stalled" : (rebasing ? "now" : (rebaseDone ? "done" : "idle")),
          in_review: approved ? "done" : (reviewing ? "now" : "idle"),
          approved: deployed ? "done" : (approved ? "now" : "idle"),
          deployed: deployed ? "now" : "idle",
        };
        if (g.status === "cancelled") {
          Object.keys(st).forEach((k) => { if (st[k] === "now") st[k] = "stalled"; });
        }
        return st;
      }
      /**
       * Renders the lifecycle rail. Several stages can read as live at once --
       * see {@link reviewPipelineStates} for why that is the normal case here
       * rather than an edge one.
       * @param {GuardianView} g - The review.
       * @returns {string}
       */
      function reviewPipeline(g) {
        const st = reviewPipelineStates(g);
        const branches = (g.branches || []).filter((b) => b.enabled !== false);
        const merged = branches.filter((b) =>
          ["done", "merged", "closed", "conflict_resolved"].includes(b.merge_status || "")).length;
        /** @type {{[stop: string]: string}} */
        const counts = { merging: branches.length ? `${merged}/${branches.length}` : "" };
        /** @type {{[stop: string]: string}} */
        const TIPS = {
          collecting: "Waiting on the task cells that produce this review's branches.",
          merging: "Rebasing the stack. Branches rebase concurrently, so this can be live while other stages are too.",
          in_review: "The stack is readable and can be approved. Post-merge gates and manual-check generation run here without blocking it.",
          approved: "Approved, whether or not its PR stack has merged.",
          deployed: "Shipped.",
        };
        /** @type {{[state: string]: string}} */
        const WORD = { done: "complete", now: "in progress now", stalled: "stalled here", idle: "not started" };
        return `<div class="review-pipe">${REVIEW_PIPELINE.map((stop, i) => {
          const state = st[stop] || "idle";
          const cls = state === "idle" ? "" : state;
          const mark = state === "done" ? "✓" : (state === "now" ? "●" : (state === "stalled" ? "!" : ""));
          const count = counts[stop] ? ` <span class="pipe-count num">${counts[stop]}</span>` : "";
          const tip = `${REVIEW_PIPELINE_LABELS[stop]} — ${WORD[state]}.\n${TIPS[stop]}`;
          // A connector reads "done" only when the stage behind it is, so a
          // later stage going live early never retroactively colours an earlier
          // one that is still running.
          const line = i < REVIEW_PIPELINE.length - 1
            ? `<span class="pipe-line ${state === "done" ? "done" : ""}"></span>` : "";
          return `<span class="pipe-stop ${cls}" data-tip="${esc(tip)}">`
            + `<span class="pipe-dot">${mark}</span>${REVIEW_PIPELINE_LABELS[stop]}${count}</span>${line}`;
        }).join("")}</div>`;
      }

      /**
       * How to name the repository a review's branches live in. Prefers the
       * registered project's name over the on-disk path (RAL-396): a review can
       * span a remote machine, so the path is not something the board can treat
       * as stable or even knowable.
       * @param {GuardianView} g - The review.
       * @returns {string}
       */
      function projectLabelFor(g) {
        if (g.projects && g.projects.length > 1) return `${g.projects.length} projects`;
        if (g.project) return g.project;
        const root = g.git_root || "";
        return root.split(/[\\/]/).filter(Boolean).pop() || "—";
      }
      /**
       * The frame every runnable section wears: a header carrying the section's
       * run control and what its commands promise, then its rows.
       *
       * One frame for all three so a section reads the same whether it is
       * populated, waiting, or empty. Previously each state produced quite
       * different furniture -- a row of chips, a lone coloured chip, a bare
       * disabled button with a stray "Show Live View" beside it -- which is why
       * a review with nothing generated yet looked like a different, older page.
       * @param {string} control - The run control: a button, or a badge when the section has nothing to trigger.
       * @param {string} note - One line on what these commands are and when they run.
       * @param {string} rows - The section's command rows, or "".
       * @param {boolean} [dim] - Render muted, for a section whose commands are currently skipped.
       * @returns {string}
       */
      function reviewRunGroup(control, note, rows, dim) {
        // An empty note renders nothing at all rather than an empty span: what
        // a section *is* belongs in its heading's tooltip, so these notes are
        // reserved for state that changes (why a run is unavailable right now),
        // and a standing description no longer takes a line of the pane.
        return `<div class="rungroup"${dim ? ' style="opacity:.5"' : ""}>
            <div class="rg-head rg-run">${control}${note ? `<span class="rg-note">${note}</span>` : ""}</div>
            ${rows}
          </div>`;
      }
      /**
       * The split run control shared by the sections that can actually trigger
       * their commands. `kind` picks the run-all action; the ▾ half opens the
       * section's own menu so one command can be run instead of all of them.
       * @param {GuardianView} g - The review.
       * @param {string} kind - "manual" or "actions".
       * @param {boolean} enabled - Whether running is possible right now.
       * @param {string} label - Button text, which carries the gating reason when disabled.
       * @param {string} tip - Tooltip explaining what pressing it does, or why it cannot be pressed.
       * @returns {string}
       */
      function reviewRunControl(g, kind, enabled, label, tip) {
        const action = kind === "manual" ? "runAllManualChecks" : "runAllActionHints";
        const btn = `<button class="btn primary rg-runbtn" ${enabled ? "" : "disabled"} `
          + `data-click="${action}" data-guardian-id="${esc(g.id)}"${enabled ? ` data-tip="${esc(tip)}"` : ""}>${esc(label)}</button>`;
        // A disabled <button> swallows mouseover, so its tooltip has to live on
        // a wrapper -- same reason the merge/approve buttons do this.
        return enabled ? btn : `<span data-tip="${esc(tip)}">${btn}</span>`;
      }

      /**
       * Builds one chip for the setup strip.
       * @param {string} gid - The review this chip edits.
       * @param {string} label - Short uppercase key, e.g. "onto".
       * @param {string} value - The current value, already escaped.
       * @param {string} tip - Tooltip explaining what the setting does.
       * @param {boolean} [warn] - Render in the warning register (a setting that leaves the review unverified).
       * @returns {string}
       */
      function setupChip(gid, label, value, tip, warn) {
        // The chip's own label doubles as the group it jumps to, so the setup
        // modal opens on the field the chip was showing rather than at its top.
        return `<button class="setup-chip${warn ? " warn" : ""}" data-click="openEditReviewDetails" `
          + `data-guardian-id="${esc(gid)}" data-focus="${esc(label)}" data-tip="${esc(tip)}\n\nClick to edit — opens review setup on this setting.">`
          + `<span class="sc-k">${esc(label)}</span><b>${value}</b></button>`;
      }
      /**
       * The review's settings as one strip of chips, replacing the column of
       * read-only kv-rows RAL-410 left behind when it moved editing into the
       * Edit Details modal. Those rows cost a screen of vertical space to show
       * settings you could not act on from there, and pushed the branch stack
       * -- the thing a reviewer actually came for -- below the fold.
       *
       * Every chip opens the same modal, so the strip is both the display and
       * the edit affordance: there is no separate button to go looking for.
       * @param {GuardianView} g - The review.
       * @returns {string}
       */
      function reviewSetupStrip(g) {
        const gates = (g.checks || []).length;
        const squashOn = g.squash_projects || [];
        const projects = (g.projects && g.projects.length) ? g.projects : [g.git_root || ""];
        const squashCount = projects.filter((p) => squashOn.includes(p)).length;
        const squashText = projects.length > 1
          ? `${squashCount} of ${projects.length}`
          : (squashCount ? "on" : "off");
        const proof = (g.effective_proof_scope || "each_branch").replace(/_/g, " ");
        const model = g.resolver_model ? ` · ${esc(g.resolver_model)}` : "";
        // Gates is the one setting that can leave a review provably unverified,
        // so it is the one that earns the warning register.
        //
        // With no gates and auto-build allowed, the chip must report what has
        // actually happened rather than what is intended: the daemon only
        // infers and runs a build once the stack finishes merging. Saying
        // "auto" before then reads as "already verified" and contradicts the
        // check-gates section right below, which is still warning that nothing
        // has verified this review yet. Same `auto-built via ` prefix the
        // daemon writes into `detail` that the section keys off.
        const autoBuilt = (g.detail || "").startsWith("auto-built via ");
        const unverified = gates === 0 && !!g.skip_auto_build;
        const gatesText = gates
          ? `${gates}`
          : (g.skip_auto_build ? "none" : (autoBuilt ? "auto-built" : "inferred"));
        return `<div class="setup-strip">`
          + `<div class="setup-chips">`
          + setupChip(g.id, "onto", esc(g.base_branch || "—"),
            `Upstream branch. Every branch in this review rebases onto ${g.base_branch || "it"}, each on top of the one before it.\nChanging it rebuilds the whole stack.`)
          + setupChip(g.id, "resolver", esc(resolverOf(g)) + model,
            "The agent that resolves rebase conflicts, writes the change summary, and generates the suggested manual checks.")
          + setupChip(g.id, "proof", esc(proof),
            `How often the dedicated LLM proof pass runs${g.proof_scope ? "" : " — currently the project default"}.`)
          + setupChip(g.id, "squash", esc(squashText),
            "Whether each task branch collapses to a single commit in the review worktree.\nScope is per git project, so a multi-project review sets it independently.")
          + setupChip(g.id, "gates", esc(gatesText),
            unverified
              ? "No check gates, and skip auto-build is on — nothing verifies this review. It can reach 'in review', and be approved, without a single build or test having run.\nAdd a gate, or set [review] auto_build in the project's .ralphus.toml."
              : gates
                ? `${gates} check gate(s) must pass before this review can be approved.`
                : autoBuilt
                  ? "No gates configured, so the daemon inferred a build command and ran it once the stack merged. See the check gates section for which command."
                  : "No gates configured. The daemon will infer a build command from the diff once the stack finishes merging — until then nothing has verified this review.",
            unverified)
          + setupChip(g.id, "worktrees", g.skip_worktrees ? "shared" : "per-branch",
            g.skip_worktrees
              ? "The whole stack builds in one shared worktree instead of one per branch."
              : "Each branch rebases in its own worktree.")
          // Identity chips. These are not settings, so they do not open the
          // editor -- they carry hovercards instead, which is where the detail
          // the old kv-rows spelled out now lives.
          + `<span class="setup-chip ident" data-tip="${esc(`Where this review's branches live.\nProject: ${g.project || g.git_root || "—"}`)}">`
          + `<span class="sc-k">project</span><b>${esc(projectLabelFor(g))}</b></span>`
          + `<span class="setup-chip ident hc-anchor" data-card="gCombinedWorktree" data-guardian-id="${esc(g.id)}">`
          + `<span class="sc-k">review branch</span><b>${esc(g.review_branch || "—")}</b></span>`
          + `</div>`
          + `<button class="btn setup-edit" data-click="openEditReviewDetails" data-guardian-id="${esc(g.id)}" `
          + `data-tip="Edit this review's settings — name, upstream, resolver, proof scope, build and squash options, PR settings.\nEvery chip to the left opens this same editor, landing on the setting it shows.\nEnvironment overrides are not here: each section's ⋯ edits the environment its own commands run in.\nNothing takes effect until you click Save; Save applies every change in one request and triggers at most one rebase.">`
          + `✎ Edit setup</button></div>`;
      }

      /**
       * Renders the full review detail pane (branch stack, checks, manual commands, chat, etc).
       * @returns {void}
       */
      function renderReviewDetail() {
        const el = byId("review-detail");
        const g = guardians.find((x) => x.id === selectedGuardian);
        // `g.branches` is only present once `pollReviews`'s per-selected-guardian
        // fetch has merged full `GuardianView` detail onto this lean-list entry
        // (see the `guardians` declaration in `05-engines.js`) -- absent either
        // because the review hasn't reached the lean list yet (RAL-382,
        // `reviewDetailLoading`) or because its full detail is still in flight.
        // Either way: never the previously selected review's details, and never
        // a bare "Select a review." that reads as if the click did nothing.
        if (!g || !g.branches) {
          el.innerHTML = selectedGuardian ? `<div class="empty">Loading review…</div>` : `<div class="empty">Select a review.</div>`;
          return;
        }
        reviewDetailLoading = null;
        // RAL-14: reorder is allowed while the review is still open — not once it
        // is merged/approved/deployed (those branches are considered merged/shipped).
        const canReorder = !["merged", "approved", "cancelled", "deployed"].includes(g.status);
        const drag = canReorder ? `draggable="true" ondragstart="brDragStart(event)" ondragover="brDragOver(event)" ondragleave="brDragLeave(event)" ondrop="brDrop(event,this.dataset.guardianId)" ondragend="brDragEnd(event)"` : "";
        // Staged reorder + enable state (RAL-6, RAL-43): pendingReorder holds both
        // the branch order and per-branch enabled flags, committed only on Save.
        // All pre-save changes are local UI state — nothing hits the server until
        // Save, which persists both order and enabled states, then kicks off the
        // rebase. Discard reverts to the last server-side state.
        const serverOrder = g.branches.map((b) => b.branch);
        const serverEnabled = Object.fromEntries(g.branches.map((b) => [b.branch, b.enabled !== false]));
        const pr = (pendingReorder && pendingReorder.gid === g.id) ? pendingReorder : null;
        const hasPending = !!pr;
        const stagedOrder = pr ? pr.order : serverOrder;
        const stagedEnabled = (pr && pr.enabled) ? pr.enabled : serverEnabled;
        const byBranch = Object.fromEntries(g.branches.map((b) => [b.branch, b]));
        const orderedBranches = stagedOrder.map((n) => byBranch[n]).filter(Boolean);
        // RAL-29: multi-project tab view — filter branches to the active project tab.
        const gProjects = g.projects || [];
        const isMultiProject = gProjects.length > 1;
        const activeTab = isMultiProject
          ? (selectedProjectTabs[g.id] && gProjects.includes(selectedProjectTabs[g.id])
              ? selectedProjectTabs[g.id] : gProjects[0])
          : null;
        const visibleBranches = isMultiProject
          ? orderedBranches.filter((b) => (b.project || g.git_root) === activeTab)
          : orderedBranches;
        // RAL-43: warn when all branches are staged as disabled.
        const enabledCount = visibleBranches.filter((b) => stagedEnabled[b.branch] !== false).length;
        const allDisabled = visibleBranches.length > 0 && enabledCount === 0;
        const branches = visibleBranches.map((b, i) => {
          const isEnabled = stagedEnabled[b.branch] !== false;
          const hasDetail = !!(b.worktree || b.detail || b.source_squad_id != null || b.resolver_agent_session_id);
          const open = expandedBranches.has(`${g.id}:${b.id}`);
          const toggle = hasDetail
            ? `<span class="br-toggle" data-tip="Expand or collapse merge detail for this branch." data-click="toggleBranch" data-guardian-id="${esc(g.id)}" data-branch-id="${esc(b.id)}">${open ? "▾" : "▸"}</span>`
            : `<span class="br-toggle placeholder">▸</span>`;
          const detail = hasDetail ? `<div class="branch-detail ${open ? "" : "hidden"}">
              ${b.detail ? `<div class="kv-row" style="margin:0 0 4px"><span class="k" style="text-transform:none;letter-spacing:0">status</span><span class="v" style="font-size:12px">${detailSummary(b.detail, "Branch detail")}</span></div>` : ""}
              ${b.worktree ? `<div class="kv-row" style="margin:0"><span class="k" style="text-transform:none;letter-spacing:0">review worktree</span><span class="v mono hc-anchor" style="font-size:11px" data-card="gBranchWorktree" data-guardian-id="${esc(g.id)}" data-branch-id="${esc(b.id)}">${esc(b.worktree)}</span>${b.merge_status === "merged" ? ` ${mergedBranchBadge()}` : ""}</div>` : ""}
              ${(b.worktree || b.source_squad_id != null) ? `<div class="row" style="margin:2px 0 4px">${worktreeCellBtn(b, `${g.id}:${b.id}`)}</div>` : ""}
              ${branchPrSection(g, b)}
            </div>` : "";
          // RAL-43: enable/disable toggle — staged like drag-reorder, takes effect on Save.
          const enableToggle = canReorder
            ? `<button class="icon-btn" data-click="toggleBranchEnabled" data-guardian-id="${esc(g.id)}" data-branch="${esc(b.branch)}" title="${isEnabled ? 'Drop this branch from the rebase stack — it stays visible and can be re-enabled. Staged: takes effect on Save.' : 'Re-enable this branch — it will be included in the rebase stack again. Staged: takes effect on Save.'}" style="font-size:11px;padding:1px 5px;border-color:${isEnabled ? 'var(--border)' : 'var(--queued)'};color:${isEnabled ? 'var(--muted)' : 'var(--queued)'}">${isEnabled ? '⊙' : '⊘'}</button>`
            : "";
          // RAL-69: inform the reviewer when a force-disabled branch is now safe to re-enable.
          // Shown only when the server signals can_reenable (not yet dismissed, all sources done).
          const reEnableIcon = (!hasPending && b.can_reenable)
            ? `<span style="display:inline-flex;align-items:center;gap:2px">
                 <span class="badge" style="color:var(--accent);border-color:var(--accent);font-size:11px" data-tip="This branch was disabled because it wasn't ready when the review was force-started, but it can be safely enabled now.\nUse the ⊘ toggle and Save to re-enable it.\nDismissing this notice does not auto-enable the branch.">ℹ can enable now</span>
                 <button class="icon-btn" data-click="dismissReenable" data-guardian-id="${esc(g.id)}" data-branch-id="${esc(b.id)}" style="font-size:10px;padding:1px 4px;color:var(--muted);border-color:var(--border)" data-tip="Permanently dismiss this notice for this branch.\nThe branch stays disabled — use the ⊘ toggle and Save to re-enable it when ready.">✕</button>
               </span>`
            : "";
          // RAL-118: move this branch into a different review's stack. Gated the
          // same as enableToggle (canReorder) plus an explicit exclusion of
          // "merging" -- the server enforces the real gate, but hiding the
          // button while a rebase is in flight avoids a guaranteed 409.
          // Everything that acts on one branch now lives behind its own ⋯,
          // rather than as a row of bare glyphs whose meaning you had to hover
          // to learn. Matches the ⋯ every section heading carries.
          const branchMenuBtn = `<button class="section-menu" data-click="openReviewBranchMenu" `
            + `data-guardian-id="${esc(g.id)}" data-branch-id="${esc(b.id)}" `
            + `data-can-move="${canReorder && g.status !== "merging" ? "1" : "0"}" `
            + `data-tip="Actions for this branch and its worktree — logs, environment, terminal, and moving it to another review.">⋯</button>`;
          const isBranchSel = selectedBranch[g.id] === b.branch;
          return `
          <div class="branch-item"${isEnabled ? "" : ' style="opacity:0.45"'}>
            <div class="branch-row selectable${isBranchSel ? " sel" : ""}" data-branch="${esc(b.branch)}" data-branch-id="${esc(b.id)}" ${drag} data-click="selectBranchRow" data-dblclick="toggleBranch" data-guardian-id="${esc(g.id)}" data-tip="Click to inspect this branch in the pane on the right.\nDouble-click to expand its detail here. Drag to reorder.">
              ${canReorder ? '<span class="grip" data-tip="Drag to reorder branches — the merge order determines the rebase stack.">⋮⋮</span>' : ""}
              <span class="branch-idx" data-tip="This branch's place in the rebase stack.">${i + 1}</span>
              ${toggle}
              <span>${gdot(b.merge_status || "")}</span>
              <span class="mono" style="flex:1${isEnabled ? "" : ";color:var(--muted)"}" data-tip="${esc(b.branch)}\nSelect it to read its position, source and status in the inspector.">${esc(b.branch)}</span>
              ${isEnabled ? `${branchBadge(b)} ${autoFixExhaustedBadge(pullRequests[g.id] || [], b)} ${pill(b.merge_status || "")} ${branchPrLink(g, b)}` : '<span class="badge" style="color:var(--muted);border-color:var(--border);font-size:11px">disabled</span>'}
              ${enableToggle}${reEnableIcon}${branchMenuBtn}
            </div>
            ${branchConflictBar(b)}
            ${branchConflictFiles(g.id, b)}
            ${detail}
          </div>`;
        }).join("");
        // RAL-101/RAL-110: when no explicit check gates are configured, the
        // daemon falls back first to the project's `auto_build` default
        // (`.ralphus.toml`), and — if that isn't configured either — to a
        // build command the resolver agent infers from the diff in the same
        // call that generates the manual-check commands. Either way this runs
        // once the stack finishes merging, so "in review" still means
        // "testable" instead of silently reporting done with zero
        // verification. The daemon records what happened as g.detail,
        // prefixed distinctively so the UI can tell it apart from an
        // unrelated merge-failure/opt-out detail string.
        const autoBuildPrefixes = [
          { prefix: "auto-built via project default: ", source: "the project's .ralphus.toml [review] auto_build" },
          { prefix: "auto-built via inferred build command: ", source: "the resolver agent, inferred from the diff" },
        ];
        const gDetail = g.detail || "";
        const autoBuildMatch = gDetail && autoBuildPrefixes.find((p) => gDetail.startsWith(p.prefix));
        const autoBuiltCmd = autoBuildMatch ? gDetail.slice(autoBuildMatch.prefix.length) : null;
        const gChecks = g.checks || [];
        // Check gates render as a command list -- the same shape manual checks
        // and test actions use -- rather than a row of chips. A gate is a
        // command you want to read in full and whose outcome you want to see,
        // so it gets a row of its own with its text elided rather than
        // wrapping, and its own ⋯ for the log.
        // Check gates as a run-group: a header saying what the gates promise,
        // then one row per command. Previously a row of wrapping chips for the
        // configured case and a lone coloured chip for every other case, which
        // gave four quite different situations the same undifferentiated shape.
        // Check gates wear the same frame as the other runnable sections.
        // Their run slot is a badge, not a button: gates fire automatically
        // after each merge commit and the daemon exposes no route to trigger
        // one by hand, so a button here would be a lie.
        const gateBadge = (/** @type {string} */ text, /** @type {string} */ tip) =>
          `<span class="rg-when" data-tip="${esc(tip)}">${esc(text)}</span>`;
        const gateRow = (/** @type {string} */ cmd, /** @type {number} */ i, /** @type {string} */ icon, /** @type {string} */ iconTip, /** @type {string} */ iconColor) => {
          const key = `${g.id}:gate:${i}`;
          return `<div class="cmd-row selectable${isCommandRowSelected(key) ? " sel" : ""}" data-click="selectReviewCommandRow" data-dblclick="toggleReviewCommandFull" data-guardian-id="${esc(g.id)}" data-key="${esc(key)}" data-cmd="${esc(cmd)}" data-tip="Select this gate to scope the log drawer to it.\nDouble-click to open it in full.">
              <span class="cmd-lock"${iconColor ? ` style="color:${iconColor}"` : ""} data-tip="${esc(iconTip)}">${icon}</span>
              <span class="cmd-text mono" data-tip="${esc(cmd)}">${esc(cmd)}</span>
              ${commandRunStatus(key)}
              <button class="cmd-expand" data-click="toggleReviewCommandFull" data-key="${esc(key)}" data-tip="Show or hide this gate in full beneath its row.">${commandFullOpen[key] ? "−" : "+"}</button>
              <button class="section-menu" data-click="openReviewCommandMenu" data-guardian-id="${esc(g.id)}" data-key="${esc(key)}" data-cmd="${esc(cmd)}" data-tip="Actions for this gate — its logs, its environment, copy it.">⋯</button>
            </div>${commandFullBlock(key, cmd)}`;
        };
        const checks = gChecks.length
          ? reviewRunGroup(
            gateBadge("runs after merge", "Check gates are run by the daemon after each merge commit and on the combined worktree. There is no way to trigger one by hand from here."),
            `All ${gChecks.length} must pass before this review can be approved.${
              g.skip_auto_build ? " <b>Skip auto-build is on, so these are skipped.</b>" : ""}`,
            gChecks.map((c, i) => gateRow(c, i, "🔒", "Check gate — runs after each merge commit and on the combined review worktree.\nAll gates must pass before the review can be approved.", "")).join(""),
            !!g.skip_auto_build)
          : autoBuiltCmd
            ? reviewRunGroup(
              gateBadge("auto-built", `No gates were configured, so this build command was inferred and run once the stack merged — sourced from ${autoBuildMatch ? autoBuildMatch.source : ""}.`),
              `No gates configured, so a build was inferred and run once the stack merged. Nothing to do.`,
              gateRow(autoBuiltCmd, 0, "🔧", "Inferred and run automatically, so 'in review' still means the code builds even with no gates configured.", "var(--done)"))
            : reviewRunGroup(
              gateBadge(g.skip_auto_build ? "nothing runs" : "after merge",
                g.skip_auto_build
                  ? "No gates, and skip auto-build is on — nothing verifies this review at any point."
                  : "The daemon infers a build command from the diff once the stack finishes merging."),
              g.skip_auto_build
                ? `<b class="rg-warn">Nothing verifies this review.</b> No check gates, and skip auto-build is on — it can reach <i>in review</i> and be approved without a build or test having run. Add a gate in Setup, or set <span class="mono">[review] auto_build</span> in the project's <span class="mono">.ralphus.toml</span>.`
                : `No check gates configured. A build is inferred from the diff once the stack finishes merging — until then, nothing has verified this review.`,
              "")
;
        // RAL-410 moved skip-auto-build/skip-per-branch-worktrees, squash,
        // resolver agent/model and proof scope into the "Edit Details" modal
        // but left a column of read-only kv-rows behind for them. Those now
        // render as the setup strip (reviewSetupStrip) directly under the
        // banner, where the display is also the edit affordance.
        el.innerHTML = `<div class="review-cmdbar">
            <div class="cmd-row hc-anchor" data-card="gReviewId" data-guardian-id="${esc(g.id)}">
              ${gdot(g.status)}<span class="rid">${esc(g.name)}</span> ${pill(g.status)} ${arbiterBadge(g)}
              ${reviewCostChip(g)}
              <div class="cmd-actions">
                ${watchersHtml(`guardian:${g.id}`)}
                <button class="icon-btn" data-click="toggleReviewDock" data-guardian-id="${esc(g.id)}" data-tip="Open the log drawer docked at the bottom of this review.\nIt follows whatever you select — the whole review, one branch, or one command — and stays open while you work instead of covering the page.">☰ Logs</button>
                <button class="icon-btn" data-click="openEditReviewDetails" data-guardian-id="${esc(g.id)}" data-tip="Edit this review's settings — name, upstream branch, resolver, proof scope, build/squash options, PR settings.\nEnvironment overrides live on each section's ⋯, next to the commands they govern.\nNothing takes effect until you click Save; Save applies every change in a single request and triggers at most one rebase.">✎ Setup</button>
                ${reviewPrimaryAction(g)}
              </div>
            </div>
            ${reviewPipeline(g)}
          </div>
          ${g.status === "merging" ? mergeProgress(g) : ""}
          ${conflictProgress(g)}
          ${reviewSetupStrip(g)}
          ${reviewIdentityRow(g, isMultiProject)}
          ${renderChangeSummary(g)}
          ${rebaseExhaustedNotice(g, canReorder)}
          <h3 class="section">branches${sectionMenuBtn(g.id, "branches")}${canReorder ? ' <span class="k" style="text-transform:none;letter-spacing:0">— drag to reorder · toggle ⊙/⊘ to enable/disable</span>' : ""}${hasPending ? ' <span class="badge warn2" data-tip="Unsaved order or enable/disable changes — click Save to apply, or Discard to revert.">● unsaved changes</span>' : ""}</h3>
          ${allDisabled ? `<div class="warn" style="margin:4px 0 8px">All branches are disabled — saving will make this review a no-op (no rebase runs). Re-enable at least one branch before saving, or click Discard.</div>` : ""}
          ${isMultiProject ? `<div class="row" style="margin-bottom:8px;gap:4px">${(g.projects||[]).map((p) => {
              const label = p.split(/[\\/]/).filter(Boolean).pop() || p;
              const count = g.branches.filter((b) => (b.project || g.git_root) === p).length;
              const anyFailed = g.branches.filter((b) => (b.project || g.git_root) === p).some((b) => b.merge_status === "failed");
              return `<span class="chip ${p === activeTab ? "active" : ""}" style="${anyFailed ? "border-color:var(--failed);" : ""}" data-click="selectProjectTab" data-guardian-id="${esc(g.id)}" data-project="${esc(p)}" data-tip="${esc(p)}">${esc(label)} <span style="color:var(--muted);font-size:11px">(${count})</span></span>`;
            }).join("")}</div>` : ""}
          <div id="branch-list">${branches||'<div class="empty">none</div>'}</div>
          ${hasPending ? `<div class="row" style="margin-top:8px;align-items:center">
            <button class="btn primary" data-click="saveReorder" data-guardian-id="${esc(g.id)}" data-tip="Persist the new branch order and enabled states, then re-run the rebase.\nDisabled branches are skipped in the rebase stack.">Save</button>
            <button class="btn" data-click="discardReorder" data-guardian-id="${esc(g.id)}" data-tip="Discard the unsaved branch reorder and enable/disable changes, reverting to the last saved state.">Discard</button>
            <span class="k" style="text-transform:none;letter-spacing:0;color:var(--muted)">Save persists the order and enabled states, then rebases</span>
          </div>` : ""}
          ${(() => {
            // RAL-69: warn upfront when some enabled branches aren't ready yet --
            // Merge / rebase still works, but will offer to continue with just
            // the ready ones (see mergeReview's confirmMergeSubset prompt).
            const hasNotReady = g.status === "collecting" && (g.branches || []).some(
              (b) => b.enabled !== false && b.source_cell_state !== "done"
            );
            return hasNotReady
              ? `<div class="warn" style="margin:8px 0 4px">Some branches are not yet ready (still running or never submitted). Merge / rebase will offer to continue with just the ready branches.</div>`
              : "";
          })()}
          <h3 class="section">check gates${sectionMenuBtn(g.id, "gates")}</h3>${checks}
          ${g.detail && !autoBuiltCmd ? `<div class="warn">${detailSummary(g.detail, "Review detail")}</div>` : ""}
          ${(() => {
            // RAL-77: user-declared test actions from [[review.action]] in TOML.
            const hints = g.action_hints || [];
            // An absent section reads as "not applicable to this review", which
            // is wrong: every review *could* have test actions, they just have
            // to be declared in the task file. Saying so -- and saying where --
            // is the difference between a missing feature and a missing input.
            if (hints.length === 0) {
              return `<h3 class="section" data-tip="User-declared test actions from the task TOML [[review.action]] blocks.\nLabelled buttons give reviewers one-click access to targeted manual checks.">test actions <span class="k" style="text-transform:none;letter-spacing:0">— none</span>${sectionMenuBtn(g.id, "actions")}</h3>
                ${reviewRunGroup(
                  reviewRunControl(g, "actions", false, "▶ Run all",
                    "There are no test actions to run. They are authored, not generated — add [[review.action]] blocks to the task file and each becomes a one-click check here."),
                  `None declared. Test actions are authored in <span class="mono">[[review.action]]</span> blocks in the task file — unlike manual checks below, which the resolver agent writes for you.`,
                  "")}`;
            }
            // Same command-list shape as check gates and manual checks. A
            // labelled button alone hid what the action would actually run,
            // which for a one-click check against someone else's branch is
            // exactly the thing you want to read before pressing it.
            const rows = hints.map((h, i) => {
              const key = `${g.id}:action:${i}`;
              const needsInput = !!(h.inputs && h.inputs.length);
              const label = esc(h.label || "Run");
              if (!h.command) {
                return `<div class="cmd-row" style="opacity:.55">
                    <button class="cmd-run" disabled data-tip="Prompt-based test actions expand via the resolver LLM before running, and that expansion isn't wired up — only command-based [[review.action]] entries are runnable today.">▶</button>
                    <span class="cmd-label">${label}</span>
                    <span class="cmd-text mono" data-tip="Prompt: ${esc(h.prompt || "")}">${esc(h.prompt || "(prompt)")}</span>
                  </div>`;
              }
              const cmdText = h.command || "";
              return `<div class="cmd-row selectable${isCommandRowSelected(key) ? " sel" : ""}" data-click="selectReviewCommandRow" data-dblclick="toggleReviewCommandFull" data-guardian-id="${esc(g.id)}" data-key="${esc(key)}" data-cmd="${esc(cmdText)}" data-tip="Select this action to scope the log drawer to it.\nDouble-click to open it in full.">
                  <button class="cmd-run" data-click="runCheck" data-kind="action" data-guardian-id="${esc(g.id)}" data-i="${i}" data-runkey="${esc(key)}"
                    data-tip="Run this action in the built review worktree.\nRun: ${esc(cmdText)}${needsInput ? `\nUses the values in + — its current ones, or this review's last ones if you have not opened it.` : ""}">▶</button>
                  <span class="cmd-label">${label}</span>
                  <span class="cmd-text mono" data-tip="${esc(cmdText)}">${esc(cmdText)}</span>
                  ${commandRunStatus(key)}
                  <button class="cmd-expand" data-click="toggleReviewCommandFull" data-key="${esc(key)}" data-tip="${needsInput ? "Show this command in full, with its values filled in and editable beneath it." : "Show or hide this command in full beneath its row."}">${commandFullOpen[key] ? "−" : "+"}</button>
                  <button class="section-menu" data-click="openReviewCommandMenu" data-guardian-id="${esc(g.id)}" data-key="${esc(key)}" data-cmd="${esc(cmdText)}" data-tip="Actions for this check — its logs, its environment, copy it.">⋯</button>
                </div>${commandFullBlock(key, cmdText, { g, check: h, kind: "action", i })}`;
            }).join("");
            return `<h3 class="section" data-tip="User-declared test actions from the task TOML [[review.action]] blocks.\nAuthored by the task author, not generated — each runs in the built review worktree.\nLabelled buttons give reviewers one-click access to targeted manual checks.">test actions${sectionMenuBtn(g.id, "actions")}</h3>
              ${reviewRunGroup(
                reviewRunControl(g, "actions", hints.some((h) => h.command && !(h.inputs && h.inputs.length)), "▶ Run all",
                  "Run every command-based test action, each in the built review worktree.\nActions needing input are skipped — run those from their own row."),
                "",
                rows)}`;
          })()}
          ${(() => {
            // RAL-103: checks_state is "ready" (commands available), "generating"
            // (every enabled branch has finished rebasing cleanly and the
            // resolver agent is producing commands right now — a narrow window,
            // not the whole merging phase), or "waiting" (branches are still
            // being collected or rebased, so generation hasn't started). The
            // button is always shown so users can see the gating state instead
            // of the section silently vanishing.
            const cmds = g.manual_commands || [];
            const state = g.checks_state || (cmds.length ? "ready" : "waiting");
            const isReady = state === "ready";
            const menuOpen = !!manualMenuOpen[g.id];
            // Every item here runs, including a parameterised one -- picking a
            // command from a "run one of these" menu can only sensibly mean run
            // it. Its values are edited from its row's + instead.
            const menuItems = cmds.map((cmd, i) => {
              const cmdText = cmd.command || "";
              const needsInput = !!(cmd.inputs && cmd.inputs.length);
              return `<div data-click="runCheck" data-kind="manual" data-guardian-id="${esc(g.id)}" data-i="${i}" style="padding:6px 12px;cursor:pointer;font-size:12px;font-family:monospace;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;max-width:360px" data-tip="Run: ${esc(cmdText)}\nLaunches in the built review worktree.${needsInput ? "\nUses this review's current values for its parameters — edit them from the row's + in the section below." : ""}" onmouseover="this.style.background='var(--panel-2)'" onmouseout="this.style.background=''">${esc(cmdText)}</div>`;
            }).join("");
            const gateTip = isReady
              ? `Run all ${cmds.length} suggested manual check command(s) in a new terminal window.\nEach launches in the built review worktree.`
              : state === "generating"
                ? "Generating suggested manual check commands now — every enabled branch has finished rebasing cleanly and the resolver agent is producing them.\nThis button enables once they're ready."
                : "Manual checks aren't generated yet — they're only produced after every enabled branch in this review has finished rebasing with no pending conflicts.\nStill collecting or rebasing branches.";
            const label = isReady ? "▶ Run all" : (state === "generating" ? "▶ Generating…" : "▶ Run all");
            const runControl = reviewRunControl(g, "manual", isReady && !!cmds.length, label, gateTip);
            // What the section says about itself while it has nothing to show.
            // It used to be a lone disabled button labelled "Waiting on
            // branches…" with a stray Live View button beside it, which read
            // as a different, older widget than the populated case.
            const waitingNote = state === "generating"
              ? `Every enabled branch has rebased cleanly, and the resolver agent is writing these now.`
              : `Not generated yet. The resolver agent writes these once every enabled branch has rebased with no pending conflicts — this review is still collecting or rebasing.`;
            return `<h3 class="section" data-tip="Shell commands suggested by the resolver agent to manually verify these changes.\nSuggested against this stack's changes and advisory — they never block Approve or Merge / rebase.\nGenerated once when the review branch is rebuilt (or when the rebuilt stack's changes change), and re-generated on demand from this section's ⋯ menu.">manual checks${sectionMenuBtn(g.id, "manual")}</h3>
              ${isReady && cmds.length
                ? reviewRunGroup(
                  runControl,
                  "",
                  cmds.map((cmd, i) => {
                      const cmdText = cmd.command || "";
                      const key = `${g.id}:manual:${i}`;
                      const needsInput = !!(cmd.inputs && cmd.inputs.length);
                      return `<div class="cmd-row selectable${isCommandRowSelected(key) ? " sel" : ""}" data-click="selectReviewCommandRow" data-dblclick="toggleReviewCommandFull" data-guardian-id="${esc(g.id)}" data-key="${esc(key)}" data-cmd="${esc(cmdText)}" data-tip="Select this check to scope the log drawer to it.\nDouble-click to open it in full.">
                          <button class="cmd-run" data-click="runCheck" data-kind="manual" data-guardian-id="${esc(g.id)}" data-i="${i}" data-runkey="${esc(key)}"
                            data-tip="Run this check in the built review worktree.\nRun: ${esc(cmdText)}${needsInput ? `\nUses the values in + — its current ones, or this review's last ones if you have not opened it.` : ""}">▶</button>
                          <span class="cmd-text mono" data-tip="${esc(cmdText)}">${esc(cmdText)}</span>
                          ${commandRunStatus(key)}
                          <button class="cmd-expand" data-click="toggleReviewCommandFull" data-key="${esc(key)}" data-tip="${needsInput ? "Show this command in full, with its values filled in and editable beneath it." : "Show or hide this command in full beneath its row."}">${commandFullOpen[key] ? "−" : "+"}</button>
                          <button class="section-menu" data-click="openReviewCommandMenu" data-guardian-id="${esc(g.id)}" data-key="${esc(key)}" data-cmd="${esc(cmdText)}" data-tip="Actions for this check — its logs, its environment, copy it.">⋯</button>
                        </div>${commandFullBlock(key, cmdText, { g, check: cmd, kind: "manual", i })}`;
                    }).join(""))
                : reviewRunGroup(runControl, waitingNote, "")}
              `;
          })()}`;
        attachPeekResizeHandlers();
        restorePeekScrollPositions(); // RAL-471: the innerHTML rewrite above just destroyed/recreated any peek `<pre>` nodes, dropping their scroll position
        // The inspector and the dock are siblings of this pane, not children of
        // it, so they re-render alongside rather than being rebuilt by the
        // innerHTML above -- which is what lets the inspector keep its own tab
        // and scroll while the stack behind it updates.
        renderReviewInspector();
        renderReviewDock();
      }
      // CCTL-135: per-branch merge detail is collapsed by default; toggle open.
      /**
       * Toggles a review branch's expanded merge-detail block.
       * @param {MouseEvent} e
       * @param {string} gid
       * @param {string} branchId
       * @returns {void}
       */
      function toggleBranch(e, gid, branchId) {
        e.stopPropagation();
        const key = `${gid}:${branchId}`;
        if (expandedBranches.has(key)) expandedBranches.delete(key); else expandedBranches.add(key);
        // RAL-481: expanding/collapsing a different branch's detail (and the
        // live-terminal peek box it may reveal) shouldn't jump the pane back
        // to the top -- see selectBranchRow's matching fix.
        preservePaneScroll(reviewDetailScrollEl(), renderReviewDetail);
      }
      // RAL-29: switch the active project tab in a multi-project review.
      /**
       * Switches the active project tab in a multi-project review.
       * @param {string} gid
       * @param {string} proj
       * @returns {void}
       */
      function selectProjectTab(gid, proj) { selectedProjectTabs[gid] = proj; preservePaneScroll(reviewDetailScrollEl(), renderReviewDetail); }
      // branch drag-to-reorder + enable/disable (RAL-14/RAL-6/RAL-43).
      // pendingReorder holds both the staged order and per-branch enabled flags.
      // Nothing hits the server until Save, which persists both and kicks off the
      // rebase. Discard reverts to the last server-saved state.
      /** @type {string|null} */
      let dragBranch = null;
      /**
       * @typedef {object} PendingReorder
       * @property {string} gid
       * @property {string[]} order
       * @property {{[key: string]: boolean}} enabled
       */
      /** @type {PendingReorder|null} */
      let pendingReorder = null;
      /**
       * Drag-start handler for a branch row: records the dragged branch name.
       * @param {DragEvent} e
       * @returns {void}
       */
      function brDragStart(e) { dragBranch = /** @type {HTMLElement} */ (e.currentTarget).dataset.branch ?? null; if (e.dataTransfer) e.dataTransfer.effectAllowed = "move"; }
      /**
       * Drag-over handler for a branch row: shows the drop-target highlight.
       * @param {DragEvent} e
       * @returns {void}
       */
      function brDragOver(e) { e.preventDefault(); /** @type {HTMLElement} */ (e.currentTarget).classList.add("dragover"); }
      /**
       * Drag-leave handler for a branch row: clears the drop-target highlight.
       * @param {DragEvent} e
       * @returns {void}
       */
      function brDragLeave(e) { /** @type {HTMLElement} */ (e.currentTarget).classList.remove("dragover"); }
      /**
       * Drag-end handler: clears any lingering drop-target highlight.
       * @returns {void}
       */
      function brDragEnd() { document.querySelectorAll(".branch-row.dragover").forEach((x) => x.classList.remove("dragover")); }
      /**
       * Drop handler for a branch row: stages a reordered branch list (not yet saved).
       * @param {DragEvent} e
       * @param {string} gid
       * @returns {void}
       */
      function brDrop(e, gid) {
        e.preventDefault();
        const target = /** @type {HTMLElement} */ (e.currentTarget); target.classList.remove("dragover");
        const tgt = target.dataset.branch;
        if (!dragBranch || dragBranch === tgt) { dragBranch = null; return; }
        const dragged = dragBranch;
        const g = guardians.find((x) => x.id === gid);
        if (!g) { dragBranch = null; return; }
        const cur = (pendingReorder && pendingReorder.gid === gid) ? pendingReorder : null;
        // Base the move on the current staged order (or the server order if none).
        const base = cur ? cur.order.slice() : g.branches.map((b) => b.branch);
        // Preserve any staged enabled state so a drag does not reset toggle changes.
        const curEnabled = (cur && cur.enabled) ? cur.enabled : Object.fromEntries(g.branches.map((b) => [b.branch, b.enabled !== false]));
        // Move-to-index: pull the dragged branch out, reinsert at the target's
        // slot — after it when moving down, before it when moving up. So in
        // [1,2,3], dragging 1 onto 3 yields [2,3,1] (not a no-op).
        const from = base.indexOf(dragged), to = base.indexOf(tgt ?? "");
        const order = base.filter((b) => b !== dragged);
        let insertAt = order.indexOf(tgt ?? "");
        if (from < to) insertAt += 1;
        order.splice(insertAt, 0, dragged);
        dragBranch = null;
        // RAL-6: any drop stages a pending reorder. No exact-order comparison —
        // dragging back to the original arrangement is undone via Discard, not by
        // silently clearing the pending state here.
        pendingReorder = { gid, order, enabled: curEnabled };
        renderReviewDetail();
      }
      // RAL-43: toggle a branch's staged enabled/disabled state. Staged like a
      // drag-reorder — nothing hits the server until Save. Preserves any pending order.
      /**
       * Toggles a branch's staged enabled/disabled state (not yet saved).
       * @param {string} gid
       * @param {string} branchName
       * @returns {void}
       */
      function toggleBranchEnabled(gid, branchName) {
        const g = guardians.find((x) => x.id === gid);
        if (!g) return;
        const cur = (pendingReorder && pendingReorder.gid === gid) ? pendingReorder : null;
        const curOrder = cur ? cur.order : g.branches.map((b) => b.branch);
        const curEnabled = (cur && cur.enabled)
          ? { ...cur.enabled }
          : Object.fromEntries(g.branches.map((b) => [b.branch, b.enabled !== false]));
        curEnabled[branchName] = !curEnabled[branchName];
        pendingReorder = { gid, order: curOrder, enabled: curEnabled };
        renderReviewDetail();
      }
      // Save (RAL-6/RAL-43): persist the staged order and enabled states, then kick
      // off the rebase so branches are re-stacked (disabled ones skipped).
      /**
       * Persists the staged branch order/enabled states and triggers a rebase.
       * @param {string} gid
       * @returns {Promise<void>}
       */
      async function saveReorder(gid) {
        if (!pendingReorder || pendingReorder.gid !== gid) return;
        const order = pendingReorder.order;
        const enabled = pendingReorder.enabled || {};
        pendingReorder = null;
        await guardianAction(`/api/guardians/${gid}/branches/reorder`, { order, enabled });
        await guardianAction(`/api/guardians/${gid}/merge`);
        tick();
      }
      // Discard (RAL-6/RAL-43): drop all unsaved local changes; the next render
      // falls back to the last server-saved order and enabled states.
      /**
       * Discards the staged branch reorder/enable changes, reverting to the last server state.
       * @param {string} gid
       * @returns {void}
       */
      function discardReorder(gid) { if (pendingReorder && pendingReorder.gid !== gid) return; pendingReorder = null; renderReviewDetail(); }
      /**
       * `POST .../pull-requests` replies 202 the instant the request is
       * accepted -- the actual git push and forge API call happen on a
       * detached background thread (see `start_submit_pull_requests` in
       * `daemon/src/pr.rs`) so a slow/failing network call doesn't block the
       * request thread. That means a failure (missing forge token, a
       * rejected push, ...) is otherwise silent: the click "succeeds" and
       * nothing ever tells the user it didn't actually work. This polls for
       * a few seconds right after the click for a new PR row, a fresh
       * Cartographer `source=pr`/`level=error` row (toasted red), or -- for a
       * whole-stack submission that created nothing new but still corrected
       * an existing PR's base (RAL-190+, see `submit_stack_for_guardian` in
       * `daemon/src/pr.rs`) -- the `source=pr`/`level=info` "pr stack
       * submission completed" summary row. If nothing shows up within the
       * budget this gives up and reports `completed: false` -- the passive
       * `pollPrErrors` (wired into the normal 60s board refresh) still
       * catches a slower failure eventually.
       * @param {string} gid
       * @param {number} sinceMs
       * @returns {Promise<{created: number, resynced: number, errored: boolean, completed: boolean}>}
       */
      async function waitForPrOutcome(gid, sinceMs) {
        const before = (pullRequests[gid] || []).length;
        for (let i = 0; i < 6; i++) {
          await new Promise((resolve) => setTimeout(resolve, 700));
          await pollPullRequests(gid);
          const created = (pullRequests[gid] || []).length - before;
          if (created > 0) return { created, resynced: 0, errored: false, completed: true };
          if (await surfacePrErrors(gid, sinceMs)) return { created: 0, resynced: 0, errored: true, completed: true };
          const summary = await fetchPrStackSummary(gid, sinceMs);
          if (summary) return { created: summary.created || 0, resynced: summary.resynced || 0, errored: false, completed: true };
        }
        return { created: 0, resynced: 0, errored: false, completed: false };
      }
      /**
       * Looks for a fresh `source=pr`/`level=info` "pr stack submission
       * completed" Cartographer row (RAL-190+) at/after `sinceMs`, returning
       * its `{created, resynced}` payload, or `null` if it hasn't landed yet.
       * @param {string} gid
       * @param {number} sinceMs
       * @returns {Promise<{created?: number, resynced?: number}|null>}
       */
      async function fetchPrStackSummary(gid, sinceMs) {
        try {
          const p = new URLSearchParams({ guardian_id: gid, source: "pr", level: "info", limit: "5", since_ms: String(sinceMs) });
          const res = await fetch(`/api/cartographer?${p.toString()}`);
          if (!res.ok) return null;
          /** @type {{rows: CartographerRow[], total: number}} */
          const data = await res.json();
          const row = (data.rows || []).find((r) => r.message === "pr stack submission completed");
          return row ? row.payload || {} : null;
        } catch (e) { return null; }
      }
      /**
       * Fetches recent Cartographer `source=pr`/`level=error` rows for a
       * guardian and toasts any not yet surfaced -- shared by
       * `waitForPrOutcome` (fast path, right after a click) and
       * `pollPrErrors` (passive path, the normal board refresh cycle).
       * @param {string} gid
       * @param {number} [sinceMs] - only consider rows at/after this time; omit to consider all recent rows (used by the passive poll, which relies on `prErrorHighWater` instead to avoid re-toasting).
       * @returns {Promise<boolean>} whether a new error was found and toasted
       */
      async function surfacePrErrors(gid, sinceMs) {
        try {
          const p = new URLSearchParams({ guardian_id: gid, source: "pr", level: "error", limit: "5" });
          if (sinceMs !== undefined) p.set("since_ms", String(sinceMs));
          const res = await fetch(`/api/cartographer?${p.toString()}`);
          if (!res.ok) return false;
          /** @type {{rows: CartographerRow[], total: number}} */
          const data = await res.json();
          const rows = data.rows || [];
          if (!rows.length) return false;
          const seen = prErrorHighWater.get(gid);
          const maxId = Math.max(...rows.map((r) => r.id));
          if (seen === undefined) {
            // first look at this guardian this session -- record the current
            // high-water mark without toasting, so pre-existing errors from
            // before the page was open don't retroactively pop up.
            prErrorHighWater.set(gid, maxId);
            return sinceMs !== undefined && rows.some((r) => r.at_ms >= sinceMs);
          }
          const fresh = rows.filter((r) => r.id > seen);
          if (!fresh.length) return false;
          prErrorHighWater.set(gid, maxId);
          fresh.sort((a, b) => a.id - b.id).forEach((r) => {
            const detail = (r.payload && r.payload.error) || r.message;
            notify("error", `PR submission failed: ${detail}`);
          });
          return true;
        } catch (e) { return false; }
      }
      /**
       * Passive check for PR-submission failures on the currently selected
       * review, wired into the normal board refresh cycle (`pollReviews`) so
       * a failure is surfaced even if `waitForPrOutcome`'s short post-click
       * window already elapsed.
       * @param {string} gid
       * @returns {Promise<void>}
       */
      async function pollPrErrors(gid) {
        await surfacePrErrors(gid, undefined);
      }
      /**
       * Submits the review's whole PR stack (RAL-190+): a PR for every
       * enabled branch lacking an open one, chained onto the branch below it.
       * `branch_id` omitted is what tells the daemon this is a whole-stack
       * request rather than one specific branch -- see the `PrRequest` doc
       * comment in `daemon/src/pr.rs`. Toasts once when the request is
       * accepted, then again once `waitForPrOutcome` resolves what actually
       * happened -- a submission that only corrected an existing PR's base
       * (nothing new to open) would otherwise look like it silently did
       * nothing.
       * @param {string} gid
       * @returns {Promise<void>}
       */
      async function submitPrStack(gid) {
        const sinceMs = Date.now();
        const resp = await guardianAction(`/api/guardians/${gid}/pull-requests`, { prs: [{}] });
        if (!resp || !resp.ok) return;
        notify("info", "PR stack requested — pushing branches and opening PRs in the background.");
        const outcome = await waitForPrOutcome(gid, sinceMs);
        if (!outcome.errored) {
          const parts = [];
          if (outcome.created > 0) parts.push(`${outcome.created} PR${outcome.created === 1 ? "" : "s"} opened`);
          if (outcome.resynced > 0) parts.push(`${outcome.resynced} base${outcome.resynced === 1 ? "" : "s"} corrected`);
          if (parts.length) {
            notify("info", `PR stack: ${parts.join(", ")}.`);
          } else if (outcome.completed) {
            notify("info", "PR stack already up to date — nothing to open.");
          }
        }
        renderReviewDetail();
      }
      /**
       * Pulls a PR branch's fetched commits into its owning review worktree (RAL-190).
       * @param {string} prId
       * @returns {Promise<void>}
       */
      async function pullPrCommits(prId) {
        await guardianAction(`/api/pull-requests/${prId}/pull-from-pr`);
        tick();
      }
      // RAL-24: candidate base-branch lists, keyed by guardian id. Fetched once on
      // focus and cleared when the user commits a base change so the next open
      // re-fetches with the new remote scope.
      /** @type {{[key: string]: string[]}} */
      let baseBranchCache = {};
      // RAL-139: persists the user-dragged height of live tmux pane peek boxes
      // across re-renders and tab navigation within this browser session.
      let peekPaneHeight = 320;
      // RAL-272: per-branch read-only feedback thread, keyed by `${gid}:${branchId}`.
      /** @type {{[key: string]: ChatMessage[]}} */
      let branchMessages = {};
      // RAL-272: keys (`${gid}:${branchId}:${seq}`) of bubbles the user has
      // expanded past their collapsed first-line preview.
      /** @type {Set<string>} */
      let expandedChatBubbles = new Set();
      /**
       * Strips any residual <route …>…</route> block from a feedback-thread
       * message (the daemon strips them before storing; this guards old rows).
       * @param {string} t
       * @returns {string}
       */
      function stripRoute(t) { return t.replace(/<route\s[^>]*>[\s\S]*?<\/route>/g, '').trim(); }
      /**
       * Fetches and caches one review branch's read-only feedback thread (RAL-272), re-rendering unless `opts.silent`.
       * @param {string} gid
       * @param {string} bid
       * @param {{silent?: boolean}} [opts]
       * @returns {Promise<void>}
       */
      async function loadBranchMessages(gid, bid, opts) {
        const key = `${gid}:${bid}`;
        try {
          const data = await (await fetch(`/api/guardians/${gid}/branches/${bid}/messages`)).json();
          branchMessages[key] = data.messages || [];
        } catch (_) { branchMessages[key] = branchMessages[key] || []; }
        // `silent` callers (the poll loop) re-render themselves afterwards.
        if (!(opts && opts.silent) && selectedGuardian === gid) {
          renderReviewDetail();
        }
      }
      /**
       * Refreshes the read-only feedback thread for every currently-expanded branch of `gid` (RAL-272).
       * @param {string} gid
       * @param {{silent?: boolean}} [opts]
       * @returns {Promise<void>}
       */
      async function refreshExpandedBranchMessages(gid, opts) {
        const prefix = `${gid}:`;
        /** @type {string[]} */
        const bids = [];
        expandedBranches.forEach((k) => { if (k.startsWith(prefix)) bids.push(k.slice(prefix.length)); });
        await Promise.all(bids.map((bid) => loadBranchMessages(gid, bid, opts)));
      }
      /**
       * Toggles whether one chat bubble shows its full message or just its collapsed first line.
       * @param {string} key
       * @returns {void}
       */
      function toggleChatBubble(key) {
        if (expandedChatBubbles.has(key)) expandedChatBubbles.delete(key); else expandedChatBubbles.add(key);
        renderReviewDetail();
        // The same bubbles render in the inspector's Feedback tab, which is its
        // own pane -- without this, expanding one there did nothing visible.
        renderReviewInspector();
      }
      /**
       * Opens the "copy chat as" (Markdown/JSON) menu for one branch's feedback thread.
       * @param {MouseEvent} e
       * @param {string} gid
       * @param {string} bid
       * @returns {void}
       */
      function showChatCopyMenu(e, gid, bid) {
        e.stopPropagation();
        const existing = document.getElementById("chat-copy-menu");
        if (existing) { existing.remove(); return; }
        // Click handling is delegated from `document`, so `currentTarget` is
        // the document -- which has no bounding box. Reading one off it threw,
        // and the menu never opened at all. Resolve the button that was
        // actually clicked instead.
        const btn = /** @type {HTMLElement|null} */ (
          /** @type {HTMLElement} */ (e.target).closest("[data-click]"));
        const rect = (btn || /** @type {HTMLElement} */ (e.target)).getBoundingClientRect();
        const menu = document.createElement("div");
        menu.id = "chat-copy-menu";
        menu.className = "copy-menu";
        // Anchored above the button when it sits low in the viewport, so the
        // composer's own copy control doesn't open a menu off the bottom edge.
        const below = rect.bottom + 4;
        menu.style.top = (below + 80 > window.innerHeight ? Math.max(4, rect.top - 72) : below) + "px";
        menu.style.left = Math.min(rect.left, window.innerWidth - 160) + "px";
        menu.innerHTML = `<div class="copy-menu-item" data-click="copyChatAs" data-guardian-id="${esc(gid)}" data-branch-id="${esc(bid)}" data-format="markdown">Markdown</div><div class="copy-menu-item" data-click="copyChatAs" data-guardian-id="${esc(gid)}" data-branch-id="${esc(bid)}" data-format="json">JSON</div>`;
        document.body.appendChild(menu);
        setTimeout(() => document.addEventListener("click", () => { const m = document.getElementById("chat-copy-menu"); if (m) m.remove(); }, { once: true }), 0);
      }
      /**
       * Copies one branch's feedback thread to the clipboard as Markdown or JSON.
       * @param {string} gid
       * @param {string} bid
       * @param {string} format
       * @returns {Promise<void>}
       */
      async function copyChatAs(gid, bid, format) {
        const menu = document.getElementById("chat-copy-menu");
        if (menu) menu.remove();
        const msgs = branchMessages[`${gid}:${bid}`] || [];
        // Both formats carry when each message was posted. A thread pasted into
        // an issue or handed to another agent is mostly useless without it --
        // "the resolver replied" and "the resolver replied four hours later"
        // are different facts, and neither format said which.
        const text = format === "markdown"
          ? msgs.map((m) => {
            const who = m.role === "reviewer" ? (m.author || "You") : "Resolver";
            const when = m.at_ms ? ` · ${fmtMsgTimeFull(m.at_ms)}` : "";
            const status = m.action_status ? ` · ${m.action_status}` : "";
            return `**${who}**${when}${status}\n\n${m.text}`;
          }).join("\n\n")
          : JSON.stringify(msgs.map((m) => ({
            role: m.role,
            author: m.author,
            // Epoch milliseconds for machines, ISO-8601 UTC for a reader --
            // a bare local string would be ambiguous once it leaves this
            // browser, which is the whole point of copying it.
            at_ms: m.at_ms,
            at: m.at_ms ? new Date(m.at_ms).toISOString() : null,
            action_status: m.action_status,
            text: m.text,
          })), null, 2);
        await navigator.clipboard.writeText(text);
      }
      // RAL-24: base-branch change dropdown — fetch branches on demand and post the change.
      /**
       * Lazily fetches and caches a review's candidate base branches from the remote.
       * @param {string} gid
       * @returns {Promise<void>}
       */
      async function loadBaseBranches(gid) {
        if (baseBranchCache[gid]) return;
        try {
          const data = await (await fetch(`/api/guardians/${gid}/base-branches`)).json();
          baseBranchCache[gid] = Array.isArray(data) ? data : [];
        } catch (_) { baseBranchCache[gid] = []; }
        const dl = document.getElementById("base-datalist");
        if (dl) dl.innerHTML = baseBranchCache[gid].map(b => `<option value="${esc(b)}"></option>`).join('');
      }
      // RALPHUS-AGENT-SELECT:BEGIN
      // RAL-466: this block backs every agent-picking `<select>` in the
      // board UI (the review resolver-agent field, the Project Review
      // Settings resolver-agent field, and the Squad cell-edit agent
      // field) -- one fetch/cache/fallback/lazy-load machinery instead of
      // three divergent copies.
      /** Canonical hardcoded agent options used only when the real list from `GET /api/agents` isn't available for a `cwd` yet (or a fetch ultimately fails/times out) -- mirrors `BUILTIN_AGENTS` in daemon/src/agent_access.rs. */
      const AGENT_SELECT_FALLBACK_AGENTS = [
        { id: "claude", kind: "builtin", backend: "claude" },
        { id: "claude-code", kind: "builtin", backend: "claude-code" },
        { id: "codex", kind: "builtin", backend: "codex" },
        { id: "pi", kind: "builtin", backend: "pi" },
        { id: "ollama", kind: "builtin", backend: "ollama" },
        { id: "anthropic", kind: "builtin", backend: "anthropic" },
      ];
      const AGENT_SELECT_FALLBACK_DEFAULT = "ollama";
      /** How long an agent-select dropdown open waits on `GET /api/agents` before giving up and painting the hardcoded fallback list instead -- RAL-444. */
      const AGENT_SELECT_LOAD_TIMEOUT_MS = 5000;
      /** @type {Map<string, Promise<AgentOptionsCacheEntry>>} cwd -> in-flight `GET /api/agents` fetch, so concurrent agent-dropdown opens for the same cwd share one request instead of firing duplicates -- RAL-444. */
      const agentOptionsLoading = new Map();

      /**
       * Builds one agent-select `<select>`'s `<option>`s from an
       * already-resolved agent list, sorted alphabetically. Whichever agent
       * matches `entry.defaultAgent` gets " (default)" appended to its label
       * and is preselected when `selected` is `""` (unset) -- there's no
       * separate synthetic "(default)" entry.
       * @param {AgentOptionsCacheEntry} entry
       * @param {string} selected
       * @returns {string}
       */
      function agentSelectOptionHtmlFromEntry(entry, selected) {
        const sorted = [...entry.agents].sort((a, b) => a.id.localeCompare(b.id));
        return sorted.map((a) => {
          const isSelected = selected === a.id || (selected === "" && a.id === entry.defaultAgent);
          const profileSuffix = a.kind === "profile" ? ` (profile → ${esc(a.backend)})` : "";
          const defaultSuffix = a.id === entry.defaultAgent ? " (default)" : "";
          return `<option value="${esc(a.id)}" ${isSelected ? "selected" : ""}>${esc(a.id)}${profileSuffix}${defaultSuffix}</option>`;
        }).join("");
      }
      /**
       * Builds an agent-select `<select>`'s *initial* `<option>`s for `cwd`,
       * RAL-444: synchronous, and built only from state already known --
       * never a guessed/incomplete list -- so the box can never default away
       * from the field's real value while `GET /api/agents` is still
       * loading (the "defaults to claude" race this fixes). Renders the
       * complete list immediately once one is already cached for this `cwd`
       * (e.g. an earlier dropdown open this page load, or another field
       * sharing the same `cwd`); otherwise renders exactly one option --
       * `selected` itself, or a generic "agent default" placeholder when
       * unset -- so there is nothing else for the browser to fall back to.
       * The real, complete list loads lazily the moment the dropdown is
       * actually opened; see `onAgentSelectMouseDown`.
       * @param {string} cwd
       * @param {string} selected
       * @returns {string}
       */
      function agentSelectOptionHtml(cwd, selected) {
        const cached = agentOptionsByCwd.get(cwd);
        if (cached && cached.agents.length) return agentSelectOptionHtmlFromEntry(cached, selected);
        const label = selected || "agent default";
        return `<option value="${esc(selected)}" selected>${esc(label)}</option>`;
      }
      /**
       * Fetches `GET /api/agents` for `cwd`, caches a successful non-empty
       * result, and refreshes the review-detail pane so its read-only
       * resolver-agent label (`resolverOf`) picks up the newly-known
       * effective default. Never throws -- a network failure or an empty
       * agent list resolves to the hardcoded fallback list instead.
       * @param {string} cwd
       * @returns {Promise<AgentOptionsCacheEntry>}
       */
      async function fetchAgentOptionsEntry(cwd) {
        try {
          const d = await (await fetch(`/api/agents?cwd=${encodeURIComponent(cwd)}`)).json();
          const entry = { agents: d.agents || [], defaultAgent: d.default_agent || AGENT_SELECT_FALLBACK_DEFAULT };
          if (!entry.agents.length) return { agents: AGENT_SELECT_FALLBACK_AGENTS, defaultAgent: entry.defaultAgent };
          agentOptionsByCwd.set(cwd, entry);
          renderReviewDetail();
          return entry;
        } catch (e) {
          return { agents: AGENT_SELECT_FALLBACK_AGENTS, defaultAgent: AGENT_SELECT_FALLBACK_DEFAULT };
        }
      }
      /**
       * Resolves to the complete agent list + effective default for `cwd`,
       * RAL-444. Returns the cached entry immediately if one is already
       * loaded; otherwise reuses an in-flight fetch for this `cwd` rather
       * than firing a duplicate, or starts one now. Bounded to
       * AGENT_SELECT_LOAD_TIMEOUT_MS so a stalled request can't hang a
       * dropdown open forever -- degrades to the hardcoded fallback list on
       * timeout, same as on a fetch error. `cwd === ""` (no project resolved
       * yet, e.g. the New Squad modal before a project is picked, or a cell
       * with no `cwd` override) is still fetched, not short-circuited --
       * `GET /api/agents` returns the global/`$RALPHUS_CONFIGURATION_PATH`
       * agent profiles regardless of `cwd`, so skipping the fetch here hid
       * every registered profile behind an unrelated project selection.
       * @param {string} cwd
       * @returns {Promise<AgentOptionsCacheEntry>}
       */
      async function ensureAgentOptionsLoaded(cwd) {
        const cached = agentOptionsByCwd.get(cwd);
        if (cached && cached.agents.length) return cached;
        let pending = agentOptionsLoading.get(cwd);
        if (!pending) {
          pending = fetchAgentOptionsEntry(cwd);
          agentOptionsLoading.set(cwd, pending);
          pending.finally(() => { if (agentOptionsLoading.get(cwd) === pending) agentOptionsLoading.delete(cwd); });
        }
        const timeout = new Promise((resolve) => setTimeout(() => resolve({ agents: AGENT_SELECT_FALLBACK_AGENTS, defaultAgent: AGENT_SELECT_FALLBACK_DEFAULT }), AGENT_SELECT_LOAD_TIMEOUT_MS));
        return Promise.race([pending, timeout]);
      }
      /**
       * `mousedown` handler for an agent-select `<select>`, RAL-444: makes
       * sure the native dropdown never opens showing anything less than the
       * complete agent list. A cached list is spliced in synchronously (no
       * visible delay, and the native popup opens normally); otherwise the
       * native popup is suppressed for this event while
       * `ensureAgentOptionsLoaded` resolves (reusing any fetch already in
       * flight), then reopened via `showPicker()` once the real options are
       * in place -- falling back to just refocusing the element on browsers
       * without `showPicker`, where the next click opens it with an
       * already-warm cache.
       * @param {MouseEvent} e
       * @param {HTMLSelectElement} select
       * @param {string} cwd
       * @returns {void}
       */
      function onAgentSelectMouseDown(e, select, cwd) {
        const cached = agentOptionsByCwd.get(cwd);
        if (cached && cached.agents.length) {
          select.innerHTML = agentSelectOptionHtmlFromEntry(cached, select.value);
          return;
        }
        e.preventDefault();
        ensureAgentOptionsLoaded(cwd).then((entry) => {
          select.innerHTML = agentSelectOptionHtmlFromEntry(entry, select.value);
          if (typeof select.showPicker === "function") {
            try { select.showPicker(); } catch (_) { select.focus(); }
          } else {
            select.focus();
          }
        });
      }
      /**
       * Renders a complete agent-select `<select>` element, RAL-466: the
       * one shared combo box used by the review resolver-agent field, the
       * Project Review Settings resolver-agent field, and the Squad
       * cell-edit agent field. `onChangeFnName` must be a literal global
       * function name (inline `onchange="..."` can't reference a closure)
       * taking the new value as its only argument.
       * @param {string} id
       * @param {string} cwd
       * @param {string} selected
       * @param {string} onChangeFnName
       * @param {string} [style]
       * @param {string} [dataTip]
       * @returns {string}
       */
      function renderAgentSelectHtml(id, cwd, selected, onChangeFnName, style, dataTip) {
        const idAttr = id ? ` id="${esc(id)}"` : "";
        const styleAttr = style ? ` style="${style}"` : "";
        const tipAttr = dataTip ? ` data-tip="${esc(dataTip)}"` : "";
        return `<select${idAttr}${styleAttr} onchange="${onChangeFnName}(this.value)" onmousedown="onAgentSelectMouseDown(event,this,${JSON.stringify(cwd)})"${tipAttr}>${agentSelectOptionHtml(cwd, selected)}</select>`;
      }
      /**
       * Eagerly warms `agentOptionsByCwd` for `cwd` the moment an
       * agent-select field's containing modal/form opens, RAL-466: without
       * this, the real agent list only ever loads on the field's own
       * `mousedown` (see `onAgentSelectMouseDown`), so a field whose value
       * is unset renders just a single "agent default" placeholder option
       * until the user actually opens it -- this is what made the Project
       * Review Settings resolver-agent dropdown look permanently stuck on
       * "Agent Default". Never throws. Also warms `cwd === ""` (no project
       * resolved yet) rather than no-op'ing on it -- see
       * `ensureAgentOptionsLoaded`'s doc comment for why an empty `cwd`
       * still has a real agent list (registered profiles) to fetch.
       * `isStillRelevant` is re-checked once the fetch resolves so a
       * closed/replaced modal's late fetch doesn't re-render stale state.
       * @param {string} cwd
       * @param {() => boolean} isStillRelevant
       * @param {() => void} rerender
       * @returns {void}
       */
      function preloadAgentSelect(cwd, isStillRelevant, rerender) {
        const cached = agentOptionsByCwd.get(cwd);
        if (cached && cached.agents.length) return;
        ensureAgentOptionsLoaded(cwd).then(() => { if (isStillRelevant()) rerender(); });
      }
      // RALPHUS-AGENT-SELECT:END
      /**
       * Opens the Create Review modal.
       * @returns {void}
       */
      function openCreateReview() {
        if (!projects.length) {
          pollProjects().then(() => {
            if (document.getElementById("cr-project")) renderCreateReviewProjectOptions();
          });
        }
        const projectOptions = `<option value="">(select a project)</option>` + projects.map((project) => `<option value="${esc(project.name)}">${esc(project.name)}</option>`).join("");
        byId("modal-root").innerHTML = `
          <div class="modal-bg" onclick="if(event.target===this)closeModal()"><div class="modal">
            <h2>Create Review</h2>
            <div class="edit-form">
              <label data-tip="Display name for this review — shown in the sidebar list.">name<input id="cr-name" value="my review"></label>
              <label data-tip="Review type. 'git' stacks branches via rebase with AI conflict resolution. Other types are placeholders.">type<select id="cr-type" onchange="onReviewTypeChange()"><option value="git">git</option><option value="document">document (placeholder)</option></select></label>
              <div id="cr-git-fields">
                <label data-tip="The branch that every submitted branch is ultimately rebased onto. Usually 'main' or 'master'.">upstream branch<input id="cr-base" value="main"></label>
                <label data-tip="Choose whether this review is identified by a registered project or directly by a raw repository path.">target<select id="cr-target-kind" onchange="onReviewTargetKindChange()"><option value="project">registered project</option><option value="path">directory path</option></select></label>
                <div id="cr-project-field">
                  <label data-tip="The stable registered project identity for this review. Its concrete path may differ by machine.">project<select id="cr-project">${projectOptions}</select></label>
                </div>
                <div id="cr-path-field" class="hidden">
                  <label data-tip="Absolute path to an unregistered git repository. Use this only when the review is not based on a registered project.">git root (absolute path)<input id="cr-root" placeholder="C:/path/to/repo"></label>
                </div>
                <label data-tip="Shell commands to run as check gates after each merge commit (comma-separated). Leave blank to skip gates. Example: cargo test, npm test">checks (comma-separated shell commands, optional)<input id="cr-checks" placeholder="cargo test"></label>
                <label style="display:flex;align-items:center;gap:6px;margin-top:10px" data-tip="Skip the finalize-time build/check step entirely: explicit check gates, the project's .ralphus.toml auto_build default, and the AI-inferred build command are all skipped.\nGates are still stored — you can re-enable this later."><input type="checkbox" id="cr-skip-auto-build" style="width:auto;margin:0">skip auto-build</label>
                <label style="display:flex;align-items:center;gap:6px;margin-top:6px" data-tip="Use one shared worktree for the entire stack instead of per-branch worktrees. Faster for large repos."><input type="checkbox" id="cr-skip-worktrees" style="width:auto;margin:0">skip per-branch worktrees (large repos)</label>
              </div>
              <div id="cr-other-fields" class="hidden"><div class="empty" style="margin:12px 0">No extra fields for this review type yet.</div></div>
            </div>
            <div id="cr-err" class="verr"></div>
            <div class="btn-row"><button class="btn" onclick="closeModal()">Cancel</button><button class="btn primary" onclick="createReview()">Create</button></div>
          </div></div>`;
      }
      /**
       * Refreshes the registered-project choices after a background project poll.
       * @returns {void}
       */
      function renderCreateReviewProjectOptions() {
        const select = /** @type {HTMLSelectElement|null} */ (document.getElementById("cr-project"));
        if (!select) return;
        const current = select.value;
        select.innerHTML = `<option value="">(select a project)</option>` + projects.map((project) => `<option value="${esc(project.name)}">${esc(project.name)}</option>`).join("");
        select.value = current;
      }
      /**
       * Toggles between registered-project and raw-directory review creation.
       * @returns {void}
       */
      function onReviewTargetKindChange() {
        const isProject = /** @type {HTMLSelectElement} */ (document.getElementById("cr-target-kind")).value === "project";
        byId("cr-project-field").classList.toggle("hidden", !isProject);
        byId("cr-path-field").classList.toggle("hidden", isProject);
      }
      // CCTL-112: reviews are schema-based; the type dropdown toggles which
      // fields are shown (git-specific fields for `git`, a generic state else).
      /**
       * Toggles the Create Review modal's fields between git-specific and generic.
       * @returns {void}
       */
      function onReviewTypeChange() {
        const isGit = /** @type {HTMLSelectElement} */ (document.getElementById("cr-type")).value === "git";
        byId("cr-git-fields").classList.toggle("hidden", !isGit);
        byId("cr-other-fields").classList.toggle("hidden", isGit);
      }
      /**
       * Validates and submits the Create Review modal.
       * @returns {Promise<void>}
       */
      async function createReview() {
        /**
         * @param {string} id
         * @returns {string}
         */
        const v = (id) => /** @type {HTMLInputElement} */ (document.getElementById(id)).value.trim();
        const review_type = /** @type {HTMLSelectElement} */ (document.getElementById("cr-type")).value;
        if (review_type !== "git") {
          byId("cr-err").textContent = "only git reviews can be created here for now";
          return;
        }
        const checks = v("cr-checks") ? v("cr-checks").split(",").map((s) => s.trim()).filter(Boolean) : [];
        const skip_auto_build = /** @type {HTMLInputElement} */ (document.getElementById("cr-skip-auto-build")).checked;
        const skip_worktrees = /** @type {HTMLInputElement} */ (document.getElementById("cr-skip-worktrees")).checked;
        /** @type {Record<string, unknown>} */
        const body = { name: v("cr-name"), review_type, base_branch: v("cr-base"), checks, skip_auto_build, skip_worktrees };
        const targetKind = /** @type {HTMLSelectElement} */ (document.getElementById("cr-target-kind")).value;
        if (targetKind === "project") {
          const project = /** @type {HTMLSelectElement} */ (document.getElementById("cr-project")).value;
          if (!project) { byId("cr-err").textContent = "select a project"; return; }
          body.project = project;
        } else {
          const gitRoot = v("cr-root");
          if (!gitRoot) { byId("cr-err").textContent = "git root is required"; return; }
          body.git_root = gitRoot;
        }
        const resp = await fetch("/api/guardians", { method: "POST", body: JSON.stringify(body) });
        if (!resp.ok) { byId("cr-err").textContent = "create failed"; return; }
        const g = await resp.json(); selectedGuardian = g.id; closeModal(); tick();
        notify("success", `Review "${g.name || g.id}" created.`);
      }

      // Review-action POSTs (merge, approve, force-start, ...) used to ignore
      // response.ok entirely, so a rejected request (e.g. a 409 on an invalid
      // state transition) looked exactly like success — the button appeared to
      // silently do nothing. `guardianAction` below surfaces the real failure
      // through the shared notify() system (RAL-433; originally a
      // review-only `showReviewError` helper introduced by RAL-108), which
      // dedupes identical messages within a cooldown window so a burst of
      // the same failure (a double-click, a few failed polls in a row)
      // doesn't flood the screen with toasts.

      // ---- Guardian notices -> toast (RAL-273) ----
      // A guardian notice (`notice_kind`/`notice_message`/`notice_at_ms`) is
      // server-recorded, one-shot, purely informational state -- e.g. an
      // incoming GitHub/GitLab stack reorder interrupting a local reorder in
      // flight (`forge_drift_interrupted_local`; see `Store::set_guardian_notice`).
      // There is no server-side "seen" tracking: each poll of `/api/guardians`
      // re-sends whatever the last notice was, so the board itself remembers
      // which `notice_at_ms` it already showed per guardian and only toasts
      // once per new one. RAL-451: a linked PR merging out-of-band while a
      // rebase/feedback pass owned the review's worktrees (`pr_merged_mid_flight`,
      // RAL-300) used to go through this same mechanism, but repeated drops
      // kept re-obstructing the board with no way to dismiss them -- it now
      // goes through the dismissible mailbox widget instead (`82-mailbox.js`,
      // `daemon/src/pr.rs`'s `settle_pr_merge_states`).
      // RALPHUS-GUARDIAN-NOTICE:BEGIN
      /**
       * Which of `list`'s guardian notices are newer than what `shown` last
       * recorded for that guardian, as ready-to-display toast text (RAL-273).
       * Pure: does not touch the DOM or mutate `shown` -- the caller applies
       * the result.
       * @param {GuardianView[]} list
       * @param {Map<string, number>} shown - guardian id -> last-shown notice_at_ms
       * @returns {{id: string, notice_at_ms: number, text: string}[]}
       */
      function pendingGuardianNoticeToasts(list, shown) {
        const out = [];
        for (const g of list) {
          if (!g.notice_kind || !g.notice_at_ms) continue;
          const lastShown = shown.get(g.id) || 0;
          if (g.notice_at_ms <= lastShown) continue;
          out.push({ id: g.id, notice_at_ms: g.notice_at_ms, text: `${g.name}: ${g.notice_message || g.notice_kind}` });
        }
        return out;
      }
      // RALPHUS-GUARDIAN-NOTICE:END
      /** @type {Map<string, number>} guardian id -> last-shown notice_at_ms */
      const _guardianNoticeShown = new Map();
      /**
       * Shows a toast for any guardian whose `notice_at_ms` is newer than
       * what was last shown for it (RAL-273).
       * @param {GuardianView[]} list
       * @returns {void}
       */
      function checkGuardianNotices(list) {
        for (const toast of pendingGuardianNoticeToasts(list, _guardianNoticeShown)) {
          _guardianNoticeShown.set(toast.id, toast.notice_at_ms);
          notify("info", toast.text);
        }
      }
      // POST to a guardian/review action endpoint; on failure, surfaces a
      // deduped toast instead of silently no-oping. `body`, when given, is
      // JSON-encoded. Returns the Response (or null on a network error) so
      // callers that need to branch on success can still do so.
      /**
       * POSTs to a guardian action endpoint, surfacing a deduped error toast on failure.
       * @param {string} url
       * @param {*} [body]
       * @returns {Promise<Response|null>}
       */
      async function guardianAction(url, body) {
        let resp;
        try {
          resp = await fetch(url, { method: "POST", body: body !== undefined ? JSON.stringify(body) : undefined });
        } catch (_) {
          notify("error", "Action failed: network error");
          return null;
        }
        if (!resp.ok) {
          const e = await resp.json().catch(() => ({}));
          notify("error", ((e.error || {}).message) || `Action failed (${resp.status})`);
        }
        return resp;
      }

      /**
       * Starts, resumes, or restarts a review's stacked-rebase merge.
       *
       * Feedback first, reload second. The daemon answers this in
       * milliseconds (a DB state transition plus a thread spawn -- see
       * `guardian_merge::kickoff_merge`), but the `tick()` behind it reloads
       * the whole board, and the rebase it starts then runs for minutes. So
       * the button goes pending and the acknowledgement toast goes up before
       * the request is even sent: neither waits on the round-trip, and
       * nothing here claims the rebase has finished. The pending state is
       * held through the reload too, so the button can't be pressed again in
       * the gap between the daemon's write landing and the board picking up
       * the new status.
       * @param {string} id
       * @param {string} status
       * @returns {Promise<void>}
       */
      async function mergeReview(id, status) {
        // The rendered button is disabled while pending, but this is what
        // makes double submission impossible rather than merely unlikely --
        // a re-render can be skipped (see `userIsSelecting`), and the click
        // handler is reachable regardless of what the pane currently shows.
        if (pendingMergeActions.has(id)) return;
        if (status === "merging" && !confirm("There is a rebase currently in progress. Do you want to cancel it and start another?")) return;
        // RAL-69: merged into this button (there is no separate Force-Start
        // button anymore) -- while still collecting, some enabled branches
        // may not be ready yet. Ask before disabling them and starting with
        // only whichever branches are actually rebaseable right now.
        if (status === "collecting") {
          const g = (guardians || []).find((x) => x.id === id);
          const hasNotReady = !!g && (g.branches || []).some((b) => b.enabled !== false && b.source_cell_state !== "done");
          if (hasNotReady) { confirmMergeSubset(id); return; }
        }
        pendingMergeActions.add(id);
        if (!userIsSelecting()) renderReviewDetail();
        notify("info", mergeRequestedToast(status));
        try {
          const path = status === "merging" ? "cancel_and_merge" : "merge";
          // A non-2xx already surfaced the red error toast inside
          // `guardianAction`; the reload and the pending-state clear below
          // still run, so the button comes back rather than staying stuck.
          const resp = await guardianAction(`/api/guardians/${id}/${path}`);
          // `kickoff_merge` claims the review before it answers, so once this
          // resolves the daemon is already in `merging` -- reflect that locally
          // straight away. What the pending state must not do is outlive the
          // daemon's answer: `tick()` reloads the whole board and ends in
          // `pollReviews`, which waits on every open PR's drift check, so
          // holding the button pending across it leaves it reading "Starting…"
          // for tens of seconds after the rebase has already begun.
          //
          // The button stays correctly disabled without the pending flag,
          // because `merging` is not in `MERGE_STARTABLE`.
          if (resp && resp.ok) {
            const g = (guardians || []).find((x) => x.id === id);
            if (g) g.status = "merging";
          }
        } finally {
          pendingMergeActions.delete(id);
          if (!userIsSelecting()) renderReviewDetail();
        }
        // Unawaited on purpose -- see above; matches `stopMerge`'s own call.
        tick();
      }
      /**
       * Stops an in-progress rebase at its next checkpoint, leaving the review
       * in the resumable `merge_stopped` state (RAL-249) — distinct from
       * cancelling (which discards the review back to collecting).
       * @param {string} id
       * @returns {Promise<void>}
       */
      async function stopMerge(id) {
        if (!confirm("Stop this rebase mid-flight? It will halt at the next checkpoint and pause the review (resumable). Nothing is discarded — this is not a cancel.")) return;
        const resp = await guardianAction(`/api/guardians/${id}/stop`);
        if (resp && resp.ok) notify("success", "Rebase stopped — resumable from where it left off.");
        tick();
      }
      /**
       * Approves a review that is `in_review` or `merge_stopped` (RAL-535).
       * Marks the button pending/disabled immediately (RAL-234) -- the
       * daemon write itself is a single status-string match, but the
       * follow-up `tick()` reload still takes a network round-trip or two,
       * and without this the button looked unresponsive for that whole
       * window.
       * @param {string} id
       * @returns {Promise<void>}
       */
      async function approveReview(id) {
        pendingGuardianActions.add(id);
        if (!userIsSelecting()) renderReviewDetail();
        try {
          const resp = await guardianAction(`/api/guardians/${id}/approve`);
          if (resp && resp.ok) notify("success", "Review approved.");
          // Stay pending through the reload too, so the button can't be
          // double-clicked in the gap between the write completing and the
          // board picking up the new (no-longer-in_review) status.
          await tick();
        } finally {
          pendingGuardianActions.delete(id);
          if (!userIsSelecting()) renderReviewDetail();
        }
      }

      /**
       * Explicit "Sync PR" (RAL-273): asks the daemon to check this
       * review's open PRs' live forge bases (GitHub or GitLab) for a
       * reorder made outside ralphus and apply it if found. Fires and
       * forgets -- the daemon runs detection/apply in the background, so
       * the result (a new branch order, a rebuild, or nothing at all) shows
       * up through the normal `tick()` refresh, same as the 5-minute
       * background poll.
       * @param {string} id
       * @returns {Promise<void>}
       */
      async function syncPrReview(id) {
        await guardianAction(`/api/guardians/${id}/sync-pr`);
        notify("info", "Checking GitHub/GitLab for a stack reorder…");
        tick();
      }

      // ---------- merge / rebase with a not-ready subset of branches (RAL-69) ----------
      // Clicking Merge / rebase while some enabled branches aren't ready yet
      // (still running, or never submitted) shows this confirmation instead of
      // silently doing nothing or silently disabling branches. Continuing
      // calls POST /api/guardians/{id}/force_start, which disables the
      // not-ready branches and kicks off the rebase with the rest.
      /**
       * Shows a confirmation modal naming which branches can actually be
       * rebased right now (the ones closest to upstream that are ready),
       * before disabling the rest and starting the rebase.
       * @param {string} id
       * @returns {void}
       */
      function confirmMergeSubset(id) {
        const g = (guardians || []).find((x) => x.id === id);
        if (!g) return;
        const enabledBranches = (g.branches || []).filter((b) => b.enabled !== false);
        const rebaseable = enabledBranches.filter((b) => b.source_cell_state === "done");
        const notReady = enabledBranches.filter((b) => b.source_cell_state !== "done");
        const names = rebaseable.map((b) => b.branch);
        const namesList = names.length === 0
          ? "no"
          : names.length === 1
            ? names[0]
            : `${names.slice(0, -1).join(", ")} and ${names[names.length - 1]}`;
        const rows = enabledBranches.map((b) => {
          const ready = b.source_cell_state === "done";
          const st = ready ? "ready" : (b.source_cell_state || "not submitted");
          return `<div style="display:flex;gap:8px;align-items:center;padding:5px 0;border-bottom:1px solid var(--border)">
            <span class="mono" style="flex:1;font-size:12px${ready ? "" : ";color:var(--muted)"}">${esc(b.branch)}</span>
            <span class="badge" style="color:${ready ? "var(--done)" : "var(--queued)"};border-color:${ready ? "var(--done)" : "var(--queued)"};font-size:11px">${esc(st)}</span>
          </div>`;
        }).join("");
        byId("modal-root").innerHTML = `
          <div class="modal-bg" onclick="if(event.target===this)closeModal()"><div class="modal">
            <h2>Merge / rebase with fewer branches?</h2>
            <p style="font-size:13px;margin:0 0 10px;color:var(--muted)">We can only rebase ${esc(namesList)} ${names.length === 1 ? "branch" : "branches"} right now. ${notReady.length === 1 ? "The other branch is" : "The other branches are"} not yet ready (still running or never submitted) and will be <b>disabled</b> in the review stack. You can re-enable ${notReady.length === 1 ? "it" : "them"} later once ${notReady.length === 1 ? "its" : "their"} source cell${notReady.length === 1 ? "" : "s"} finish.</p>
            <div style="border-top:1px solid var(--border);margin-bottom:10px">${rows}</div>
            <p style="font-size:12px;color:var(--failed);margin:0 0 2px">This cannot be undone.</p>
            <div class="btn-row">
              <button class="btn" onclick="closeModal()" data-tip="Cancel — leave the review in its current collecting state.">Cancel</button>
              <button class="btn primary" data-click="doForceStart" data-guardian-id="${esc(id)}" data-tip="Disable the not-ready branches and start the rebase immediately with whatever branches remain enabled.">Continue anyway</button>
            </div>
          </div></div>`;
      }
      /**
       * Confirms and executes a rebase with the not-ready branches disabled.
       * @param {string} id
       * @returns {Promise<void>}
       */
      async function doForceStart(id) {
        closeModal();
        await guardianAction(`/api/guardians/${id}/force_start`);
        tick();
      }
      // Permanently dismiss the "can enable now" notice for a disabled branch (RAL-69).
      /**
       * Permanently dismisses the "can enable now" notice for a disabled branch.
       * @param {string} id
       * @param {string} branchId
       * @returns {Promise<void>}
       */
      async function dismissReenable(id, branchId) {
        await guardianAction(`/api/guardians/${id}/branches/${branchId}/dismiss_reenable`);
        tick();
      }

      // ---------- move a branch between reviews (RAL-118) ----------
      /**
       * Opens the "move this branch to another review" context menu.
       * @param {MouseEvent} e
       * @param {string} gid
       * @param {string} branchId
       * @returns {void}
       */
      function openMoveBranchMenu(e, gid, branchId) {
        e.preventDefault(); e.stopPropagation(); closeSquadMenu();
        // Exclude the current review and reviews that can no longer accept new
        // work (mirrors the canReorder gate used for the move button itself).
        const targets = guardians.filter((x) => x.id !== gid && !["merged", "approved", "cancelled", "deployed"].includes(x.status));
        const menu = document.createElement("div");
        menu.className = "ctx-menu"; menu.id = "squad-menu";
        menu.innerHTML = targets.length
          ? targets.map((t) => `<div data-click="moveBranchTo" data-guardian-id="${esc(gid)}" data-branch-id="${esc(branchId)}" data-target-id="${esc(t.id)}" data-tip="Move this branch into \"${esc(t.name)}\".\nBoth reviews rebuild afterward.\nBlocked if either review has an active merge/rebase.">→ ${esc(t.name)}</div>`).join("")
          : `<div style="color:var(--muted);padding:6px 10px;font-size:12px">No other open reviews to move into.</div>`;
        document.body.appendChild(menu);
        menu.style.left = Math.min(e.clientX, window.innerWidth - 220) + "px";
        menu.style.top = Math.min(e.clientY, window.innerHeight - 90) + "px";
      }
      /**
       * Moves a branch out of one review's stack and into another, after confirmation.
       * @param {string} gid
       * @param {string} branchId
       * @param {string} toId
       * @returns {Promise<void>}
       */
      async function moveBranchTo(gid, branchId, toId) {
        closeSquadMenu();
        const g = guardians.find((x) => x.id === gid);
        const branch = (g && (g.branches.find((b) => b.id === branchId) || {}).branch) || branchId;
        const to = guardians.find((x) => x.id === toId);
        if (!confirm(`Move branch "${branch}" to review "${to ? to.name : toId}"? Both reviews will rebuild. This cannot be undone.`)) return;
        await guardianAction(`/api/guardians/${gid}/branches/${branchId}/move`, { to_guardian_id: toId });
        tick();
      }

      // ---------- manual review commands (RAL-27) ----------
      /**
       * Toggles the "run individual manual check" dropdown.
       * @param {string} id
       * @returns {void}
       */
      function toggleManualMenu(id) {
        manualMenuOpen[id] = !manualMenuOpen[id];
        renderReviewDetail();
      }
      /**
       * Runs every suggested manual-check command in a new terminal.
       * @param {string} id
       * @returns {Promise<void>}
       */
      async function runAllManualChecks(id) {
        await guardianAction(`/api/guardians/${id}/run-manual-commands`, {});
      }
      // RAL-520: regenerate the manual checks on demand, with optional focus.
      /**
       * Asks the daemon to regenerate this review's manual checks, passing the
       * reviewer's steering text from the section's focus field (empty = none).
       *
       * The pending key is `${id}:regen` so it never collides with the merge
       * button's pending entry for the same review; the two are independent
       * actions and one must never block the other.
       * @param {string} id
       * @returns {Promise<void>}
       */
      async function regenManualChecks(id) {
        pendingMergeActions.add(`${id}:regen`);
        renderReviewDetail();
        const el = /** @type {HTMLInputElement|null} */ (document.getElementById(`manual-focus-${id}`));
        const focus = el && typeof el.value === "string" ? el.value.trim() : "";
        const resp = await guardianAction(`/api/guardians/${id}/manual-checks/regenerate`, focus ? { focus } : {});
        if (resp && resp.ok) notify("info", "Manual-checks regeneration requested — it runs in the background.");
        pendingMergeActions.delete(`${id}:regen`);
        tick();
      }
      // RAL-77: run a user-declared action hint by index.
      /**
       * Runs a user-declared `[[review.action]]` hint by index.
       * @param {string} id
       * @param {number} index
       * @returns {Promise<void>}
       */
      /**
       * Asks the daemon to write this review's change summary again.
       *
       * The automatic pass skips a rewrite whose enabled-branch set is
       * unchanged, which is precisely the case a reviewer wants to override, so
       * this forces one. It lands through the same debounced queue, so the new
       * text arrives on a later poll rather than immediately.
       * @param {string} id - The review id.
       * @returns {Promise<void>}
       */
      async function regenerateSummary(id) {
        closeSquadMenu();
        const resp = await guardianAction(`/api/guardians/${id}/regenerate-summary`);
        if (resp && resp.ok) notify("info", "Summary regeneration requested — it runs in the background.");
      }
      /**
       * Runs every command-based test action for a review, in declaration
       * order. There is no run-all route for action hints the way there is for
       * manual checks (`/run-manual-commands`), so this fans out over the
       * per-index one. Prompt-based hints are skipped: they expand through the
       * resolver LLM before running and that path is not wired up, so firing
       * them would fail server-side rather than do nothing.
       *
       * Each one goes through the same path a row's ▶ uses, so every row it
       * launches marks itself launched too -- running them all should leave the
       * section saying exactly what running them one by one would.
       * @param {string} id - The review id.
       * @returns {Promise<void>}
       */
      async function runAllActionHints(id) {
        const g = guardians.find((x) => x.id === id);
        if (!g) return;
        const runnable = (g.action_hints || [])
          .map((h, i) => ({ h: h, i: i }))
          .filter((x) => !!x.h.command);
        if (!runnable.length) {
          notify("info", "No test actions declare a command to run.");
          return;
        }
        for (const x of runnable) await runCheck("action", id, x.i);
      }

      // ---------- structured check inputs (RAL-164) ----------
      /**
       * Runs a manual-check or action-hint check that declares input fields,
       * reading current values from its inline form (submitted values become
       * the new default for those inputs on this review).
       * @param {"manual"|"action"} kind
       * @param {string} id
       * @param {number} index
       * @returns {Promise<void>}
       */
      async function runCheckWithInputs(kind, id, index) {
        const g = guardians.find((x) => x.id === id);
        const check = g && (kind === "manual" ? (g.manual_commands || [])[index] : (g.action_hints || [])[index]);
        const key = `${id}:${kind}:${index}`;
        // Pressing ▶ means run, always -- so a parameterised check runs with
        // whatever its fields currently hold, or with the review's stored
        // values when they are not on screen. It never silently becomes a
        // "expand the form" button instead.
        const inputs = (g && check) ? checkInputValues(g, check, key) : {};
        const cleanupEl = /** @type {HTMLInputElement|null} */ (document.getElementById(`check-cleanup:${key}`));
        const run_cleanup = !!(cleanupEl && cleanupEl.checked);
        markCommandLaunched(key);
        const url = `/api/guardians/${id}/${kind === "manual" ? "run-manual-commands" : "run-action-hint"}`;
        await guardianAction(url, { index, inputs, run_cleanup });
        tick();
      }
      /**
       * Runs one manual check or test action. The single meaning of the ▶ on a
       * command row, whether or not that command takes parameters.
       * @param {"manual"|"action"} kind - Which section the row belongs to.
       * @param {string} id - The review id.
       * @param {number} index - The command's index within its section.
       * @returns {Promise<void>}
       */
      async function runCheck(kind, id, index) {
        manualMenuOpen[id] = false;
        await runCheckWithInputs(kind, id, index);
      }
      // "Set it for me" (RAL-164): delegates resolution of one named check
      // input to the resolver agent. Spam-proofing is enforced server-side
      // via an atomic claim (a concurrent duplicate request 409s) — this
      // button only disables itself for UX; it is not the actual guard.
      /**
       * Delegates "set it for me" resolution of one named check input to the resolver agent.
       * @param {string} id
       * @param {string} inputName
       * @returns {Promise<void>}
       */
      async function resolveCheckInput(id, inputName) {
        await guardianAction(`/api/guardians/${id}/resolve-input`, { input_name: inputName });
        tick();
      }

      // ---------- review-ready banner (CCTL-145) ----------
      // A guardian in `in_review` has its stack built and all check gates passed —
      // the "ready to act on" signal. Show a dismissible banner that jumps to it.
      /**
       * Renders the dismissible "review is ready" banner for every in_review guardian.
       * @param {GuardianView[]|GuardianIndexEntry[]} [items]
       * @returns {void}
       */
      function renderReadyBanner(items = guardians) {
        const el = document.getElementById("ready-banner");
        if (!el) return;
        const ready = items.filter((g) => g.status === "in_review" && !dismissedReady.has(g.id));
        el.innerHTML = ready.map((g) => `<div class="ready-banner">
            <span>✅ Review <b>${esc(g.name)}</b> is ready.</span>
            <a href="#" data-click="gotoReview" data-guardian-id="${esc(g.id)}" data-tip="Open this review on the Reviews tab to approve or inspect it.">Open review →</a>
            <span style="flex:1"></span>
            <button class="icon-btn" data-click="dismissReady" data-guardian-id="${esc(g.id)}" data-tip="Dismiss this banner. You can still open the review from the Reviews tab.">✕</button>
          </div>`).join("");
      }
      /**
       * Dismisses the "review is ready" banner for one review, persisted to localStorage.
       * @param {string} id
       * @returns {void}
       */
      function dismissReady(id) {
        dismissedReady.add(id);
        localStorage.setItem("ralphus-dismissed-ready", JSON.stringify([...dismissedReady]));
        renderReadyBanner();
      }
      /**
       * Refreshes and re-renders the ready-review banner.
       * @returns {Promise<void>}
       */
      async function refreshBanner() {
        // On the reviews tab `guardians` is already fresh; elsewhere fetch
        // it -- the lean list is all `renderReadyBanner` below needs
        // (status/id/name), and this runs on every tick regardless of tab.
        // Keep that lean snapshot local: a request can start on another tab
        // and finish after the user opens a review, and replacing `guardians`
        // then would discard the selected review's already-loaded full detail.
        /** @type {GuardianView[]|GuardianIndexEntry[]} */
        let items = guardians;
        if (tab !== "reviews") { try { items = await (await fetch("/api/guardian-index")).json(); } catch (_) {} }
        if (!userIsSelecting()) renderReadyBanner(items);
      }
