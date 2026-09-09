      // ---------- center graph ----------
      /**
       * Renders the center dependency-graph pane for the selected squad.
       * @returns {void}
       */
      function renderGraph() {
        const el = byId("graph");
        const squad = findSquad(selectedSquadId);
        if (!squad) { el.innerHTML = `<div class="empty">Select a squad.</div>`; return; }
        /**
         * @param {string} k
         * @param {number} [t]
         * @param {number} [s]
         * @param {number} [v]
         * @returns {string}
         */
        const selCls = (k, t, s, v = -1) => {
          if (k === "squad") return sel.kind === "squad" ? "sel" : "";
          if ((k === "task" || k === "cell" || k === "proof") && graphNodeSelected(/** @type {"task"|"cell"|"proof"} */ (k), t ?? 0, s ?? 0, v)) return "sel";
          if (sel.kind !== k) return "";
          if (k === "task") return sel.taskIdx === t ? "sel" : "";
          if (k === "cell") return (sel.taskIdx === t && sel.cellIdx === s) ? "sel" : "";
          if (k === "proof") return (sel.taskIdx === t && sel.cellIdx === s && (sel.proofIdx ?? -1) === v) ? "sel" : "";
          return "";
        };
        const banner = `<div class="squad-banner selectable ${sel.kind === "squad" ? "sel" : ""}" onclick="pick('squad')">
            ${sdot(squad.state)}<span class="rid">${esc(squad.label || squad.id)}</span>${copyBtn(squad.label || squad.id)} ${pill(squad.state)}${squadLogsBtn(squad.id)}${squadTimelineBtn(squad.id)}
            <span style="flex:1"></span></div>`;
        const tasks = squad.tasks.map((t, ti) => {
          const cells = t.cells.map((s, si) => `
            <div class="cell selectable ${selCls("cell", ti, si)}" id="n-${ti}-${si}" data-click="onGraphNodeClick" data-ctx="openCellNodeMenu" data-squad-id="${esc(squad.id)}" data-kind="cell" data-ti="${ti}" data-si="${si}" data-vi="-1"
                 data-tip="${esc(s.name ?? s.id)} · ${esc(s.state)}\nAgent: ${esc(s.agent)}${s.model ? " · " + esc(s.model) : ""}${(s.depends_on||[]).length ? "\nDepends on: " + esc((s.depends_on||[]).join(", ")) : ""}\nClick to view details; Shift/Ctrl-click to multi-select; right-click for actions.">
              <div class="sid">${esc(s.name ?? s.id)} ${pill(s.state)}${detachedGraphBadge(s.detached_at_ms)}${s.error ? failLogBtn(s.error) : ""}</div>
              <div class="kv">${esc(s.agent)}${s.model ? " · " + esc(s.model) : ""}</div>
              ${cellProof(s, ti, si, selCls, squad.id)}
            </div>`).join("");
          const tproof = (t.proof || []).map((v, vi) => `<span class="vchip selectable ${selCls("proof", ti, -1, vi)}" id="v-${ti}-${vi}" data-tip="Task proof step: ${esc(v.kind)}${v.id ? " — " + esc(v.id) : ""}\nRuns after all cells finish to validate the task result.\nState: ${esc(v.state)}.\nShift/Ctrl-click to multi-select; right-click for batch actions." data-click="onGraphNodeClick" data-ctx="openProofMenu" data-squad-id="${esc(squad.id)}" data-kind="proof" data-ti="${ti}" data-si="-1" data-vi="${vi}">${sdot(v.state)} ${esc(v.id || v.kind)}</span>`).join("");
          const tdep = (t.depends_on || []).length ? `<span class="k" style="font-size:11px" data-tip="This task depends on ${esc((t.depends_on||[]).join(", "))}.\nIt will not start until the upstream task(s) finish successfully.">⇠ ${esc((t.depends_on||[]).join(", "))}</span>` : "";
          return `<div class="task" id="tk-${ti}">
            <div class="task-head selectable ${selCls("task", ti)}" data-click="onGraphNodeClick" data-ctx="openTaskNodeMenu" data-squad-id="${esc(squad.id)}" data-kind="task" data-ti="${ti}" data-si="-1" data-vi="-1" data-tip="${esc(t.name)} · ${esc(t.state)}\nClick to view details; Shift/Ctrl-click to multi-select; right-click for actions.">${sdot(t.state)} <span>${esc(t.name)}</span> ${pill(t.state)} ${soloBadge(t)} ${tdep}</div>
            <div class="task-body">${cells || '<span class="kv">no cells</span>'}</div>
            ${tproof ? `<div class="proof-chain"><span class="label">proof</span>${tproof}</div>` : ""}
          </div>`;
        }).join("");
        el.innerHTML = banner + tasks;
        requestAnimationFrame(drawArrows);
      }

      // ---------- SVG dependency arrows ----------
      const SVGNS = "http://www.w3.org/2000/svg";
      /**
       * Draws SVG arrows over the graph pane connecting each cell to its dependencies.
       * @returns {void}
       */
      function drawArrows() {
        const graph = byId("graph");
        const existing = document.getElementById("arrow-svg");
        if (existing) existing.remove();
        const squad = findSquad(selectedSquadId);
        if (!squad) return;
        // Map every dependency reference (cell id and "taskName/cellId") to a node id.
        /** @type {{[key: string]: string}} */
        const byRef = {};
        squad.tasks.forEach((t, ti) => t.cells.forEach((s, si) => {
          byRef[s.id] = `n-${ti}-${si}`;
          byRef[`${t.name}/${s.id}`] = `n-${ti}-${si}`;
        }));
        // First task with a given name wins, so a later name collision can't
        // silently overwrite an earlier task's node id.
        /** @type {{[key: string]: string}} */
        const taskByName = {};
        squad.tasks.forEach((t, ti) => { if (!(t.name in taskByName)) taskByName[t.name] = `tk-${ti}`; });
        /** @type {{from: string, to: string, kind: string}[]} */
        const edges = [];
        // task -> task dependencies (routed card-to-card, in the gaps)
        squad.tasks.forEach((t, ti) => (t.depends_on || []).forEach((dep) => {
          const from = taskByName[dep]; if (from) edges.push({ from, to: `tk-${ti}`, kind: "dep" });
        }));
        // cell -> cell dependencies. A cell's own depends_on is a
        // within-task (bare) cell id, so resolve it qualified by the
        // owning task first -- the bare key can be last-write-wins clobbered
        // by another task reusing the same cell id (e.g. "work").
        squad.tasks.forEach((t, ti) => t.cells.forEach((s, si) => {
          (s.depends_on || []).forEach((dep) => {
            const from = byRef[`${t.name}/${dep}`] || byRef[dep];
            if (from) edges.push({ from, to: `n-${ti}-${si}`, kind: "dep" });
          });
        }));
        // proof chain
        squad.tasks.forEach((t, ti) => {
          const vs = t.proof || [];
          for (let i = 1; i < vs.length; i++) edges.push({ from: `v-${ti}-${i - 1}`, to: `v-${ti}-${i}`, kind: "vchain" });
        });
        if (!edges.length) return;
        const gr = graph.getBoundingClientRect();
        /**
         * @param {string} id
         * @returns {DOMRect|null}
         */
        const rectOf = (id) => { const e = document.getElementById(id); return e ? e.getBoundingClientRect() : null; };
        /**
         * @param {DOMRect} r
         * @returns {number}
         */
        const px = (r) => r.left - gr.left + graph.scrollLeft;
        /**
         * @param {DOMRect} r
         * @returns {number}
         */
        const py = (r) => r.top - gr.top + graph.scrollTop;
        const svg = document.createElementNS(SVGNS, "svg");
        svg.id = "arrow-svg";
        svg.setAttribute("width", String(graph.scrollWidth));
        svg.setAttribute("height", String(graph.scrollHeight));
        // Above the cards (z-index 5 > card z-index 1) but click-through.
        svg.style.cssText = "position:absolute;left:0;top:0;pointer-events:none;z-index:5;overflow:visible";
        svg.innerHTML = `<defs>
          <marker id="ah-dep" markerWidth="8" markerHeight="8" refX="6" refY="4" orient="auto"><path d="M0,0 L8,4 L0,8 z" fill="var(--muted)"/></marker>
          <marker id="ah-v" markerWidth="8" markerHeight="8" refX="6" refY="4" orient="auto"><path d="M0,0 L8,4 L0,8 z" fill="var(--teal)"/></marker>
        </defs>`;
        // Anchor on the facing edges so the line travels in the gap, not across faces.
        /**
         * @param {DOMRect} a
         * @param {DOMRect} b
         * @returns {{x1: number, y1: number, x2: number, y2: number, h: boolean}}
         */
        const anchor = (a, b) => {
          const acx = px(a) + a.width / 2, acy = py(a) + a.height / 2;
          const bcx = px(b) + b.width / 2, bcy = py(b) + b.height / 2;
          const dx = bcx - acx, dy = bcy - acy;
          if (Math.abs(dx) > Math.abs(dy)) {
            return { x1: dx > 0 ? px(a) + a.width : px(a), y1: acy, x2: dx > 0 ? px(b) : px(b) + b.width, y2: bcy, h: true };
          }
          return { x1: acx, y1: dy > 0 ? py(a) + a.height : py(a), x2: bcx, y2: dy > 0 ? py(b) : py(b) + b.height, h: false };
        };
        edges.forEach((e) => {
          const a = rectOf(e.from), b = rectOf(e.to); if (!a || !b) return;
          const { x1, y1, x2, y2, h } = anchor(a, b);
          const d = h
            ? `M${x1},${y1} C${(x1 + x2) / 2},${y1} ${(x1 + x2) / 2},${y2} ${x2},${y2}`
            : `M${x1},${y1} C${x1},${(y1 + y2) / 2} ${x2},${(y1 + y2) / 2} ${x2},${y2}`;
          const p = document.createElementNS(SVGNS, "path");
          p.setAttribute("d", d);
          p.setAttribute("fill", "none");
          p.setAttribute("stroke", e.kind === "dep" ? "var(--muted)" : "var(--teal)");
          p.setAttribute("stroke-width", "1.75");
          p.setAttribute("opacity", "0.9");
          p.setAttribute("marker-end", e.kind === "dep" ? "url(#ah-dep)" : "url(#ah-v)");
          svg.appendChild(p);
        });
        graph.style.position = "relative";
        graph.appendChild(svg);
      }
      window.addEventListener("resize", () => { applyPaneWidths(); requestAnimationFrame(drawArrows); });
      /**
       * Selects a task/cell/proof node in the graph and shows it in the details pane.
       * @param {"squad"|"task"|"cell"|"proof"} kind
       * @param {number} [ti]
       * @param {number} [si]
       * @param {number} [vi]
       * @returns {void}
       */
      function pick(kind, ti = 0, si = 0, vi = -1) {
        if (kind === "squad") clearNodeMultiSel();
        else if (selectedSquadId && (kind === "task" || kind === "cell" || kind === "proof")) {
          const item = graphNodeItem(selectedSquadId, kind, ti, si, vi);
          nodeMultiSel = item ? new Set([item.key]) : new Set();
        }
        sel = { kind, taskIdx: ti, cellIdx: si, proofIdx: vi };
        editing = false;
        renderGraph();
        renderDetails();
        syncHash(true);
      }

      // ---------- right details pane ----------
      /**
       * Renders the right details pane for the current selection.
       * @returns {void}
       */
      function renderDetails() {
        const el = byId("details");
        const squad = findSquad(selectedSquadId);
        if (!squad || !sel.kind) { el.innerHTML = `<div class="empty">Select a squad, task, cell, or proof step.</div>`; return; }
        const tabs = multiSel.size > 1 ? selectionTabs() : "";
        if (editing) { el.innerHTML = tabs + editForm(squad); return; }
        if (sel.kind === "squad") el.innerHTML = tabs + squadView(squad);
        else if (sel.kind === "task") el.innerHTML = tabs + taskView(squad, squad.tasks[sel.taskIdx]);
        else if (sel.kind === "proof") el.innerHTML = tabs + proofView(squad);
        else el.innerHTML = tabs + cellView(squad, squad.tasks[sel.taskIdx], squad.tasks[sel.taskIdx].cells[sel.cellIdx]);
        attachPeekResizeHandlers();
      }
      /**
       * Renders the shared "Edit" button row shown at the bottom of the details pane.
       * @returns {string}
       */
      const editBtn = () => `<div class="btn-row"><button class="btn primary" onclick="startEdit()" data-tip="Edit this item — change its label, prompt, agent, model, or command.\nIf the squad is currently running it will be stopped and reset to Pending.">✎ Edit</button></div>`;
      /**
       * Formats a Unix-epoch-ms timestamp as an unambiguous UTC string for the
       * Details Pane's "started at" field.
       * @param {number} ms
       * @returns {string}
       */
      function fmtUtc(ms) {
        return `${new Date(ms).toISOString().replace("T", " ").replace(/\.\d+Z$/, "")} UTC`;
      }
      /**
       * Formats a millisecond duration as a compact "1h 5m 32s"-style string
       * for the Details Pane's "time running" field.
       * @param {number} ms
       * @returns {string}
       */
      function fmtDuration(ms) {
        const totalSec = Math.max(0, Math.floor(ms / 1000));
        const h = Math.floor(totalSec / 3600);
        const m = Math.floor((totalSec % 3600) / 60);
        const s = totalSec % 60;
        if (h > 0) return `${h}h ${m}m ${s}s`;
        if (m > 0) return `${m}m ${s}s`;
        return `${s}s`;
      }
      /**
       * Renders the shared "started at" (UTC) / "time running" kv-rows for a
       * squad/task/cell's Details Pane, from its started_at_ms/finished_at_ms.
       * Live (ticks via the normal 2s poll re-render) while unfinished;
       * frozen at the total elapsed time once finished.
       * @param {number|null|undefined} startedAtMs
       * @param {number|null|undefined} finishedAtMs
       * @returns {string}
       */
      function timingRows(startedAtMs, finishedAtMs) {
        const startedTip = "When this first started executing, in UTC — not when it was submitted or queued, if those differ."
          + "\nUseful for confirming exactly when execution began without doing timezone math from a local timestamp.";
        const runningTip = "How long this has been executing."
          + "\nUpdates live while still running; freezes at the total elapsed time once it finishes."
          + "\nUseful for spotting a squad/task/cell that's taking longer than expected without cross-referencing logs.";
        if (!startedAtMs) {
          return `<div class="kv-row"><span class="k">started at</span><span class="v" data-tip="${startedTip}">—</span></div>
            <div class="kv-row"><span class="k">time running</span><span class="v" data-tip="${runningTip}">—</span></div>`;
        }
        const elapsed = (finishedAtMs || Date.now()) - startedAtMs;
        const running = !finishedAtMs;
        const runningAttrs = running ? ` data-running="1" data-started="${startedAtMs}"` : "";
        return `<div class="kv-row"><span class="k">started at</span><span class="v mono" data-tip="${startedTip}">${fmtUtc(startedAtMs)}</span></div>
          <div class="kv-row"><span class="k">time running</span><span class="v"${runningAttrs} data-tip="${runningTip}">${fmtDuration(elapsed)}</span></div>`;
      }
      /**
       * Renders the squad-level details pane.
       * @param {SquadView} r
       * @returns {string}
       */
      function squadView(r) {
        const rReviews = r.reviews || [];
        const reviews = rReviews.length
          ? `<h3 class="section">reviews</h3>` + rReviews.map((rv) =>
              `<div class="kv-row"><span>${gdot(rv.status)}</span><span class="v"><a href="#" data-click="gotoReview" data-guardian-id="${esc(rv.id)}" style="color:var(--accent)" data-tip="Open this review on the Reviews tab.">${esc(rv.name)}</a></span><span class="k">${esc(rv.status)}</span></div>`).join("")
          : "";
        const tasks = (r.tasks || []).length
          ? `<h3 class="section">tasks</h3>` + r.tasks.map((t, ti) =>
              `<div class="kv-row"><span>${sdot(t.state)}</span>` +
              `<span class="v" style="flex:1">${esc(t.name)}</span>` +
              `<span class="k">${esc(t.state)}</span>` +
              `<button class="btn" data-click="pick" data-kind="task" data-ti="${ti}" data-tip="Jump to this task's detail pane — view its cells and proof steps.">Go</button></div>`).join("")
          : `<h3 class="section">tasks</h3><div class="kv-row"><span class="v">None</span></div>`;
        return `<div class="dhead"><span class="k">▶ squad</span></div>
          <div class="kv-row"><span class="k">id</span><span class="v mono">${esc(r.id)}${copyBtn(r.id)}</span></div>
          <div class="kv-row"><span class="k">label</span><span class="v">${esc(r.label || "—")}</span></div>
          <div class="kv-row"><span class="k">state</span><span class="v"${isDowntimeWaiting(r) ? ` data-tip="${WAITING_TIP}"` : ""}>${pill(squadDisplayState(r))}${["pending","queued"].includes(r.state) ? "" : squadLogsBtn(r.id) + squadTimelineBtn(r.id)}</span></div>
          ${timingRows(r.started_at_ms, r.finished_at_ms)}
          <div class="kv-row"><span class="k">watchers</span><span class="v">${watchersHtml(`squad:${r.id}`)}</span></div>
          ${tasks}
          ${reviews}
          ${envOverridesSection(r)}
          ${editBtn()}`;
      }
      /**
       * Shared markup for an "environment overrides" section at any scope
       * (squad/task/task-proof/cell/cell-proof; hierarchical env
       * overrides, extending RAL-150): a warning banner when non-empty, a
       * row per KEY/VALUE with "Edit"/"Remove" buttons, and an "+ Add
       * override" button. Always rendered, even when empty, so the
       * add-override affordance stays reachable. `label` is the
       * human-readable scope name used in the heading/tooltips (e.g.
       * "squad", "task", "task proof steps"); `apiPath` is the
       * `POST .../env`-shaped endpoint this scope's overrides live at.
       *
       * RAL-271: "Edit" changes one key's value in place (via the same
       * `{set: ...}` body "+ Add override" uses) without a Remove +
       * re-Add round trip.
       * @param {{[key: string]: string} | undefined} overrides
       * @param {string} label
       * @param {string} apiPath
       * @returns {string}
       */
      function renderEnvOverrides(overrides, label, apiPath) {
        const map = overrides || {};
        const keys = Object.keys(map).sort();
        const banner = keys.length
          ? `<div class="kv-row" data-tip="Retries scoped to this ${esc(label)} will use these overridden values instead of the environment ralphus would otherwise use.\nA parent scope's value for the same key is shadowed here.">⚠ ${keys.length} override${keys.length === 1 ? "" : "s"} active — retries are not identical replays</div>`
          : "";
        const rows = keys.length
          ? keys.map((k) =>
              `<div class="kv-row"><span class="v mono" style="flex:1">${esc(k)}</span>` +
              `<span class="v mono" style="flex:1">${esc(map[k])}</span>` +
              `<button class="btn" data-click="editEnvOverrideAt" data-api-path="${esc(apiPath)}" data-key="${esc(k)}" data-value="${esc(map[k])}" data-tip="Change this environment-variable override's value in place, without removing and re-adding it.\nWho/when: quick iteration, e.g. tweaking a flag for a single retry.\nApplies the next time this ${esc(label)} executes.">✎ Edit</button>` +
              `<button class="btn" data-click="removeEnvOverrideAt" data-api-path="${esc(apiPath)}" data-key="${esc(k)}" data-tip="Remove this environment-variable override from this ${esc(label)}.\nFuture retries here fall back to whatever the next-higher scope resolves to.\nThis cannot be undone.">✕ Remove</button></div>`).join("")
          : `<div class="kv-row"><span class="v">None</span></div>`;
        return `<h3 class="section">environment overrides (${esc(label)})</h3>
          ${banner}
          ${rows}
          <div class="kv-row">${envViewerBtn(apiPath, `this ${label}`)}<button class="btn" data-click="addEnvOverrideAt" data-api-path="${esc(apiPath)}" data-tip="Set a persistent environment-variable override for this ${esc(label)} (hierarchical env overrides, extending RAL-150).\nUse this to retry with a different model/resolver, feature flag, or credential scoped to just this ${esc(label)}, without editing and resubmitting the TOML.\nOverrides a parent scope's value for the same key; applies the next time this ${esc(label)} executes, and stays set across any number of retries until removed.">+ Add override</button></div>`;
      }
      /**
       * Renders the "environment overrides" section of the squad details pane.
       * @param {SquadView} r
       * @returns {string}
       */
      function envOverridesSection(r) {
        return renderEnvOverrides(r.env_overrides, "squad", `/api/squads/${r.id}/env`);
      }
      /**
       * Renders the task-level "environment overrides" section of the task
       * details pane — overrides here win over the squad's, for every cell
       * under this task.
       * @param {SquadView} r
       * @param {number} ti
       * @param {TaskView} t
       * @returns {string}
       */
      function taskEnvOverridesSection(r, ti, t) {
        return renderEnvOverrides(t.env_overrides, "task", `/api/squads/${r.id}/tasks/${ti}/env`);
      }
      /**
       * Renders the "environment overrides" section for a task's own
       * (task-scoped) proof steps — overrides here win over the task's own
       * (and the squad's), but only for this task's proof steps.
       * @param {SquadView} r
       * @param {number} ti
       * @param {TaskView} t
       * @returns {string}
       */
      function taskProofEnvOverridesSection(r, ti, t) {
        return renderEnvOverrides(
          t.proof_env_overrides,
          "task proof steps",
          `/api/squads/${r.id}/tasks/${ti}/proof/env`,
        );
      }
      /**
       * Renders the cell-level "environment overrides" section of the
       * cell details pane — overrides here win over the owning task's
       * (and the squad's), for this cell's own subprocess.
       * @param {SquadView} r
       * @param {number} ti
       * @param {number} si
       * @param {CellView} s
       * @returns {string}
       */
      function cellEnvOverridesSection(r, ti, si, s) {
        return renderEnvOverrides(
          s.env_overrides,
          "cell",
          `/api/squads/${r.id}/cells/${ti}/${si}/env`,
        );
      }
      /**
       * Renders the "environment overrides" section for a cell's own
       * (cell-scoped) proof steps — overrides here win over the
       * cell's own (and its task's/squad's), but only for this cell's
       * proof steps.
       * @param {SquadView} r
       * @param {number} ti
       * @param {number} si
       * @param {CellView} s
       * @returns {string}
       */
      function cellProofEnvOverridesSection(r, ti, si, s) {
        return renderEnvOverrides(
          s.proof_env_overrides,
          "cell proof steps",
          `/api/squads/${r.id}/cells/${ti}/${si}/proof/env`,
        );
      }
      /**
       * Renders the "environment overrides" section for **one individual**
       * proof step (RAL-191) — the narrowest layer, winning over the
       * scope-wide proof overrides rendered alongside it. Kept separate from
       * those because `proof` is an array: two steps under the same task can
       * set the same key to different values.
       * @param {SquadView} r
       * @param {number} ti
       * @param {number} si - Owning cell index, or -1 for a task-scoped step.
       * @param {number} vi
       * @returns {string}
       */
      function proofStepEnvOverridesSection(r, ti, si, vi) {
        const t = r.tasks[ti];
        if (!t) return "";
        const v = si === -1
          ? (t.proof || [])[vi]
          : ((t.cells || [])[si] || /** @type {CellView} */ ({})).proof?.[vi];
        if (!v) return "";
        const apiPath = si === -1
          ? `/api/squads/${r.id}/tasks/${ti}/proof/${vi}/env`
          : `/api/squads/${r.id}/cells/${ti}/${si}/proof/${vi}/env`;
        return renderEnvOverrides(v.env_overrides, "this proof step", apiPath);
      }
      /**
       * Prompts for a KEY and VALUE and sets it as a persistent
       * environment-variable override at `apiPath` (hierarchical env
       * overrides, extending RAL-150) — shared by every scope's "+ Add
       * override" button.
       * @param {string} apiPath
       * @returns {Promise<void>}
       */
      async function addEnvOverrideAt(apiPath) {
        const rawKey = prompt("Environment variable name (e.g. RALPHUS_RESOLVER_MODEL):");
        if (rawKey === null) return;
        const key = rawKey.trim();
        if (!/^[A-Za-z_][A-Za-z0-9_]*$/.test(key)) {
          alert(`"${key}" is not a valid environment variable name.`);
          return;
        }
        const value = prompt(`Value for ${key}:`);
        if (value === null) return;
        const res = await post(apiPath, { set: { [key]: value } });
        if (!res.ok) { alert("Failed to set the environment override."); return; }
        tick();
      }
      /**
       * Prompts for a new VALUE for an already-set KEY and updates it in
       * place at `apiPath` (same `{set: ...}` semantics as
       * `addEnvOverrideAt`) — shared by every scope's "Edit" button
       * (RAL-271). Lets a single value be tweaked without a Remove +
       * re-Add round trip; the prompt is pre-filled with the current value.
       * @param {string} apiPath
       * @param {string} key
       * @param {string} currentValue
       * @returns {Promise<void>}
       */
      async function editEnvOverrideAt(apiPath, key, currentValue) {
        const value = prompt(`New value for ${key}:`, currentValue);
        if (value === null || value === currentValue) return;
        const res = await post(apiPath, { set: { [key]: value } });
        if (!res.ok) { alert("Failed to update the environment override."); return; }
        tick();
      }
      /**
       * Removes a persistent environment-variable override at `apiPath`
       * (hierarchical env overrides, extending RAL-150) — shared by every
       * scope's "Remove" button.
       * @param {string} apiPath
       * @param {string} key
       * @returns {Promise<void>}
       */
      async function removeEnvOverrideAt(apiPath, key) {
        if (!confirm(`Remove environment override "${key}"?\nThis cannot be undone.`)) return;
        const res = await post(apiPath, { unset: [key] });
        if (!res.ok) { alert("Failed to remove the environment override."); return; }
        tick();
      }
      /**
       * Navigates to the Reviews tab for a given guardian id.
       * @param {string} id
       * @returns {void}
       */
      function gotoReview(id) { selectedGuardian = id; revealedGuardianId = id; showTab("reviews", true); }
      /**
       * Navigates to a specific branch within a review, expanding its detail block.
       * @param {string} gid
       * @param {string} branch
       * @returns {void}
       */
      function gotoReviewBranch(gid, branch) {
        selectedGuardian = gid;
        revealedGuardianId = gid;
        selectedBranch[gid] = branch;
        // Auto-expand the branch so its detail is visible.
        const g = guardians.find((x) => x.id === gid);
        if (g) {
          const b = g.branches.find((br) => br.branch === branch);
          if (b != null) expandedBranches.add(`${gid}:${b.id}`);
        }
        showTab("reviews", true);
      }
      /**
       * Toggles a branch row's selection within a review's branch list.
       * @param {MouseEvent} e
       * @param {string} gid
       * @param {string} branch
       * @returns {void}
       */
      function selectBranchRow(e, gid, branch) {
        // Toggle: clicking a selected branch deselects it; clicking another selects it.
        selectedBranch[gid] = selectedBranch[gid] === branch ? null : branch;
        renderReviewDetail();
      }
      /**
       * Navigates to the Squads tab and selects a squad.
       * @param {string} id
       * @returns {void}
       */
      function gotoSquad(id) { clearNodeMultiSel(); selectedSquadId = id; revealedSquadId = id; sel = { kind: "squad", taskIdx: 0, cellIdx: 0 }; editing = false; showTab("squads", true); }
      /**
       * Navigates to the Squads tab and selects a specific task/cell/proof item within a squad.
       * @param {string} squadId
       * @param {string} kind
       * @param {number} ti
       * @param {number} si
       * @param {number} vi
       * @returns {void}
       */
      function gotoSquadItem(squadId, kind, ti, si, vi) { clearNodeMultiSel(); selectedSquadId = squadId; revealedSquadId = squadId; sel = { kind, taskIdx: ti, cellIdx: si, proofIdx: vi }; editing = false; showTab("squads", true); }
      /**
       * Builds the tooltip text for an inherited resolved value in the details pane.
       * @param {string} field
       * @param {string} sourceLevel
       * @param {string} detail
       * @returns {string}
       */
      function inheritedValueTip(field, sourceLevel, detail) {
        return `${field} inherited from ${sourceLevel}.\nWho/when: check this to see which TOML scope currently controls the resolved value.\n${detail}`;
      }
      /**
       * Renders one details-pane value span, optionally marked as inherited.
       * @param {string} value
       * @param {string} tip
       * @returns {string}
       */
      function detailValueHtml(value, tip) {
        const tipAttr = tip ? ` data-tip="${esc(tip)}"` : "";
        const klass = tip ? "v inherited-value" : "v";
        return `<span class="${klass}"${tipAttr}>${esc(value)}</span>`;
      }
      /**
       * Returns the cell-agent inheritance tooltip, or an empty string when the
       * cell's agent was set explicitly at the cell level.
       * @param {TaskView} t
       * @param {CellView} s
       * @returns {string}
       */
      function cellAgentInheritanceTip(t, s) {
        if (t.agent && s.agent === t.agent) {
          return inheritedValueTip(
            "agent",
            "task level",
            "Edit the task's `agent` to change every inheriting cell, or set this cell's own `agent` to override it.",
          );
        }
        if (t.agent == null && s.agent === DEFAULT_AGENT) {
          return inheritedValueTip(
            "agent",
            "task level",
            `The task left \`agent\` unset, so the built-in default \"${DEFAULT_AGENT}\" applies until a task/cell override is added.`,
          );
        }
        return "";
      }
      /**
       * Returns the cell-model inheritance tooltip, or an empty string when the
       * cell's model was set explicitly at the cell level.
       * @param {TaskView} t
       * @param {CellView} s
       * @returns {string}
       */
      function cellModelInheritanceTip(t, s) {
        if (t.model != null && s.model === t.model) {
          return inheritedValueTip(
            "model",
            "task level",
            "Edit the task's `model` to change every inheriting cell, or set this cell's own `model` to override it.",
          );
        }
        return "";
      }
      /**
       * Renders the task-level details pane.
       * @param {SquadView} r
       * @param {TaskView} t
       * @returns {string}
       */
      function taskView(r, t) {
        const ti = sel.taskIdx;
        const cells = (t.cells || []).length
          ? `<h3 class="section">cells</h3>` + t.cells.map((s, si) =>
              `<div class="kv-row"><span>${sdot(s.state)}</span>` +
              `<span class="v mono" style="flex:1">${esc(s.name ?? s.id)}</span>` +
              `<span class="k">${esc(s.state)}</span>` +
              `<button class="btn" data-click="pick" data-kind="cell" data-ti="${ti}" data-si="${si}" data-tip="Jump to this cell's detail pane — view its prompt, command, and run details.">Go</button></div>`).join("")
          : `<h3 class="section">cells</h3><div class="kv-row"><span class="v">None</span></div>`;
        const tProof = t.proof || [];
        const proof = tProof.length
          ? `<h3 class="section">proof steps</h3>` + tProof.map((v, vi) =>
              `<div class="kv-row"><span>${sdot(v.state)}</span>` +
              `<span class="v" style="flex:1">${esc(v.id || v.kind)}</span>` +
              `<span class="k">${esc(v.state)}</span>` +
              `<button class="btn" data-click="pick" data-kind="proof" data-ti="${ti}" data-si="-1" data-vi="${vi}" data-tip="Jump to this proof step's detail pane — view its kind, state, and output.">Go</button></div>`).join("")
          : `<h3 class="section">proof steps</h3><div class="kv-row"><span class="v">None</span></div>`;
        return `<div class="dhead"><span class="k">⯀ task</span></div>
          <div class="kv-row"><span class="k">name</span><span class="v">${esc(t.name)}</span></div>
          <div class="kv-row"><span class="k">project</span><span class="v">${esc(t.project)}</span></div>
          <div class="kv-row"><span class="k">state</span><span class="v">${pill(t.state)}${outOfDateBadge(t.env_out_of_date)}${["pending","queued"].includes(t.state) ? "" : squadLogsBtn(r.id)}${t.error ? failLogBtn(t.error) : ""}</span></div>
          ${timingRows(t.started_at_ms, t.finished_at_ms)}
          ${cells}
          ${proof}
          ${taskEnvOverridesSection(r, ti, t)}
          ${editBtn()}`;
      }
      /**
       * Renders the proof-step-level details pane.
       * @param {SquadView} r
       * @returns {string}
       */
      function proofView(r) {
        const ti = sel.taskIdx, si = sel.cellIdx, vi = sel.proofIdx ?? -1;
        const t = r.tasks[ti];
        if (!t) return `<div class="empty">Task not found.</div>`;
        let v, scopeLabel, envSection, parentCell;
        // RAL-172: matches the daemon's own key format (scheduler.rs::run_proofs)
        // for the Cartographer `cell_id` tag it logs on each "proof completed"
        // event — task-scope steps have no owning cell, so the key is just the
        // step's position; cell-scope steps prefix it with the owning cell's id.
        const proofCartoKey = si === -1 ? `task-proof:${vi}` : `cell-proof:${(t.cells || [])[si] ? (t.cells || [])[si].id : ""}:${vi}`;
        const proofCartoElId = `cum-cost-proof-${ti}-${proofCartoKey.replace(/[^a-zA-Z0-9_-]/g, "_")}`;
        if (si === -1) {
          v = (t.proof || [])[vi];
          parentCell = (t.cells || [])[0];
          scopeLabel = "task";
          // RAL-191: the scope-wide layer first, then this individual step's
          // own — narrowest last, matching the order they resolve in.
          envSection = taskProofEnvOverridesSection(r, ti, t)
            + proofStepEnvOverridesSection(r, ti, -1, vi);
        } else {
          parentCell = (t.cells || [])[si];
          v = parentCell && (parentCell.proof || [])[vi];
          scopeLabel = parentCell ? `cell · ${esc(parentCell.name ?? parentCell.id)}` : "cell";
          envSection = parentCell
            ? cellProofEnvOverridesSection(r, ti, si, parentCell)
              + proofStepEnvOverridesSection(r, ti, si, vi)
            : "";
        }
        if (!v) return `<div class="empty">Proof step not found.</div>`;
        const parentModel = parentCell ? parentCell.model : t.model;
        const proofModel = v.model || parentModel || null;
        const proofModelTip = (!v.model && parentModel)
          ? inheritedValueTip(
              "model",
              parentCell ? "cell level" : "task level",
              "Edit the parent scope's `model` to change this proof step, or set this proof step's own `model` to override it.",
            )
          : "";
        const logBtn = v.output
          ? `<button class="logs-btn" data-tip="View the raw output from this proof step — the agent's response or command stdout/stderr." data-full="${esc(v.output)}" onclick="openCmdPopup(event)">📄 Output log</button>`
          : "—";
        const specSection = v.kind === "command"
          ? `<h3 class="section">command</h3>${cmdBox(v.spec || "")}`
          : (v.spec ? `<h3 class="section">prompt</h3>${promptBox(v.spec)}` : "");
        const systemPromptSection = v.system_prompt
          ? `<h3 class="section">system prompt</h3>${promptBox(v.system_prompt, null, SYSTEM_PROMPT_TIP)}`
          : "";
        const terminalBtns = (() => {
          const scope = si === -1 ? "task" : "cell";
          const liveViewCapable = v.kind === "prompt" || v.kind === "command";
          if (!liveViewCapable) {
            const tip = "Terminal access is not available — this proof step has no execution backend to show a live view for.";
            return `<div class="btn-row"><span data-tip="${tip}"><button class="btn primary" disabled style="pointer-events:none">Show Live View</button></span></div>`;
          }
          // RAL-102/RAL-151: every command/prompt proof step runs inside
          // tmux, so the dropdown is available for either kind — "not
          // currently live" is handled gracefully at click/peek time, not by
          // pre-disabling. "Open Agent" only makes sense for a prompt-kind
          // step (there is no agent conversation to resume for a plain
          // shell command), so it's omitted outright for command-kind
          // rather than shown disabled.
          const key = `proof|${r.id}|${ti}|${scope}|${si}|${vi}`;
          const previewBtn = `<button class="btn primary" data-click="togglePeek" data-key="${esc(key)}" style="border-radius:6px 0 0 6px" data-tip="Peek at this proof step's live tmux pane — auto-refreshing, read-only.\nNothing you do here is ever sent to the agent.\nShows 'terminal session has ended' once the proof step isn't running.">${peekOpen[key] ? "Hide Live View" : "Show Live View"}</button>`;
          const openAgentItem = v.kind === "prompt"
            ? openAgentMenuItem(key, "openAgentProofTerminalMenuItem",
                { squadId: r.id, ti, scope, si, vi },
                v.state === "running", v.agent_session_id, v.agent)
            : "";
          const items = terminalMenuItem(key, "Open Terminal Log",
              "openProofTerminalMenuItem", { squadId: r.id, ti, scope, si, vi },
              "Attach an interactive terminal to this proof step's live tmux cell.\nShows the runner's own log/event stream, not the agent's actual conversation.\nOnly available while the proof step is actively running — fails gracefully otherwise.")
            + openAgentItem
            + terminalMenuItem(key, "View Attempt History",
                "toggleHistoryMenuItem", {},
                "List every durably-persisted terminal-log attempt for this proof step, including past reattaches.\nWho/when: the pane died or reattached and you need to see what happened right before, after the live view is gone.\nEach attempt's log survives pane death and daemon restarts.");
          return `<div class="btn-row" style="position:relative;gap:0">${previewBtn}${terminalMenuHtml(key, items)}</div>${peekBox(key)}${historyBox(key)}`;
        })();
        return `<div class="dhead"><span class="k">✓ proof step</span></div>
          <div class="kv-row"><span class="k">id</span><span class="v mono">${esc(v.id || "—")}</span></div>
          <div class="kv-row"><span class="k">kind</span><span class="v">${esc(v.kind)}</span></div>
          <div class="kv-row"><span class="k">scope</span><span class="v">${scopeLabel}</span></div>
          <div class="kv-row"><span class="k">task</span><span class="v">${esc(t.name)}</span></div>
          <div class="kv-row"><span class="k">state</span><span class="v">${pill(v.state)}${outOfDateBadge(v.env_out_of_date)}</span></div>
          <div class="kv-row"><span class="k">model</span>${detailValueHtml(proofModel || "—", proofModelTip)}</div>
          <div class="kv-row" data-tip="${PROOF_TOKENS_COST_TIP}"><span class="k">tokens</span><span class="v">${usageSummary(v)}${estimatedBadge(v.cost_is_estimated)}</span></div>
          <div class="kv-row" data-tip="${COMPACTION_TIP}"><span class="k">compaction</span><span class="v">${compactionSummary(v, proofModel)}</span></div>
          <div class="kv-row" data-tip="${PROOF_LIFETIME_COST_TIP}"><span class="k">lifetime</span><span class="v"><span id="${proofCartoElId}">—</span> <button class="btn" data-click="loadCumulativeCost" data-carto-key="${esc(proofCartoKey)}" data-carto-kind="proof" data-el-id="${esc(proofCartoElId)}" data-squad-id="${esc(r.id)}" data-tip="${PROOF_LIFETIME_COST_TIP}">Σ load total</button></span></div>
          <div class="kv-row"><span class="k">result</span><span class="v">${logBtn}</span></div>
          ${maximumToolOutputTokensRow(v)}
          ${specSection}${systemPromptSection}${terminalBtns}
          ${envSection}`;
      }

      // CCTL-148: worktree (the cell's own cwd) and project (its shared git
      // root) are distinct — fetch the derived project root lazily and cache it.
      /**
       * Lazily fetches and caches a squad's per-cell worktree/project paths.
       * @param {string} id
       * @returns {Promise<void>}
       */
      async function loadSquadPaths(id) {
        if (!id || squadPaths[id]) return;
        squadPaths[id] = {}; // mark loading so we don't refetch on every re-render
        try {
          /** @type {CellPathInfo[]} */
          const arr = await (await fetch(`/api/squads/${id}/worktrees`)).json();
          /** @type {{[key: string]: CellPathInfo}} */
          const map = {}; arr.forEach((p) => { map[`${p.task_idx}:${p.cell_idx}`] = p; });
          squadPaths[id] = map;
          if (selectedSquadId === id && sel.kind === "cell") renderDetails();
        } catch (_) { delete squadPaths[id]; }
      }
      /**
       * Renders the RAL-304 context-window controls row (`maximum_context`/
       * `auto_compact_threshold`) for the cell details pane, or an empty
       * string when neither is set — most cells set neither, so the row is
       * omitted rather than shown empty.
       * @param {CellView} s
       * @returns {string}
       */
      function contextLimitsRow(s) {
        if (s.maximum_context == null && s.auto_compact_threshold == null) return "";
        const parts = [];
        if (s.maximum_context != null) parts.push(`max ${s.maximum_context.toLocaleString()}`);
        if (s.auto_compact_threshold != null) parts.push(`compact @ ${s.auto_compact_threshold.toLocaleString()}`);
        return `<div class="kv-row" data-tip="${CONTEXT_LIMITS_TIP}"><span class="k">context limits</span><span class="v">${parts.join(" · ")}</span></div>`;
      }
      /**
       * Renders the RAL-333 tool-output token cap row (`maximum_tool_output_tokens`)
       * for the cell/proof-step details pane, always shown — "—" when unset
       * (RAL-356), matching the placeholder style used elsewhere in this pane.
       * @param {CellView|ProofView} s
       * @returns {string}
       */
      function maximumToolOutputTokensRow(s) {
        const value = s.maximum_tool_output_tokens == null ? "—" : `${s.maximum_tool_output_tokens.toLocaleString()} tokens`;
        return `<div class="kv-row" data-tip="${MAXIMUM_TOOL_OUTPUT_TOKENS_TIP}"><span class="k">tool output cap</span><span class="v">${value}</span></div>`;
      }
      /**
       * Renders a placeholder for a Triage-opted-in cell (RAL-318) that
       * hasn't been swept into an actual review yet, shown in the cell
       * details pane's "in reviews" section in place of a real review link.
       * Empty once a real review exists (`s.reviews` non-empty) or the cell
       * never opted into Triage (`s.triage_types` empty) -- `cellView()`
       * only calls this as the fallback when `s.reviews` is empty.
       *
       * Three states, matching the cell's own `state` rather than any
       * stored Triage/pool field (there's nothing to poll for beyond what
       * `CellView` already carries -- and `CellView.state` is already the
       * proof-aware effective state, not the raw persisted column): "failed"
       * (permanently excluded -- a failed cell can never pass review, so it
       * is never swept into an auto-review), "scheduled" for a cell that
       * hasn't reached `done` yet (pending or running -- nothing to
       * meaningfully review), and "queued" once it has finished and passed
       * and is simply waiting on its Triage pool's count threshold or a cron
       * schedule to fire. "scheduled"/"queued" read the same `--arbiter` hue
       * (this is the Arbiter/Triage subsystem's provenance color, not a
       * distinct global status) and differ by icon/opacity/copy only;
       * "failed" reads the shared `--failed` status red instead.
       * @param {CellView} s
       * @returns {string}
       */
      function triagePlaceholderSection(s) {
        const types = s.triage_types || [];
        if (!types.length || (s.reviews || []).length) return "";
        const typeList = types.join(", ");
        if (s.state === "failed") {
          const tip = `This cell opted into Triage (type(s): ${typeList}) but failed.\nA failed cell can never pass review, so it is permanently excluded from being swept into an automatic review.`;
          return `<h3 class="section">in reviews</h3><div class="kv-row" data-tip="${tip}">` +
            `<span><span class="dot" style="background:${cvar("--failed")}"></span></span>` +
            `<span class="v" style="flex:1"><div>✗ Excluded (failed)</div><div class="mono" style="font-size:11px;color:var(--muted)">${esc(typeList)}</div></span>` +
            `</div>`;
        }
        const done = s.state === "done";
        const label = done ? "Queued for auto-review" : "Scheduled for auto-review";
        const icon = done ? "✓" : "⏳";
        const tip = done
          ? `This cell finished and resolved to Triage type(s): ${typeList}.\nIt's pooled and waiting for that pool's count threshold or a cron schedule to fire -- once it does, the Arbiter drains the pool into a fresh review automatically.\nCheck the Triage tab for the pool's current count vs. threshold.`
          : `This cell opted into Triage (type(s): ${typeList}) and is already pooled for an automatic review, but hasn't reached "done" yet.\nOnce it finishes, this will show "Queued for auto-review" instead -- a pending or running cell has nothing to contribute to a review yet.`;
        return `<h3 class="section">in reviews</h3><div class="kv-row" data-tip="${tip}">` +
          `<span style="opacity:${done ? "1" : "0.55"}"><span class="dot" style="background:${cvar("--arbiter")}"></span></span>` +
          `<span class="v" style="flex:1"><div>${icon} ${esc(label)}</div><div class="mono" style="font-size:11px;color:var(--muted)">${esc(typeList)}</div></span>` +
          `</div>`;
      }
      /**
       * Renders the cell-level details pane.
       * @param {SquadView} r
       * @param {TaskView} t
       * @param {CellView} s
       * @returns {string}
       */
      function cellView(r, t, s) {
        if (!squadPaths[r.id]) loadSquadPaths(r.id);
        const paths = (squadPaths[r.id] || {})[`${sel.taskIdx}:${sel.cellIdx}`];
        const project = paths ? (paths.project || "— (not a git worktree)") : "…";
        const agentTip = cellAgentInheritanceTip(t, s);
        const modelTip = cellModelInheritanceTip(t, s);
        // Only shown once the git check resolves and confirms this cell's
        // cwd is actually inside a git project — a plain command cell (or
        // one whose cwd isn't a worktree at all) has no upstream to show.
        const upstreamTip = "Read-only — computed live from git, not stored config; there's nothing here to edit."
          + "\nShows what this cell's branch is based on: a chained dependency's branch (when this cell declares an upstream = <<task:...>> sentinel) or, otherwise, the worktree's own git tracking branch."
          + "\nWho/when: check this while reviewing a cell's worktree to see what it branched from or gets rebased onto before each run.";
        const upstreamRow = (paths && paths.project)
          ? `<div class="kv-row"><span class="k">upstream <span class="ro-badge" data-tip="${upstreamTip}">🔒</span></span><span class="v mono">${esc(paths.upstream || "—")}</span></div>`
          : "";
        // RAL-17: reviews this cell participates in, sorted by name, each with
        // its status and a button that jumps to that review's page.
        const sReviews = s.reviews || [];
        const inReviews = sReviews.length
          ? `<h3 class="section">in reviews</h3>` + [...sReviews]
              .sort((a, b) => (a.name || "").localeCompare(b.name || ""))
              .map((rv) => {
                const tip = rv.branch
                  ? `Jump to the Reviews tab and select the branch ${esc(rv.branch)} in review '${esc(rv.name)}'.`
                  : `Jump to this review on the Reviews tab to see its full status and branch stack.`;
                // The whole row is clickable (not just the button) so the
                // badge, name, and status text are all part of the same
                // link region -- `closest("[data-click]")` resolves to the
                // button when it's the actual click target, so this doesn't
                // double-fire.
                const rowClickAttrs = rv.branch
                  ? `data-click="gotoReviewBranch" data-guardian-id="${esc(rv.id)}" data-branch="${esc(rv.branch)}"`
                  : `data-click="gotoReview" data-guardian-id="${esc(rv.id)}"`;
                return `<div class="kv-row" style="cursor:pointer" ${rowClickAttrs} data-tip="${tip}"><span>${gdot(rv.status)}</span>` +
                  `<span class="v" style="flex:1"><div>${esc(rv.name)} ${originBadge(rv.origin)}</div>${rv.branch ? `<div class="mono" style="font-size:11px;color:var(--muted)">${esc(rv.branch)}</div>` : ""}</span>` +
                  `<span class="k">${esc(rv.status)}</span>` +
                  (rv.branch
                    ? `<button class="btn" data-click="gotoReviewBranch" data-guardian-id="${esc(rv.id)}" data-branch="${esc(rv.branch)}" data-tip="${tip}">Go to branch</button>`
                    : `<button class="btn" data-click="gotoReview" data-guardian-id="${esc(rv.id)}" data-tip="${tip}">Go to review</button>`
                  ) +
                  `</div>`;
              }).join("")
          : triagePlaceholderSection(s);
        const proofSteps = (s.proof || []).length
          ? `<h3 class="section">proof steps</h3>` + (s.proof || []).map((v, vi) =>
              `<div class="kv-row"><span>${sdot(v.state)}</span>` +
              `<span class="v" style="flex:1">${esc(v.id || v.kind)}</span>` +
              `<span class="k">${esc(v.state)}</span>` +
              `<button class="btn" data-click="pick" data-kind="proof" data-ti="${sel.taskIdx}" data-si="${sel.cellIdx}" data-vi="${vi}" data-tip="Jump to this proof step's detail pane — view its kind, state, and output.">Go</button></div>`).join("")
          : "";
        const systemPromptSection = (!s.command && s.system_prompt)
          ? `<h3 class="section">system prompt</h3>${promptBox(s.system_prompt, null, SYSTEM_PROMPT_TIP)}`
          : "";
        return `<div class="dhead"><span class="k">◉ cell</span> <span>${esc(s.name ?? s.id)}</span></div>
          ${s.name ? `<div class="kv-row"><span class="k">name</span><span class="v">${esc(s.name)}</span></div>` : ""}
          <div class="kv-row"><span class="k">id</span><span class="v mono">${esc(s.id)}${copyBtn(s.id)}</span></div>
          <div class="kv-row"><span class="k">task</span><span class="v">${esc(t.name)}</span></div>
          <div class="kv-row"><span class="k">state</span><span class="v">${pill(s.state)}${detachedBadge(s.detached_at_ms)}${outOfDateBadge(s.env_out_of_date)}${["pending","queued"].includes(s.state) ? "" : squadLogsBtn(r.id)}${s.error ? failLogBtn(s.error) : ""}</span></div>
          ${timingRows(s.started_at_ms, s.finished_at_ms)}
          <div class="kv-row"><span class="k">agent</span>${detailValueHtml(s.agent, agentTip)}</div>
          <div class="kv-row"><span class="k">model</span>${detailValueHtml(s.model || "—", modelTip)}</div>
          <div class="kv-row"><span class="k">project</span><span class="v mono">${esc(project)}${paths && paths.project ? copyBtn(paths.project) : ""}</span></div>
          <div class="kv-row"><span class="k">worktree</span><span class="v mono">${esc(s.cwd || "—")}${s.cwd ? copyBtn(s.cwd) : ""}</span></div>
          ${upstreamRow}
          <div class="kv-row" data-tip="${TOKENS_COST_TIP}"><span class="k">tokens</span><span class="v">${usageSummary(s)}${s.maximum_budget_usd ? ` <span style="color:var(--muted)">/ cap $${s.maximum_budget_usd.toFixed(4)}</span>` : ""}${estimatedBadge(s.cost_is_estimated)}</span></div>
          <div class="kv-row" data-tip="${COMPACTION_TIP}"><span class="k">compaction</span><span class="v">${compactionSummary(s, s.model)}</span></div>
          <div class="kv-row" data-tip="${LIFETIME_COST_TIP}"><span class="k">lifetime</span><span class="v"><span id="cum-cost-${esc(s.id)}">—</span> <button class="btn" data-click="loadCumulativeCost" data-carto-key="${esc(s.id)}" data-carto-kind="cell" data-el-id="cum-cost-${esc(s.id)}" data-squad-id="${esc(r.id)}" data-tip="${LIFETIME_COST_TIP}">Σ load total</button></span></div>
          ${contextLimitsRow(s)}
          ${maximumToolOutputTokensRow(s)}
          <h3 class="section">${s.command ? "command" : "prompt"}</h3>
          ${s.command ? cmdBox(s.command) : promptBox(s.prompt || "—", `prompt-box-${sel.taskIdx}-${sel.cellIdx}`)}
          ${systemPromptSection}
          ${inReviews}
          ${proofSteps}
          ${cellEnvOverridesSection(r, sel.taskIdx, sel.cellIdx, s)}
          <div class="btn-row">
            <button class="btn primary" onclick="startEdit()" data-tip="Edit this cell — change its prompt, agent, model, command, or working directory.\nIf the squad is currently running it will be stopped and reset to Pending.">✎ Edit</button>
            <button class="btn" data-click="restartCell" data-squad-id="${esc(r.id)}" data-ti="${sel.taskIdx}" data-si="${sel.cellIdx}" data-tip="Re-run this cell and all downstream cells within this squad.\nDependent squads in other tasks are also re-queued.\nThis cannot be undone.">⟳ Restart cell</button>
          </div>
          ${(() => {
            // RAL-102/RAL-151: every command/prompt cell runs inside
            // tmux, so both a cell's kind is always live-view capable
            // ("prompt" XOR "command" is enforced by the schema) — "not
            // currently live" is handled gracefully at click/peek time
            // rather than by pre-emptively disabling the buttons. "Open
            // Agent" only makes sense for a prompt-kind cell (there is
            // no agent conversation to resume for a plain shell command),
            // so it's omitted outright for a command cell rather than
            // shown disabled.
            const key = `cell|${r.id}|${sel.taskIdx}|${sel.cellIdx}`;
            const previewBtn = `<button class="btn primary" data-click="togglePeek" data-key="${esc(key)}" style="border-radius:6px 0 0 6px" data-tip="Peek at this cell's live tmux pane — auto-refreshing, read-only.\nNothing you do here is ever sent to the agent.\nShows 'terminal session has ended' once the cell isn't running.">${peekOpen[key] ? "Hide Live View" : "Show Live View"}</button>`;
            // Open Agent spawns a window on the *daemon's own desktop* — it
            // has no way to reach a remote cell's conversation, so a remote
            // cell gets "Remote Terminal" (the WebSocket relay) instead of
            // "Open Agent", never both (RAL-355 Phase 10).
            const openAgentItem = s.command || s.machine
              ? ""
              : openAgentMenuItem(key, "openAgentTerminalMenuItem",
                  { squadId: r.id, ti: sel.taskIdx, si: sel.cellIdx },
                  s.state === "running", s.agent_session_id, s.agent, s.machine, true);
            const remoteTerminalItem = s.command || !s.machine
              ? ""
              : remoteTerminalMenuItem(key, "remoteTerminalMenuItem",
                  { squadId: r.id, ti: sel.taskIdx, si: sel.cellIdx },
                  s.state === "running", s.agent_session_id, s.agent);
            const resumeAutomationItem = s.command
              ? ""
              : resumeAutomationMenuItem(key, "resumeAutomationMenuItem",
                  { squadId: r.id, ti: sel.taskIdx, si: sel.cellIdx },
                  s.state === "running", s.agent_session_id);
            const items = terminalMenuItem(key, "Open Terminal Log",
                "openTerminalMenuItem", { squadId: r.id, ti: sel.taskIdx, si: sel.cellIdx },
                "Attach an interactive terminal to this cell's live tmux cell.\nShows the runner's own log/event stream, not the agent's actual conversation.\nOnly available while the cell is actively running — fails gracefully otherwise.")
              + openAgentItem
              + remoteTerminalItem
              + resumeAutomationItem
              + terminalMenuItem(key, "View Attempt History",
                  "toggleHistoryMenuItem", {},
                  "List every durably-persisted terminal-log attempt for this cell, including past reattaches.\nWho/when: the pane died or reattached and you need to see what happened right before, after the live view is gone.\nEach attempt's log survives pane death and daemon restarts.");
            return `<div class="btn-row" style="position:relative;gap:0">${previewBtn}${terminalMenuHtml(key, items)}</div>${peekBox(key, s.started_at_ms, s.detached_at_ms)}${historyBox(key)}`;
          })()}`;
      }
      // CCTL-115: render each command line elided in a fixed-height box; a
      // double-click opens a floating popup with the full, scrollable text.
      /**
       * Renders a command's lines as an elided box; double-click opens the full-text popup.
       * @param {string} text
       * @returns {string}
       */
      function cmdBox(text) {
        const lines = String(text).split("\n").filter((l, i, a) => l !== "" || a.length === 1);
        return `<div class="cmd-list" data-tip="Cell command — double-click a line to view the full text.">`
          + lines.map((l) => `<div class="cmd-line" data-full="${esc(l)}" ondblclick="openCmdPopup(event)">${esc(l) || "&nbsp;"}</div>`).join("")
          + `</div>`;
      }
      /**
       * Renders a read-only prompt box using the same widget styling as the main prompt display.
       * @param {string} text
       * @param {string|null} [id]
       * @param {string|null} [tip]
       * @returns {string}
       */
      function promptBox(text, id = null, tip = null) {
        const idAttr = id ? ` id="${id}"` : "";
        const tipAttr = tip ? ` data-tip="${esc(tip)}"` : "";
        return `<div${idAttr} class="prompt-box"${tipAttr}>${esc(text || "—")}</div>`;
      }
      /**
       * Renders a modal popup showing arbitrary text content under `title`.
       * Shared by `openCmdPopup` (synchronous, from a `data-full` attribute)
       * and `openLinkedOutputPopup` (RAL-295, fetched asynchronously).
       * @param {string} title
       * @param {string} text
       * @returns {void}
       */
      function showTextPopup(title, text) {
        byId("modal-root").innerHTML = `
          <div class="modal-bg" onclick="if(event.target===this)closeModal()"><div class="modal" style="width:720px;max-width:94vw">
            <h2>${esc(title)}</h2>
            <pre style="margin:0;max-height:60vh;overflow:auto;white-space:pre-wrap;word-break:break-all;background:var(--bg);border:1px solid var(--border);border-radius:8px;padding:12px;font-family:ui-monospace,monospace;font-size:13px">${esc(text)}</pre>
            <div class="btn-row"><button class="btn" onclick="closeModal()" data-tip="Close this popup.">Close</button></div>
          </div></div>`;
      }
      /**
       * Opens a modal popup showing a command/output's full text (from `data-full`).
       * @param {MouseEvent} e
       * @returns {void}
       */
      function openCmdPopup(e) {
        const text = /** @type {HTMLElement} */ (e.currentTarget).dataset.full || "";
        showTextPopup("Command", text);
      }
      // RALPHUS-LINKED-OUTPUT-FETCH:BEGIN
      // How a cell/proof-step's output popup resolves its text: durable
      // terminal-log attempt first, live/last pane snapshot second, `null` if
      // neither backend ever captured anything. Its only external calls are
      // `terminalLogAttemptsUrlFor`/`peekUrlFor`/`scrubSecrets` (all injected
      // as parameters by test/board-linked-output-fetch.mjs) and the global
      // `fetch` (stubbed per-test) -- keep it that way so the fallback order
      // can be exercised under `node --test` without a live daemon.
      /**
       * Resolves a cell/proof-step peek `key`'s most recently captured raw
       * output text (RAL-295): a durably-persisted terminal-log attempt if one
       * exists (the fuller, final record), else the live/last tmux pane
       * snapshot, else `null` when neither backend has ever captured anything
       * for this key. Shared by the Logs modal's cells/proofs tabs, which link
       * to this data on demand rather than carrying a separate stored/
       * summarized output field.
       * @param {string} key
       * @returns {Promise<string|null>}
       */
      async function fetchLinkedOutputText(key) {
        const attemptsUrl = terminalLogAttemptsUrlFor(key);
        if (attemptsUrl) {
          try {
            const resp = await fetch(attemptsUrl);
            if (resp.ok) {
              const data = await resp.json();
              /** @type {AttemptMeta[]} */
              const attempts = data.attempts || [];
              if (attempts.length) {
                const contentUrl = terminalLogAttemptsUrlFor(key, attempts[attempts.length - 1].attempt);
                if (contentUrl) {
                  const contentResp = await fetch(contentUrl);
                  if (contentResp.ok) {
                    const contentData = await contentResp.json();
                    return scrubSecrets(contentData.content || "");
                  }
                }
              }
            }
          } catch (_) { /* fall through to the live/last pane snapshot */ }
        }
        const paneUrl = peekUrlFor(key);
        if (paneUrl) {
          try {
            const resp = await fetch(paneUrl);
            if (resp.ok) {
              /** @type {PeekPaneResponse} */
              const data = await resp.json();
              if (data.content) return scrubSecrets(data.content);
            }
          } catch (_) { /* nothing left to try */ }
        }
        return null;
      }
      // RALPHUS-LINKED-OUTPUT-FETCH:END
      /**
       * Opens the Logs modal's output popup for a cell/proof-step row
       * (RAL-295), fetching content on demand from the pane/terminal-log-
       * attempts endpoints instead of a pre-stored summary field.
       * @param {string} key
       * @returns {Promise<void>}
       */
      async function openLinkedOutputPopup(key) {
        showTextPopup("Output", "Loading…");
        const text = await fetchLinkedOutputText(key);
        // The modal may have been closed while the fetch was in flight --
        // don't pop a stale result back open over whatever the user did next.
        if (!byId("modal-root").innerHTML) return;
        showTextPopup("Output", text || "No output recorded yet.");
      }
      // A failed cell/finalize shows only a badge + this button; the full
      // error (often a long traceback) opens here instead of flooding the graph.
      /**
       * Opens a modal popup showing a cell's full failure traceback (from `data-full`).
       * @param {MouseEvent} e
       * @returns {void}
       */
      function openErrPopup(e) {
        const text = /** @type {HTMLElement} */ (e.currentTarget).dataset.full || "";
        byId("modal-root").innerHTML = `
          <div class="modal-bg" onclick="if(event.target===this)closeModal()"><div class="modal" style="width:720px;max-width:94vw">
            <h2 style="color:var(--failed)">Failure log</h2>
            <pre style="margin:0;max-height:60vh;overflow:auto;white-space:pre-wrap;word-break:break-all;background:var(--bg);border:1px solid var(--failed);border-radius:8px;padding:12px;font-family:ui-monospace,monospace;font-size:13px;color:var(--failed)">${esc(text)}</pre>
            <div class="btn-row"><button class="btn" onclick="closeModal()" data-tip="Close this popup.">Close</button></div>
          </div></div>`;
      }

      /**
       * Switches the details pane into edit mode.
       * @returns {void}
       */
      function startEdit() { editing = true; renderDetails(); }
      /**
       * Exits edit mode without saving.
       * @returns {void}
       */
      function cancelEdit() { editing = false; renderDetails(); }
      // Cell edit form: purely cosmetic show/hide of the active mode's fields.
      // It writes nothing and clears nothing — values stay in the DOM so an
      // accidental toggle back loses no input. saveEdit() reads the dropdown.
      /**
       * Toggles the cell edit form between its agent-mode and command-mode field groups.
       * @returns {void}
       */
      function onModeChange() {
        const agent = /** @type {HTMLSelectElement} */ (document.getElementById("e-mode")).value === "agent";
        byId("agent-fields").classList.toggle("hidden", !agent);
        byId("command-fields").classList.toggle("hidden", agent);
      }
      /**
       * Renders the edit form for the current selection (squad label, task fields, or cell fields).
       * @param {SquadView} r
       * @returns {string}
       */
      function editForm(r) {
        const running = r.state === "running";
        const warn = running ? `<div class="warn">This squad is Running. Saving will stop it and set it back to Pending.</div>` : "";
        let fields = "";
        // RAL-271: environment overrides aren't part of the Save/Cancel
        // form fields below (they POST directly via their own
        // Add/Edit/Remove buttons, same as outside edit mode) — surfaced
        // here too so they're visible/editable while editing this item,
        // not just when not editing.
        let envSection = "";
        if (sel.kind === "squad") {
          fields = `<label>label<input id="e-label" value="${esc(r.label || "")}"></label>`;
          envSection = envOverridesSection(r);
        } else if (sel.kind === "task") {
          const t = r.tasks[sel.taskIdx];
          fields = `<label>name<input id="e-name" value="${esc(t.name)}"></label>
            <label>project<input id="e-project" value="${esc(t.project)}"></label>`;
          envSection = taskEnvOverridesSection(r, sel.taskIdx, t) + taskProofEnvOverridesSection(r, sel.taskIdx, t);
        } else {
          const s = r.tasks[sel.taskIdx].cells[sel.cellIdx];
          const isCmd = !!s.command;
          // Known agents in this codebase (core/src/schema.rs default is "claude";
          // the runner dispatches "claude"/"anthropic", "claude-code"/"claude-cli",
          // "codex"/"codex-cli", and "ollama"). Backed by a datalist so the input stays free-text.
          const agents = ["claude", "claude-code", "codex", "codex-cli", "ollama"];
          fields = `<label>mode<select id="e-mode" onchange="onModeChange()">
              <option value="agent"${isCmd ? "" : " selected"}>Agent</option>
              <option value="command"${isCmd ? " selected" : ""}>Command</option>
            </select></label>
            <label>cwd<input id="e-cwd" value="${esc(s.cwd || "")}"></label>
            <div id="agent-fields"${isCmd ? ' class="hidden"' : ""}>
              <label>agent<input id="e-agent" list="e-agent-list" value="${esc(s.agent || "")}">
                <datalist id="e-agent-list">${agents.map((a) => `<option value="${a}">`).join("")}</datalist></label>
              <label>model<input id="e-model" value="${esc(s.model || "")}"></label>
              <label>prompt<textarea id="e-prompt" rows="5">${esc(s.prompt || "")}</textarea></label>
            </div>
            <div id="command-fields"${isCmd ? "" : ' class="hidden"'}>
              <label>command<textarea id="e-command" rows="5">${esc(s.command || "")}</textarea></label>
            </div>`;
          envSection = cellEnvOverridesSection(r, sel.taskIdx, sel.cellIdx, s) + cellProofEnvOverridesSection(r, sel.taskIdx, sel.cellIdx, s);
        }
        return `<div class="dhead"><span class="k">✎ editing ${sel.kind}</span></div>${warn}
          <div class="edit-form">${fields}</div>
          ${envSection}
          <div id="save-err" class="verr"></div>
          <div class="btn-row"><button class="btn primary" onclick="saveEdit()" data-tip="Save these edits.\nIf the squad is currently running it will be stopped and reset to Pending.">Save</button><button class="btn" onclick="cancelEdit()" data-tip="Discard these edits and exit edit mode.">Cancel</button></div>`;
      }
      /**
       * @typedef {object} EditBody
       * @property {string} kind
       * @property {number} task_idx
       * @property {number} cell_idx
       * @property {string} [label]
       * @property {string} [name]
       * @property {string} [project]
       * @property {string} [cwd]
       * @property {string} [command]
       * @property {string} [agent]
       * @property {string} [model]
       * @property {string} [prompt]
       */
      /**
       * Reads the edit form's fields and POSTs them to persist the change.
       * @returns {Promise<void>}
       */
      async function saveEdit() {
        const r = findSquad(selectedSquadId); if (!r) return;
        /** @type {EditBody} */
        const body = { kind: /** @type {string} */ (sel.kind), task_idx: sel.taskIdx, cell_idx: sel.cellIdx };
        /**
         * @param {string} id
         * @returns {string}
         */
        const val = (id) => {
          const el = /** @type {{value?: string}|null} */ (document.getElementById(id));
          return (el && el.value) || "";
        };
        if (sel.kind === "squad") body.label = val("e-label");
        else if (sel.kind === "task") { body.name = val("e-name"); body.project = val("e-project"); }
        else {
          body.cwd = val("e-cwd");
          // Submit only the active mode's fields, driven by the dropdown — not by
          // the cell's pre-existing kind.
          if (val("e-mode") === "command") {
            body.command = val("e-command");
          } else {
            body.agent = val("e-agent"); body.model = val("e-model"); body.prompt = val("e-prompt");
          }
        }
        try {
          const resp = await fetch(`/api/squads/${r.id}/edit`, { method: "POST", body: JSON.stringify(body) });
          if (!resp.ok) { byId("save-err").textContent = "save failed (" + resp.status + ")"; return; }
          editing = false; await tick();
        } catch (e) { byId("save-err").textContent = "save failed: " + e; }
      }

      // ---------- running dropdown ----------
      /**
       * Toggles the header's "running work" dropdown, listing every in-flight cell/proof/review.
       * @param {MouseEvent} e
       * @returns {void}
       */
      function toggleRunning(e) {
        e.stopPropagation();
        const m = byId("running-menu");
        if (!m.classList.contains("hidden")) { m.classList.add("hidden"); return; }
        /** @type {string[]} */
        const rows = [];
        // Running cells
        for (const r of squads) {
          for (let ti = 0; ti < r.tasks.length; ti++) {
            const t = r.tasks[ti];
            for (let si = 0; si < t.cells.length; si++) {
              const s = t.cells[si];
              if (s.state !== "running") continue;
              const squadLabel = esc(r.label || r.id);
              // Cell-level proofs
              const sProof = s.proof || [];
              // A cell's displayed state folds to "running" once any of its own
              // proof steps is still in flight, even after the cell's own agent
              // body has already finished (see `effective_cell_state` in
              // daemon/src/store.rs) -- that fold means "not fully resolved yet",
              // not "the cell body is currently executing". Only show the Cell
              // pill when the body itself holds the concurrency slot; a proof
              // already in flight gets its own Proof row below, and showing both
              // would present one running slot as two.
              const cellProofRunning = sProof.some((v) => v.state === "running");
              if (!cellProofRunning) {
                rows.push(`<div data-click="gotoRunningItem" data-squad-id="${esc(r.id)}" data-kind="cell" data-ti="${ti}" data-si="${si}" data-vi="-1" data-tip="Running cell in squad ${squadLabel}, task ${esc(t.name)}.\nClick to navigate directly to this cell."><span class="pill p-running" style="font-size:10px;margin-right:4px">Cell</span>${squadLabel} / ${esc(t.name)}</div>`);
              }
              for (let vi = 0; vi < sProof.length; vi++) {
                const v = sProof[vi];
                if (v.state !== "running") continue;
                rows.push(`<div data-click="gotoRunningItem" data-squad-id="${esc(r.id)}" data-kind="proof" data-ti="${ti}" data-si="${si}" data-vi="${vi}" data-tip="Running proof step in squad ${squadLabel}, task ${esc(t.name)}, cell ${si}.\nClick to navigate directly to this proof step."><span class="pill p-running" style="font-size:10px;margin-right:4px">Proof</span>${squadLabel} / ${esc(t.name)} / s${si} proof</div>`);
              }
            }
            // Task-level proofs
            const tProof2 = t.proof || [];
            for (let vi = 0; vi < tProof2.length; vi++) {
              const v = tProof2[vi];
              if (v.state !== "running") continue;
              const squadLabel = esc(r.label || r.id);
              rows.push(`<div data-click="gotoRunningItem" data-squad-id="${esc(r.id)}" data-kind="proof" data-ti="${ti}" data-si="-1" data-vi="${vi}" data-tip="Running task-level proof in squad ${squadLabel}, task ${esc(t.name)}.\nClick to navigate directly to this proof step."><span class="pill p-running" style="font-size:10px;margin-right:4px">Proof</span>${squadLabel} / ${esc(t.name)} / task proof</div>`);
            }
          }
        }
        // Guardian review merges (from daemon status)
        const reviewList = (/** @type {any} */ (window)._daemonStatus && /** @type {any} */ (window)._daemonStatus.running_reviews) || [];
        for (const rv of reviewList) {
          const g = guardians.find((x) => x.id === rv.id);
          const inProg = g && g.branches.find((b) => b.merge_status === "in_progress" || b.merge_status === "proof_pending" || b.merge_status === "actioning");
          const tip = inProg
            ? inProg.merge_status === "proof_pending"
              ? `Guardian review '${esc(rv.name)}' is running final verification on branch ${esc(inProg.branch)}.\nClick to jump to this worktree row and expand it.`
              : inProg.merge_status === "actioning"
                ? `Guardian review '${esc(rv.name)}' is applying reviewer feedback to branch ${esc(inProg.branch)}.\nClick to jump to this worktree row and expand it.`
                : `Guardian review '${esc(rv.name)}' is rebasing branch ${esc(inProg.branch)}.\nClick to jump to this worktree row and expand it.`
            : `Guardian review '${esc(rv.name)}' is building its stacked rebase.\nClick to navigate to this review.`;
          const label = inProg
            ? `${esc(rv.name)} <span style="color:var(--muted);font-size:10px">/ ${esc(inProg.branch)}</span>`
            : esc(rv.name);
          rows.push(`<div data-click="gotoRunningReview" data-guardian-id="${esc(rv.id)}"${inProg ? ` data-branch="${esc(inProg.branch)}"` : ""} data-tip="${tip}"><span class="pill p-running" style="font-size:10px;margin-right:4px">Review</span>${label}</div>`);
        }
        m.innerHTML = rows.length ? rows.join("") : `<div class="empty" style="margin:6px">No running work</div>`;
        m.classList.remove("hidden");
      }
      document.addEventListener("click", () => byId("running-menu").classList.add("hidden"));

