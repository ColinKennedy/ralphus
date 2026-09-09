      // ---- Set Status picker (RAL-74) ----
      // Floating ctx-menu showing valid states. stopPropagation on the trigger click
      // prevents the document listener from immediately closing the just-opened picker.
      /**
       * Closes the "Set Status" picker menu, if open.
       * @returns {void}
       */
      function closeStatusPicker() { const m = document.getElementById("status-picker"); if (m) m.remove(); }
      document.addEventListener("click", closeStatusPicker);

      /**
       * Opens the "Set Status" floating menu listing valid states for the given items.
       * @param {MouseEvent} e
       * @param {StatusPickerItem[]} items
       * @returns {void}
       */
      function openStatusPicker(e, items) {
        closeStatusPicker();
        _statusPickerItems = items;
        const allSquads = items.every(i => i.kind === "squad");
        const states = allSquads ? SQUAD_STATES : NODE_STATES;
        const menu = document.createElement("div");
        menu.className = "ctx-menu"; menu.id = "status-picker";
        menu.innerHTML = `<div style="color:var(--muted);font-size:11px;padding:3px 10px 6px;text-transform:uppercase;letter-spacing:.5px">Set status to…</div>`
          + states.map(s =>
            `<div data-click="doPickStatus" data-state="${esc(s)}" data-tip="Set status to ${s}.${IRREVERSIBLE_STATES.has(s) ? "\nThis cannot be undone." : ""}">${sdot(s)} ${s}</div>`
          ).join("");
        document.body.appendChild(menu);
        menu.style.left = Math.min(e.clientX, window.innerWidth - 200) + "px";
        menu.style.top = Math.min(e.clientY, window.innerHeight - 220) + "px";
      }
      /**
       * Confirms and applies a status override to every item in `_statusPickerItems`.
       * @param {string} state
       * @returns {Promise<void>}
       */
      async function doPickStatus(state) {
        closeStatusPicker();
        const items = _statusPickerItems; if (!items || !items.length) return;
        // RAL-116: Set Status → cancelled for a single squad goes through the
        // exact same cascading-cancel path as the Cancel Squad button (the
        // daemon's set-status handler special-cases `cancelled` to call the
        // identical store function) — show the matching dry-run preview here
        // too instead of a bare confirm(), so both entry points look and
        // behave the same.
        if (state === "cancelled" && items.length === 1 && items[0].kind === "squad") {
          const item = items[0];
          await showCancelPreview(`Set "${item.label}" to cancelled?`, `/api/squads/${item.squadId}/cancel/preview`, `/api/squads/${item.squadId}/cancel`);
          return;
        }
        const isDestructive = IRREVERSIBLE_STATES.has(state);
        let msg = items.length > 1
          ? `Set ${items.length} items to "${state}"?\n\n${items.map(i => "• " + i.label).join("\n")}`
          : `Set "${items[0].label}" to "${state}"?`;
        if (isDestructive) msg += "\n\nThis cannot be undone.";
        if (!confirm(msg)) return;
        const succeeded = [];
        const failed = [];
        for (const item of items) {
          try {
            const resp = await post(`/api/squads/${item.squadId}/set-status`, {
              kind: item.kind, task_idx: item.taskIdx, cell_idx: item.cellIdx,
              proof_idx: item.proofIdx, proof_scope: item.proofScope, state
            });
            if (!resp.ok) failed.push(`${item.label}: ${await responseError(resp, `set status to ${state} failed`)}`);
            else succeeded.push(item.label);
          } catch (_) { failed.push(`${item.label}: network error`); }
        }
        if (items.every((item) => item.kind !== "squad")) {
          reportGraphActionOutcome(`Set to ${state}`, /** @type {GraphNodeSelectionItem[]} */ (items), succeeded, failed);
        } else if (failed.length) {
          const head = succeeded.length
            ? `Set ${succeeded.length} of ${items.length} squad(s) to "${state}".`
            : `Failed to set any squads to "${state}".`;
          alert(`${head}\n\nFailures:\n${failed.map((msg) => `- ${msg}`).join("\n")}`);
        }
        tick();
        if (tab === "queue") pollQueue();
      }
      // ---- Add Dependency dialog (RAL-105) ----
      // Lets the user wire up a manual cross-squad dependency after submission:
      // the selected source squad(s) will not be scheduled until the chosen
      // target squad reaches Done. Appends to the same `[[default]] depends_on`
      // list the TOML `[[default]]` block populates at submit time, so it is
      // picked up by the existing whole-squad gating (`Store::list_ready`) —
      // no new scheduling path. The daemon rejects self-references and
      // cycles (409); the error is shown inline and the dialog stays open.
      /** @type {string[]} */
      let _addDepSourceIds = [];
      /** @type {string|null} */
      let _addDepTargetId = null;
      /**
       * Opens the Add Dependency dialog for one squad, or every multi-selected squad if `id` is part of the selection.
       * @param {MouseEvent} e
       * @param {string} id
       * @returns {void}
       */
      function openAddDependencyDialogFor(e, id) {
        e.preventDefault(); e.stopPropagation();
        const ids = (multiSel.has(id) && multiSel.size > 1) ? [...multiSel] : [id];
        openAddDependencyDialog(e, ids);
      }
      /**
       * Opens the Add Dependency dialog for an explicit set of source squads.
       * @param {MouseEvent|null} e
       * @param {string[]} sourceIds
       * @returns {void}
       */
      function openAddDependencyDialog(e, sourceIds) {
        if (e) { e.preventDefault(); e.stopPropagation(); }
        closeSquadMenu();
        if (!sourceIds || !sourceIds.length) return;
        _addDepSourceIds = sourceIds;
        _addDepTargetId = null;
        renderAddDependencyDialog();
      }
      /**
       * Closes the Add Dependency dialog and clears its draft state.
       * @returns {void}
       */
      function closeAddDependencyDialog() { closeModal(); _addDepSourceIds = []; _addDepTargetId = null; }
      // Elides the "For: ..." squad list to a fixed 80-char max width (RAL-119):
      // shows as many full names as fit, then "... and N others"; falls back
      // to a mid-name ellipsis when even the first name alone is too long.
      /**
       * Elides a squad-name list to fit a max display width.
       * @param {string[]} names
       * @param {number} [maxLen]
       * @returns {string}
       */
      function elideForSquadsLabel(names, maxLen) {
        maxLen = maxLen || 80;
        const prefix = "For: ";
        const full = names.join(", ");
        if (prefix.length + full.length <= maxLen) return full;
        const n = names.length;
        for (let k = n - 1; k >= 1; k--) {
          const remaining = n - k;
          const suffix = `, ... and ${remaining} other${remaining === 1 ? "" : "s"}`;
          const head = names.slice(0, k).join(", ");
          if (prefix.length + head.length + suffix.length <= maxLen) return head + suffix;
        }
        const remaining = n - 1;
        const tail = remaining > 0 ? ` and ${remaining} other${remaining === 1 ? "" : "s"}` : "";
        const budget = Math.max(0, maxLen - prefix.length - 3 - tail.length);
        return names[0].slice(0, budget) + "..." + tail;
      }
      /**
       * Renders the Add Dependency modal (search box + candidate list).
       * @returns {void}
       */
      function renderAddDependencyDialog() {
        const sourceNames = _addDepSourceIds.map((id) => findSquad(id)?.label || id);
        const sourceFull = sourceNames.join(", ");
        const sourceDisplay = elideForSquadsLabel(sourceNames);
        const forTip = "The squad(s) below will not be scheduled until the squad you pick reaches Done.\nThe squad you pick is not modified by this action."
          + (sourceDisplay !== sourceFull ? `\nFull list: ${sourceFull}` : "");
        byId("modal-root").innerHTML = `
          <div class="modal-bg" onclick="if(event.target===this)closeAddDependencyDialog()"><div class="modal" style="width:520px;max-width:94vw">
            <h2>Add Dependency</h2>
            <div class="k" style="margin-bottom:8px" data-tip="${esc(forTip)}">For: <b>${esc(sourceDisplay)}</b></div>
            <div class="edit-form">
              <label data-tip="Type part of a squad ID or its name. Matching squads appear below with a status chip — pick the exact squad if multiple share a name.">search by squad ID or name<input id="add-dep-search" placeholder="e.g. squad-000000000004 or my-task" autofocus oninput="renderAddDependencyCandidates(this.value)"></label>
            </div>
            <div id="add-dep-list" style="max-height:280px;overflow:auto;margin-top:8px;border:1px solid var(--border);border-radius:8px"></div>
            <div id="add-dep-err" class="verr"></div>
            <div class="btn-row">
              <button class="btn" onclick="closeAddDependencyDialog()" data-tip="Close without adding a dependency.">Cancel</button>
              <span data-tip="Add the selected squad as a dependency of ${_addDepSourceIds.length > 1 ? "all selected squads" : "this squad"}.\nThey will wait for it to complete before running.\nDisabled until you select a squad above."><button class="btn primary" id="add-dep-confirm" onclick="confirmAddDependency()" disabled>Add Dependency</button></span>
            </div>
          </div></div>`;
        renderAddDependencyCandidates("");
        const inp = document.getElementById("add-dep-search");
        if (inp) inp.focus();
      }
      /**
       * Renders the Add Dependency candidate-squad list, filtered by a search query.
       * @param {string} query
       * @returns {void}
       */
      function renderAddDependencyCandidates(query) {
        const q = (query || "").trim().toLowerCase();
        const candidates = squads.filter((r) => !_addDepSourceIds.includes(r.id)
          && (!q || r.id.toLowerCase().includes(q) || (r.label || "").toLowerCase().includes(q)))
          .sort((a, b) => b.created_at_ms - a.created_at_ms)
          .slice(0, 50);
        const el = document.getElementById("add-dep-list");
        if (!el) return;
        if (!candidates.length) { el.innerHTML = `<div class="empty">No matching squads.</div>`; return; }
        el.innerHTML = candidates.map((r) => `
          <div class="squad-item ${r.id === _addDepTargetId ? "selected" : ""}" data-click="selectAddDependencyTarget" data-squad-id="${esc(r.id)}"
               data-tip="Squad ${esc(r.id)} (${esc(squadDisplayState(r))}).\nClick to select it as the dependency target.">
            ${sdot(squadDisplayState(r))}<span class="rid">${esc(r.label || r.id)}</span> ${pill(squadDisplayState(r))} <span class="meta">${esc(r.id)}</span>
          </div>`).join("");
      }
      /**
       * Selects a candidate squad as the Add Dependency target and enables the confirm button.
       * @param {string} id
       * @returns {void}
       */
      function selectAddDependencyTarget(id) {
        _addDepTargetId = id;
        renderAddDependencyCandidates(/** @type {HTMLInputElement} */ (document.getElementById("add-dep-search"))?.value || "");
        const btn = /** @type {HTMLButtonElement} */ (document.getElementById("add-dep-confirm"));
        if (btn) btn.disabled = false;
      }
      /**
       * POSTs the selected Add Dependency target for every source squad and closes the dialog on success.
       * @returns {Promise<void>}
       */
      async function confirmAddDependency() {
        if (!_addDepTargetId || !_addDepSourceIds.length) return;
        const errEl = document.getElementById("add-dep-err");
        if (errEl) errEl.textContent = "";
        const target = _addDepTargetId;
        for (const id of _addDepSourceIds) {
          const resp = await post(`/api/squads/${id}/add-dependency`, { target_id: target });
          if (!resp.ok) {
            const b = await resp.json().catch(() => ({}));
            if (errEl) errEl.textContent = (b.error && b.error.message) || `failed to add dependency for ${id}`;
            return; // leave the dialog open so the user can pick a different target
          }
        }
        closeAddDependencyDialog();
        tick();
      }
      /**
       * Opens the Set Status picker scoped to a single squad.
       * @param {MouseEvent} e
       * @param {string} id
       * @returns {void}
       */
      function openStatusPickerForSquad(e, id) {
        const r = findSquad(id); if (!r) return;
        openStatusPicker(e, [{squadId: id, kind: "squad", taskIdx: 0, cellIdx: -1, proofIdx: -1, proofScope: "", label: r.label || id}]);
      }
      /**
       * Opens the Set Status picker scoped to every multi-selected squad.
       * @param {MouseEvent} e
       * @returns {Promise<void>}
       */
      async function bulkSetStatus(e) {
        e.stopPropagation();
        const items = [...multiSel].map((id) => ({squadId: id, kind: "squad", taskIdx: 0, cellIdx: -1, proofIdx: -1, proofScope: "", label: findSquad(id)?.label || id}));
        if (!items.length) return;
        openStatusPicker(e, items);
      }
      /**
       * Opens the context menu for a proof-step graph node (restart / set status).
       * @param {MouseEvent} e
       * @param {string} squadId
       * @param {number} ti
       * @param {number} si
       * @param {number} vi
       * @returns {void}
       */
      function openProofMenu(e, squadId, ti, si, vi) {
        const item = graphNodeItem(squadId, "proof", ti, si, vi); if (!item) return;
        const items = graphMenuSelection(squadId, item);
        if (items.length === 1) selectSingleGraphNode(squadId, "proof", ti, si, vi);
        renderGraph(); renderDetails(); syncHash(true);
        openGraphNodeMenu(e, items);
      }
      /**
       * Opens the context menu for a task graph node (restart / set status).
       * @param {MouseEvent} e
       * @param {string} squadId
       * @param {number} ti
       * @returns {void}
       */
      function openTaskNodeMenu(e, squadId, ti) {
        const item = graphNodeItem(squadId, "task", ti, -1, -1); if (!item) return;
        const items = graphMenuSelection(squadId, item);
        if (items.length === 1) selectSingleGraphNode(squadId, "task", ti, -1, -1);
        renderGraph(); renderDetails(); syncHash(true);
        openGraphNodeMenu(e, items);
      }
      /**
       * Solos or un-solos a task within a squad (RAL-157). Soloing pauses every
       * other task in the squad until un-soloed.
       * @param {string} squadId
       * @param {number} ti
       * @param {boolean} solo
       * @returns {Promise<void>}
       */
      async function soloTaskAct(squadId, ti, solo) {
        closeGraphMenu();
        await post(`/api/squads/${squadId}/tasks/${ti}/${solo ? "solo" : "unsolo"}`);
        tick();
      }
      /**
       * Opens the context menu for a cell graph node (restart / set status).
       * @param {MouseEvent} e
       * @param {string} squadId
       * @param {number} ti
      * @param {number} si
      * @returns {void}
      */
      function openCellNodeMenu(e, squadId, ti, si) {
        const item = graphNodeItem(squadId, "cell", ti, si, -1); if (!item) return;
        const items = graphMenuSelection(squadId, item);
        if (items.length === 1) selectSingleGraphNode(squadId, "cell", ti, si, -1);
        renderGraph(); renderDetails(); syncHash(true);
        openGraphNodeMenu(e, items);
      }
      /**
       * Shows a lightweight restart-confirmation modal with the same optional
       * note/"Apply To All Children" fields as `showRestartPreview` (RAL-174),
       * for restarts that have no downstream-impact preview to show (proof
       * steps). Submitting posts to `restartUrl` via the same `confirmRestart`
       * used by the impact-preview modal.
       * @param {string} title
       * @param {string} restartUrl
       * @returns {void}
       */
      function showRestartNotePrompt(title, restartUrl) {
        byId("modal-root").innerHTML = `
          <div class="modal-bg" onclick="if(event.target===this)closeModal()"><div class="modal" style="width:520px;max-width:94vw">
            <h2>${esc(title)}</h2>
            ${restartNoteFieldsHtml()}
            <p style="font-size:12px;color:var(--failed);margin:10px 0 2px">This cannot be undone.</p>
            <div class="btn-row">
              <button class="btn" onclick="closeModal()" data-tip="Cancel — do not restart anything.">Cancel</button>
              <button class="btn primary" data-click="confirmRestart" data-restart-url="${esc(restartUrl)}" data-tip="Proceed with the restart described above.\nThis cannot be undone.">⟳ Restart</button>
            </div>
          </div></div>`;
      }
      /**
       * Restarts one proof step (task-level when si === -1).
       * @param {string} id
       * @param {number} ti
       * @param {number} si
       * @param {number} vi
       * @returns {Promise<void>}
       */
      async function restartProof(id, ti, si, vi) {
        const url = si === -1
          ? `/api/squads/${id}/tasks/${ti}/proof/${vi}/restart`
          : `/api/squads/${id}/cells/${ti}/${si}/proof/${vi}/restart`;
        showRestartNotePrompt("Restart this proof step?", url);
      }
      /**
       * Immediately cancels a task/cell/proof node and everything
       * downstream of it within the same squad (RAL-181), stopping any live
       * agents/checks in that branch and capturing whatever they produced so
       * far into their ghosts first. Equivalent to Set Status → cancelled for
       * that scoped branch; exposed as a direct "Stop" action (mirroring
       * "Cancel Squad"'s icon) since cutting off one bad dependency branch is
       * common enough to not require opening the full status picker.
       * @param {string} squadId
       * @param {"task"|"cell"|"proof"} kind
       * @param {number} ti
       * @param {number} si
       * @param {number} vi
       * @param {string} proofScope
       * @param {string} label
       * @returns {Promise<void>}
       */
      async function stopNode(squadId, kind, ti, si, vi, proofScope, label) {
        closeGraphMenu();
        if (!confirm(`Stop "${label}"? This cannot be undone.`)) return;
        await post(`/api/squads/${squadId}/set-status`, {
          kind, task_idx: ti, cell_idx: si, proof_idx: vi, proof_scope: proofScope, state: "cancelled"
        });
        tick();
        if (tab === "queue") pollQueue();
      }
      /**
       * Deletes a squad permanently after confirmation.
       * @param {string} id
       * @returns {Promise<void>}
       */
      async function deleteSquad(id) {
        closeSquadMenu();
        if (!confirm("Delete this squad? This cannot be undone.")) return;
        await del(`/api/squads/${id}`);
        if (selectedSquadId === id) selectedSquadId = null;
        multiSel.delete(id); tick();
      }
      /**
       * Hides or unhides a squad from the current user's own view (RAL-331) --
       * a personal preference that never changes the squad itself or what any
       * other user sees. Only updates local state once the daemon confirms
       * the change (e.g. an unresolved current user 400s the request) --
       * an unconditional optimistic update would make a failed hide look
       * like it worked until the next full reload silently reverted it.
       * No confirmation prompt, since it's reversible via "show hidden".
       * @param {string} id
       * @param {boolean} hide
       * @returns {Promise<void>}
       */
      async function setSquadHidden(id, hide) {
        closeSquadMenu();
        let resp;
        try {
          resp = await (hide ? post(`/api/hidden/squads/${id}`) : del(`/api/hidden/squads/${id}`));
        } catch (e) { alert("daemon unreachable"); return; }
        if (!resp.ok) { alert(await responseError(resp, hide ? "hide failed" : "unhide failed")); return; }
        if (hide) hiddenSquadIds.add(id); else hiddenSquadIds.delete(id);
        renderSquads();
      }
      /**
       * Hides or unhides from the squad context menu (RAL-331) -- applies to
       * every multi-selected squad when the clicked squad is part of an
       * active multi-selection (size > 1), otherwise just the clicked squad.
       * @param {string} id
       * @param {boolean} hide
       * @returns {Promise<void>}
       */
      async function setSquadHiddenFromMenu(id, hide) {
        closeSquadMenu();
        if (multiSel.has(id) && multiSel.size > 1) await (hide ? bulkHideSquads() : bulkUnhideSquads());
        else await setSquadHidden(id, hide);
      }
      /**
       * Hides or unhides every multi-selected squad in a single request
       * (RAL-331) -- one POST to the batch endpoint instead of one round
       * trip per squad. Reports (via `alert`) any squad the daemon refused
       * to hide/unhide; a squad that fails does not block the rest of the
       * batch.
       * @param {boolean} hide
       * @returns {Promise<void>}
       */
      async function setSquadsHiddenBatch(hide) {
        const ids = [...multiSel];
        if (!ids.length) return;
        let resp;
        try {
          resp = await post("/api/hidden/squads/batch", { ids, hidden: hide });
        } catch (e) { alert("daemon unreachable"); return; }
        if (!resp.ok) { alert(await responseError(resp, hide ? "hide failed" : "unhide failed")); return; }
        /** @type {HiddenBatchResult} */
        const result = await resp.json();
        const failedIds = new Set(result.failed.map((f) => f.id));
        for (const id of ids) {
          if (failedIds.has(id)) continue;
          if (hide) hiddenSquadIds.add(id); else hiddenSquadIds.delete(id);
        }
        renderSquads();
        if (result.failed.length) alert(result.failed.map((f) => `${f.id}: ${f.error}`).join("\n"));
      }
      /**
       * Hides every multi-selected squad from the current user's own view
       * (RAL-331) in one batch request.
       * @returns {Promise<void>}
       */
      async function bulkHideSquads() { await setSquadsHiddenBatch(true); }
      /**
       * Re-enables every multi-selected squad in the current user's own view
       * (RAL-331) in one batch request.
       * @returns {Promise<void>}
       */
      async function bulkUnhideSquads() { await setSquadsHiddenBatch(false); }
      // selection tabs / bulk actions (multi mode)
      /**
       * Focuses one multi-selected squad's details pane without changing the selection set.
       * @param {string} id
       * @returns {void}
       */
      function focusSel(id) { clearNodeMultiSel(); selectedSquadId = id; sel = { kind: "squad", taskIdx: 0, cellIdx: 0 }; editing = false; renderAll(); syncHash(); }
      /**
       * Removes one squad from the multi-selection.
       * @param {string} id
       * @returns {void}
       */
      function unpickSel(id) { multiSel.delete(id); if (selectedSquadId === id) selectedSquadId = [...multiSel][0] || null; renderAll(); }
      /**
       * Cancels every multi-selected squad.
       * @returns {Promise<void>}
       */
      async function bulkCancel() { for (const id of [...multiSel]) { try { await post(`/api/squads/${id}/cancel`); } catch (_) {} } tick(); }
      /**
       * Deletes every multi-selected squad after confirmation.
       * @returns {Promise<void>}
       */
      async function bulkDelete() {
        if (!confirm(`Delete ${multiSel.size} squad(s)? This cannot be undone.`)) return;
        for (const id of [...multiSel]) { try { await del(`/api/squads/${id}`); } catch (_) {} }
        multiSel.clear(); selectedSquadId = null; tick();
      }
      /**
       * Renders the multi-select tab strip and bulk-action button row.
       * @returns {string}
       */
      function selectionTabs() {
        const chips = [...multiSel].map((id) => {
          const r = findSquad(id); if (!r) return "";
          return `<span class="sel-tab ${id === selectedSquadId ? "active" : ""}" data-click="focusSel" data-squad-id="${esc(id)}">${esc(r.label || id)}<span class="x" data-click="unpickSel" data-squad-id="${esc(id)}">✕</span></span>`;
        }).join("");
        return `<div class="sel-tabs">${chips}</div>
          <div class="btn-row" style="margin-top:0"><button class="btn" onclick="bulkSetStatus(event)" data-tip="Override the status of all ${multiSel.size} selected squads to any valid state.\nA confirmation dialog will list all affected squads before applying.\nTerminal states (done/failed/cancelled) cannot be undone.">Set Status ${multiSel.size}</button><button class="btn" onclick="openAddDependencyDialog(event,[...multiSel])" data-tip="Make all ${multiSel.size} selected squads wait for another squad to finish before they can be scheduled.\nSearch for the target squad by ID or name, then confirm.\nThe target squad itself is not modified.">🔗 Add Dependency ${multiSel.size}</button><button class="btn" onclick="bulkCancel()" data-tip="Cancel all selected squads — stops running cells and marks them as cancelled.">Cancel ${multiSel.size}</button><button class="btn" onclick="bulkHideSquads()" data-tip="Hide all ${multiSel.size} selected squads from your own view.\nWho/when: use this after shift-selecting a range of squads you want to declutter at once.\nA personal preference — it never affects what other users see or any squad's status.">🙈 Hide ${multiSel.size}</button><button class="btn" onclick="bulkUnhideSquads()" data-tip="Re-enable all ${multiSel.size} selected squads in your own view, if hidden.">👁 Unhide ${multiSel.size}</button><button class="btn danger" onclick="bulkDelete()" data-tip="Delete all selected squads and their data permanently. This cannot be undone.">Delete ${multiSel.size}</button></div>`;
      }

      // A cell's own proof steps (`[[task.cell.proof]]`), listed under a
      // dashed separator inside the cell card with a status dot each, so you
      // can see which proof step is running/passed/failed. Clicking one selects it
      // in the details pane. Empty = nothing to show.
      /**
       * Renders a cell's own proof-step rows.
       * @param {CellView} s
       * @param {number} ti
       * @param {number} si
       * @param {(k:string,t:number,s:number,v?:number)=>string} selCls
       * @param {string} squadId
       * @returns {string}
       */
      function cellProof(s, ti, si, selCls, squadId) {
        const vs = s.proof || [];
        if (!vs.length) return "";
        const rows = vs.map((v, vi) =>
          `<div class="sv-row selectable ${selCls("proof", ti, si, vi)}" data-tip="Cell proof step: ${esc(v.kind)}${v.id ? " — " + esc(v.id) : ""}\nRuns after this cell completes to validate its output.\nCurrent state: ${esc(v.state)}.\nShift/Ctrl-click to multi-select; right-click for batch actions." data-click="onGraphNodeClick" data-ctx="openProofMenu" data-squad-id="${esc(squadId)}" data-kind="proof" data-ti="${ti}" data-si="${si}" data-vi="${vi}">
             ${sdot(v.state)}<span class="sv-name">${esc(v.id || v.kind)}</span> ${pill(v.state)}${v.output ? `<button class="logs-btn" data-tip="View proof output — the raw response from the proof step's AI call." data-full="${esc(v.output)}" onclick="event.stopPropagation();openCmdPopup(event)">📄</button>` : ""}</div>`).join("");
        return `<div class="sproof"><span class="label">proof</span>${rows}</div>`;
      }

