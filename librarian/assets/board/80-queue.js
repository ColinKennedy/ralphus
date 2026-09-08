      // ---------- Queue tab (RAL Queue) ----------
      /**
       * @typedef {object} QueueItem
       * @property {string} path
       * @property {string} readiness - "ready" | "blocked" | "excluded" | "running"
       * @property {string} state
       * @property {string} kind - "cell" | "proof"
       * @property {string} name
       * @property {number} indent
       * @property {string} squad_id
       * @property {string|null} squad_label
       * @property {string} squad_state
       * @property {number} task_idx
       * @property {string} task_name
       * @property {string[]} [task_depends_on]
       * @property {number} cell_idx
       * @property {number} [proof_idx]
       * @property {string} [proof_scope]
       * @property {string[]} [depends_on]
       * @property {string[]} [deps_paths]
       * @property {string[]} [blocked_by]
       */
      /** @type {QueueItem[]} last server snapshot */
      let queueItems = [];
      /** @type {Map<string, QueueItem>} path -> item */
      let queueItemMap = new Map();
      /** @type {string[]} working (possibly staged) order of paths */
      let queueOrder = [];
      let queueDirty = false;           // an unsaved reordering exists
      /** @type {Set<string>} selected item paths */
      let queueSel = new Set();
      /** @type {Set<string>} items the last move relocated as a side effect (dependency drag-along) */
      let queuePulled = new Set();
      /** @type {string|null} anchor for shift-range selection */
      let queueAnchor = null;
      let queueLoaded = false;          // first load done for this tab visit
      /** @type {string[]|null} paths currently being dragged */
      let queueDrag = null;
      // Collapse model: a per-LEVEL default plus a per-BLOCK override set. Blocks
      // are keyed by their first item's path, so a squad split into several groups
      // (by interleaving) collapses/expands each group independently. Default:
      // squads expanded, tasks collapsed (cell-level interleaving is rare).
      let queueDefault = { squad: "expanded", task: "collapsed" };
      /** @type {Set<string>} block keys toggled AWAY from their default */
      let queueOverride = new Set();
      /** @type {Map<string, string[]>} block key -> [member paths], rebuilt each render */
      let queueBlockPaths = new Map();
      const queueUI = { search: "", readyOnly: true, hideRunning: false, autoUpdate: false, status: new Set(["ready", "blocked", "excluded", "running"]) };
      const Q_READINESS = ["ready", "blocked", "excluded", "running"];
      /** @type {{[key: string]: string}} */
      const qDotColor = { ready: "--done", blocked: "--queued", excluded: "--failed", running: "--running" };
      /**
       * Renders a colored status dot for a queue readiness value.
       * @param {string} s
       * @returns {string}
       */
      const qDot = (s) => `<span class="dot" style="background:${cvar(qDotColor[s] || "--muted")}"></span>`;
      // Roll a squad/task block's member readinesses up into one badge: fully
      // ready only if every member is; otherwise the worst case (excluded beats
      // blocked) so a header never claims "ready" while something under it isn't.
      /**
       * Rolls up a block's member readinesses into one aggregate badge.
       * @param {string[]} paths
       * @returns {{agg: string, ready: number, total: number}}
       */
      function queueBlockReadiness(paths) {
        const items = /** @type {QueueItem[]} */ (paths.map((p) => queueItemMap.get(p)).filter(Boolean));
        const total = items.length;
        const ready = items.filter((i) => i.readiness === "ready").length;
        if (!total || ready === total) return { agg: "ready", ready, total };
        if (items.some((i) => i.readiness === "excluded")) return { agg: "excluded", ready, total };
        return { agg: "blocked", ready, total };
      }
      // The Queue's own item state doesn't carry a task-level state; look the
      // task's current node state up from the Tasks-tab `squads` cache instead
      // (kept fresh every tick regardless of active tab via updateCounter()).
      /**
       * Looks up a task's current node state from the Tasks-tab `squads` cache.
       * @param {string} squadId
       * @param {number} taskIdx
       * @returns {string|null}
       */
      function queueTaskState(squadId, taskIdx) {
        const r = findSquad(squadId);
        const t = r && r.tasks && r.tasks[taskIdx];
        return t ? t.state : null;
      }

      // Browsers restore checkbox/text-input state across F5, but the JS re-inits
      // to defaults — so on reload the ready-only/hide-running/auto-update controls
      // and the search box can disagree with queueUI. Adopt the DOM's actual state
      // so what's displayed and what's filtered always match.
      /**
       * Adopts the Queue tab's filter controls' actual DOM state (browser-restored on reload) into `queueUI`.
       * @returns {void}
       */
      function queueSyncControlsFromDOM() {
        const ro = /** @type {HTMLInputElement|null} */ (document.getElementById("queue-ready-only"));
        const hr = /** @type {HTMLInputElement|null} */ (document.getElementById("queue-hide-running"));
        const au = /** @type {HTMLInputElement|null} */ (document.getElementById("queue-autoupdate"));
        const se = /** @type {HTMLInputElement|null} */ (document.getElementById("queue-search"));
        if (ro) queueUI.readyOnly = ro.checked;
        if (hr) queueUI.hideRunning = hr.checked;
        if (au) queueUI.autoUpdate = au.checked;
        if (se) { queueUI.search = (se.value || "").toLowerCase(); se.classList.toggle("has-text", !!se.value); }
      }
      /**
       * Polls `/api/queue` and reconciles the staged order against the server snapshot.
       * @returns {Promise<void>}
       */
      async function pollQueue() {
        queueSyncControlsFromDOM();
        try {
          const d = await (await fetch("/api/queue")).json();
          byId("conn").className = "dot on";
          byId("updated").textContent = "updated " + new Date().toLocaleTimeString();
          /** @type {QueueItem[]} */
          const items = d.items || [];
          queueItems = items;
          queueItemMap = new Map(items.map((i) => [i.path, i]));
          if (!queueDirty) {
            // Clean slate: adopt the server's canonical (rank) order.
            queueOrder = items.map((i) => i.path);
          } else {
            // Incremental reconcile: keep the user's staged order, drop items that
            // are gone (started/finished) and append newly-ready ones at the end.
            queueOrder = queueOrder.filter((p) => queueItemMap.has(p));
            for (const i of items) if (!queueOrder.includes(i.path)) queueOrder.push(i.path);
          }
          queueSel = new Set([...queueSel].filter((p) => queueItemMap.has(p)));
          // A clean (non-staged) refresh has no side-effect moves to highlight;
          // while staged, keep the highlight but drop items that are gone.
          queuePulled = queueDirty ? new Set([...queuePulled].filter((p) => queueItemMap.has(p))) : new Set();
          queueLoaded = true;
          renderQueue();
          updateQueueFooter();
          queueEnsureSelectionVisible();
        } catch (e) {
          byId("conn").className = "dot off";
          byId("updated").textContent = "daemon unreachable";
        }
      }

      /**
       * Renders the Queue tab's per-readiness filter checkboxes.
       * @returns {void}
       */
      function renderQueueFilters() {
        const el = document.getElementById("queue-status-filters");
        if (!el) return;
        el.innerHTML = Q_READINESS.map((s) =>
          `<label data-tip="Show or hide ${s} items (only applies when 'ready-only' is off).">${qDot(s)}<input type="checkbox" ${queueUI.status.has(s) ? "checked" : ""} data-state="${esc(s)}" onchange="queueToggleStatus(this.dataset.state,this.checked)">${s}</label>`
        ).join("");
      }
      /**
       * Toggles one readiness value in/out of the Queue tab's visibility filter.
       * @param {string} s
       * @param {boolean} on
       * @returns {void}
       */
      function queueToggleStatus(s, on) { on ? queueUI.status.add(s) : queueUI.status.delete(s); renderQueue(); queueEnsureSelectionVisible(); }
      /**
       * Toggles the Queue tab's "ready only" filter.
       * @param {boolean} on
       * @returns {void}
       */
      function queueSetReadyOnly(on) { queueUI.readyOnly = on; renderQueue(); queueEnsureSelectionVisible(); }
      /**
       * Toggles the Queue tab's "hide running" filter.
       * @param {boolean} on
       * @returns {void}
       */
      function queueSetHideRunning(on) { queueUI.hideRunning = on; renderQueue(); queueEnsureSelectionVisible(); }
      /**
       * Toggles the Queue tab's auto-update polling.
       * @param {boolean} on
       * @returns {void}
       */
      function queueSetAutoUpdate(on) { queueUI.autoUpdate = on; if (on) pollQueue(); }
      /**
       * Applies a free-text search filter to the Queue tab.
       * @param {string} v
       * @returns {void}
       */
      function queueSetSearch(v) {
        queueUI.search = v.toLowerCase();
        const box = document.getElementById("queue-search");
        if (box) box.classList.toggle("has-text", !!v);
        renderQueue(); queueEnsureSelectionVisible();
      }
      /**
       * Clears the Queue tab's search filter.
       * @returns {void}
       */
      function queueClearSearch() {
        queueUI.search = "";
        const box = /** @type {HTMLInputElement|null} */ (document.getElementById("queue-search"));
        if (box) { box.value = ""; box.classList.remove("has-text"); box.focus(); }
        renderQueue(); queueEnsureSelectionVisible();
      }

      // After a filter change or refresh reflows the list, keep the selection in
      // view: if no selected row (or selected group header) is on-screen, scroll
      // the topmost selected element into view. No selection → leave scroll alone.
      /**
       * Scrolls the topmost selected Queue row into view if it's off-screen.
       * @returns {void}
       */
      function queueEnsureSelectionVisible() {
        if (!queueSel.size) return;
        const scroller = document.getElementById("queue-page");
        const list = document.getElementById("queue-list");
        if (!scroller || !list) return;
        const sel = [...list.querySelectorAll(".q-row.selected")];
        if (!sel.length) return;
        const box = scroller.getBoundingClientRect();
        const onScreen = sel.some((r) => { const rr = r.getBoundingClientRect(); return rr.bottom > box.top && rr.top < box.bottom; });
        if (onScreen) return;
        const top = sel.reduce((a, b) => (a.getBoundingClientRect().top <= b.getBoundingClientRect().top ? a : b));
        top.scrollIntoView({ behavior: "smooth", block: "center" });
      }

      /**
       * Checks whether a queue item matches the active search text.
       * @param {QueueItem} it
       * @returns {boolean}
       */
      function queueMatchesSearch(it) {
        const q = queueUI.search;
        if (!q) return true;
        return (it.name + " " + it.task_name + " " + (it.squad_label || "") + " " + it.squad_id).toLowerCase().includes(q);
      }
      /**
       * Checks whether a queue item passes the active search + readiness filters.
       * @param {QueueItem} it
       * @returns {boolean}
       */
      function queueVisible(it) {
        if (!queueMatchesSearch(it)) return false;
        if (queueUI.readyOnly) return it.readiness === "ready";
        return queueUI.status.has(it.readiness);
      }

      /**
       * Renders the Queue tab's list (pinned running section + filtered/grouped rest).
       * @returns {void}
       */
      function renderQueue() {
        renderQueueFilters();
        const el = document.getElementById("queue-list");
        if (!el) return;
        const ordered = /** @type {QueueItem[]} */ (queueOrder.map((p) => queueItemMap.get(p)).filter(Boolean));
        // Running work is an immovable floor pinned to the top (you cannot schedule
        // ahead of work already in flight). A toggle hides it.
        const running = ordered.filter((i) => i.readiness === "running" && queueMatchesSearch(i));
        const rest = ordered.filter((i) => i.readiness !== "running" && queueVisible(i));
        let html = "";
        if (!queueUI.hideRunning && running.length) {
          html += `<div class="q-section" data-tip="Work already running. It is pinned here and cannot be reordered — nothing can be scheduled ahead of it.">running now — pinned</div>`;
          html += running.map(queueItemRow).join("");
        }
        if (!rest.length && !html) { el.innerHTML = `<div class="empty">No queued work${queueUI.readyOnly ? " ready to run" : ""}.</div>`; return; }
        queueBlockPaths = new Map();
        let squadCollapsed = false, taskCollapsed = false;
        for (let i = 0; i < rest.length; i++) {
          const it = rest[i];
          const squadStart = i === 0 || rest[i - 1].squad_id !== it.squad_id;
          if (squadStart) {
            const key = "squadblk:" + it.path;
            const paths = [];
            for (let j = i; j < rest.length && rest[j].squad_id === it.squad_id; j++) paths.push(rest[j].path);
            queueBlockPaths.set(key, paths);
            squadCollapsed = queueIsCollapsed("squad", key);
            html += queueHeaderRow("squad", it, key, paths.length);
          }
          if (squadCollapsed) continue;                       // whole squad block folded
          const taskStart = i === 0 || rest[i - 1].squad_id !== it.squad_id || rest[i - 1].task_idx !== it.task_idx;
          if (taskStart) {
            const key = "taskblk:" + it.path;
            const paths = [];
            for (let j = i; j < rest.length && rest[j].squad_id === it.squad_id && rest[j].task_idx === it.task_idx; j++) paths.push(rest[j].path);
            queueBlockPaths.set(key, paths);
            taskCollapsed = queueIsCollapsed("task", key);
            html += queueHeaderRow("task", it, key, paths.length);
          }
          if (taskCollapsed) continue;                      // task block folded
          html += queueItemRow(it);
        }
        el.innerHTML = html;
      }

      /**
       * Renders a Queue squad/task group header row.
       * @param {string} kind
       * @param {QueueItem} it
       * @param {string} key
       * @param {number} count
       * @returns {string}
       */
      function queueHeaderRow(kind, it, key, count) {
        const collapsed = queueIsCollapsed(kind, key);
        const paths = queueBlockPaths.get(key) || [];
        const selCls = paths.length && paths.every((p) => queueSel.has(p)) ? " selected"
          : (paths.length && paths.every((p) => queuePulled.has(p)) ? " pulled" : "");
        const caret = `<span class="q-caret" data-click="queueToggleCollapse" data-key="${esc(key)}" data-tip="Expand or collapse this ${kind}.">${collapsed ? "▸" : "▾"}</span>`;
        const cnt = collapsed ? `<span class="q-count">${count} item${count === 1 ? "" : "s"}</span>` : "";
        const dnd = `draggable="true" ondragstart="queueHeaderDragStart(event,this.dataset.key)" ondragend="queueDragEnd(event)"`;
        const dropBefore = `data-drop-before="${esc(paths[0] || "")}"`;
        const evts = `data-click="queueHeaderClick" data-ctx="queueHeaderMenu" data-key="${esc(key)}" data-kind="${esc(kind)}" data-squad-id="${esc(it.squad_id)}" data-ti="${it.task_idx}"`;
        const rd = queueBlockReadiness(paths);
        const rdFrac = rd.agg !== "ready" && rd.total ? ` ${rd.ready}/${rd.total}` : "";
        const rdBadge = `<span class="q-badge q-${rd.agg}" data-tip="Rolled-up readiness for this ${kind}: ${rd.ready} of ${rd.total} item${rd.total === 1 ? "" : "s"} ready to run.\nMirrors the ready/blocked/excluded badge shown on each cell — 'ready' here only when everything under it is.">${rd.agg}${rdFrac}</span>`;
        if (kind === "squad") {
          return `<div class="q-row header squad${selCls}" style="padding-left:8px" ${dnd} ${dropBefore} ${evts}
              data-tip="Squad ${esc(it.squad_id)} (${esc(it.squad_state)}).\nClick to select it; drag to move the whole squad as one group and interleave it with others; the caret collapses it.\nRight-click for Expand/Collapse all or Set status (e.g. 'ignored').">${caret}<span class="q-grip">⋮⋮</span>${sdot(it.squad_state)}<b>${esc(it.squad_label || it.squad_id)}</b> <span class="q-meta">${esc(it.squad_id)} · ${esc(it.squad_state)}</span>${rdBadge}${cnt}</div>`;
        }
        const taskState = queueTaskState(it.squad_id, it.task_idx);
        const dep = (it.task_depends_on && it.task_depends_on.length)
          ? `<span class="q-dep" data-tip="This task depends on: ${esc(it.task_depends_on.join(", "))}.\nIt can only run after those are done or ignored — drag it up and its dependency comes with it.">↳ needs ${esc(it.task_depends_on.join(", "))}</span>` : "";
        return `<div class="q-row header task${selCls}" style="padding-left:28px" ${dnd} ${dropBefore} ${evts}
            data-tip="Task '${esc(it.task_name)}'${taskState ? " (" + esc(taskState) + ")" : ""}.\nClick to select it; drag to move the task and all its cells/proofs together; the caret expands it to show cells.\nRight-click for Expand/Collapse all or Set status.">${caret}<span class="q-grip">⋮⋮</span>${taskState ? sdot(taskState) : ""}${esc(it.task_name)}${dep}${rdBadge}${cnt}</div>`;
      }

      /**
       * Renders one Queue leaf row (cell or proof item).
       * @param {QueueItem} it
       * @returns {string}
       */
      function queueItemRow(it) {
        const pad = 8 + it.indent * 20;
        const selCls = queueSel.has(it.path) ? " selected" : (queuePulled.has(it.path) ? " pulled" : "");
        const rd = it.readiness;
        const kindLabel = it.kind === "cell" ? "cell" : (it.proof_scope === "task" ? "task-proof" : "proof");
        const blocked = (rd === "blocked" || rd === "excluded") && it.blocked_by && it.blocked_by.length
          ? `<span class="q-blockedby" data-tip="Waiting on: ${esc(it.blocked_by.join(", "))}">⛔ ${esc(it.blocked_by.join(", "))}</span>` : "";
        const dep = (it.depends_on && it.depends_on.length)
          ? `<span class="q-dep" data-tip="This cell depends on: ${esc(it.depends_on.join(", "))}.\nDrag it up and its dependency is pulled along.">↳ after ${esc(it.depends_on.join(", "))}</span>` : "";
        const draggable = rd !== "running";
        const dnd = draggable
          ? `draggable="true" ondragstart="queueDragStart(event,this.dataset.path)" ondragend="queueDragEnd(event)"`
          : "";
        const grip = draggable ? `<span class="q-grip">⋮⋮</span>` : `<span class="q-grip" style="opacity:.3">▣</span>`;
        const gotoKind = it.kind === "cell" ? "cell" : "proof";
        const vi = gotoKind === "proof" ? it.proof_idx : -1;
        const goto = `<button class="q-goto" data-click="gotoSquadItemStopProp" data-squad-id="${esc(it.squad_id)}" data-kind="${esc(gotoKind)}" data-ti="${it.task_idx}" data-si="${it.cell_idx}" data-vi="${vi}" data-tip="Jump to this ${esc(kindLabel)} on the Squads tab.\nSelects the matching squad/task/cell/proof so you don't have to hunt for it manually.">↗</button>`;
        return `<div class="q-row item${selCls}" data-path="${esc(it.path)}" data-drop-before="${esc(it.path)}" style="padding-left:${pad}px" ${dnd}
            data-click="queueRowClick" data-ctx="queueMenu"
            data-tip="${esc(kindLabel)} '${esc(it.name)}' — ${rd}.\nDrag to reorder (dependencies come along); right-click for Set position / Set status.\nOrder is a best-effort hint honored when a compute slot frees, not a hard guarantee.">${grip}${sdot(it.state)}<span class="q-name">${esc(it.name)} <span class="q-meta">${kindLabel}</span></span>${dep}${blocked}<span class="q-badge q-${rd}">${rd}</span>${goto}</div>`;
      }

      // ---- selection ----
      /**
       * Returns the currently selected queue paths in their staged order.
       * @returns {string[]}
       */
      function orderedSelection() { return queueOrder.filter((p) => queueSel.has(p)); }
      // Mirrors renderQueue's own visibility split (pinned running section vs.
      // the filtered/searched rest) so "select all" selects exactly what's on screen.
      /**
       * Checks whether a queue item is currently visible under renderQueue's own filter split.
       * @param {QueueItem} it
       * @returns {boolean}
       */
      function queueRowVisible(it) {
        return it.readiness === "running" ? (!queueUI.hideRunning && queueMatchesSearch(it)) : queueVisible(it);
      }
      /**
       * Selects every currently-visible queue row.
       * @returns {void}
       */
      function queueSelectAll() {
        const ordered = /** @type {QueueItem[]} */ (queueOrder.map((p) => queueItemMap.get(p)).filter(Boolean));
        queueSel = new Set(ordered.filter(queueRowVisible).map((i) => i.path));
        queueAnchor = orderedSelection().slice(-1)[0] || null;
        renderQueue();
      }
      /**
       * Clears the Queue tab's selection.
       * @returns {void}
       */
      function queueSelectNone() {
        queueSel = new Set();
        queueAnchor = null;
        renderQueue();
      }
      /**
       * Handles a click on a queue row: plain select, ctrl/cmd toggle, or shift range-select.
       * @param {MouseEvent} e
       * @param {string} path
       * @returns {void}
       */
      function queueRowClick(e, path) {
        if (e.metaKey || e.ctrlKey) { queueSel.has(path) ? queueSel.delete(path) : queueSel.add(path); }
        else if (e.shiftKey && queueAnchor) {
          const ai = queueOrder.indexOf(queueAnchor), bi = queueOrder.indexOf(path);
          if (ai >= 0 && bi >= 0) { const [lo, hi] = ai < bi ? [ai, bi] : [bi, ai]; queueSel = new Set(queueOrder.slice(lo, hi + 1)); }
        } else { queueSel = new Set([path]); }
        queueAnchor = path;
        renderQueue();
      }

      // ---- collapse / expand groups (per-block, level-defaulted) ----
      /**
       * Checks whether a squad/task block is currently collapsed.
       * @param {string} kind
       * @param {string} key
       * @returns {boolean}
       */
      function queueIsCollapsed(kind, key) {
        const defCollapsed = /** @type {{[key: string]: string}} */ (queueDefault)[kind] === "collapsed";
        return queueOverride.has(key) ? !defCollapsed : defCollapsed;
      }
      /**
       * Toggles one block's collapsed override.
       * @param {MouseEvent} e
       * @param {string} key
       * @returns {void}
       */
      function queueToggleCollapse(e, key) {
        e.stopPropagation();
        queueOverride.has(key) ? queueOverride.delete(key) : queueOverride.add(key);
        renderQueue();
      }
      /**
       * Expands every squad and task group.
       * @returns {void}
       */
      function queueExpandAll() { closeQueueMenu(); queueDefault = { squad: "expanded", task: "expanded" }; queueOverride = new Set(); renderQueue(); }
      /**
       * Collapses every task group (squads stay expanded).
       * @returns {void}
       */
      function queueCollapseTasks() { closeQueueMenu(); queueDefault = { squad: "expanded", task: "collapsed" }; queueOverride = new Set(); renderQueue(); }
      /**
       * Collapses every squad group.
       * @returns {void}
       */
      function queueCollapseSquads() { closeQueueMenu(); queueDefault = { squad: "collapsed", task: "collapsed" }; queueOverride = new Set(); renderQueue(); }

      // ---- header select / drag ----
      /**
       * Handles a click on a squad/task header row: selects its whole block.
       * @param {MouseEvent} e
       * @param {string} key
       * @returns {void}
       */
      function queueHeaderClick(e, key) {
        e.stopPropagation();
        const paths = queueBlockPaths.get(key) || [];
        if (e.metaKey || e.ctrlKey) { paths.forEach((p) => queueSel.add(p)); }
        else queueSel = new Set(paths);
        queueAnchor = paths[paths.length - 1] || queueAnchor;
        renderQueue();
      }

      // ---- drag & drop reorder ----
      // Lazy, anchored constraint repair: the dragged item(s) stay exactly where
      // dropped (the "anchor"); only the minimal set of OTHER items move to keep
      // every dependency before its dependent. So dragging dependent B upward does
      // nothing to its dependency A until B actually crosses above A — at which
      // point A is pulled up to just before B. Symmetrically, dragging dependency
      // A down below B pushes B down after A. A move that violates nothing leaves
      // everything else untouched.
      // `moved` (optional Set) collects the NON-anchor items the repair relocated,
      // so the UI can highlight what "came along for the ride" vs what you picked.
      /**
       * Repairs dependency ordering around a drag-anchored block, pulling/pushing minimal other items to keep dependencies before dependents.
       * @param {string[]} arr
       * @param {Set<string>} anchorSet
       * @param {Set<string>} [moved]
       * @returns {string[]}
       */
      function queueAnchoredRepair(arr, anchorSet, moved) {
        let changed = true, guard = 0, max = arr.length * arr.length + 20;
        while (changed && guard++ < max) {
          changed = false;
          const idx = new Map(); arr.forEach((p, i) => idx.set(p, i));
          outer:
          for (const x of arr) {
            const it = queueItemMap.get(x); if (!it) continue;
            for (const d of (it.deps_paths || [])) {
              if (!idx.has(d)) continue;
              const i = idx.get(x), j = idx.get(d);
              if (j <= i) continue;                          // dep already before dependent — fine
              if (anchorSet.has(d) && !anchorSet.has(x)) {
                // dependency d was dragged down past dependent x → push x below d
                arr.splice(i, 1);
                arr.splice(arr.indexOf(d) + 1, 0, x);
                if (moved) moved.add(x);
              } else {
                // dependent x is anchored (or neither is) → pull dep d up before x
                arr.splice(j, 1);
                arr.splice(arr.indexOf(x), 0, d);
                if (moved && !anchorSet.has(d)) moved.add(d);
              }
              changed = true;
              break outer;
            }
          }
        }
        return arr;
      }
      // Move an anchored block to sit before targetPath, then repair constraints.
      /**
       * Moves an anchored block of paths to sit before `targetPath`, then repairs dependency constraints.
       * @param {string[]} order
       * @param {string[]} anchorPaths
       * @param {string|null} targetPath
       * @returns {string[]}
       */
      function queueApplyMove(order, anchorPaths, targetPath) {
        const anchorSet = new Set(anchorPaths);
        const block = order.filter((p) => anchorSet.has(p));   // keep their relative order
        const arr = order.filter((p) => !anchorSet.has(p));
        const at = arr.indexOf(targetPath ?? "");
        arr.splice(at < 0 ? arr.length : at, 0, ...block);
        const moved = new Set();
        const result = queueAnchoredRepair(arr, anchorSet, moved);
        queuePulled = moved;
        return result;
      }
      /**
       * Drag-start handler for a queue row: stages the dragged path(s).
       * @param {DragEvent} e
       * @param {string} path
       * @returns {void}
       */
      function queueDragStart(e, path) {
        queueDrag = (queueSel.has(path) && queueSel.size > 1) ? orderedSelection() : [path];
        if (!queueSel.has(path)) queueSel = new Set([path]);  // no re-render mid-drag
        if (e.dataTransfer) { e.dataTransfer.effectAllowed = "move"; try { e.dataTransfer.setData("text/plain", path); } catch (_) {} }
      }
      // Dragging a squad/task header moves that specific contiguous block as a unit.
      /**
       * Drag-start handler for a squad/task header row: stages its whole block.
       * @param {DragEvent} e
       * @param {string} key
       * @returns {void}
       */
      function queueHeaderDragStart(e, key) {
        e.stopPropagation();
        queueDrag = (queueBlockPaths.get(key) || []).slice();
        queueSel = new Set(queueDrag);
        if (e.dataTransfer) { e.dataTransfer.effectAllowed = "move"; try { e.dataTransfer.setData("text/plain", key); } catch (_) {} }
      }
      /**
       * Clears every queue row's drop-indicator highlight.
       * @returns {void}
       */
      function queueClearDropInd() {
        document.querySelectorAll("#queue-list .q-row.dropind, #queue-list .q-row.dropind-end")
          .forEach((x) => x.classList.remove("dropind", "dropind-end"));
      }
      /**
       * Drag-end handler: clears drop indicators and the staged drag.
       * @returns {void}
       */
      function queueDragEnd() { queueClearDropInd(); queueDrag = null; }
      // The row the item would land BEFORE for a given cursor Y: the first row whose
      // vertical midpoint is below the cursor. `null` means "drop at the very end".
      /**
       * Finds the queue row the dragged item would land before, for a given cursor Y.
       * @param {number} clientY
       * @returns {Element|null}
       */
      function queueDropTargetRow(clientY) {
        const rows = document.querySelectorAll("#queue-list .q-row[data-drop-before]");
        for (const r of rows) {
          const rect = r.getBoundingClientRect();
          if (clientY < rect.top + rect.height / 2) return r;
        }
        return null;
      }
      // Drop is handled at the LIST level, so the whole area (rows AND the gaps /
      // whitespace between them) is a valid, forgiving drop zone.
      /**
       * Drag-over handler for the queue list: shows the drop-target indicator.
       * @param {DragEvent} e
       * @returns {void}
       */
      function queueListDragOver(e) {
        if (!queueDrag) return;
        e.preventDefault();
        if (e.dataTransfer) e.dataTransfer.dropEffect = "move";
        queueClearDropInd();
        const r = queueDropTargetRow(e.clientY);
        if (r) r.classList.add("dropind");
        else {
          const rows = document.querySelectorAll("#queue-list .q-row[data-drop-before]");
          if (rows.length) rows[rows.length - 1].classList.add("dropind-end");
        }
      }
      /**
       * Drop handler for the queue list: stages the reorder.
       * @param {DragEvent} e
       * @returns {void}
       */
      function queueListDrop(e) {
        e.preventDefault();
        queueClearDropInd();
        const dragged = queueDrag; queueDrag = null;
        if (!dragged || !dragged.length) return;
        const r = /** @type {HTMLElement|null} */ (queueDropTargetRow(e.clientY));
        const targetPath = r ? (r.dataset.dropBefore ?? null) : null;   // null → append at end
        if (targetPath && dragged.includes(targetPath)) return;
        queueOrder = queueApplyMove(queueOrder, dragged, targetPath);
        queueDirty = true;
        renderQueue(); updateQueueFooter();
      }

      // ---- footer (staged save) ----
      /**
       * Shows/hides the "unsaved reorder" footer.
       * @returns {void}
       */
      function updateQueueFooter() {
        const f = document.getElementById("queue-footer");
        if (f) f.classList.toggle("show", queueDirty && tab === "queue");
      }
      /**
       * Persists the staged queue reorder to the server.
       * @returns {Promise<void>}
       */
      async function queueSave() {
        try {
          const r = await fetch("/api/queue/reorder", { method: "POST", body: JSON.stringify({ order: queueOrder }) });
          const d = await r.json();
          queueDirty = false;
          queuePulled = new Set();
          if (d.items) { queueItems = d.items; queueItemMap = new Map(d.items.map((/** @type {QueueItem} */ i) => [i.path, i])); queueOrder = d.order || d.items.map((/** @type {QueueItem} */ i) => i.path); }
          renderQueue(); updateQueueFooter();
        } catch (_) {}
      }
      /**
       * Discards the staged queue reorder, reverting to the last server order.
       * @returns {void}
       */
      function queueCancel() {
        queueDirty = false;
        queuePulled = new Set();
        queueOrder = queueItems.map((i) => i.path);
        queueSel = new Set();
        renderQueue(); updateQueueFooter();
      }

      // ---- context menu: Set position / Set status ----
      /**
       * Closes the Queue tab's row context menu, if open.
       * @returns {void}
       */
      function closeQueueMenu() { const m = document.getElementById("queue-menu"); if (m) m.remove(); }
      document.addEventListener("click", closeQueueMenu);
      /**
       * Converts a queue item into a StatusPickerItem for the Set Status menu.
       * @param {string} path
       * @returns {StatusPickerItem|null}
       */
      function queueStatusItem(path) {
        const it = queueItemMap.get(path); if (!it) return null;
        if (it.kind === "cell") return { squadId: it.squad_id, kind: "cell", taskIdx: it.task_idx, cellIdx: it.cell_idx, proofIdx: -1, proofScope: "", label: it.name };
        return { squadId: it.squad_id, kind: "proof", taskIdx: it.task_idx, cellIdx: it.cell_idx, proofIdx: it.proof_idx ?? -1, proofScope: it.proof_scope, label: it.name };
      }
      /**
       * Opens a queue row's right-click context menu (Set position / Expand-collapse / Set status).
       * @param {MouseEvent} e
       * @param {string} path
       * @returns {void}
       */
      function queueMenu(e, path) {
        e.preventDefault(); e.stopPropagation(); closeQueueMenu(); closeStatusPicker();
        if (!queueSel.has(path)) { queueSel = new Set([path]); renderQueue(); }
        const paths = orderedSelection().length ? orderedSelection() : [path];
        _statusPickerItems = /** @type {StatusPickerItem[]} */ (paths.map(queueStatusItem).filter(Boolean));
        const menu = document.createElement("div");
        menu.className = "ctx-menu"; menu.id = "queue-menu";
        menu.innerHTML =
          `<div onclick="event.stopPropagation();closeQueueMenu();queuePositionModal()" data-tip="Move the selected item(s) to a specific queue position — an absolute index or a relative shift.\nStaged until you press Save in the footer.">↕ Set position…</div>`
          + `<div style="border-top:1px solid var(--border);margin:4px 0"></div>`
          + queueViewMenuItems()
          + `<div style="border-top:1px solid var(--border);margin:4px 0"></div>`
          + `<div style="color:var(--muted);font-size:11px;padding:3px 10px 4px;text-transform:uppercase;letter-spacing:.5px">Set status to…</div>`
          + NODE_STATES.map((s) => `<div data-click="doPickStatus" data-state="${esc(s)}" data-tip="Set status to ${s}.${s === "ignored" ? "\nIgnored skips the item and unblocks its downstream exactly like done. Reversible." : (IRREVERSIBLE_STATES.has(s) ? "\nThis cannot be undone." : "")}">${sdot(s)} ${s}</div>`).join("");
        document.body.appendChild(menu);
        menu.style.left = Math.min(e.clientX, window.innerWidth - 220) + "px";
        menu.style.top = Math.min(e.clientY, window.innerHeight - 360) + "px";
      }
      // Shared Expand/Collapse-all menu items (used by item + header menus).
      /**
       * Renders the shared Expand-all/Collapse-tasks/Collapse-squads menu items.
       * @returns {string}
       */
      function queueViewMenuItems() {
        return `<div onclick="queueExpandAll()" data-tip="Expand every squad and task group.">⊞ Expand all</div>`
          + `<div onclick="queueCollapseTasks()" data-tip="Collapse every task to one row (squads stay open).">⊟ Collapse tasks</div>`
          + `<div onclick="queueCollapseSquads()" data-tip="Collapse every squad to a single row — the fastest way to reorder whole squads as units.">⊟ Collapse squads</div>`;
      }
      /**
       * Opens a squad/task header row's right-click context menu.
       * @param {MouseEvent} e
       * @param {string} kind
       * @param {string} squadId
       * @param {number} ti
       * @param {string} key
       * @returns {void}
       */
      function queueHeaderMenu(e, kind, squadId, ti, key) {
        e.preventDefault(); e.stopPropagation(); closeQueueMenu(); closeStatusPicker();
        const blockPaths = queueBlockPaths.get(key) || [];
        if (blockPaths.length && !blockPaths.every((p) => queueSel.has(p))) { queueSel = new Set(blockPaths); renderQueue(); }
        const it0 = queueOrder.map((p) => queueItemMap.get(p)).find((it) => it && it.squad_id === squadId && (kind === "squad" || it.task_idx === ti));
        const label = kind === "squad" ? (it0?.squad_label || squadId) : (it0?.task_name || ("t" + ti));
        _statusPickerItems = [kind === "squad"
          ? { squadId, kind: "squad", taskIdx: 0, cellIdx: -1, proofIdx: -1, proofScope: "", label }
          : { squadId, kind: "task", taskIdx: ti, cellIdx: -1, proofIdx: -1, proofScope: "", label }];
        const states = kind === "squad" ? SQUAD_STATES : NODE_STATES;
        const menu = document.createElement("div");
        menu.className = "ctx-menu"; menu.id = "queue-menu";
        menu.innerHTML =
          queueViewMenuItems()
          + `<div style="border-top:1px solid var(--border);margin:4px 0"></div>`
          + `<div style="color:var(--muted);font-size:11px;padding:3px 10px 4px;text-transform:uppercase;letter-spacing:.5px">Set ${kind} status to…</div>`
          + states.map((s) => `<div data-click="doPickStatus" data-state="${esc(s)}" data-tip="Set status to ${s}.${s === "ignored" ? "\nIgnored skips it and unblocks downstream like done. Reversible." : (IRREVERSIBLE_STATES.has(s) ? "\nThis cannot be undone." : "")}">${sdot(s)} ${s}</div>`).join("");
        document.body.appendChild(menu);
        menu.style.left = Math.min(e.clientX, window.innerWidth - 220) + "px";
        menu.style.top = Math.min(e.clientY, window.innerHeight - 380) + "px";
      }

      // ---- Set position modal ----
      /**
       * Opens the "Set position" modal for the current queue selection.
       * @returns {void}
       */
      function queuePositionModal() {
        closeQueueMenu();
        const paths = orderedSelection();
        if (!paths.length) return;
        const back = document.createElement("div"); back.className = "q-modal-back"; back.id = "q-pos-modal";
        back.onclick = (ev) => { if (ev.target === back) back.remove(); };
        back.innerHTML = `<div class="q-modal">
            <h3>Set position</h3>
            <p style="color:var(--muted);font-size:12px;margin:0 0 8px">Moving ${paths.length} item(s). Staged until you press Save in the footer. 0 = first (picked up next).</p>
            <input type="number" id="q-pos-input" value="0" min="0" />
            <div class="row">
              <label data-tip="Place the selection at this exact 0-based index in the queue. A very large number drops it to the bottom."><input type="radio" name="q-pos-mode" value="absolute" checked> Absolute index</label>
              <label data-tip="Shift the selection up by this many places (negative moves down)."><input type="radio" name="q-pos-mode" value="relative"> Relative (move up N)</label>
            </div>
            <div class="row" style="justify-content:flex-end">
              <button class="queue-btn" onclick="document.getElementById('q-pos-modal').remove()">Cancel</button>
              <button class="queue-btn" style="border-color:var(--accent)" onclick="queueApplyPosition()">Apply</button>
            </div></div>`;
        document.body.appendChild(back);
        setTimeout(() => document.getElementById("q-pos-input")?.focus(), 20);
      }
      /**
       * Applies the "Set position" modal's absolute/relative move to the current selection.
       * @returns {void}
       */
      function queueApplyPosition() {
        const n = parseInt(/** @type {HTMLInputElement} */ (document.getElementById("q-pos-input")).value, 10);
        const checked = /** @type {HTMLInputElement|null} */ (document.querySelector("input[name=q-pos-mode]:checked"));
        const mode = checked?.value || "absolute";
        const modal = document.getElementById("q-pos-modal"); if (modal) modal.remove();
        if (isNaN(n)) return;
        const paths = orderedSelection(); if (!paths.length) return;
        const movedOrder = queueMoveBlock(queueOrder, paths, n, mode === "absolute");
        /** @type {Set<string>} */
        const pulled = new Set();
        queueOrder = queueAnchoredRepair(movedOrder, new Set(paths), pulled);
        queuePulled = pulled;
        queueDirty = true; renderQueue(); updateQueueFooter();
      }
      /**
       * Computes a reordered queue array with `selected` moved to an absolute or relative position.
       * @param {string[]} current
       * @param {string[]} selected
       * @param {number} position
       * @param {boolean} absolute
       * @returns {string[]}
       */
      function queueMoveBlock(current, selected, position, absolute) {
        const selSet = new Set(selected);
        const remaining = current.filter((p) => !selSet.has(p));
        const block = selected.filter((p) => current.includes(p));
        let at;
        if (absolute) at = Math.max(0, Math.min(position, remaining.length));
        else { const first = current.findIndex((p) => selSet.has(p)); at = Math.max(0, Math.min((first < 0 ? 0 : first) - position, remaining.length)); }
        return [...remaining.slice(0, at), ...block, ...remaining.slice(at)];
      }

      document.addEventListener("keydown", (/** @type {KeyboardEvent} */ e) => {
        if (e.key === "Escape") closeModal();
        else if (e.key === "/" && !e.ctrlKey && !e.metaKey && !e.altKey && !isTypingShortcutTarget(e.target)) {
          e.preventDefault();
          openGotoSearch();
        } else if ((e.key === "l" || e.key === "L") && tab === "squads" && !isTypingShortcutTarget(e.target)) openLogs();
      });
      window.addEventListener("popstate", () => {
        _inPopstate = true;
        try {
          const h = parseHash();
          if (!h || h.tab === "squads") {
            if (h && h.squadId) {
              if (findSquad(h.squadId)) {
                clearNodeMultiSel();
                selectedSquadId = h.squadId;
                revealedSquadId = h.squadId;
                if (h.sel) { const [k, ti, si, vi] = h.sel.split(":"); sel = { kind: k, taskIdx: +ti || 0, cellIdx: +si || 0, proofIdx: vi !== undefined ? +vi : -1 }; }
                else sel = { kind: "squad", taskIdx: 0, cellIdx: 0 };
              } else { pendingHash = h; }
            }
            renderSortChips(); renderStatusFilters();
            showTab("squads");
          } else if (h.tab === "tasks") {
            renderTtStatusFilters();
            if (h.uri) {
              const wantSel = taskTabSelFromUri(h.uri);
              if (wantSel) taskTabSel = wantSel; else pendingHash = h;
            }
            showTab("tasks");
          } else if (h.tab === "reviews") {
            if (h.guardianId) { selectedGuardian = h.guardianId; revealedGuardianId = h.guardianId; }
            renderReviewStatusFilters();
            renderReviewResolverFilters();
            renderReviewOriginFilters();
            showTab("reviews");
          } else if (h.tab === "resources") {
            showTab("resources");
          } else if (h.tab === "queue") {
            showTab("queue");
          } else if (h.tab === "cartographer") {
            applyCartoQuery(h.cartoQuery || {});
            showTab("cartographer");
          } else if (h.tab === "projects") {
            showTab("projects");
          } else if (h.tab === "machines") {
            showTab("machines");
          } else if (h.tab === "triage") {
            showTab("triage");
          } else if (h.tab === "users") {
            showTab("users");
          } else if (h.tab === "secrets") {
            showTab("secrets");
          } else if (h.tab === "prefs") {
            showTab("prefs");
          }
        } finally {
          _inPopstate = false;
        }
      });
      pendingHash = parseHash();
      loadTaskTabPrefs();
      renderStatusFilters(); renderSortChips(); renderTtStatusFilters(); renderReviewStatusFilters(); renderReviewResolverFilters(); renderReviewOriginFilters();
      if (pendingHash && pendingHash.tab === "reviews") { showTab("reviews"); }  // keep pendingHash for pollReviews to apply guardianId
      else if (pendingHash && pendingHash.tab === "tasks") { showTab("tasks"); }  // keep pendingHash for pollTasksTab to apply the selection
      else if (pendingHash && pendingHash.tab === "resources") { pendingHash = null; showTab("resources"); }
      else if (pendingHash && pendingHash.tab === "queue") { pendingHash = null; showTab("queue"); }
      else if (pendingHash && pendingHash.tab === "cartographer") { applyCartoQuery(pendingHash.cartoQuery || {}); pendingHash = null; showTab("cartographer"); }
      else if (pendingHash && pendingHash.tab === "projects") { pendingHash = null; showTab("projects"); }
      else if (pendingHash && pendingHash.tab === "machines") { pendingHash = null; showTab("machines"); }
      else if (pendingHash && pendingHash.tab === "triage") { pendingHash = null; showTab("triage"); }
      else if (pendingHash && pendingHash.tab === "users") { pendingHash = null; showTab("users"); }
      else if (pendingHash && pendingHash.tab === "secrets") { pendingHash = null; showTab("secrets"); }
      else if (pendingHash && pendingHash.tab === "prefs") { pendingHash = null; showTab("prefs"); }
      else { tick(); }
      // RAL-167: push (connectEventStream) is the primary live-update path;
      // this 60s interval is only a reconciliation fallback for a
      // missed/dropped SSE event, and pollOpenPeeks runs on its own separate
      // cadence since live-terminal content isn't Cartographer-backed.
      connectEventStream();
      fetchLiveViewConfigDefault();
      setInterval(tick, 60000);
      setInterval(pollOpenPeeks, 2000);
