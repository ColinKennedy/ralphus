      // ---------- Tasks tab: rendering + interaction (RAL-362) ----------
      // Everything below turns the pure RALPHUS-TASK-TAB-LOGIC helpers above
      // into DOM: fetching (`pollWatches`/`pollTasksTab`), building rows from
      // `squads`/`taskTabPrIndex`/`taskTabWatches` (`ttBuildRows`), the
      // virtualized header/body/group-header templates, the toolbar/column/
      // row interaction handlers, and the read-only details pane.
      /** Fixed row height (px) every virtualized Tasks-tab row -- task, cell, or group-header -- renders at; `ttVisibleRange` assumes a single uniform height. */
      const TT_ROW_H = 32;
      /** Extra rows rendered above/below the visible window so fast scrolling doesn't flash empty space. */
      const TT_OVERSCAN = 8;
      /** @type {TtRow[]} every task row built by the last `renderTasksTab()` pass, keyed for reuse by selection/watch/menu handlers. */
      let ttAllRows = [];
      /** @type {TtDisplayItem[]} the filtered/sorted/grouped display list built by the last `renderTasksTab()` pass. */
      let ttDisplayItems = [];
      /**
       * Polls the acting user's own watches (RAL-362 §5). Unconditional in
       * `tick()` alongside `pollHidden()` -- like hidden-items, watch state is
       * cheap and shared, so the Tasks tab's own marginal cost is exactly the
       * one `GET /api/pull-requests/index` call `pollTasksTab` makes below.
       * @returns {Promise<void>}
       */
      async function pollWatches() {
        try {
          const qs = currentUserName ? `?user=${encodeURIComponent(currentUserName)}` : "";
          const res = await fetch(`/api/watches${qs}`);
          if (!res.ok) { taskTabWatches = []; return; }
          const d = await res.json();
          taskTabWatches = d.watches || [];
        } catch (e) { /* transient -- the next tick retries */ }
      }
      // RALPHUS-POLL-TASKS-TAB:BEGIN
      /**
       * Polls the Tasks tab's one additional endpoint (RAL-362 §1) and
       * re-renders. `squads` itself is kept current by the shared
       * `updateCounter()` call `tick()` already makes before this, so this
       * is the only extra daemon request rendering the tab adds.
       * @returns {Promise<void>}
       */
      async function pollTasksTab() {
        try {
          const res = await fetch("/api/pull-requests/index");
          if (res.ok) taskTabPrIndex = await res.json();
        } catch (e) { /* transient -- the next poll retries */ }
        markUpdated();
        if (pendingHash && pendingHash.tab === "tasks") {
          const want = pendingHash;
          pendingHash = null;
          if (want.uri) {
            const wantSel = taskTabSelFromUri(want.uri);
            if (wantSel) taskTabSel = wantSel;
          }
          renderTasksTab();
          ttScrollSelectionIntoView();
          return;
        }
        renderTasksTab();
      }
      // RALPHUS-POLL-TASKS-TAB:END
      /**
       * Builds one `TtRow` per task across every loaded squad, joining in
       * each task's usage/review/PR/watch derived state via the pure
       * helpers above. The single place that turns raw `squads`/
       * `taskTabPrIndex`/`taskTabWatches` into what the table renders.
       * @returns {TtRow[]}
       */
      function ttBuildRows() {
        /** @type {TtRow[]} */
        const rows = [];
        for (const squad of squads) {
          const tasks = squad.tasks || [];
          for (let taskIdx = 0; taskIdx < tasks.length; taskIdx++) {
            const task = tasks[taskIdx];
            const reviews = ttTaskReviews(task);
            const prs = ttPrsForTask(taskTabPrIndex, squad.id, taskIdx);
            const usage = ttTaskUsage(task);
            const watch = ttEffectiveWatch(taskTabWatches, taskTabMutedTasks, squad.id, taskIdx);
            const explicitWatch = taskTabWatches.find((w) => w.entity_uri === ttTaskEntityUri(squad.id, taskIdx));
            const squadWatch = taskTabWatches.find((w) => w.entity_uri === ttSquadEntityUri(squad.id));
            const notifyTiers = (explicitWatch || squadWatch || {}).notify_tiers || [];
            const needs = ttTaskNeedsMe(task, watch, notifyTiers, reviews, prs.length);
            const startedAtMs = task.started_at_ms ?? null;
            const finishedAtMs = task.finished_at_ms ?? null;
            const sortTimeMs = startedAtMs == null ? null : (taskTabTimeMode === "start" ? startedAtMs : (finishedAtMs ?? Date.now()) - startedAtMs);
            rows.push({
              key: `${squad.id}:${taskIdx}`,
              squadId: squad.id,
              squadLabel: squad.label,
              squadState: squadDisplayState(squad),
              taskIdx,
              task,
              name: task.name,
              state: task.state,
              cells: task.cells || [],
              reviews,
              reviewBadge: ttPickReviewBadge(reviews),
              prs,
              prPick: ttPickTaskPr(prs),
              usage,
              startedAtMs,
              finishedAtMs,
              sortTimeMs,
              watch,
              needsMe: needs.needs,
              needsMeReason: needs.reason,
            });
          }
        }
        return rows;
      }
      /**
       * Rebuilds `ttAllRows`/`ttDisplayItems` from current state and
       * re-renders the summary, header, virtualized body and details pane.
       * The single entry point every Tasks-tab mutation calls back into.
       * @returns {void}
       */
      function renderTasksTab() {
        ttAllRows = ttBuildRows();
        const needsMeKeys = new Set(ttAllRows.filter((r) => r.needsMe).map((r) => r.key));
        const filtered = ttAllRows.filter((r) => ttRowMatchesFilters(r, taskTabFilters, hiddenSquadIds, needsMeKeys));
        filtered.sort((a, b) => ttCompareRows(a, b, taskTabFilters.sort) * taskTabFilters.dir);
        ttDisplayItems = ttBuildDisplayList(filtered, taskTabFilters.groupBySquad, taskTabExpanded);
        renderTtSummary(ttAllRows, filtered, needsMeKeys);
        renderTtHead();
        renderTtBody();
        renderTaskDetailsPane();
      }
      /**
       * Renders the "N tasks across M squads · K running · J waiting on you" summary line and the needs-me chip count.
       * @param {TtRow[]} allRows
       * @param {TtRow[]} filtered
       * @param {Set<string>} needsMeKeys
       * @returns {void}
       */
      function renderTtSummary(allRows, filtered, needsMeKeys) {
        const squadCount = new Set(filtered.map((r) => r.squadId)).size;
        const running = filtered.filter((r) => r.state === "running").length;
        byId("tt-summary").textContent = `${filtered.length} task${filtered.length === 1 ? "" : "s"} across ${squadCount} squad${squadCount === 1 ? "" : "s"} · ${running} running · ${needsMeKeys.size} waiting on you`;
        byId("tt-needs-me-count").textContent = needsMeKeys.size ? String(needsMeKeys.size) : "";
      }
      /**
       * Per-column header tooltip text (RAL-40: why / who-when / caveat), keyed by `TaskTabColumn.key`.
       * @type {{[key: string]: string}}
       */
      const TT_COLUMN_TIPS = {
        sel: "Select tasks for bulk actions (RAL-350). Click a box to select that task; shift-click to select a range. This header box selects/deselects every task currently shown by the filter.\nThe selection persists even if a later filter hides some selected rows -- a bulk action (the row meatball, ⋮) then applies only to what's still visible.",
        star: "Watched status (RAL-362 §5). Click a row's star to watch/unwatch its task.\nA task covered by a watch on its whole squad shows a distinct inherited-watch glyph.",
        name: "Task name. Click the row to select it; the chevron (when present) expands its cells.",
        squad: "The owning squad's OWN state -- not a roll-up of this row -- plus its label.\nClick to open the squad on the Squads tab; hover to highlight every other row from the same squad.\nUse this menu to group the whole table by squad.",
        cells: "Proportional breakdown of this task's cells by state, plus done/total count.",
        review: "Right-aligned Review/PR lane: the most-attention-needing review this task participates in (with a +N suffix for extra reviews), then its earliest-submitted PR, always last.\nA dashed placeholder means a review approved/merging with no PR submitted yet.",
        time: "Duration (live while running) or start time, per this menu's mode toggle. A dash means the task hasn't started.",
        tokens: "Input / output token totals, summed over this task's cells, cell proof steps, and task-scope proof steps.",
        cache: "Prompt-cache write / read token totals -- both are input-side figures, unlike Tokens' in/out split.",
        cost: "Total cost in USD, aggregated the same way as Tokens.\nA dash means no backend reported a cost for this task; a \"~\" prefix means at least one contributing figure is a mid-run estimate, not final accounting.",
        meatball: "Column options: sort, group-by (Squad column), duration/start-time (Time column), and show/hide any column.",
      };
      /**
       * Renders one header cell -- label (click sorts), sort caret, meatball menu trigger, resize handle.
       * @param {TaskTabColumn} c
       * @returns {string}
       */
      function ttHeadCellHtml(c) {
        if (c.key === "sel") {
          return `<div class="tt-head-cell" data-tip="${esc(TT_COLUMN_TIPS.sel)}"><input type="checkbox" class="tt-sel-all" onclick="event.stopPropagation();ttToggleSelectAllVisible(this.checked)" data-tip="${esc(TT_COLUMN_TIPS.sel)}"></div>`;
        }
        const isSort = taskTabFilters.sort === c.key;
        const label = c.sortable
          ? `<span onclick="ttSetSort('${c.key}')" style="cursor:pointer">${esc(c.label)}</span>`
          : `<span>${esc(c.label)}</span>`;
        const caret = c.sortable
          ? `<span class="tt-sort-caret ${isSort ? "active" : ""} ${isSort && taskTabFilters.dir === -1 ? "desc" : ""}" onclick="ttSetSort('${c.key}')">▲</span>`
          : "";
        const meatball = c.key !== "star" && c.key !== "meatball"
          ? `<span class="tt-meatball" onclick="event.stopPropagation();ttOpenColMenu(event,'${c.key}')" data-tip="${esc(TT_COLUMN_TIPS[c.key] || "")}">⋯</span>`
          : "";
        const resize = c.key !== "meatball"
          ? `<span class="tt-col-resize" data-col="${c.key}"></span>`
          : "";
        return `<div class="tt-head-cell ${c.sortable ? "sortable" : ""} align-${c.align}" data-tip="${esc(TT_COLUMN_TIPS[c.key] || "")}">${label}${caret}${meatball}${resize}</div>`;
      }
      /**
       * Sets the active sort key (from an initial click) or flips its direction (on a repeat click of the same key).
       * @param {string} key
       * @returns {void}
       */
      function ttSetSort(key) {
        if (!TASK_TAB_SORTS.includes(key)) return;
        if (taskTabFilters.sort === key) taskTabFilters.dir *= -1;
        else { taskTabFilters.sort = key; taskTabFilters.dir = 1; }
        saveTaskTabSort();
        syncHash();
        renderTasksTab();
      }
      /**
       * Sets the sort key/direction explicitly (from a column meatball menu item, as opposed to the caret's toggle).
       * @param {string} key
       * @param {number} dir
       * @returns {void}
       */
      function ttSetSortDir(key, dir) {
        taskTabFilters.sort = key;
        taskTabFilters.dir = dir;
        saveTaskTabSort();
        ttCloseColMenu();
        syncHash();
        renderTasksTab();
      }
      /**
       * Opens a column's meatball menu: sort shortcuts, column-specific options (group-by for Squad, duration/start-time for Time), and the Columns show/hide checklist shared by every column's menu.
       * @param {MouseEvent} e
       * @param {string} key
       * @returns {void}
       */
      function ttOpenColMenu(e, key) {
        e.preventDefault(); e.stopPropagation(); ttCloseColMenu();
        const col = TASK_TAB_COLUMNS.find((c) => c.key === key);
        if (!col) return;
        const rows = [];
        if (col.sortable) {
          rows.push(`<div onclick="ttSetSortDir('${key}',1)">↑ Sort ascending</div>`);
          rows.push(`<div onclick="ttSetSortDir('${key}',-1)">↓ Sort descending</div>`);
          rows.push(`<div class="ctx-sep"></div>`);
        }
        if (key === "squad") {
          rows.push(`<div class="ctx-check ${taskTabFilters.groupBySquad ? "on" : ""}" onclick="ttToggleGroupBy()">${taskTabFilters.groupBySquad ? "✓" : ""} Group by squad</div>`);
          rows.push(`<div class="ctx-sep"></div>`);
        }
        if (key === "time") {
          rows.push(`<div class="ctx-check ${taskTabTimeMode === "duration" ? "on" : ""}" onclick="ttSetTimeMode('duration')">${taskTabTimeMode === "duration" ? "✓" : ""} Duration</div>`);
          rows.push(`<div class="ctx-check ${taskTabTimeMode === "start" ? "on" : ""}" onclick="ttSetTimeMode('start')">${taskTabTimeMode === "start" ? "✓" : ""} Start time</div>`);
          rows.push(`<div class="ctx-sep"></div>`);
        }
        rows.push(`<div style="color:var(--muted);font-size:11px;padding:3px 10px 4px;text-transform:uppercase;letter-spacing:.5px">Columns</div>`);
        for (const c of TASK_TAB_COLUMNS.filter((c) => c.hideable)) {
          const on = !taskTabHiddenCols.has(c.key);
          rows.push(`<div class="ctx-check ${on ? "on" : ""}" ${c.key === key ? 'style="color:var(--accent)"' : ""} onclick="ttToggleColumn('${c.key}')">${on ? "✓" : ""} ${esc(c.label)}</div>`);
        }
        const menu = document.createElement("div");
        menu.className = "ctx-menu"; menu.id = "tt-col-menu";
        menu.innerHTML = rows.join("");
        document.body.appendChild(menu);
        const r = /** @type {HTMLElement} */ (e.currentTarget).getBoundingClientRect();
        menu.style.left = Math.min(r.left, window.innerWidth - 220) + "px";
        menu.style.top = Math.min(r.bottom + 4, window.innerHeight - 360) + "px";
      }
      /**
       * Closes the open column meatball menu, if any.
       * @returns {void}
       */
      function ttCloseColMenu() { const m = document.getElementById("tt-col-menu"); if (m) m.remove(); }
      document.addEventListener("click", ttCloseColMenu);
      /**
       * Toggles a hideable column's visibility -- callable from any column's meatball menu (RAL-362 §2).
       * @param {string} key
       * @returns {void}
       */
      function ttToggleColumn(key) {
        const col = TASK_TAB_COLUMNS.find((c) => c.key === key);
        if (!col || !col.hideable) return;
        if (taskTabHiddenCols.has(key)) taskTabHiddenCols.delete(key); else taskTabHiddenCols.add(key);
        saveTaskTabHiddenCols();
        ttCloseColMenu();
        renderTasksTab();
      }
      /**
       * Sets the Time column's display mode (from its own meatball menu).
       * @param {"duration"|"start"} mode
       * @returns {void}
       */
      function ttSetTimeMode(mode) {
        taskTabTimeMode = mode;
        saveTaskTabTimeMode();
        ttCloseColMenu();
        renderTasksTab();
      }
      /**
       * Toggles group-by-squad (from the Squad column's meatball menu) and scrolls the current selection back into view.
       * @returns {void}
       */
      function ttToggleGroupBy() {
        taskTabFilters.groupBySquad = !taskTabFilters.groupBySquad;
        saveTaskTabGroupBy();
        ttCloseColMenu();
        syncHash();
        renderTasksTab();
        ttScrollSelectionIntoView();
      }
      /** @type {{key: string, startX: number, startW: number, min: number}|null} the column-resize drag in progress, if any. */
      let ttColResizeDrag = null;
      /**
       * mousedown handler on a column's resize handle -- starts a drag tracked by `ttColResizeDrag`.
       * @param {MouseEvent} e
       * @returns {void}
       */
      function onTtColResizeDown(e) {
        e.preventDefault(); e.stopPropagation();
        const el = /** @type {HTMLElement} */ (e.currentTarget);
        const key = el.dataset.col || "";
        const col = TASK_TAB_COLUMNS.find((c) => c.key === key);
        if (!col) return;
        const startW = taskTabColWidths[key] || col.width;
        ttColResizeDrag = { key, startX: e.clientX, startW, min: col.min };
        el.classList.add("dragging");
        document.body.style.userSelect = "none";
        document.body.style.cursor = "col-resize";
        window.addEventListener("mousemove", onTtColResizeMove);
        window.addEventListener("mouseup", onTtColResizeUp);
      }
      /**
       * mousemove handler while a column resize drag is active.
       * @param {MouseEvent} e
       * @returns {void}
       */
      function onTtColResizeMove(e) {
        if (!ttColResizeDrag) return;
        const { key, startX, startW, min } = ttColResizeDrag;
        taskTabColWidths[key] = Math.max(min, startW + (e.clientX - startX));
        document.documentElement.style.setProperty("--tt-grid", taskTabGridTemplate(TASK_TAB_COLUMNS, taskTabHiddenCols, taskTabColWidths));
      }
      /**
       * Ends the active column-resize drag and persists the final width.
       * @returns {void}
       */
      function onTtColResizeUp() {
        if (!ttColResizeDrag) return;
        document.querySelectorAll(".tt-col-resize.dragging").forEach((el) => el.classList.remove("dragging"));
        document.body.style.userSelect = "";
        document.body.style.cursor = "";
        window.removeEventListener("mousemove", onTtColResizeMove);
        window.removeEventListener("mouseup", onTtColResizeUp);
        saveTaskTabColWidths();
        ttColResizeDrag = null;
      }
      /**
       * Rebuilds the header row and wires up its resize handles.
       * @returns {void}
       */
      function renderTtHead() {
        document.documentElement.style.setProperty("--tt-grid", taskTabGridTemplate(TASK_TAB_COLUMNS, taskTabHiddenCols, taskTabColWidths));
        const visible = TASK_TAB_COLUMNS.filter((c) => !taskTabHiddenCols.has(c.key));
        const head = byId("tt-head");
        head.innerHTML = visible.map((c) => ttHeadCellHtml(c)).join("");
        head.querySelectorAll(".tt-col-resize").forEach((el0) => {
          const el = /** @type {HTMLElement} */ (el0);
          el.addEventListener("mousedown", (/** @type {MouseEvent} */ e) => onTtColResizeDown(e));
        });
        // Reflects the "some but not all visible rows selected" state -- HTML has
        // no attribute for it, so it's set as a DOM property after the checkbox exists.
        const selAll = /** @type {HTMLInputElement|null} */ (head.querySelector(".tt-sel-all"));
        if (selAll) {
          const { checked, indeterminate } = ttSelectAllState();
          selAll.checked = checked;
          selAll.indeterminate = indeterminate;
        }
      }
      /**
       * Renders one column's cell wrapper, or "" when that column is currently hidden (so the emitted grid-cell count matches the header's).
       * @param {string} key
       * @param {string} html
       * @param {"left"|"right"} [forceAlign]
       * @returns {string}
       */
      function ttColCell(key, html, forceAlign) {
        if (taskTabHiddenCols.has(key)) return "";
        const col = TASK_TAB_COLUMNS.find((c) => c.key === key);
        const align = forceAlign || (col && col.align === "right" ? "right" : "left");
        return `<div class="tt-cell ${align === "right" ? "align-right" : ""}">${html}</div>`;
      }
      // RALPHUS-TT-MULTISELECT:BEGIN
      /**
       * The `TtRow.key`s of every task row in the current filtered/sorted/
       * grouped display list, in display order (RAL-350) -- "visible" for the
       * double-filter rule: a bulk action or the header select-all/none only
       * ever reaches rows in this list, never rows the underlying `ttSel`
       * remembers but a filter is currently hiding.
       * @returns {string[]}
       */
      function ttVisibleTaskKeys() {
        return ttDisplayItems.filter((it) => it.type === "task").map((it) => /** @type {TtRow} */ (it.row).key);
      }
      /**
       * The header select-all checkbox's checked/indeterminate state, derived
       * from how many of the currently visible task rows are in `ttSel`.
       * @returns {{checked: boolean, indeterminate: boolean}}
       */
      function ttSelectAllState() {
        const keys = ttVisibleTaskKeys();
        if (!keys.length) return { checked: false, indeterminate: false };
        const n = keys.filter((k) => ttSel.has(k)).length;
        return { checked: n === keys.length, indeterminate: n > 0 && n < keys.length };
      }
      /**
       * Selects or deselects every currently visible task row at once (the
       * header checkbox) -- never touches a selected row the current filter
       * is hiding (RAL-350's double filter).
       * @param {boolean} checked
       * @returns {void}
       */
      function ttToggleSelectAllVisible(checked) {
        for (const k of ttVisibleTaskKeys()) { if (checked) ttSel.add(k); else ttSel.delete(k); }
        ttSelAnchor = null;
        renderTasksTab();
      }
      /**
       * Handles a click on one row's own selection checkbox: plain click
       * toggles just that row and becomes the new shift-range anchor;
       * shift-click selects the range between the anchor and this row among
       * the currently visible task rows (RAL-350, mirrors `onSquadClick`).
       * @param {MouseEvent} e
       * @param {string} squadId
       * @param {number} taskIdx
       * @returns {void}
       */
      function ttToggleRowSel(e, squadId, taskIdx) {
        const key = `${squadId}:${taskIdx}`;
        if (e.shiftKey && ttSelAnchor) {
          const vis = ttVisibleTaskKeys();
          const a = vis.indexOf(ttSelAnchor), b = vis.indexOf(key);
          if (a >= 0 && b >= 0) {
            const lo = Math.min(a, b), hi = Math.max(a, b);
            for (let i = lo; i <= hi; i++) ttSel.add(vis[i]);
          } else if (ttSel.has(key)) { ttSel.delete(key); } else { ttSel.add(key); }
        } else {
          if (ttSel.has(key)) ttSel.delete(key); else ttSel.add(key);
          ttSelAnchor = key;
        }
        renderTasksTab();
      }
      /**
       * Resolves the row set a Tasks-tab bulk action (the row meatball menu)
       * should apply to (RAL-350): every selected row that's also currently
       * visible, when the clicked row is part of a multi-row selection;
       * otherwise just the clicked row alone, same as before multi-select
       * existed. This is the double-filter rule -- a filter narrows what a
       * bulk action reaches without ever mutating `ttSel` itself.
       * @param {string} key
       * @returns {TtRow[]}
       */
      function ttVisibleSelectedRows(key) {
        if (!ttSel.has(key) || ttSel.size <= 1) {
          const row = ttAllRows.find((r) => r.key === key);
          return row ? [row] : [];
        }
        const visibleKeys = new Set(ttVisibleTaskKeys());
        return ttAllRows.filter((r) => ttSel.has(r.key) && visibleKeys.has(r.key));
      }
      // RALPHUS-TT-MULTISELECT:END
      /**
       * Renders the squad chip: the squad's OWN state pip (never a roll-up of this row) plus its label.
       * @param {TtRow} row
       * @returns {string}
       */
      function ttSquadChipHtml(row) {
        return `<span class="tt-squad-chip" onclick="event.stopPropagation();ttOpenSquadInSquadsTab('${esc(row.squadId)}')" data-tip="Squad ${esc(row.squadLabel || row.squadId)} — ${esc(row.squadState)} (this squad's own state, not a roll-up of this task).\nClick to open on the Squads tab; hover to highlight this squad's other rows.">${sdot(row.squadState)}<span class="name">${esc(row.squadLabel || row.squadId)}</span></span>`;
      }
      /**
       * Opens a squad on the Squads tab.
       * @param {string} squadId
       * @returns {void}
       */
      function ttOpenSquadInSquadsTab(squadId) {
        selectSquad(squadId);
        showTab("squads", true);
      }
      /**
       * Highlights (or un-highlights) every currently-rendered row sharing `squadId`, for the squad chip's hover relationship tint.
       * @param {string} squadId
       * @param {boolean} on
       * @returns {void}
       */
      function ttHoverSquad(squadId, on) {
        document.querySelectorAll("#tt-body .tt-row").forEach((el) => {
          const row = /** @type {HTMLElement} */ (el);
          if (row.dataset.squadId === squadId) row.classList.toggle("tt-squad-hover", on);
        });
      }
      /**
       * Renders the Cells column: a segmented proportional bar by state, plus done/total.
       * @param {CellView[]} cells
       * @returns {string}
       */
      function ttCellsBarHtml(cells) {
        if (!cells.length) return `<span class="tt-cells-count">–</span>`;
        /** @type {{[state: string]: number}} */
        const counts = {};
        for (const c of cells) counts[c.state] = (counts[c.state] || 0) + 1;
        const segs = STATES.filter((s) => counts[s]).map((s) => `<span style="flex:${counts[s]};background:${cvar("--" + safeState(s))}" data-tip="${counts[s]} ${esc(s)}"></span>`).join("");
        const done = cells.filter((c) => c.state === "done").length;
        return `<span class="tt-cells-bar"><span class="tt-cells-progress">${segs}</span><span class="tt-cells-count">${done}/${cells.length}</span></span>`;
      }
      /**
       * Renders the right-aligned Review/PR badge lane: at most a review badge then a PR badge (or dashed placeholder), PR always last.
       * @param {{review: TtReview, count: number}|null} reviewBadge
       * @param {{pr: PrIndexRow, count: number}|null} prPick
       * @returns {string}
       */
      // RALPHUS-TT-REVIEW-BADGES:BEGIN
      function ttReviewPrBadgesHtml(reviewBadge, prPick) {
        const parts = [];
        if (reviewBadge) {
          const r = reviewBadge.review;
          const color = cvar(G_COLORS[r.status] || "--muted");
          const shortName = r.name.length > 14 ? r.name.slice(0, 13) + "…" : r.name;
          // RAL-382: the badge body opens the review it displays (the attention-ranked
          // pick, per the ticket's interview decision); the +N count is a separate
          // affordance that jumps to the Reviews tab's full list instead of silently
          // picking one of the other reviews. data-click (not inline onclick) so the
          // delegation engine can stop propagation before the row's ttSelectTask fires.
          const extra = reviewBadge.count > 1 ? `<span class="tt-badge review-extra" data-click="openReviewList" data-tip="This task is on ${reviewBadge.count} reviews — the badge shows the one needing attention most.\nClick to open the full list on the Reviews tab and pick one.">+${reviewBadge.count - 1}</span>` : "";
          parts.push(`<span class="tt-badge review-badge" style="color:${color};border-color:${color}" data-click="gotoReview" data-guardian-id="${esc(r.id)}" data-tip="Review &quot;${esc(r.name)}&quot; — ${esc(r.status)}.${reviewBadge.count > 1 ? ` This task is on ${reviewBadge.count} reviews total; +N opens the full list.` : ""}\nClick to open this review on the Reviews tab.">${esc(shortName)}</span>${extra}`);
        }
        if (prPick) {
          const pr = prPick.pr;
          const color = cvar(ttPrColorVar(pr));
          const label = pr.pr_number ? `#${pr.pr_number}` : "PR";
          const extra = prPick.count > 1 ? ` +${prPick.count - 1}` : "";
          const ciNote = pr.state === "open" && pr.ci_status ? ` (CI: ${esc(pr.ci_status)})` : "";
          parts.push(`<span class="tt-badge pr-badge" style="color:${color};border-color:${color}" onclick="event.stopPropagation();ttOpenPr('${esc(pr.pr_url || "")}')" data-tip="PR ${esc(label)} — ${esc(pr.state)} on ${esc(pr.forge)}/${esc(pr.repo)}${ciNote}, earliest-submitted for this task.${prPick.count > 1 ? ` +${prPick.count - 1} more PR(s) on this task.` : ""}\nClick to open on the forge.">${esc(label)}${extra}</span>`);
        } else if (reviewBadge && (reviewBadge.review.status === "approved" || reviewBadge.review.status === "merging")) {
          parts.push(`<span class="tt-badge pr-placeholder" data-tip="Review &quot;${esc(reviewBadge.review.name)}&quot; is ${esc(reviewBadge.review.status)} but has no PR submitted yet.">no PR</span>`);
        }
        return `<span class="tt-badges">${parts.join("")}</span>`;
      }
      /**
       * Opens a PR's forge URL in a new tab.
       * @param {string} url
       * @returns {void}
       */
      function ttOpenPr(url) { if (url) window.open(url, "_blank", "noopener"); }
      // RALPHUS-TT-REVIEW-BADGES:END
      /**
       * Renders the Time column per the current duration/start-time mode; the tooltip always shows both.
       * @param {number|null} startedAtMs
       * @param {number|null} finishedAtMs
       * @param {string} state
       * @returns {string}
       */
      function ttTimeCellHtml(startedAtMs, finishedAtMs, state) {
        if (state === "queued" || state === "ignored") return `<span data-tip="Not started yet.">–</span>`;
        if (!startedAtMs) return `<span data-tip="Not started yet.">–</span>`;
        const durMs = (finishedAtMs ?? Date.now()) - startedAtMs;
        const tip = `Started ${fmtActivityTime(startedAtMs)} (local).\nDuration: ${fmtDuration(durMs)}.`;
        if (taskTabTimeMode === "start") return `<span data-tip="${esc(tip)}">${fmtRelativeAge(Date.now() - startedAtMs)} ago</span>`;
        const running = state === "running" && finishedAtMs == null;
        return `<span class="tt-time ${running ? "running" : ""}" ${running ? `data-running="1" data-started="${startedAtMs}"` : ""} data-tip="${esc(tip)}">${fmtDuration(durMs)}</span>`;
      }
      /**
       * Renders the Cost column with a per-contributor tooltip breakdown.
       * @param {TtUsage} usage
       * @param {TtUsageItem[]} items
       * @returns {string}
       */
      function ttCostCellHtml(usage, items) {
        if (!usage.anyCost) return `<span data-tip="No backend reported a cost for this task's cells/proof steps.">–</span>`;
        const parts = items.filter((it) => it.cost_usd).map((it) => `${it.cost_is_estimated ? "~" : ""}$${/** @type {number} */ (it.cost_usd).toFixed(2)}`);
        const tip = `${usage.estimated ? "Estimated — includes at least one mid-run snapshot rather than a backend's final accounting.\n" : ""}Sum over ${parts.length} contributing cell/proof step(s): ${parts.join(", ")}.`;
        return `<span data-tip="${esc(tip)}">${ttFmtCost(usage)}</span>`;
      }
      /**
       * Renders one virtualized task row, absolutely positioned at `top`.
       * @param {TtRow} row
       * @param {number} top
       * @returns {string}
       */
      function ttTaskRowHtml(row, top) {
        const hasCells = row.cells.length > 0;
        const expanded = taskTabExpanded.has(row.key);
        const multiSelected = ttSel.has(row.key);
        const selected = (taskTabSel.kind === "task" && taskTabSel.squadId === row.squadId && taskTabSel.taskIdx === row.taskIdx) || multiSelected;
        const chevron = hasCells
          ? `<span class="tt-chevron ${expanded ? "expanded" : ""}" onclick="event.stopPropagation();ttToggleExpand('${esc(row.key)}')" data-tip="Expand to show this task's cells.">▶</span>`
          : `<span class="tt-chevron hidden-chevron">▶</span>`;
        const selCb = `<input type="checkbox" class="tt-row-sel" ${multiSelected ? "checked" : ""} onclick="event.stopPropagation();ttToggleRowSel(event,'${esc(row.squadId)}',${row.taskIdx})" data-tip="Select this task for bulk actions -- the row meatball (⋮) then applies to every selected task.\nShift-click to select a range.">`;
        const starCls = row.watch.watched ? `watched ${row.watch.inherited ? "inherited" : ""}` : "";
        const starTip = !row.watch.watched
          ? "Not watched. Click to watch — \"needs me\" only ever surfaces watched work."
          : row.watch.inherited
            ? "Watched via this task's squad. Click to mute this task only — the squad watch stays in effect for its other tasks."
            : "Watching this task. Click to unwatch.";
        const star = `<span class="tt-star ${starCls}" onclick="event.stopPropagation();ttToggleWatch('${esc(row.squadId)}',${row.taskIdx})" data-tip="${esc(starTip)}">${row.watch.inherited ? "◆" : "★"}</span>`;
        const needsDot = row.needsMe ? `<span style="color:var(--accent)" data-tip="${esc(row.needsMeReason || "")}">●</span>` : "";
        return `<div class="tt-row ${selected ? "selected" : ""}" style="top:${top}px;height:${TT_ROW_H}px" data-squad-id="${esc(row.squadId)}" onclick="ttSelectTask('${esc(row.squadId)}',${row.taskIdx})" onmouseenter="ttHoverSquad('${esc(row.squadId)}',true)" onmouseleave="ttHoverSquad('${esc(row.squadId)}',false)">`
          + ttColCell("sel", selCb)
          + ttColCell("star", star)
          + ttColCell("name", `<span class="tt-name-cell">${chevron}${sdot(row.state)}<span class="tt-name-text" data-tip="${esc(row.name)}">${esc(row.name)}</span>${needsDot}</span>`)
          + ttColCell("squad", ttSquadChipHtml(row))
          + ttColCell("cells", ttCellsBarHtml(row.cells))
          + ttColCell("review", ttReviewPrBadgesHtml(row.reviewBadge, row.prPick))
          + ttColCell("time", ttTimeCellHtml(row.startedAtMs, row.finishedAtMs, row.state))
          + ttColCell("tokens", ttFmtTokens(row.usage))
          + ttColCell("cache", ttFmtCache(row.usage))
          + ttColCell("cost", ttCostCellHtml(row.usage, ttTaskUsageItems(row.task)))
          + ttColCell("meatball", (() => {
            const menuRowCount = multiSelected && ttSel.size > 1 ? ttVisibleSelectedRows(row.key).length : 1;
            const tip = `Task actions (restart, set status, stop, hide its squad) — the only place task actions live on this tab.${menuRowCount > 1 ? ` Applies to all ${menuRowCount} currently visible selected tasks (RAL-350).` : ""}`;
            return `<span class="tt-meatball" onclick="event.stopPropagation();ttOpenRowMenu(event,'${esc(row.squadId)}',${row.taskIdx})" data-tip="${esc(tip)}">⋯</span>`;
          })())
          + `</div>`;
      }
      /**
       * Renders one virtualized expanded-cell sub-row, absolutely positioned at `top` -- re-purposes Squad/Cells/Review columns for agent-model/proof-pips/branch (RAL-362 §4).
       * @param {TtRow} row
       * @param {CellView} cell
       * @param {number} top
       * @returns {string}
       */
      function ttCellRowHtml(row, cell, top) {
        const cellIdx = row.cells.indexOf(cell);
        const selected = taskTabSel.kind === "cell" && taskTabSel.squadId === row.squadId && taskTabSel.taskIdx === row.taskIdx && taskTabSel.cellIdx === cellIdx;
        const cu = ttCellUsage(cell);
        const proofPips = (cell.proof || []).map((v) => `<span data-tip="${esc(v.kind)}: ${esc(v.state)}">${sdot(v.state)}</span>`).join("") || `<span class="tt-cells-count">–</span>`;
        const branch = cell.reviews && cell.reviews.length ? cell.reviews.map((r) => esc(r.branch || "")).filter(Boolean).join(", ") : "–";
        return `<div class="tt-row tt-cell-row ${selected ? "selected" : ""}" style="top:${top}px;height:${TT_ROW_H}px" data-squad-id="${esc(row.squadId)}" onclick="event.stopPropagation();ttSelectCell('${esc(row.squadId)}',${row.taskIdx},${cellIdx})">`
          + ttColCell("sel", "")
          + ttColCell("star", "")
          + ttColCell("name", `<span class="tt-name-cell" style="padding-left:20px">${sdot(cell.state)}<span class="tt-name-text" data-tip="${esc(cell.name || cell.id)}">${esc(cell.name || cell.id)}</span></span>`)
          + ttColCell("squad", cell.model ? `<span data-tip="Agent / model for this cell.">${esc(cell.agent)} / ${esc(cell.model)}</span>` : `<span data-tip="Agent for this cell.">${esc(cell.agent)}</span>`)
          + ttColCell("cells", proofPips)
          + ttColCell("review", `<span data-tip="Review branch(es) this cell submitted under.">${branch}</span>`)
          + ttColCell("time", ttTimeCellHtml(cell.started_at_ms ?? null, cell.finished_at_ms ?? null, cell.state))
          + ttColCell("tokens", ttFmtTokens(cu))
          + ttColCell("cache", ttFmtCache(cu))
          + ttColCell("cost", ttCostCellHtml(cu, ttCellUsageItems(cell)))
          + ttColCell("meatball", "")
          + `</div>`;
      }
      /**
       * Renders one virtualized group-by-squad header, absolutely positioned at `top` -- its Tokens/Cache/Cost cells aggregate only the rows visible in this group (post-filter), with the unfiltered total noted in the tooltip when a filter is active.
       * @param {string} squadId
       * @param {TtRow[]} groupRows
       * @param {number} top
       * @returns {string}
       */
      function ttGroupHeaderHtml(squadId, groupRows, top) {
        const squad = findSquad(squadId);
        const label = squad ? (squad.label || squad.id) : squadId;
        const squadState = squad ? squadDisplayState(squad) : "pending";
        const agg = ttGroupAggregate(groupRows);
        const allForSquad = ttAllRows.filter((r) => r.squadId === squadId);
        const filteredOut = allForSquad.length !== groupRows.length;
        /**
         * @param {string} formatted
         * @returns {string}
         */
        const tip = (formatted) => filteredOut
          ? `${formatted} across the ${groupRows.length} tasks shown in this group (of ${allForSquad.length} total in the squad; a filter is hiding the rest).`
          : `${formatted} across all ${groupRows.length} tasks in this squad.`;
        return `<div class="tt-group-header" style="top:${top}px;height:${TT_ROW_H}px" data-squad-id="${esc(squadId)}">`
          + ttColCell("sel", "")
          + ttColCell("star", "")
          + ttColCell("name", `<span class="tt-group-label" onclick="ttOpenSquadInSquadsTab('${esc(squadId)}')" data-tip="Squad ${esc(label)} — click to open on the Squads tab.">${sdot(squadState)} ${esc(label)} <span style="color:var(--muted);font-weight:400">(${groupRows.length})</span></span>`)
          + ttColCell("squad", "")
          + ttColCell("cells", "")
          + ttColCell("review", "")
          + ttColCell("time", "")
          + ttColCell("tokens", `<span data-tip="${esc(tip(ttFmtTokens(agg)))}">${ttFmtTokens(agg)}</span>`)
          + ttColCell("cache", `<span data-tip="${esc(tip(ttFmtCache(agg)))}">${ttFmtCache(agg)}</span>`)
          + ttColCell("cost", `<span data-tip="${esc(tip(ttFmtCost(agg)))}">${agg.anyCost ? (agg.estimated ? "~" : "") + "$" + agg.cost.toFixed(2) : "–"}</span>`)
          + ttColCell("meatball", "")
          + `</div>`;
      }
      /**
       * Sizes `#tt-body` to the full virtual content height and renders the current visible window.
       * @returns {void}
       */
      function renderTtBody() {
        byId("tt-body").style.height = (ttDisplayItems.length * TT_ROW_H) + "px";
        ttRenderVisibleRows();
        byId("tt-table-wrap").onscroll = ttRenderVisibleRows;
      }
      /**
       * Renders only the rows within the current scrolled viewport (plus overscan) -- the virtualization step run on every scroll/data refresh.
       * @returns {void}
       */
      function ttRenderVisibleRows() {
        const wrap = byId("tt-table-wrap");
        const head = byId("tt-head");
        const bodyScrollTop = Math.max(0, wrap.scrollTop - head.offsetHeight);
        const { first, last } = ttVisibleRange(bodyScrollTop, wrap.clientHeight, TT_ROW_H, ttDisplayItems.length, TT_OVERSCAN);
        const parts = [];
        for (let i = first; i < last; i++) {
          const item = ttDisplayItems[i];
          const top = i * TT_ROW_H;
          if (item.type === "group") parts.push(ttGroupHeaderHtml(/** @type {string} */ (item.squadId), /** @type {TtRow[]} */ (item.rows), top));
          else if (item.type === "cell") parts.push(ttCellRowHtml(/** @type {TtRow} */ (item.row), /** @type {CellView} */ (item.cell), top));
          else parts.push(ttTaskRowHtml(/** @type {TtRow} */ (item.row), top));
        }
        byId("tt-body").innerHTML = parts.join("");
      }
      /**
       * Toggles a task row's expanded (cells-visible) state.
       * @param {string} key
       * @returns {void}
       */
      function ttToggleExpand(key) {
        if (taskTabExpanded.has(key)) taskTabExpanded.delete(key); else taskTabExpanded.add(key);
        renderTasksTab();
      }
      /**
       * Expands every currently-filtered task row that has cells.
       * @returns {void}
       */
      function ttExpandAll() {
        for (const item of ttDisplayItems) if (item.type === "task" && /** @type {TtRow} */ (item.row).cells.length) taskTabExpanded.add(/** @type {TtRow} */ (item.row).key);
        renderTasksTab();
      }
      /**
       * Collapses every currently-filtered task row.
       * @returns {void}
       */
      function ttCollapseAll() {
        for (const item of ttDisplayItems) if (item.type === "task") taskTabExpanded.delete(/** @type {TtRow} */ (item.row).key);
        renderTasksTab();
      }
      // RALPHUS-TT-FILTER-SELECTION-SCROLL:BEGIN
      /**
       * Updates the name filter (RAL-362 §2). Mirrors `onFilter`'s squads-tab pattern -- mutate + re-render the list, without rebuilding the toolbar itself (which would fight the user's typing). Re-centers the retained selection (RAL-383) now that filtering may have changed its position or visibility.
       * @param {string} v
       * @returns {void}
       */
      function ttSetNameFilter(v) { taskTabFilters.q = v.toLowerCase(); renderTasksTab(); ttScrollSelectionIntoView(); syncHash(); }
      /**
       * Toggles the "show hidden" (include tasks of hidden squads) filter. Re-centers the retained selection (RAL-383).
       * @param {boolean} on
       * @returns {void}
       */
      function ttToggleShowHidden(on) { taskTabFilters.showHidden = on; renderTasksTab(); ttScrollSelectionIntoView(); syncHash(); }
      /**
       * Toggles the "needs me" filter. Re-centers the retained selection (RAL-383).
       * @param {boolean} on
       * @returns {void}
       */
      function ttToggleNeedsMe(on) { taskTabFilters.needsMe = on; renderTasksTab(); ttScrollSelectionIntoView(); syncHash(); }
      /**
       * Shows or hides one status in the toolbar's status filter. Re-centers the retained selection (RAL-383).
       * @param {string} s
       * @param {boolean} on
       * @returns {void}
       */
      function ttToggleStatus(s, on) { if (on) taskTabFilters.status.add(s); else taskTabFilters.status.delete(s); renderTasksTab(); ttScrollSelectionIntoView(); syncHash(); }
      /**
       * Shows or hides every status at once (the toolbar's all/none shortcuts) -- rebuilds the checkbox list since every box's checked state changes at once. Re-centers the retained selection (RAL-383).
       * @param {boolean} on
       * @returns {void}
       */
      function ttAllStatus(on) { taskTabFilters.status = on ? new Set(STATES) : new Set(); renderTtStatusFilters(); renderTasksTab(); ttScrollSelectionIntoView(); syncHash(); }
      // RALPHUS-TT-FILTER-SELECTION-SCROLL:END
      /**
       * Renders the toolbar's per-state checkboxes and syncs the filter input/checkboxes to `taskTabFilters` -- called on load and whenever filters are reset wholesale, never on every poll (which would fight the user's typing/checking).
       * @returns {void}
       */
      function renderTtStatusFilters() {
        const el = byId("tt-status-filters");
        el.innerHTML = STATES.map((s) => `<label data-tip="Show or hide ${s} tasks.">${sdot(s)}<input type="checkbox" ${taskTabFilters.status.has(s) ? "checked" : ""} data-state="${esc(s)}" onchange="ttToggleStatus(this.dataset.state,this.checked)">${s}</label>`).join("")
          + `<span class="chip" onclick="ttAllStatus(true)" data-tip="Show tasks of every status.">all</span><span class="chip" onclick="ttAllStatus(false)" data-tip="Hide all tasks — clear the status filter entirely.">none</span>`;
        /** @type {HTMLInputElement} */ (byId("tt-filter")).value = taskTabFilters.q;
        /** @type {HTMLInputElement} */ (byId("tt-show-hidden")).checked = taskTabFilters.showHidden;
        /** @type {HTMLInputElement} */ (byId("tt-needs-me")).checked = taskTabFilters.needsMe;
      }
      /**
       * Watches/unwatches/mutes a task's star (RAL-362 §5): explicit watch/unwatch round-trips through `/api/watches`; clicking an inherited (squad-covered) watch is a client-only mute/unmute since the daemon has no "exception to a cascade" of its own.
       * @param {string} squadId
       * @param {number} taskIdx
       * @returns {Promise<void>}
       */
      async function ttToggleWatch(squadId, taskIdx) {
        const uri = ttTaskEntityUri(squadId, taskIdx);
        const squadUri = ttSquadEntityUri(squadId);
        const hasExplicit = taskTabWatches.some((w) => w.entity_uri === uri);
        const hasSquadWatch = taskTabWatches.some((w) => w.entity_uri === squadUri);
        const isMuted = taskTabMutedTasks.has(uri);
        if (hasSquadWatch && isMuted) { taskTabMutedTasks.delete(uri); saveTaskTabMuted(); renderTasksTab(); return; }
        if (hasSquadWatch && !isMuted) { taskTabMutedTasks.add(uri); saveTaskTabMuted(); renderTasksTab(); return; }
        const qs = currentUserName ? `?user=${encodeURIComponent(currentUserName)}` : "";
        if (hasExplicit) {
          let resp;
          try { resp = await del(`/api/watches/${encodeURIComponent(uri)}${qs}`); } catch (e) { alert("daemon unreachable"); return; }
          if (!resp.ok) { alert(await responseError(resp, "unwatch failed")); return; }
          taskTabWatches = taskTabWatches.filter((w) => w.entity_uri !== uri);
        } else {
          let resp;
          try { resp = await post(`/api/watches${qs}`, { entity_uri: uri }); } catch (e) { alert("daemon unreachable"); return; }
          if (!resp.ok) { alert(await responseError(resp, "watch failed")); return; }
          taskTabWatches.push(await resp.json());
        }
        renderTasksTab();
      }
      /**
       * Watches/unwatches/mutes a cell's star, one level down from {@link ttToggleWatch}: explicit watch/unwatch round-trips through `/api/watches`; clicking a watch inherited from the cell's owning task (itself possibly inherited from the squad) is a client-only mute/unmute, same as a task inheriting from its squad.
       * @param {string} squadId
       * @param {number} taskIdx
       * @param {number} cellIdx
       * @returns {Promise<void>}
       */
      async function ttToggleCellWatch(squadId, taskIdx, cellIdx) {
        const uri = ttCellEntityUri(squadId, taskIdx, cellIdx);
        const hasExplicit = taskTabWatches.some((w) => w.entity_uri === uri);
        const hasParentWatch = ttEffectiveWatch(taskTabWatches, taskTabMutedTasks, squadId, taskIdx).watched;
        const isMuted = taskTabMutedTasks.has(uri);
        if (hasParentWatch && isMuted) { taskTabMutedTasks.delete(uri); saveTaskTabMuted(); renderTasksTab(); return; }
        if (hasParentWatch && !isMuted) { taskTabMutedTasks.add(uri); saveTaskTabMuted(); renderTasksTab(); return; }
        const qs = currentUserName ? `?user=${encodeURIComponent(currentUserName)}` : "";
        if (hasExplicit) {
          let resp;
          try { resp = await del(`/api/watches/${encodeURIComponent(uri)}${qs}`); } catch (e) { alert("daemon unreachable"); return; }
          if (!resp.ok) { alert(await responseError(resp, "unwatch failed")); return; }
          taskTabWatches = taskTabWatches.filter((w) => w.entity_uri !== uri);
        } else {
          let resp;
          try { resp = await post(`/api/watches${qs}`, { entity_uri: uri }); } catch (e) { alert("daemon unreachable"); return; }
          if (!resp.ok) { alert(await responseError(resp, "watch failed")); return; }
          taskTabWatches.push(await resp.json());
        }
        renderTasksTab();
      }
      /**
       * Selects a task for the details pane and re-renders.
       * @param {string} squadId
       * @param {number} taskIdx
       * @returns {void}
       */
      function ttSelectTask(squadId, taskIdx) {
        taskTabSel = { kind: "task", squadId, taskIdx, cellIdx: -1 };
        syncHash(true);
        renderTasksTab();
      }
      /**
       * Selects a cell (within its task) for the details pane.
       * @param {string} squadId
       * @param {number} taskIdx
       * @param {number} cellIdx
       * @returns {void}
       */
      function ttSelectCell(squadId, taskIdx, cellIdx) {
        taskTabSel = { kind: "cell", squadId, taskIdx, cellIdx };
        syncHash(true);
        ttRenderVisibleRows();
        renderTaskDetailsPane();
      }
      /**
       * Runs the on-demand PR drift (git) + un-actioned-feedback (forge API) check for the selected task's picked PR (RAL-362 §5/§6) -- one PR, user-initiated, never part of the row predicate.
       * @param {string} squadId
       * @param {number} taskIdx
       * @returns {Promise<void>}
       */
      async function ttRunPrCheck(squadId, taskIdx) {
        const row = ttAllRows.find((r) => r.squadId === squadId && r.taskIdx === taskIdx);
        const pr = row && row.prPick ? row.prPick.pr : null;
        if (!pr) return;
        taskTabPrCheckFor = { squadId, taskIdx };
        taskTabPrCheckResult = { loading: true };
        renderTaskDetailsPane();
        try {
          await fetchPrSyncStatus(pr.id);
          let comments = [];
          if (pr.pr_number != null) {
            const res = await fetch(`/api/pull-requests/${pr.id}/comments`);
            if (res.ok) comments = await res.json();
          }
          taskTabPrCheckResult = { loading: false, sync: prSyncStatus[pr.id], comments };
        } catch (e) {
          taskTabPrCheckResult = { loading: false, error: "check failed — daemon or forge unreachable" };
        }
        renderTaskDetailsPane();
      }
      /**
       * Opens the row meatball menu (RAL-362 §3: "the only place task actions
       * live") -- reuses the Squads tab's own task graph-node menu
       * (restart/stop/status/solo) verbatim, plus Tasks-tab-only shortcuts
       * (open on Squads tab, hide/unhide the squad). RAL-350: when the
       * clicked row is part of the current multi-selection, every action
       * (including the graph-node menu's own restart/stop/status/solo)
       * applies to every selected row still visible under the current
       * filter (`ttVisibleSelectedRows`) instead of just this one --
       * except "Open in Squads tab", which stays single-target only and is
       * shown disabled with an explanatory tooltip rather than hidden.
       * @param {MouseEvent} e
       * @param {string} squadId
       * @param {number} taskIdx
       * @returns {void}
       */
      function ttOpenRowMenu(e, squadId, taskIdx) {
        ttCloseColMenu();
        const key = `${squadId}:${taskIdx}`;
        const rows = ttVisibleSelectedRows(key);
        if (!rows.length) return;
        const items = /** @type {GraphNodeSelectionItem[]} */ (rows.map((r) => graphNodeItem(r.squadId, "task", r.taskIdx, -1, -1)).filter(Boolean));
        if (!items.length) return;
        openGraphNodeMenu(e, items);
        const menu = document.getElementById("graph-node-menu");
        if (!menu) return;
        const multi = rows.length > 1;
        _ttRowMenuSquadIds = [...new Set(rows.map((r) => r.squadId))];
        const openRow = multi
          ? `<span data-tip="Open in Squads tab is single-target only -- it can't jump to more than one task's place in the dependency graph at once. Select just this one task to use it.">`
            + `<div style="opacity:.5;cursor:not-allowed;pointer-events:none">↗ Open in Squads tab</div></span>`
          : `<div onclick="ttOpenSquadInSquadsTab('${esc(squadId)}')" data-tip="Open this task on the Squads tab, where the full dependency graph and every task action live.">↗ Open in Squads tab</div>`;
        const squadWord = _ttRowMenuSquadIds.length === 1 ? "squad" : "squads";
        const hideRow = multi
          ? `<div onclick="ttHideSquads(true)" data-tip="Hide the ${squadWord} of all ${rows.length} selected tasks from your own view.\nHiding is squad-level only -- there is no per-task hiding yet.">🙈 Hide ${_ttRowMenuSquadIds.length} ${squadWord}</div>`
            + `<div onclick="ttHideSquads(false)" data-tip="Re-enable the ${squadWord} of all ${rows.length} selected tasks in your own view.">🙉 Unhide ${_ttRowMenuSquadIds.length} ${squadWord}</div>`
          : hiddenSquadIds.has(squadId)
            ? `<div onclick="ttHideSquads(false)" data-tip="Re-enable this task's squad in your own view (Squads tab).">🙉 Unhide squad</div>`
            : `<div onclick="ttHideSquads(true)" data-tip="Hide this task's whole squad from your own view.\nHiding is squad-level only -- there is no per-task hiding yet.">🙈 Hide squad</div>`;
        menu.insertAdjacentHTML("beforeend", `<div class="ctx-sep"></div>${openRow}${hideRow}`);
      }
      /**
       * Hides/unhides every squad captured by the currently open Tasks-tab
       * row meatball menu (`_ttRowMenuSquadIds`, RAL-350) -- one squad for a
       * single-row menu, or the deduplicated squads of every selected,
       * visible row for a multi-row menu. Hiding stays squad-level only
       * (RAL-331); there is no per-task hiding.
       * @param {boolean} hide
       * @returns {Promise<void>}
       */
      async function ttHideSquads(hide) {
        ttCloseColMenu();
        closeGraphMenu();
        const squadIds = _ttRowMenuSquadIds;
        if (!squadIds.length) return;
        for (const squadId of squadIds) {
          let resp;
          try { resp = await (hide ? post(`/api/hidden/squads/${squadId}`) : del(`/api/hidden/squads/${squadId}`)); } catch (e) { alert("daemon unreachable"); return; }
          if (!resp.ok) { alert(await responseError(resp, hide ? "hide failed" : "unhide failed")); return; }
          if (hide) hiddenSquadIds.add(squadId); else hiddenSquadIds.delete(squadId);
        }
        renderTasksTab();
      }
      // RALPHUS-TT-SCROLL-SELECTION:BEGIN
      /**
       * Scrolls the currently-selected row into view by its computed offset in `ttDisplayItems` (RAL-362 §7, RAL-383) -- under virtualization the selected row may not be mounted, so this computes its position rather than querying the DOM for it. A no-op when the selection isn't (or is no longer, post-filter) present in `ttDisplayItems`.
       * @returns {void}
       */
      function ttScrollSelectionIntoView() {
        if (taskTabSel.kind !== "task" && taskTabSel.kind !== "cell") return;
        const idx = ttDisplayItems.findIndex((it) => {
          if (it.type === "group" || !it.row) return false;
          if (it.row.squadId !== taskTabSel.squadId || it.row.taskIdx !== taskTabSel.taskIdx) return false;
          return it.type === "task" ? taskTabSel.kind === "task" : it.cell === it.row.cells[taskTabSel.cellIdx];
        });
        if (idx < 0) return;
        const wrap = byId("tt-table-wrap");
        const head = byId("tt-head");
        const targetTop = head.offsetHeight + idx * TT_ROW_H;
        wrap.scrollTop = Math.max(0, targetTop - wrap.clientHeight / 2 + TT_ROW_H / 2);
        ttRenderVisibleRows();
      }
      // RALPHUS-TT-SCROLL-SELECTION:END
      /**
       * Renders the read-only task details pane (RAL-362 §6): header/needs-you/kv/reviews/cells/task-scope-proof, plus an on-demand PR check. The watch star in its header is the only interactive control -- no action buttons live here.
       * @returns {void}
       */
      function renderTaskDetailsPane() {
        const el = byId("task-details");
        if (taskTabSel.kind !== "task" && taskTabSel.kind !== "cell") { el.innerHTML = `<div class="empty">Select a task.</div>`; return; }
        const row = ttAllRows.find((r) => r.squadId === taskTabSel.squadId && r.taskIdx === taskTabSel.taskIdx);
        if (!row) { el.innerHTML = `<div class="empty">Select a task.</div>`; return; }
        const cell = taskTabSel.kind === "cell" ? row.cells[taskTabSel.cellIdx] : null;
        let html = `<div class="row" style="align-items:center;gap:8px;margin-bottom:6px">${sdot(row.state)}<b style="flex:1;overflow:hidden;text-overflow:ellipsis;white-space:nowrap">${esc(row.name)}</b>`
          + `<span class="tt-star ${row.watch.watched ? `watched ${row.watch.inherited ? "inherited" : ""}` : ""}" onclick="ttToggleWatch('${esc(row.squadId)}',${row.taskIdx})" data-tip="${row.watch.watched ? "Watching this task. Click to change." : "Not watched. Click to watch."}">${row.watch.inherited ? "◆" : "★"}</span></div>`
          + `<div class="meta" style="margin-bottom:10px;display:flex;align-items:center;gap:6px">${ttSquadChipHtml(row)}<span>·</span><span data-tip="Registered project.">${esc(row.task.project)}</span></div>`;
        if (row.needsMe) {
          html += `<div style="margin-bottom:10px;padding:8px;border:1px solid var(--accent);border-radius:6px" data-tip="Why the &quot;needs me&quot; filter surfaces this task."><span style="color:var(--accent)">●</span><b> Needs you</b><div>${esc(row.needsMeReason || "")}</div></div>`;
        }
        html += `<div class="kv-row"><span class="k">state</span><span class="v">${pill(row.state)}</span></div>`;
        html += `<div class="kv-row"><span class="k">agent</span><span class="v">${esc(row.task.agent || "–")}</span></div>`;
        html += `<div class="kv-row"><span class="k">model</span><span class="v">${esc(row.task.model || "–")}</span></div>`;
        html += timingRows(row.startedAtMs, row.finishedAtMs);
        html += `<div class="kv-row"><span class="k">tokens</span><span class="v">${ttFmtTokens(row.usage)}</span></div>`;
        html += `<div class="kv-row"><span class="k">cache</span><span class="v">${ttFmtCache(row.usage)}</span></div>`;
        html += `<div class="kv-row"><span class="k">cost</span><span class="v">${ttCostCellHtml(row.usage, ttTaskUsageItems(row.task))}</span></div>`;
        html += `<h4 style="margin:14px 0 6px">Reviews</h4>`;
        if (!row.reviews.length) html += `<div class="empty" style="padding:6px 0">No reviews yet.</div>`;
        else html += row.reviews.map((r) => {
          const branchRows = r.branches.map((b) => {
            const pr = row.prs.find((p) => p.branch_alias === b);
            const badge = pr
              ? `<span class="tt-badge pr-badge" style="color:${cvar(ttPrColorVar(pr))};border-color:${cvar(ttPrColorVar(pr))}" onclick="ttOpenPr('${esc(pr.pr_url || "")}')" data-tip="Open PR on the forge.${pr.state === "open" && pr.ci_status ? ` CI: ${esc(pr.ci_status)}.` : ""}">${pr.pr_number ? "#" + pr.pr_number : esc(pr.state)}</span>`
              : (r.status === "approved" || r.status === "merging" ? `<span class="tt-badge pr-placeholder" data-tip="Approved/merging with no PR submitted yet.">no PR</span>` : "");
            return `<div style="display:flex;justify-content:space-between;gap:8px;padding:3px 0 3px 14px"><span style="overflow:hidden;text-overflow:ellipsis;white-space:nowrap" data-tip="${esc(b)}">${esc(b)}</span>${badge}</div>`;
          }).join("");
          return `<div style="margin-bottom:8px"><div style="display:flex;align-items:center;gap:6px">${sdot(r.status)}<b style="flex:1">${esc(r.name)}</b>${pill(r.status)}</div>${branchRows}</div>`;
        }).join("");
        html += `<h4 style="margin:14px 0 6px">Cells</h4>`;
        if (!row.cells.length) html += `<div class="empty" style="padding:6px 0">No cells.</div>`;
        else html += row.cells.map((c, ci) => {
          const cw = ttEffectiveCellWatch(taskTabWatches, taskTabMutedTasks, row.squadId, row.taskIdx, ci);
          const starCls = cw.watched ? `watched ${cw.inherited ? "inherited" : ""}` : "";
          const starTip = !cw.watched
            ? "Not watched. Click to watch this cell."
            : cw.inherited
              ? "Watched via this cell's task or squad. Click to mute this cell only — the parent watch stays in effect for its other cells."
              : "Watching this cell. Click to unwatch.";
          const star = `<span class="tt-star ${starCls}" onclick="event.stopPropagation();ttToggleCellWatch('${esc(row.squadId)}',${row.taskIdx},${ci})" data-tip="${esc(starTip)}">${cw.inherited ? "◆" : "★"}</span>`;
          return `<div class="kv-row" style="cursor:pointer${cell === c ? ";color:var(--accent)" : ""}" onclick="ttSelectCell('${esc(row.squadId)}',${row.taskIdx},${ci})">${sdot(c.state)}<span class="k">${esc(c.name || c.id)}</span>${star}<span class="v">${pill(c.state)}</span></div>`;
        }).join("");
        if (cell) {
          html += `<h4 style="margin:14px 0 6px">${esc(cell.name || cell.id)}</h4>`;
          html += `<div class="kv-row"><span class="k">agent</span><span class="v">${esc(cell.agent)}${cell.model ? " / " + esc(cell.model) : ""}</span></div>`;
          html += timingRows(cell.started_at_ms ?? null, cell.finished_at_ms ?? null);
          const cu = ttCellUsage(cell);
          html += `<div class="kv-row"><span class="k">tokens</span><span class="v">${ttFmtTokens(cu)}</span></div>`;
          html += `<div class="kv-row"><span class="k">cost</span><span class="v">${ttCostCellHtml(cu, ttCellUsageItems(cell))}</span></div>`;
          if (cell.error) html += `<div class="kv-row"><span class="k">error</span><span class="v" style="color:var(--failed)">${esc(cell.error)}</span></div>`;
        }
        html += `<h4 style="margin:14px 0 6px">Task proof steps</h4>`;
        const taskProof = row.task.proof || [];
        if (!taskProof.length) html += `<div class="empty" style="padding:6px 0">No task-scope proof steps.</div>`;
        else html += taskProof.map((v) => `<div class="kv-row">${sdot(v.state)}<span class="k">${esc(v.id || v.kind)}</span><span class="v">${pill(v.state)}</span></div>`).join("");
        if (row.prPick) {
          html += `<h4 style="margin:14px 0 6px">PR check</h4>`;
          html += `<button class="btn" onclick="ttRunPrCheck('${esc(row.squadId)}',${row.taskIdx})" data-tip="On-demand only: fetches live drift (a git fetch) and un-actioned feedback (one forge API call) for this task's earliest PR.\nNot part of &quot;needs me&quot; or polled automatically -- at board scale that would be one round-trip per PR per refresh.">Check drift &amp; feedback</button>`;
          if (taskTabPrCheckFor && taskTabPrCheckFor.squadId === row.squadId && taskTabPrCheckFor.taskIdx === row.taskIdx && taskTabPrCheckResult) {
            const res = taskTabPrCheckResult;
            if (res.loading) html += `<div class="meta">Checking…</div>`;
            else if (res.error) html += `<div class="meta" style="color:var(--failed)">${esc(res.error)}</div>`;
            else {
              if (res.sync) html += `<div class="kv-row"><span class="k">in sync</span><span class="v">${res.sync.in_sync ? "yes" : "no"}${res.sync.pr_ahead ? " (PR ahead)" : ""}${res.sync.worktree_ahead ? " (worktree ahead)" : ""}</span></div>`;
              if (res.comments) html += `<div class="kv-row"><span class="k">un-actioned feedback</span><span class="v">${res.comments.filter((c) => !c.actioned).length} of ${res.comments.length}</span></div>`;
            }
          }
        }
        el.innerHTML = html;
      }
      /**
       * Repaints every visible live-ticking duration element in place (RAL-362 §3,
       * extended by RAL-381 to squad sidebar and details pane). Never a full re-render.
       * Runs once a second for any element with `data-running="1"`.
       * @returns {void}
       */
      function ttTickRunningTimes() {
        document.querySelectorAll('[data-running="1"]').forEach((el) => {
          // Tasks-tab Time column only ticks in duration mode; start mode shows static "ago" text.
          if (el.classList.contains("tt-time") && tab === "tasks" && taskTabTimeMode !== "duration") return;
          const started = Number(/** @type {HTMLElement} */ (el).dataset.started);
          if (started) el.textContent = fmtDuration(Date.now() - started);
        });
      }
      setInterval(ttTickRunningTimes, 1000);

      const REVIEW_ORIGINS = ["explicit", "arbiter"];
      /**
       * @returns {{q: string, status: Set<string>, resolver: Set<string>, origin: Set<string>, showHidden: boolean}}
       */
      function defaultReviewFilters() { return { q: "", status: new Set(GUARDIAN_STATES), resolver: new Set(), origin: new Set(REVIEW_ORIGINS), showHidden: false }; }
      /** @type {string|null} */
      let selectedSquadId = null;
      /** @type {SelStateTasks} what's shown in the details pane */
      let sel = { kind: null, taskIdx: 0, cellIdx: 0, proofIdx: -1 };
      let editing = false;
      let filters = defaultTaskFilters();
      let reviewFilters = defaultReviewFilters();
      // Once true, reviewFilters.resolver is a user/URL-chosen selection and is no longer
      // auto-synced to newly-discovered resolvers (see renderReviewResolverFilters).
      let reviewResolverDefaulted = false;
      /** @type {string|null} */
      let selectedGuardian = null;
      // RAL-382: set by gotoReview when the target review isn't in the loaded
      // `guardians` set yet — renderReviewDetail then shows a "Loading review…"
      // placeholder for that id instead of the previously selected review's
      // details (or a misleading "Select a review."), and clears it once the
      // review's data arrives in a poll.
      /** @type {string|null} */
      let reviewDetailLoading = null;
      let _inPopstate = false;
      /** @type {{[key: string]: string}} gid -> project root shown in the tab view */
      let selectedProjectTabs = {};
      /** @type {Set<string>} "<gid>:<pos>" for branches whose detail block is open */
      let expandedBranches = new Set();
      /** @type {{[key: string]: boolean}} gid -> whether the manual-checks dropdown is open */
      let manualMenuOpen = {};
      /** @type {{[key: string]: boolean}} "<gid>:<manual|action>:<index>" -> whether that check's inline input form is open (RAL-164) */
      let checkFormOpen = {};
      /** @type {{[key: string]: boolean}} peek-key -> whether the terminal actions dropdown ("Open Terminal Log" / "Open Agent") is open */
      let terminalMenuOpen = {};
      /** @type {Set<string>} */
      let dismissedReady = new Set(JSON.parse(localStorage.getItem("ralphus-dismissed-ready") || "[]"));
      /** @type {Set<string>} multi-selected squad ids (multi mode when size > 1) */
      let multiSel = new Set();
      /** @type {Set<string>} multi-selected task/cell/proof nodes in the current graph */
      let nodeMultiSel = new Set();
      // RAL-328/RAL-331: the current user's hidden-item set, refreshed by
      // pollHidden() every tick regardless of active tab (goto-search and
      // both sidebars need it). Hiding is a personal view preference -- it
      // never changes `squads`/`guardians` themselves or their counters.
      /** @type {Set<string>} squad ids the current user has hidden */
      let hiddenSquadIds = new Set();
      /** @type {Set<string>} guardian ids the current user has hidden */
      let hiddenGuardianIds = new Set();
      // A hidden item still shown because it's the one the user just
      // navigated to (goto-search, the Running widget, a Cartographer link,
      // a direct URL) -- "reveal just this one" rather than flipping the
      // "show hidden" filter globally.
      /** @type {string|null} */
      let revealedSquadId = null;
      /** @type {string|null} */
      let revealedGuardianId = null;
      /** @type {Set<string>} multi-selected review ids (RAL-331, mirrors multiSel) */
      let guardianMultiSel = new Set();
      /** @type {string|null} anchor for shift-range selection in the Reviews sidebar */
      let guardianAnchorId = null;
      /** @type {StatusPickerItem[]|null} */
      let _statusPickerItems = null;
      /** @type {GraphNodeSelectionItem[]|null} */
      let _graphMenuItems = null;
      /** @type {string|null} anchor for shift-range selection */
      let anchorId = null;
      /** @type {{[key: string]: {[key: string]: CellPathInfo}}} squadId -> { "ti:si": CellPathInfo }, lazily loaded (CCTL-148) */
      let squadPaths = {};
      /** @type {{[key: string]: boolean}} "gid:pos" -> bool, worktree cells dropdown open (RAL-71) */
      let worktreeMenuOpen = {};
      /** @type {{[key: string]: boolean}} peek key -> bool, live tmux pane "Read-only terminal" boxes currently expanded (RAL-102) */
      let peekOpen = {};
      /** @type {{[key: string]: string}} peek key -> last-rendered Live View text (the transcript tape run through the ANSI-strip/classify pipeline, RAL-397 Phase 2G-A), so a full pane re-render redisplays the last content instead of flashing back to "Loading…" (RAL-102 follow-up) */
      let peekContent = {};
      /** @type {{[key: string]: TapeWindow}} peek key -> loaded transcript-tape byte window (RAL-397 Phase 2G-A); the Live View's single content source, paged from `.../pane-transcript`. */
      let peekTape = {};
      /** @type {Set<string>} peek keys with a load-older transcript fetch in flight, so scroll events near the top don't stack duplicate prepends (RAL-397 Phase 2G-A). */
      const peekLoadingOlder = new Set();
      /** @type {{[key: string]: boolean}} peek key -> whether the last-fetched poll reported the cell as truly, confirmedly ended (past PEEK_MISSING_STRIKE_LIMIT) -- distinct from a single transient miss, which is not treated as ended (RAL-102 follow-up) */
      let peekEnded = {};
      /** @type {{[key: string]: number}} peek key -> consecutive inactive-poll count, reset to 0 on any active response */
      let peekMissingStrikes = {};
      /** @type {{[key: string]: number|null}} peek key -> last-fetched `last_activity_ms` (Unix epoch ms) from the daemon, or null if the cell has no output yet / has ended (RAL-170) */
      let peekLastActivity = {};
      /** @type {boolean} config-driven default (`live_view.show_debug_messages_default`, `.ralphus.toml`) for whether a newly-opened Live View pane shows ralphus's own diagnostic/telemetry lines (RAL-232). Fetched once at page load by fetchLiveViewConfigDefault(); a per-pane override in peekShowDebug takes precedence over this. */
      let showDebugMessagesDefault = false;
      /** @type {{[key: string]: boolean}} peek key -> per-pane override of whether ralphus's own diagnostic/telemetry lines are shown (RAL-232), set by toggling that pane's "Show Debug Messages" checkbox. Absent means "use showDebugMessagesDefault." Session-only, like every other peek* map -- not persisted. */
      let peekShowDebug = {};
      /** @type {{[key: string]: string|null}} gid -> branch name, the currently selected branch in the review pane */
      let selectedBranch = {};
      /** @type {{[key: string]: BranchConflicts}} "gid:branch_id" -> last-fetched live conflicting-files list (RAL-148) */
      let branchConflicts = {};
      /** @type {{[gid: string]: PullRequestView[]}} PRs submitted for a review, fetched on demand (RAL-190). */
      let pullRequests = {};
      /** @type {{[prId: string]: PrSyncStatus}} last-fetched drift check per still-open PR with a recorded forge number (RAL-190). */
      let prSyncStatus = {};
      /** @type {Set<string>} PR ids with a `fetchPrSyncStatus` request currently in flight -- lets `pollPullRequests` skip a PR whose previous poll's fetch hasn't resolved yet (the daemon's per-PR git fetch can take several seconds) instead of piling up redundant concurrent requests for the same PR every poll tick. */
      const prSyncStatusInFlight = new Set();
      /** @type {Map<string, number>} gid -> highest Cartographer row id of a `source=pr` failure already surfaced as a toast, so the passive poll doesn't re-toast one `waitForPrOutcome` already showed, or re-toast on every subsequent poll. */
      const prErrorHighWater = new Map();
      /** @type {{[key: string]: boolean}} peek key -> whether the durable terminal-log attempt-history box (RAL-154) is expanded */
      let historyOpen = {};
      /** @type {{[key: string]: AttemptMeta[]}} peek key -> last-fetched list of persisted attempts for that cell/proof/resolver */
      let historyAttempts = {};
      /** @type {{[key: string]: {attempt: number, content: string, merged: boolean}|null}} peek key -> the one historical attempt currently being viewed in full, if any. `merged` (RAL-296) is true when `content` came from the merged `.../debug-events` stream (the most recent attempt) rather than that attempt's raw, un-merged terminal-log content. */
      let historyViewing = {};
      /** @type {Set<string>} gid -> an Approve is in flight, so the button shows a pending/disabled state until it resolves (RAL-234) */
      let pendingGuardianActions = new Set();
      /** @type {Set<string>} gid -> a Merge / rebase kickoff is in flight, so the button shows a pending/disabled state until the daemon has answered and the board has reloaded */
      let pendingMergeActions = new Set();

