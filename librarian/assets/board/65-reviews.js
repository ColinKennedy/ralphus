      // ---------- structured check inputs (RAL-164) ----------
      // A GuardianCheck (either an AI-synthesized manual check or a
      // user-declared [[review.action]] hint) may declare named `inputs`
      // (e.g. a port number) referenced in its command as `{name}`
      // placeholders. A check with no inputs still runs immediately, exactly
      // as before; a check that declares any renders as a toggle that
      // expands an inline form instead.
      /**
       * Renders one runnable check's trigger: a plain "run immediately" button
       * when it declares no CheckInputs, or a toggle button (plus its inline
       * input form when open) when it does. Shared by the "test actions"
       * (action hints) and "manual checks" sections.
       * @param {GuardianView} g
       * @param {"manual"|"action"} kind
       * @param {number} i
       * @param {GuardianCheck} check
       * @param {string} label
       * @returns {string}
       */
      function renderCheckControl(g, kind, i, check, label) {
        const cmdText = check.command || check.prompt || "";
        if (!check.inputs || !check.inputs.length) {
          const runAction = kind === "manual" ? "runSingleManualCheck" : "runActionHint";
          return `<button class="btn" data-click="${runAction}" data-guardian-id="${esc(g.id)}" data-i="${i}" data-tip="Run: ${esc(cmdText)}\nLaunches in the built review worktree.">${esc(label)}</button>`;
        }
        const key = `${g.id}:${kind}:${i}`;
        const open = !!checkFormOpen[key];
        const toggleBtn = `<button class="btn" data-click="toggleCheckForm" data-key="${esc(key)}" data-tip="Run: ${esc(cmdText)}\nThis check needs some values filled in first — click to expand.">${esc(label)} ${open ? "▲" : "▾"}</button>`;
        return `<div style="display:inline-block;vertical-align:top">${toggleBtn}${open ? renderCheckInputForm(g, kind, i, check) : ""}</div>`;
      }
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
      function renderCheckInputForm(g, kind, i, check) {
        const key = `${g.id}:${kind}:${i}`;
        const values = g.input_values || {};
        const resolutions = g.input_resolutions || {};
        const fields = (check.inputs || []).map((inp) => {
          const fieldId = `check-input:${key}:${inp.name}`;
          const current = Object.prototype.hasOwnProperty.call(values, inp.name) ? values[inp.name] : inp.default;
          const res = resolutions[inp.name];
          const resolving = !!res && res.status === "resolving";
          const setTip = "Ask the resolver agent to pick a value for this input.\nWho/when: you don't know (or don't care) what value to use here — let the AI decide.\nFills the field below with its answer; you still press Run to actually execute.";
          const setBtn = `<button class="btn" ${resolving ? "disabled" : ""} data-click="resolveCheckInput" data-guardian-id="${esc(g.id)}" data-input-name="${esc(inp.name)}" data-tip="${setTip}">${resolving ? "Resolving…" : "Set it for me"}</button>`;
          return `<div style="margin:4px 0">
            <label style="display:block;font-size:11px;color:var(--dim);margin-bottom:2px" data-tip="${esc(inp.message)}">${esc(inp.message)}</label>
            <div style="display:flex;gap:4px">
              <input id="${fieldId}" type="text" value="${esc(current)}" style="flex:1;font-family:monospace;font-size:12px" data-tip="Value substituted for {${esc(inp.name)}} in the command.\nDefaults to the last value used on this review; press Run to use it, or edit first.">
              ${setBtn}
            </div>
          </div>`;
        }).join("");
        const cleanupField = check.cleanup_command
          ? `<label style="display:flex;align-items:center;gap:4px;font-size:11px;margin:4px 0" data-tip="Runs '${esc(check.cleanup_command)}' immediately before the main command — e.g. to stop a stale process from a previous run.\nWho/when: the command binds a port or leaves something running that a rerun would collide with.\nThis cannot be undone once the cleanup command executes.">
              <input type="checkbox" id="check-cleanup:${key}"> Stop stale process first
            </label>`
          : "";
        return `<div style="border:1px solid var(--border);border-radius:6px;padding:8px;margin:4px 0;background:var(--panel-2);min-width:260px">
          ${fields}
          ${cleanupField}
          <div class="btn-row" style="margin-top:4px">
            <button class="btn primary" data-click="runCheckWithInputs" data-kind="${esc(kind)}" data-guardian-id="${esc(g.id)}" data-i="${i}" data-tip="Run this check with the values above.">▶ Run</button>
            <button class="btn" data-click="toggleCheckForm" data-key="${esc(key)}" data-tip="Close this form without running.">Cancel</button>
          </div>
        </div>`;
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
      const G_COLORS = { collecting:"--muted", merging:"--running", merge_failed:"--failed", merge_stopped:"--pending", in_review:"--accent", approved:"--done", cancelled:"--cancelled", deployed:"--done", pending:"--pending", ready:"--teal", in_progress:"--running", actioning:"--running", done:"--done", proof_pending:"--running", conflict_resolved:"--queued", failed:"--failed" };
      // States in which a review may be cancelled — mirrors the backend's
      // cancel_guardian() (daemon/src/guardian.rs). Both the left-hand review
      // list menu and the detail pane's upper-right ⋯ menu use this one set so
      // "Cancel review" appears in the same states everywhere.
      const G_CANCELLABLE = ["collecting", "merging", "merge_failed", "merge_stopped", "in_review", "approved"];
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
      function selectGuardian(id) { selectedGuardian = id; revealedGuardianId = id; syncHash(true); renderReviews(); renderReviewDetail(); }
      /**
       * Renders the review list in the Reviews tab's sidebar.
       * @returns {void}
       */
      function renderReviews() {
        const el = byId("reviews");
        renderReviewStatusFilters();
        renderReviewResolverFilters();
        renderReviewOriginFilters();
        if (!guardians.length) { el.innerHTML = `<div class="empty">No reviews.</div>`; return; }
        const list = visibleGuardians();
        if (!list.length) { el.innerHTML = `<div class="empty">No matching reviews.</div>`; return; }
        const bulkBar = guardianMultiSel.size > 1 ? reviewSelectionBar() : "";
        el.innerHTML = bulkBar + list.map((g) => `<div class="squad-item ${(g.id===selectedGuardian || guardianMultiSel.has(g.id))?"selected":""}" data-click="onReviewClick" data-ctx="openReviewMenu" data-guardian-id="${esc(g.id)}">
          <button class="btn squadbtn" data-click="openReviewMenu" data-guardian-id="${esc(g.id)}" data-tip="Review actions — rename, hide, cancel, or delete this review.">⋯</button>
          <div class="rid">${hiddenGuardianIds.has(g.id) ? `<span data-tip="You've hidden this review from your own view.\nIt's shown now because \"show hidden\" is on, or you navigated to it directly.\nA personal preference — it does not affect what other users see.">🙈</span> ` : ""}${esc(g.name)} ${arbiterBadge(g)}</div><div class="meta">${gdot(g.status)}<span>${g.status}</span> · ${g.branches.length} branches</div></div>`).join("");
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
      /**
       * Opens the review right-click context menu.
       * @param {MouseEvent} e
       * @param {string} id
       * @returns {void}
       */
      function openReviewMenu(e, id) {
        e.preventDefault(); e.stopPropagation(); closeSquadMenu();
        const g = guardians.find((x) => x.id === id); if (!g) return;
        const menu = document.createElement("div");
        menu.className = "ctx-menu"; menu.id = "squad-menu";
        const canCancel = G_CANCELLABLE.includes(g.status);
        const items = [`<div data-click="openEditReviewDetailsFromMenu" data-guardian-id="${esc(id)}" data-tip="Edit this review's name and other settings.">✎ Edit Details</div>`];
        const reviewUri = `guardian:${id}`;
        items.push(`<div data-click="toggleWatch" data-entity-uri="${esc(reviewUri)}" data-tip="${isWatching(reviewUri) ? "Stop receiving watcher notifications for this review." : "Watch this whole review and choose which mailbox priority tiers should notify you."}">${isWatching(reviewUri) ? "◉ Unwatch" : "◎ Watch…"}</div>`);
        items.push(hiddenGuardianIds.has(id)
          ? `<div data-click="unhideReviewMenuItem" data-guardian-id="${esc(id)}" data-tip="Show this review again in your own view.\nWho/when: use this to undo an earlier hide.\nA personal preference — it never affects what other users see.">👁 Unhide</div>`
          : `<div data-click="hideReviewMenuItem" data-guardian-id="${esc(id)}" data-tip="Hide this review from your own view — it stays fully intact and keeps running/counting normally.\nWho/when: use this to declutter your list of reviews you don't need to watch right now.\nA personal preference — it never affects what other users see, and can be undone any time via \"show hidden\".">🙈 Hide</div>`);
        if (canCancel) items.push(`<div class="danger" data-click="cancelReview" data-guardian-id="${esc(id)}" data-tip="Cancel this review — stops the current merge and discards its result.\nThe review can be restarted afterward.\nThis cannot be undone.">⊘ Cancel review</div>`);
        if (g.status === "cancelled") items.push(`<div data-click="reopenReview" data-guardian-id="${esc(id)}" data-tip="Reopen this cancelled review and immediately stage in whatever branches are already ready, without waiting for the rest.\nUse this when a review was cancelled by mistake, or you want to retry it without recreating it from scratch.\nAny branch still waiting on its task keeps the review in collecting until it finishes.">↺ Reopen review</div>`);
        items.push(`<div class="danger" data-click="deleteReview" data-guardian-id="${esc(id)}" data-tip="Delete this review and remove all review worktrees permanently.\nThis cannot be undone.">🗑 Delete</div>`);
        menu.innerHTML = items.join("");
        document.body.appendChild(menu);
        menu.style.left = Math.min(e.clientX, window.innerWidth - 180) + "px";
        menu.style.top = Math.min(e.clientY, window.innerHeight - 90) + "px";
      }
      /**
       * Deletes a review and its worktrees after confirmation.
       * @param {string} id
       * @returns {Promise<void>}
       */
      async function deleteReview(id) {
        closeSquadMenu();
        const g = guardians.find((x) => x.id === id);
        if (!confirm(`Delete review "${g ? g.name : id}"? This removes its review worktrees and cannot be undone.`)) return;
        await del(`/api/guardians/${id}`);
        if (selectedGuardian === id) selectedGuardian = null;
        guardianMultiSel.delete(id);
        tick();
      }
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
        } catch (e) { alert("daemon unreachable"); return; }
        if (!resp.ok) { alert(await responseError(resp, hide ? "hide failed" : "unhide failed")); return; }
        if (hide) hiddenGuardianIds.add(id); else hiddenGuardianIds.delete(id);
        renderReviews();
      }
      /**
       * Hides every multi-selected review from the current user's own view
       * (RAL-331). Reports (via `alert`) any review the daemon refused to hide.
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
        if (failed.length) alert(failed.join("\n"));
      }
      /**
       * Re-enables every multi-selected review in the current user's own view
       * (RAL-331). Reports (via `alert`) any review the daemon refused to unhide.
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
        if (failed.length) alert(failed.join("\n"));
      }
      /**
       * Opens the review detail pane's title-bar ⋯ context menu.
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
            : `<div style="color:var(--muted);padding:6px 10px;font-size:12px" data-tip="No actions are available because this review's status is '${g.status}'.\nActions like Cancel are only available while the review is collecting, merging, merge_failed, in_review, or approved.">No actions available</div>`;
        const stacksItem = `<div data-click="openReviewPrStacks" data-guardian-id="${esc(id)}" data-tip="View every PR stack previously submitted for this review, in any state -- including ones dropped because their linked PR merged on the forge while the review was still mid-flight (RAL-300).\nWho/when: use this to see what was submitted before deciding whether/how to resubmit.\nRead-only -- does not resubmit or replay anything.">📜 View past PR stacks</div>`;
        menu.innerHTML = cancelItem + stacksItem;
        document.body.appendChild(menu);
        menu.style.left = Math.min(e.clientX, window.innerWidth - 180) + "px";
        menu.style.top = Math.min(e.clientY, window.innerHeight - 90) + "px";
      }
      /**
       * Cancels a review's in-progress merge after confirmation.
       * @param {string} id
       * @returns {Promise<void>}
       */
      async function cancelReview(id) {
        closeSquadMenu();
        const g = guardians.find((x) => x.id === id);
        if (!confirm(`Cancel review "${g ? g.name : id}"? The current merge will be discarded. This cannot be undone.`)) return;
        await guardianAction(`/api/guardians/${id}/cancel`);
        tick();
      }
      /**
       * Reopens a cancelled review, immediately trying a fresh merge pass.
       * @param {string} id
       * @returns {Promise<void>}
       */
      async function reopenReview(id) {
        closeSquadMenu();
        await guardianAction(`/api/guardians/${id}/reopen`);
        tick();
      }
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
        const done = cnt((b) => b.merge_status === "done");
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
       * Renders a review's own resolver/proof cost (current attempt + cumulative).
       * @param {GuardianView} g
       * @returns {string}
       */
      function reviewCostSummary(g) {
        const hasAttempt = (g.attempt_tokens_in ?? 0) > 0 || (g.attempt_tokens_out ?? 0) > 0 || (g.attempt_cost_usd ?? 0) > 0;
        const hasCumulative = (g.cumulative_tokens_in ?? 0) > 0 || (g.cumulative_tokens_out ?? 0) > 0 || (g.cumulative_cost_usd ?? 0) > 0;
        if (!hasAttempt && !hasCumulative) return "";
        const cap = g.maximum_budget_usd;
        const capNote = cap ? ` <span style="color:var(--muted)">/ cap $${cap.toFixed(4)}</span>` : "";
        const overCap = cap != null && (g.cumulative_cost_usd ?? 0) > cap;
        const attemptTip = "This review's own conflict-resolution and proof agent cost for the CURRENT merge attempt only.\nExcludes the cost of the tasks/cells that fed into the review.\nWho/when: check this to see what the most recent rebase/re-merge attempt cost by itself.";
        const cumulativeTip = "This review's own conflict-resolution and proof agent cost, summed across EVERY rebase/re-merge attempt this review has gone through.\nExcludes the cost of the tasks/cells that fed into the review.\nWho/when: check this to see the full spend on a review that needed several re-merges.\nIf maximum_budget_usd is set, this is the value it's enforced against — once exceeded, the daemon stops making further resolver/proof calls and fails the review.";
        return `<div class="kv-row" data-tip="${attemptTip}"><span class="k">attempt cost</span><span>input ${g.attempt_tokens_in ?? 0} · output ${g.attempt_tokens_out ?? 0} · ${fmtCostUsd(g.attempt_cost_usd)}</span></div>
          <div class="kv-row" data-tip="${cumulativeTip}"><span class="k">cumulative cost</span><span>input ${g.cumulative_tokens_in ?? 0} · output ${g.cumulative_tokens_out ?? 0} · ${fmtCostUsd(g.cumulative_cost_usd)}${capNote}${overCap ? ` <span style="color:var(--failed)">⚠ over cap</span>` : ""}</span></div>`;
      }
      /**
       * Renders a review branch's merge-status badge (ready / conflict / resolved).
       * @param {GuardianBranch} b
       * @returns {string}
       */
      function branchBadge(b) {
        // Checked before every status badge — including "failed": an empty
        // branch fails the review (RAL-190), and the generic red "⚠ conflict"
        // badge below would say only that something broke. This says *which*
        // thing, so the reviewer knows to go look at the task's cell rather
        // than at a diff or a rebase conflict.
        if (b.is_empty) return `<span class="badge empty" data-tip="This branch adds no changes over the branch beneath it in the stack, so it failed the review.
Almost always means its task never committed its work — the review would otherwise have approved a stack containing none of that task's changes.
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
        if (b.auto_submit_error) return `<span class="badge bad" data-tip="Auto-submitting this branch's pull request failed: ${esc(b.auto_submit_error)}\nWho/when: you enabled auto-submit for this review's PR stack and this branch's PR wasn't opened/updated as a result.\nCheck forge credentials/connectivity, then resubmit manually (review pr submit) or wait for the next auto-submit attempt.">⚠ auto-submit failed</span>`;
        if (b.merge_status === "ready") return `<span class="badge ready" data-tip="All tasks are done — this branch is queued for the automatic rebase.\nThe scheduler will start rebasing it into the review stack shortly.">⚡ ready</span>`;
        if (b.merge_status === "failed") return `<span class="badge bad" data-tip="Merge failed — ${esc(b.detail || "conflict during rebase")}">⚠ conflict</span>`;
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
      // RAL-88: a small inspect button revealing which agent (and model, if known)
      // produced an AI-generated review artifact. `agent`/`model` are the stored
      // provenance values (with a resolver fallback for pre-provenance guardians).
      /**
       * Renders a 🔍 button revealing the agent/model that produced an AI-generated review artifact.
       * @param {string} gid
       * @param {string} key
       * @param {string} label
       * @param {string} [agent]
       * @param {string} [model]
       * @returns {string}
       */
      function agentInspectBtn(gid, key, label, agent, model) {
        const a = agent ? esc(agent) : "unknown";
        const m = model ? ` · model: ${esc(model)}` : "";
        const tid = `agi-${key}-${gid}`;
        return ` <button class="copy-btn" data-tip="Reveal which agent${model ? " and model" : ""} generated this ${label}.\nWho/when: use it as a reviewer to judge or debug AI-produced review content.\nRead-only — it only reveals stored provenance." data-click="toggleAgentInspect" data-tid="${esc(tid)}">🔍</button>` +
          `<span id="${tid}" style="display:none;margin-left:8px;font-size:12px;font-weight:normal;color:var(--muted)">agent: ${a}${m}</span>`;
      }
      /**
       * Toggles an agent-inspect reveal span's visibility.
       * @param {MouseEvent} ev
       * @param {string} tid
       * @returns {void}
       */
      function toggleAgentInspect(ev, tid) {
        ev.stopPropagation();
        const el = document.getElementById(tid);
        if (el) el.style.display = el.style.display === "none" ? "inline" : "none";
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
          return `<h3 class="section" data-tip="Computed from git log as each branch becomes ready — a plain commit-subject listing at first, replaced by an agent-written summary once that branch's review worktree is built.\nRecomputed whenever another branch becomes ready or the review is rebuilt.">change summary${agentInspectBtn(g.id, "summary", "change summary", g.summary_agent || g.resolver_agent, g.summary_model || g.resolver_model)}</h3>
            <div style="font-size:13px;line-height:1.55;white-space:pre-wrap;border:1px solid var(--border);border-radius:6px;padding:10px 12px;background:var(--bg);color:var(--text)">${esc(g.change_summary)}</div>`;
        }
        return `<h3 class="section" data-tip="A summary appears here as soon as one branch's source task cell finishes — no need to wait for merging/rebasing.">change summary</h3><div class="empty">waiting for a branch to be ready…</div>`;
      }
      // ---------- PR submission + sync (RAL-117/RAL-190) ----------
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
        return `<div class="pr-card" style="border:1px solid var(--border);border-radius:6px;padding:6px 8px;margin-bottom:4px" data-tip="Pull/merge request submitted via ${esc(p.forge)}.">
            <div class="row" style="justify-content:space-between;gap:6px">
              <span>${esc(p.forge)} ${link} <span class="mono" style="color:var(--muted);font-size:11px">${esc(p.branch_alias)} → ${esc(p.base_ref)}</span></span>
              <span class="badge" style="font-size:11px">${esc(p.state)}</span>
            </div>
            ${drift}
          </div>`;
      }
      /**
       * Renders a compact clickable link to this branch's open PR/MR, if one
       * exists, directly in the branch row (RAL-190) -- so a branch that
       * already has a PR stays one click away from GitHub/GitLab without
       * expanding its detail. `branchPrSection` below shows the fuller card
       * (with drift info) once expanded; this is just the always-visible
       * shortcut the per-branch submit button used to give for free.
       * @param {GuardianView} g
       * @param {GuardianBranch} b
       * @returns {string}
       */
      function branchPrLink(g, b) {
        const pr = (pullRequests[g.id] || []).find((p) => p.branch_id === b.id && p.state === "open");
        if (!pr || !pr.pr_url) return "";
        return `<a href="${esc(pr.pr_url)}" target="_blank" rel="noopener" class="badge mono" style="color:var(--accent);border-color:var(--accent)" onclick="event.stopPropagation()" data-tip="Open this branch's pull/merge request on ${esc(pr.forge)}.">${esc(pr.forge)} #${pr.pr_number ?? "?"}</a>`;
      }
      /**
       * Renders the PR status section for one stacked branch (RAL-190+):
       * its existing open PR(s), if any. Submission itself is triggered once,
       * for the whole stack, via `combinedPrSection`'s button -- not per
       * branch (a stray manual "submit PR" per branch made it too easy to
       * submit branches out of order and break the base chain).
       * @param {GuardianView} g
       * @param {GuardianBranch} b
       * @returns {string}
       */
      function branchPrSection(g, b) {
        if (!b.worktree) return "";
        const prs = (pullRequests[g.id] || []).filter((p) => p.branch_id === b.id && p.state === "open");
        if (!prs.length) return "";
        return `<div class="pr-section" style="margin-top:6px" onclick="event.stopPropagation()">${prs.map(prCard).join("")}</div>`;
      }
      /**
       * Renders the review's PR-stack submission control (RAL-190+): one
       * button that pushes every enabled branch lacking an open PR as its own
       * PR, based on the branch below it -- never a single squashed PR -- and,
       * on GitHub, registers/grows a native PR stack spanning them. Each
       * branch's own PR card is still shown inline under that branch's row
       * (`branchPrSection`); this section is just the whole-stack trigger.
       * @param {GuardianView} g
       * @returns {string}
       */
      function combinedPrSection(g) {
        if (!g.branches.some((b) => b.enabled && b.worktree)) return "";
        // RAL-410: separate-PR-branch/match-worktree-branch-name/auto-submit
        // are edited via the Edit Details modal (openEditReviewDetails) --
        // shown here as a read-only summary alongside the submit action.
        return `<h3 class="section" data-tip="Pull/merge requests submitted for this review on GitHub/GitLab (RAL-117/RAL-190).">pull requests</h3>
            <div class="row" style="gap:10px;align-items:center;flex-wrap:wrap">
              <button class="btn primary" style="padding:3px 10px;font-size:11px" data-click="submitPrStack" data-guardian-id="${esc(g.id)}" data-tip="Push every enabled branch in this review as its own PR, each based on the branch below it -- never one squashed PR containing everything.\nOn GitHub, also registers/grows a native PR stack so GitHub's own UI shows them as a linked stack.\nWho/when: once the stack looks good, open real PRs for the whole thing without leaving the board.\nSafe to press again after adding a new branch on top -- only the new branch gets its own PR.\nRuns in the background -- each branch's PR card above updates once its forge call completes.">submit PR stack</button>
              <span class="k" style="text-transform:none;letter-spacing:0;font-size:11px;color:var(--muted)" data-tip="separate PR branch: ${g.effective_separate_pr_branch ? "yes" : "no"}\nmatch worktree branch name: ${g.effective_match_pr_branch_name ? "yes" : "no"}\nauto-submit PR stack: ${g.effective_auto_submit_pr_stack ? "yes" : "no"}\nEdited via Edit Details.">${g.effective_separate_pr_branch ? "separate branch" : "same branch"}${g.effective_auto_submit_pr_stack ? " · auto-submit" : ""}</span>
            </div>`;
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
        approved: "This review has already been approved — merge/rebase locks once a review is approved.\nOnly available when status is collecting, in_review, merge_stopped, or merge_failed.",
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
            ? "Resume the stopped rebase from where it left off — rebuilds the review stack from the first remaining worktree.\nA review left stopped mid-rebase is paused, not cancelled: branches and worktrees are kept."
            : "Start the Guardian: rebase each branch onto the prior in the stack, resolve conflicts with the AI agent, and run check gates.\nOnly available when status is collecting, in_review, merge_stopped, or merge_failed.",
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

      /**
       * Renders the full review detail pane (branch stack, checks, manual commands, chat, etc).
       * @returns {void}
       */
      function renderReviewDetail() {
        const el = byId("review-detail");
        const g = guardians.find((x) => x.id === selectedGuardian);
        if (!g) {
          // RAL-382: a review just navigated to (gotoReview) whose data hasn't
          // arrived yet shows a loading placeholder — never the previously
          // selected review's details, and never a bare "Select a review." that
          // reads as if the click did nothing.
          el.innerHTML = (selectedGuardian && reviewDetailLoading === selectedGuardian) ? `<div class="empty">Loading review…</div>` : `<div class="empty">Select a review.</div>`;
          return;
        }
        reviewDetailLoading = null;
        // RAL-14: reorder is allowed while the review is still open — not once it
        // is approved/deployed (those branches are considered merged/shipped).
        const canReorder = !["approved", "cancelled", "deployed"].includes(g.status);
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
              ${b.detail ? `<div class="kv-row" style="margin:0 0 4px"><span class="k" style="text-transform:none;letter-spacing:0">status</span><span class="v" style="font-size:12px">${esc(b.detail)}</span></div>` : ""}
              ${b.worktree ? `<div class="kv-row" style="margin:0"><span class="k" style="text-transform:none;letter-spacing:0">review worktree</span><span class="v mono" style="font-size:11px">${esc(b.worktree)}</span></div>` : ""}
              ${(b.worktree || b.source_squad_id != null) ? `<div class="row" style="margin:2px 0 4px">${worktreeCellBtn(b, `${g.id}:${b.id}`)}</div>` : ""}
              <div class="btn-row" style="margin-top:4px;position:relative;gap:0">${resolverTerminalBtns(g, b)}</div>
              ${resolverPeekBox(g, b)}
              ${branchPrSection(g, b)}
              ${open ? branchFeedbackSection(g, b) : ""}
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
          const moveBtn = (canReorder && g.status !== "merging")
            ? `<button class="icon-btn" data-click="openMoveBranchMenu" data-guardian-id="${esc(g.id)}" data-branch-id="${esc(b.id)}" style="font-size:11px;padding:1px 5px" data-tip="Move this branch to a different review.\nWho/when: a change needs to ship independently of the review it started in -- e.g. this review is stalled but this one branch is ready, or another review needs just this branch.\nBoth reviews rebuild afterward: this one renumbers its remaining branches, the destination review rebases this one into its own stack.\nBlocked while either review has an active merge/rebase in progress.\nThis cannot be undone.">⇄</button>`
            : "";
          const isBranchSel = selectedBranch[g.id] === b.branch;
          return `
          <div class="branch-item"${isEnabled ? "" : ' style="opacity:0.45"'}>
            <div class="branch-row selectable${isBranchSel ? " sel" : ""}" data-branch="${esc(b.branch)}" ${drag} data-click="selectBranchRow" data-guardian-id="${esc(g.id)}" data-tip="Click to select this branch. Drag to reorder.">
              ${canReorder ? '<span class="grip" data-tip="Drag to reorder branches — the merge order determines the rebase stack.">⋮⋮</span>' : ""}
              ${toggle}
              <span>${gdot(b.merge_status || "")}</span>
              <span class="mono" style="flex:1${isEnabled ? "" : ";color:var(--muted)"}">${esc(b.branch)}</span>
              ${isEnabled ? `${branchBadge(b)} ${pill(b.merge_status || "")} ${branchPrLink(g, b)}` : '<span class="badge" style="color:var(--muted);border-color:var(--border);font-size:11px">disabled</span>'}
              ${enableToggle}${reEnableIcon}${moveBtn}
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
        const checks = gChecks.length
          ? `<div class="row"${g.skip_auto_build ? ' style="opacity:.45"' : ""}>${gChecks.map((c) => `<span class="vchip" data-tip="Check gate — runs after each merge commit and on the combined review worktree.\nAll gates must pass before the review can be approved.">🔒 <span class="mono">${esc(c)}</span></span>`).join("")}</div>`
          : autoBuiltCmd
            ? `<div class="row"><span class="vchip" style="border-color:var(--done);color:var(--done)" data-tip="No check gates were configured for this review, so a build ran automatically once the stack finished merging, sourced from ${esc(autoBuildMatch ? autoBuildMatch.source : "")}.\nWho/when: nothing to do — this runs on its own so 'in review' reliably means the code builds, even when the review author configured no checks.\nConfigure explicit check gates above to replace this with your own build/test commands.">🔧 auto-built: <span class="mono">${esc(autoBuiltCmd)}</span></span></div>`
            : g.skip_auto_build
              ? "—"
              : `<div class="row"><span class="vchip" style="border-color:var(--unverified);color:var(--unverified)" data-tip="No check gates are configured for this review, the project has no auto_build default in .ralphus.toml, and either the resolver agent found no build step to infer or skip auto-build is on.\nWho/when: relevant to anyone relying on 'in review' meaning 'built and tested' — right now this review reached that status with zero build or test verification.\nAdd checks above, or set [review] auto_build in the project's .ralphus.toml, to close this gap.">⚠ no build verification configured</span></div>`;
        // RAL-410: skip-auto-build/skip-per-branch-worktrees, squash,
        // resolver agent/model, and proof scope are all now edited via the
        // "Edit Details" modal (openEditReviewDetails) -- these are
        // read-only summaries here.
        const skipInfo = `<div class="kv-row"><span class="k">skip auto-build</span><span class="v">${g.skip_auto_build ? "yes" : "no"}${g.skip_auto_build && gChecks.length ? ' — <span style="color:var(--queued)">gates will be skipped</span>' : ""}</span></div>
          <div class="kv-row"><span class="k">skip per-branch worktrees</span><span class="v">${g.skip_worktrees ? "yes" : "no"}</span></div>`;
        // RAL-91: per-(git-project) squash. Scope is each git project in the
        // review, NOT the whole review — a review spanning N projects honours
        // each project's setting independently.
        const squashOn = g.squash_projects || [];
        const squashProjects = (g.projects && g.projects.length) ? g.projects : [g.git_root || ""];
        const squashSummary = squashProjects.map((p) => {
          const on = squashOn.includes(p);
          const label = (p || "").split(/[\\/]/).filter(Boolean).pop() || p;
          return `<div class="kv-row"><span class="k">squash${isMultiProject ? ` — <span class="mono" style="font-size:11px">${esc(label)}</span>` : ""}</span><span class="v">${on ? "yes" : "no"}</span></div>`;
        }).join("");
        // Conflict-resolver backend for this review.
        const resolverRow = `<div class="kv-row"><span class="k">resolver agent</span><span class="v mono">${esc(resolverOf(g))}</span></div>
             <div class="kv-row"><span class="k">resolver model</span><span class="v mono">${esc(g.resolver_model || "agent default")}</span></div>`;
        // RAL-168: "Proof" scope -- how often the dedicated LLM-based
        // final-proof pass runs.
        const effectiveProofScope = g.effective_proof_scope || "each_branch";
        const proofScopeRow = `<div class="kv-row"><span class="k">proof scope</span><span class="v">${esc(effectiveProofScope.replace(/_/g, " "))}${!g.proof_scope ? " (project default)" : ""}</span></div>
          ${effectiveProofScope === "each_branch" ? `<div class="kv-row"><span class="k">skip auto-clean branches</span><span class="v">${g.effective_proof_skip_auto_clean ? "yes" : "no"}</span></div>` : ""}`;
        el.innerHTML = `<div class="squad-banner">${gdot(g.status)}<span class="rid">${esc(g.name)}</span> ${pill(g.status)} ${arbiterBadge(g)}
            <span style="flex:1"></span>${watchersHtml(`guardian:${g.id}`)}<button class="icon-btn" data-click="openEditReviewDetails" data-guardian-id="${esc(g.id)}" data-tip="Edit this review's settings — name, upstream branch, resolver, proof scope, build/squash options, PR settings, and environment overrides — all in one place.\nNothing takes effect until you click Save; Save applies every change in a single request and triggers at most one rebase.">✎ Edit Details</button><button class="icon-btn" data-click="openReviewLogs" data-guardian-id="${esc(g.id)}" data-tip="View the audit log for this review — state changes, branch merge events, and notes.">📄 Logs</button><button class="btn squadbtn" data-click="openReviewTitleMenu" data-guardian-id="${esc(g.id)}" data-tip="Review actions — cancel this review.">⋯</button></div>
          ${mergeProgress(g)}
          ${conflictProgress(g)}
          ${reviewCostSummary(g)}
          <div class="kv-row"><span class="k">review id</span><span class="mono" style="cursor:pointer" data-tip="The unique identifier for this Guardian review.\nUse this ID in API calls, daemon logs, or to find the review worktree on disk.\nClick to copy the full ID." data-copy="${esc(g.id)}" onclick="copyText(event)">${esc(g.id)}</span></div>
          ${g.squad_id ? `<div class="kv-row"><span class="k">from squad</span><span class="v"><a href="#" data-click="gotoSquad" data-squad-id="${esc(g.squad_id)}" style="color:var(--accent)" data-tip="Switch to the Squads tab and open this squad.">${esc(g.squad_id)}</a></span></div>` : ""}
          ${g.status === "collecting" && g.squad_id ? `<div class="kv-row"><span class="k" style="text-transform:none;letter-spacing:0">gate</span><span class="v">${g.branches.some((b) => b.merge_status === "ready") ? "tasks complete — rebase will start automatically" : "starts automatically when its squad finishes — or start it now below"}</span></div>` : ""}
          <div class="kv-row"><span class="k">type</span><span class="v">${esc(g.review_type || "git")}</span></div>
          <div class="kv-row"><span class="k">upstream</span><span class="mono">${esc(g.base_branch)}</span></div>
          ${resolverRow}
          ${proofScopeRow}
          ${isMultiProject
            ? `<div class="kv-row"><span class="k">projects</span><span class="v" style="display:flex;flex-direction:column;gap:2px">${(g.projects||[]).map((p) => `<span class="mono" style="font-size:11px">${esc(p)}</span>`).join("")}</span></div>`
            : `<div class="kv-row"><span class="k">git root</span><span class="mono">${esc(g.git_root)}</span></div>`}
          <div class="kv-row"><span class="k">review branch</span><span class="mono">${esc(g.review_branch||"—")}</span></div>
          ${g.combined_worktree ? `<div class="kv-row"><span class="k">combined worktree</span><span class="v mono" style="font-size:11px">${esc(g.combined_worktree)}</span></div>` : ""}
          ${renderChangeSummary(g)}
          ${combinedPrSection(g)}
          <h3 class="section">check gates</h3>${checks}${skipInfo}
          <div class="kv-row">${envViewerBtn(`/api/guardians/${g.id}/tests-env`, "this review's check gates (tests)")}</div>
          <h3 class="section" data-tip="Squash controls how each task branch's commits appear in the review worktree.\nScope is per git project — set it independently for each project in the review.">squash</h3>${squashSummary}
          ${g.detail && !autoBuiltCmd ? `<div class="warn">${esc(g.detail)}</div>` : ""}
          <h3 class="section">branches${canReorder ? ' <span class="k" style="text-transform:none;letter-spacing:0">— drag to reorder · toggle ⊙/⊘ to enable/disable</span>' : ""}${hasPending ? ' <span class="badge warn2" data-tip="Unsaved order or enable/disable changes — click Save to apply, or Discard to revert.">● unsaved changes</span>' : ""}</h3>
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
          <div class="btn-row">
            ${(() => {
              // RAL-40: a disabled <button> gets `pointer-events:none` from the
              // global `.btn[disabled]` rule, so the tooltip engine's
              // mouseover listener never sees it — data-tip must live on a
              // wrapping <span> instead whenever the button may be disabled.
              const merge = mergeButtonView(g.status, pendingMergeActions.has(g.id));
              const mergeTip = esc(merge.tip);
              const mergeBtn = `<button class="btn primary" data-click="mergeReview" data-guardian-id="${esc(g.id)}" data-status="${esc(g.status)}" ${merge.enabled ? "" : "disabled"} data-tip="${mergeTip}">${esc(merge.label)}</button>`;
              // A disabled <button> swallows mouseover, so the tooltip has to
              // live on a wrapping <span> whenever the button is unavailable.
              return merge.enabled ? mergeBtn : `<span data-tip="${mergeTip}">${mergeBtn}</span>`;
            })()}
            ${(() => {
              // RAL-249: halt an in-progress rebase at its next checkpoint,
              // keeping the review resumable — distinct from "Cancel review".
              const canStop = g.status === "merging";
              if (!canStop) return "";
              return `<button class="btn" data-click="stopMerge" data-guardian-id="${esc(g.id)}" data-tip="Stop this rebase mid-flight — halts at the next checkpoint and pauses the review.\nThe review and its branches are kept, so you can resume the rebase afterward.\nThis is not a cancel: nothing is discarded.">⏸ Stop</button>`;
            })()}
            ${(() => {
              const isPending = pendingGuardianActions.has(g.id);
              const approveTip = isPending
                ? "Approval is in flight — waiting for the daemon to confirm."
                : "Approve this review for deployment — marks it as approved once all merges and check gates have passed.\nCan be pressed at any time; the daemon rejects it if the review isn't in a state that can be approved yet.";
              const approveBtn = `<button class="btn" data-click="approveReview" data-guardian-id="${esc(g.id)}" ${isPending ? "disabled" : ""} data-tip="${approveTip}">${isPending ? "Approving…" : "Approve"}</button>`;
              return isPending ? `<span data-tip="${approveTip}">${approveBtn}</span>` : approveBtn;
            })()}
            ${(() => {
              const canCancel = G_CANCELLABLE.includes(g.status);
              if (!canCancel) return "";
              return `<button class="btn danger" data-click="cancelReview" data-guardian-id="${esc(g.id)}" data-tip="Cancel this review — stops the current merge and discards its result.\nThe review can be restarted afterward.\nThis cannot be undone.">⊘ Cancel review</button>`;
            })()}
            ${(() => {
              // RAL-273: only meaningful once a stack is actually built and
              // open against the forge -- same scope as the 5-minute
              // background poll (in_review/merging).
              const canSync = ["in_review", "merging"].includes(g.status);
              const syncTip = canSync
                ? "Check GitHub/GitLab for a stack reorder made outside ralphus (e.g. dragging PRs into a new order) and apply it here, retriggering a rebase.\nRuns in the background; watch this review's branch order/status for the result.\nAlso happens automatically every 5 minutes for reviews with an active stack."
                : "Not available — a stack reorder can only be detected once this review has an open PR stack (status in_review or merging).";
              const syncBtn = `<button class="btn" data-click="syncPrReview" data-guardian-id="${esc(g.id)}" ${canSync ? "" : "disabled"}${canSync ? ` data-tip="${syncTip}"` : ""}>Sync PR</button>`;
              return canSync ? syncBtn : `<span data-tip="${syncTip}">${syncBtn}</span>`;
            })()}
          </div>
          ${(() => {
            // RAL-77: user-declared test actions from [[review.action]] in TOML.
            const hints = g.action_hints || [];
            if (hints.length === 0) return "";
            const btns = hints.map((h, i) => {
              if (h.command) {
                return renderCheckControl(g, "action", i, h, h.label || "Run");
              }
              return `<span data-tip="Prompt: ${esc(h.prompt||'')}\nNot runnable yet — prompt-based test actions expand via the resolver LLM before running, and that expansion isn't wired up.\nOnly command-based [[review.action]] entries are clickable today."><button class="btn" disabled style="opacity:0.6">${esc(h.label)}</button></span>`;
            }).join("");
            return `<h3 class="section" data-tip="User-declared test actions from the task TOML [[review.action]] blocks.\nLabelled buttons give reviewers one-click access to targeted manual checks.">test actions</h3>
              <div class="btn-row" style="flex-wrap:wrap">${btns}</div>`;
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
            const menuItems = cmds.map((cmd, i) => {
              const cmdText = cmd.command || "";
              if (cmd.inputs && cmd.inputs.length) {
                const key = `${g.id}:manual:${i}`;
                const open = !!checkFormOpen[key];
                return `<div style="padding:2px 4px">
                  <div data-click="toggleCheckForm" data-key="${esc(key)}" style="padding:4px 8px;cursor:pointer;font-size:12px;font-family:monospace;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;max-width:360px" data-tip="Run: ${esc(cmdText)}\nThis check needs some values filled in first — click to expand." onmouseover="this.style.background='var(--panel-2)'" onmouseout="this.style.background=''">${esc(cmdText)} ${open ? "▲" : "▾"}</div>
                  ${open ? renderCheckInputForm(g, "manual", i, cmd) : ""}
                </div>`;
              }
              return `<div data-click="runSingleManualCheck" data-guardian-id="${esc(g.id)}" data-i="${i}" style="padding:6px 12px;cursor:pointer;font-size:12px;font-family:monospace;white-space:nowrap;overflow:hidden;text-overflow:ellipsis;max-width:360px" data-tip="Run: ${esc(cmdText)}\nLaunches in the built review worktree." onmouseover="this.style.background='var(--panel-2)'" onmouseout="this.style.background=''">${esc(cmdText)}</div>`;
            }).join("");
            const gateTip = isReady
              ? `Run all ${cmds.length} suggested manual check command(s) in a new terminal window.\nEach launches in the built review worktree.`
              : state === "generating"
                ? "Generating suggested manual check commands now — every enabled branch has finished rebasing cleanly and the resolver agent is producing them.\nThis button enables once they're ready."
                : "Manual checks aren't generated yet — they're only produced after every enabled branch in this review has finished rebasing with no pending conflicts.\nStill collecting or rebasing branches.";
            const label = isReady ? "▶ Run all" : (state === "generating" ? "▶ Generating…" : "▶ Waiting on branches…");
            const runAllBtn = `<button class="btn primary" ${isReady ? "" : "disabled"} data-click="runAllManualChecks" data-guardian-id="${esc(g.id)}" style="${cmds.length > 1 && isReady ? 'border-radius:6px 0 0 6px' : ''}"${isReady ? ` data-tip="${gateTip}"` : ""}>${label}</button>`;
            return `<h3 class="section" data-tip="Shell commands suggested by the resolver agent to manually verify these changes.\nRegenerated every time the review branch is rebuilt.">manual checks${agentInspectBtn(g.id, "manual", "manual checks", g.manual_commands_agent || g.resolver_agent, g.manual_commands_model || g.resolver_model)}</h3>
              <div class="btn-row" style="position:relative;gap:0">
                ${isReady ? runAllBtn : `<span data-tip="${gateTip}">${runAllBtn}</span>`}
                ${isReady && cmds.length > 1 ? `<button class="btn" data-click="toggleManualMenu" data-guardian-id="${esc(g.id)}" style="border-left:none;border-radius:0 6px 6px 0;padding:4px 8px" data-tip="Show individual commands — run one at a time.">▾</button>
                ${menuOpen ? `<div style="position:absolute;top:100%;left:0;background:var(--bg);color:var(--text);border:1px solid var(--border);border-radius:6px;min-width:200px;z-index:50;box-shadow:0 4px 12px rgba(0,0,0,.4);padding:4px 0;margin-top:2px">${menuItems}</div>` : ""}` : ""}
              </div>
              <div class="btn-row" style="margin-top:4px;position:relative;gap:0">${manualChecksTerminalBtns(g)}</div>
              ${manualChecksPeekBox(g)}`;
          })()}`;
        attachPeekResizeHandlers();
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
        renderReviewDetail();
      }
      // RAL-29: switch the active project tab in a multi-project review.
      /**
       * Switches the active project tab in a multi-project review.
       * @param {string} gid
       * @param {string} proj
       * @returns {void}
       */
      function selectProjectTab(gid, proj) { selectedProjectTabs[gid] = proj; renderReviewDetail(); }
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
            showReviewError(`PR submission failed: ${detail}`);
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
        showInfoToast("PR stack requested — pushing branches and opening PRs in the background.");
        const outcome = await waitForPrOutcome(gid, sinceMs);
        if (!outcome.errored) {
          const parts = [];
          if (outcome.created > 0) parts.push(`${outcome.created} PR${outcome.created === 1 ? "" : "s"} opened`);
          if (outcome.resynced > 0) parts.push(`${outcome.resynced} base${outcome.resynced === 1 ? "" : "s"} corrected`);
          if (parts.length) {
            showInfoToast(`PR stack: ${parts.join(", ")}.`);
          } else if (outcome.completed) {
            showInfoToast("PR stack already up to date — nothing to open.");
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
       * The first line of a (possibly multi-line) message body.
       * @param {string} t
       * @returns {string}
       */
      function firstLine(t) { return t.split("\n")[0]; }
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
      }
      /**
       * Renders one review branch's read-only feedback thread (RAL-272):
       * lazily loads it into `branchMessages` on first render, then shows
       * either the collapsed message bubbles or a "No feedback yet" placeholder.
       * @param {GuardianView} g
       * @param {GuardianBranch} b
       * @returns {string}
       */
      function branchFeedbackSection(g, b) {
        const key = `${g.id}:${b.id}`;
        if (branchMessages[key] === undefined) loadBranchMessages(g.id, b.id);
        const msgs = branchMessages[key] || [];
        if (msgs.length === 0) {
          return `<div class="empty" style="margin-top:8px" data-tip="This branch has no feedback yet, so its thread stays hidden until it does.\nGive feedback from the CLI: ralphus review feedback <selector> <text>\nThe reviewer's message and the guardian's short acknowledgment both appear here, read-only, once given.">No feedback yet</div>`;
        }
        const thread = msgs.map((m) => {
          const isUser = m.role === "reviewer";
          const bubbleKey = `${key}:${m.seq}`;
          const text = stripRoute(m.text);
          const expanded = expandedChatBubbles.has(bubbleKey);
          const shown = expanded ? text : firstLine(text);
          // RAL-89: datetime the message was sent (reviewer) or received (guardian).
          const tStr = fmtMsgTime(m.at_ms);
          const ts = tStr
            ? `<span class="chat-time" data-tip="When this message was ${isUser ? "sent" : "received"} (${esc(fmtMsgTimeFull(m.at_ms))}, your local time).">${esc(tStr)}</span>`
            : "";
          const expandBtn = text.includes("\n")
            ? `<button class="chat-expand-btn" data-click="toggleChatBubble" data-key="${esc(bubbleKey)}" data-tip="${expanded ? "Collapse this message back to its first line." : "Expand to show the full message."}">${expanded ? "−" : "+"}</button>`
            : "";
          // RAL-379: only the attributed author is ever shown here -- the
          // authenticated submitter (who may differ, e.g. an assistant
          // posting on someone else's behalf) is audit-only and never
          // rendered in the UI.
          const label = isUser ? (m.author || "you") : "guardian";
          return `<div class="chat-msg chat-bubble-wrap ${isUser ? "user" : "guardian"}"><div>
              <div class="chat-label"${isUser ? ' style="text-align:right"' : ""}>${esc(label)}${ts}</div>
              <div class="chat-bubble">${esc(shown)}${expandBtn}</div>
            </div></div>`;
        }).join("");
        return `<h3 class="section" style="display:flex;align-items:center;gap:6px;margin-top:8px">branch feedback <button class="copy-btn" data-tip="Copy this branch's feedback thread to clipboard.\nChoose Markdown for readable text or JSON for raw data." data-click="showChatCopyMenu" data-guardian-id="${esc(g.id)}" data-branch-id="${esc(b.id)}">⧉</button></h3>
          <div style="max-height:260px;overflow-y:auto;border:1px solid var(--border);border-radius:8px;padding:8px;background:var(--bg)">${thread}</div>`;
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
        const rect = /** @type {HTMLElement} */ (e.currentTarget).getBoundingClientRect();
        const menu = document.createElement("div");
        menu.id = "chat-copy-menu";
        menu.className = "copy-menu";
        menu.style.top = (rect.bottom + 4) + "px";
        menu.style.left = rect.left + "px";
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
        const text = format === "markdown"
          ? msgs.map((m) => `**${m.role === "reviewer" ? (m.author || "You") : "Guardian"}:** ${m.text}`).join("\n\n")
          : JSON.stringify(msgs.map((m) => ({ role: m.role, text: m.text, author: m.author })), null, 2);
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
      /**
       * Builds the resolver `<select>`'s `<option>`s for `cwd`: every real
       * agent (built-ins plus configured custom profiles) cached from
       * `GET /api/agents`, sorted alphabetically, falling back to a hardcoded
       * 3 while the fetch is in flight (or if it fails) so the dropdown is
       * never empty. Whichever agent matches the effective
       * `[review].default_resolver_agent` (`GET /api/agents`'s
       * `default_agent`, or `"ollama"` while that hasn't loaded yet) gets
       * " (default)" appended to its label and is preselected when `selected`
       * is `""` (unset) -- there's no separate synthetic "(default)" entry.
       * @param {string} cwd
       * @param {string} selected
       * @returns {string}
       */
      function resolverOptionHtml(cwd, selected) {
        const cached = agentOptionsByCwd.get(cwd);
        if (!cached) {
          fetchAgentOptions(cwd);
        }
        const agents = cached && cached.agents.length
          ? cached.agents
          : [
              { id: "claude", kind: "builtin", backend: "claude" },
              { id: "claude-code", kind: "builtin", backend: "claude-code" },
              { id: "codex-cli", kind: "builtin", backend: "codex" },
            ];
        const defaultAgent = cached && cached.agents.length ? cached.defaultAgent : "ollama";
        const sorted = [...agents].sort((a, b) => a.id.localeCompare(b.id));
        return sorted.map((a) => {
          const isSelected = selected === a.id || (selected === "" && a.id === defaultAgent);
          const profileSuffix = a.kind === "profile" ? ` (profile → ${esc(a.backend)})` : "";
          const defaultSuffix = a.id === defaultAgent ? " (default)" : "";
          return `<option value="${esc(a.id)}" ${isSelected ? "selected" : ""}>${esc(a.id)}${profileSuffix}${defaultSuffix}</option>`;
        }).join("");
      }
      /**
       * Fetches `GET /api/agents` for `cwd` and caches the agents plus the
       * effective default, re-rendering the review detail pane once it lands.
       * A no-op (beyond the cache miss already having triggered it) while a
       * fetch for this `cwd` is in flight.
       * @param {string} cwd
       * @returns {Promise<void>}
       */
      async function fetchAgentOptions(cwd) {
        if (!cwd || agentOptionsByCwd.has(cwd)) return;
        agentOptionsByCwd.set(cwd, { agents: [], defaultAgent: "ollama" }); // placeholder so concurrent renders don't refetch
        try {
          const d = await (await fetch(`/api/agents?cwd=${encodeURIComponent(cwd)}`)).json();
          agentOptionsByCwd.set(cwd, { agents: d.agents || [], defaultAgent: d.default_agent || "ollama" });
        } catch (e) {
          agentOptionsByCwd.delete(cwd); // transient failure — allow a later render to retry
          return;
        }
        renderReviewDetail();
      }
      /**
       * Opens the Create Review modal.
       * @returns {void}
       */
      function openCreateReview() {
        byId("modal-root").innerHTML = `
          <div class="modal-bg" onclick="if(event.target===this)closeModal()"><div class="modal">
            <h2>Create Review</h2>
            <div class="edit-form">
              <label data-tip="Display name for this review — shown in the sidebar list.">name<input id="cr-name" value="my review"></label>
              <label data-tip="Review type. 'git' stacks branches via rebase with AI conflict resolution. Other types are placeholders.">type<select id="cr-type" onchange="onReviewTypeChange()"><option value="git">git</option><option value="document">document (placeholder)</option></select></label>
              <div id="cr-git-fields">
                <label data-tip="The branch that every submitted branch is ultimately rebased onto. Usually 'main' or 'master'.">upstream branch<input id="cr-base" value="main"></label>
                <label data-tip="Absolute path to the git repository root on this machine. The daemon checks out worktrees here.">git root (absolute path)<input id="cr-root" placeholder="C:/path/to/repo"></label>
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
        const body = { name: v("cr-name"), review_type, base_branch: v("cr-base"), git_root: v("cr-root"), checks, skip_auto_build, skip_worktrees };
        if (!body.git_root) { byId("cr-err").textContent = "git root is required"; return; }
        const resp = await fetch("/api/guardians", { method: "POST", body: JSON.stringify(body) });
        if (!resp.ok) { byId("cr-err").textContent = "create failed"; return; }
        const g = await resp.json(); selectedGuardian = g.id; closeModal(); tick();
      }

      // ---- Review-action error surfacing (RAL-108) ----
      // Review-action POSTs (merge, approve, force-start, ...) used to ignore
      // response.ok entirely, so a rejected request (e.g. a 409 on an invalid
      // state transition) looked exactly like success — the button appeared to
      // silently do nothing. showReviewError() surfaces the real failure, but
      // dedupes identical messages within a cooldown window so a burst of the
      // same failure (a double-click, a few failed polls in a row) doesn't
      // flood the screen with toasts.
      /** @type {Map<string, number>} message -> ms timestamp */
      const _reviewErrLastShown = new Map();
      const REVIEW_ERR_COOLDOWN_MS = 8000;
      /**
       * Shows a deduped toast for a failed review/guardian action.
       * @param {string} message
       * @returns {void}
       */
      function showReviewError(message) {
        const now = Date.now();
        const last = _reviewErrLastShown.get(message) || 0;
        if (now - last < REVIEW_ERR_COOLDOWN_MS) return;
        _reviewErrLastShown.set(message, now);
        const root = document.getElementById("toast-root");
        if (!root) return;
        const el = document.createElement("div");
        el.className = "toast";
        el.textContent = message;
        el.setAttribute("data-tip", "A review/guardian action was rejected by the daemon — this is the real error, not a silent no-op.\nShown when a button like Merge / rebase or Approve fails (e.g. an invalid state transition).\nDisappears automatically; identical errors are suppressed for a short cooldown so repeated failures don't spam the screen.");
        root.appendChild(el);
        setTimeout(() => el.remove(), 6000);
      }
      /**
       * Shows a neutral, non-error acknowledgement toast (accent-colored
       * border, distinct from `showReviewError`'s red one) for an action that
       * was accepted but whose real outcome is only known later -- e.g. a
       * background PR submission. Not deduped (unlike `showReviewError`):
       * each call is a distinct action the user just took.
       * @param {string} message
       * @returns {void}
       */
      function showInfoToast(message) {
        const root = document.getElementById("toast-root");
        if (!root) return;
        const el = document.createElement("div");
        el.className = "toast info";
        el.textContent = message;
        el.setAttribute("data-tip", "Confirms the action was accepted and is running in the background.\nDisappears automatically after a few seconds; if it fails, a separate red error toast follows once the failure is detected.");
        root.appendChild(el);
        setTimeout(() => el.remove(), 4000);
      }
      /**
       * Shows a warning toast, optionally with a one-click follow-up action.
       * @param {string} message
       * @param {{label: string, run: () => Promise<void>}} [action]
       * @returns {void}
       */
      function showWarningToast(message, action) {
        const root = document.getElementById("toast-root");
        if (!root) return;
        const el = document.createElement("div");
        el.className = "toast warn";
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
            try {
              await action.run();
            } finally {
              el.remove();
            }
          };
          actions.appendChild(btn);
          el.appendChild(actions);
        }
        el.setAttribute("data-tip", "Warns that the base change was saved while a review rebase was already in progress.\nUse the button to safely stop and restart now, or leave it alone and the board will rebuild again after the current rebase finishes.");
        root.appendChild(el);
        setTimeout(() => el.remove(), 8000);
      }

      // ---- Guardian notices -> toast (RAL-273) ----
      // A guardian notice (`notice_kind`/`notice_message`/`notice_at_ms`) is
      // server-recorded, one-shot, purely informational state -- e.g. an
      // incoming GitHub/GitLab stack reorder interrupting a local reorder in
      // flight, or a linked PR merging out-of-band while a rebase/feedback
      // pass owned the review's worktrees (`pr_merged_mid_flight`, RAL-300;
      // see `Store::set_guardian_notice`). There is no server-side "seen"
      // tracking: each poll of `/api/guardians` re-sends whatever the last
      // notice was, so the board itself remembers which `notice_at_ms` it
      // already showed per guardian and only toasts once per new one.
      // RALPHUS-GUARDIAN-NOTICE:BEGIN
      /**
       * Which of `list`'s guardian notices are newer than what `shown` last
       * recorded for that guardian, as ready-to-display toast text (RAL-273;
       * RAL-300 adds the `pr_merged_mid_flight` notice kind). Pure: does not
       * touch the DOM or mutate `shown` -- the caller applies the result.
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
          showInfoToast(toast.text);
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
          showReviewError("Action failed: network error");
          return null;
        }
        if (!resp.ok) {
          const e = await resp.json().catch(() => ({}));
          showReviewError(((e.error || {}).message) || `Action failed (${resp.status})`);
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
        showInfoToast(mergeRequestedToast(status));
        try {
          const path = status === "merging" ? "cancel_and_merge" : "merge";
          // A non-2xx already surfaced the red error toast inside
          // `guardianAction`; the reload and the pending-state clear below
          // still run, so the button comes back rather than staying stuck.
          await guardianAction(`/api/guardians/${id}/${path}`);
          await tick();
        } finally {
          pendingMergeActions.delete(id);
          if (!userIsSelecting()) renderReviewDetail();
        }
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
        await guardianAction(`/api/guardians/${id}/stop`);
        tick();
      }
      /**
       * Approves an in-review review for deployment. Marks the button
       * pending/disabled immediately (RAL-234) -- the daemon write itself is
       * a single status-string match, but the follow-up `tick()` reload
       * still takes a network round-trip or two, and without this the button
       * looked unresponsive for that whole window.
       * @param {string} id
       * @returns {Promise<void>}
       */
      async function approveReview(id) {
        pendingGuardianActions.add(id);
        if (!userIsSelecting()) renderReviewDetail();
        try {
          await guardianAction(`/api/guardians/${id}/approve`);
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
        showInfoToast("Checking GitHub/GitLab for a stack reorder…");
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
        const targets = guardians.filter((x) => x.id !== gid && !["approved", "cancelled", "deployed"].includes(x.status));
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
      /**
       * Runs a single suggested manual-check command by index.
       * @param {string} id
       * @param {number} index
       * @returns {Promise<void>}
       */
      async function runSingleManualCheck(id, index) {
        manualMenuOpen[id] = false;
        renderReviewDetail();
        await guardianAction(`/api/guardians/${id}/run-manual-commands`, { index });
      }
      // RAL-77: run a user-declared action hint by index.
      /**
       * Runs a user-declared `[[review.action]]` hint by index.
       * @param {string} id
       * @param {number} index
       * @returns {Promise<void>}
       */
      async function runActionHint(id, index) {
        await guardianAction(`/api/guardians/${id}/run-action-hint`, { index });
      }

      // ---------- structured check inputs (RAL-164) ----------
      /**
       * Toggles a check's inline input form open/closed.
       * @param {string} key - "<gid>:<manual|action>:<index>"
       * @returns {void}
       */
      function toggleCheckForm(key) {
        checkFormOpen[key] = !checkFormOpen[key];
        renderReviewDetail();
      }
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
        /** @type {Record<string,string>} */
        const inputs = {};
        (check && check.inputs || []).forEach((inp) => {
          const el = /** @type {HTMLInputElement|null} */ (document.getElementById(`check-input:${key}:${inp.name}`));
          if (el) inputs[inp.name] = el.value;
        });
        const cleanupEl = /** @type {HTMLInputElement|null} */ (document.getElementById(`check-cleanup:${key}`));
        const run_cleanup = !!(cleanupEl && cleanupEl.checked);
        checkFormOpen[key] = false;
        renderReviewDetail();
        const url = `/api/guardians/${id}/${kind === "manual" ? "run-manual-commands" : "run-action-hint"}`;
        await guardianAction(url, { index, inputs, run_cleanup });
        tick();
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
       * @returns {void}
       */
      function renderReadyBanner() {
        const el = document.getElementById("ready-banner");
        if (!el) return;
        const ready = (guardians || []).filter((g) => g.status === "in_review" && !dismissedReady.has(g.id));
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
        // On the reviews tab `guardians` is already fresh; elsewhere fetch it.
        if (tab !== "reviews") { try { guardians = await (await fetch("/api/guardians")).json(); } catch (_) {} }
        if (!userIsSelecting()) renderReadyBanner();
      }

