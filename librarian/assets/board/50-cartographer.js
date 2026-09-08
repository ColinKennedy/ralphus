      // ---------- Cartographer (RAL-98): the unified structured event log ----------
      // One shared table renderer + fetcher is used by three places: the global
      // Cartographer tab (full filter UI + pagination), a squad's Logs modal
      // "events" tab (filtered by squad_id), and a review's Logs button (filtered
      // by guardian_id) — all three are the same underlying view with a filter
      // applied, not separate implementations.
      /**
       * @typedef {object} CartoFilter
       * @property {string} source
       * @property {string} scope
       * @property {string} level
       * @property {string} squad_id
       * @property {string} guardian_id
       * @property {string} cell_id
       * @property {string} q
       * @property {number} limit
       * @property {number} offset
       * @property {string} sortCol
       * @property {string} dir
       */
      /** @type {CartoFilter} */
      let cartoFilter = { source: "", scope: "", level: "", squad_id: "", guardian_id: "", cell_id: "", q: "", limit: 50, offset: 0, sortCol: "time", dir: "desc" };
      /** @type {string|null} set when viewing a single row via its copy-URL link (RAL-112) */
      let cartoSnapshotId = null;
      // Row ids (as strings) whose payload is currently expanded (RAL-113) — kept
      // outside cartoRows so a poll re-render can reapply it instead of losing it,
      // and mirrored into the URL so a link reproduces the same expanded rows.
      /** @type {Set<string>} */
      let cartoExpanded = new Set();
      /** @type {CartographerRow[]|null} */
      let cartoRows = null;
      let cartoTotal = 0;
      /**
       * Fetches a page of Cartographer rows for the given filter.
       * @param {CartoFilter} filter
       * @returns {Promise<{rows: CartographerRow[], total: number}>}
       */
      async function cartoFetch(filter) {
        const p = new URLSearchParams();
        for (const k of /** @type {(keyof CartoFilter)[]} */ (["source", "scope", "level", "squad_id", "guardian_id", "cell_id", "q"])) {
          if (filter[k]) p.set(k, String(filter[k]));
        }
        p.set("limit", String(filter.limit || 50));
        p.set("offset", String(filter.offset || 0));
        // Only the time column is sorted server-side; sorting by any other
        // column re-orders just the rows already on the page (see cartoSortRows).
        p.set("sort", filter.sortCol === "time" ? (filter.dir || "desc") : "desc");
        try {
          const r = await fetch(`/api/cartographer?${p.toString()}`);
          if (!r.ok) return { rows: [], total: 0 };
          return await r.json();
        } catch (_) { return { rows: [], total: 0 }; }
      }
      /**
       * Fetches a single Cartographer row by id (for a copy-URL snapshot link).
       * @param {string} id
       * @returns {Promise<{rows: CartographerRow[], total: number}>}
       */
      async function cartoFetchOne(id) {
        try {
          const r = await fetch(`/api/cartographer/${encodeURIComponent(id)}`);
          if (!r.ok) return { rows: [], total: 0 };
          return { rows: [await r.json()], total: 1 };
        } catch (_) { return { rows: [], total: 0 }; }
      }
      /**
       * RAL-160/RAL-172: fetches every completed-attempt Cartographer event
       * for one cell or proof step and sums their tokens/cost, so lifetime
       * spend across restarts is visible — not just the current attempt shown
       * in the `tokens`/`cost_usd` fields (which a restart overwrites in
       * place). Writes straight into `elId` rather than going through
       * renderDetails(), since this is triggered on demand by a button click,
       * not on every poll tick.
       *
       * RAL-326: `squadId` is required, not optional. `cartoKey` is the cell's
       * `sid` (or, for a proof step, its `cell-proof:<sid>:<idx>` /
       * `task-proof:<idx>` key), and a cell whose TOML declares no explicit
       * `id` gets a positional `cell-<n>` default — so every squad's Nth cell
       * shares one, and a query filtered on `cell_id` alone sums unrelated
       * cells from unrelated squads across the daemon's whole history. With no
       * squad to scope to there is no honest total to show, so say so rather
       * than fetch a wrong one.
       * @param {string} cartoKey - the daemon's own Cartographer `cell_id` tag for this node
       * @param {string} squadId
       * @param {string} elId - id of the span to write the total into
       * @param {string} kind - "cell" or "proof"; picks the event scope/message to sum
       * @returns {Promise<void>}
       */
      async function loadCumulativeCost(cartoKey, squadId, elId, kind) {
        const message = kind === "proof" ? "proof completed" : "cell completed";
        const target = document.getElementById(elId);
        if (target) target.textContent = "loading…";
        if (!squadId || !cartoKey) {
          if (target) target.textContent = "unavailable (no squad selected)";
          return;
        }
        const { rows } = await cartoFetch({
          source: "", scope: kind, level: "", squad_id: squadId, guardian_id: "",
          cell_id: cartoKey, q: message, limit: 500, offset: 0,
          sortCol: "time", dir: "desc",
        });
        const completed = rows.filter((row) => row.message === message);
        let tokensIn = 0, tokensOut = 0, cacheCreation = 0, cacheRead = 0, compactionInput = 0, cost = 0;
        let anyEstimated = false;
        for (const row of completed) {
          const payload = /** @type {{tokens_in?: number, tokens_out?: number, cache_creation_tokens?: number, cache_read_tokens?: number, compaction_input_tokens?: number, cost_usd?: number, cost_is_estimated?: boolean}} */ (row.payload || {});
          tokensIn += payload.tokens_in || 0;
          tokensOut += payload.tokens_out || 0;
          cacheCreation += payload.cache_creation_tokens || 0;
          cacheRead += payload.cache_read_tokens || 0;
          compactionInput += payload.compaction_input_tokens || 0;
          cost += payload.cost_usd || 0;
          anyEstimated = anyEstimated || !!payload.cost_is_estimated;
        }
        const el = document.getElementById(elId);
        if (!el) return;
        const total = usageSummary({ tokens_in: tokensIn, tokens_out: tokensOut, cache_creation_tokens: cacheCreation, cache_read_tokens: cacheRead, compaction_input_tokens: compactionInput, cost_usd: cost });
        el.textContent = completed.length
          ? `${total} across ${completed.length} attempt${completed.length === 1 ? "" : "s"}${anyEstimated ? " (includes estimated attempts)" : ""}`
          : "no completed attempts recorded yet";
      }
      // Restore filter state (and a possible single-row snapshot) from the URL
      // (RAL-112) — mirrors it into both cartoFilter and the filter-bar inputs so
      // reloading or following a copy-URL link reproduces exactly what it showed.
      /**
       * Restores Cartographer filter/expansion state from a parsed hash query.
       * @param {{[key: string]: string}} q
       * @returns {void}
       */
      function applyCartoQuery(q) {
        cartoSnapshotId = q.id || null;
        cartoFilter.q = q.q || "";
        cartoFilter.source = q.source || "";
        cartoFilter.scope = q.scope || "";
        cartoFilter.level = q.level || "";
        cartoFilter.squad_id = q.squad_id || "";
        cartoFilter.guardian_id = q.guardian_id || "";
        cartoFilter.cell_id = q.cell_id || "";
        cartoFilter.offset = 0;
        cartoExpanded = new Set((q.expanded || "").split(",").filter(Boolean));
        /**
         * @param {string} id
         * @param {string} v
         * @returns {void}
         */
        const set = (id, v) => { const el = /** @type {HTMLInputElement|null} */ (document.getElementById(id)); if (el) el.value = v || ""; };
        set("carto-f-q", cartoFilter.q);
        set("carto-f-source", cartoFilter.source);
        set("carto-f-scope", cartoFilter.scope);
        set("carto-f-level", cartoFilter.level);
        set("carto-f-squad", cartoFilter.squad_id);
        set("carto-f-guardian", cartoFilter.guardian_id);
        set("carto-f-cell", cartoFilter.cell_id);
      }
      /** @type {{[key: string]: string}} */
      const CARTO_LEVEL_PILL = { error: "p-failed", warning: "p-warn", info: "p-pending", debug: "p-cancelled", trace: "p-cancelled" };
      /**
       * Renders a colored pill for a Cartographer log level.
       * @param {string} level
       * @returns {string}
       */
      function cartoLevelPill(level) { return `<span class="pill ${CARTO_LEVEL_PILL[level] || "p-pending"}">${esc(level)}</span>`; }
      /**
       * Navigates from a Cartographer row to its owning squad.
       * @param {MouseEvent} e
       * @param {string} squadId
       * @returns {void}
       */
      function gotoCartoSquad(e, squadId) { e.stopPropagation(); showTab("squads", true); selectSquad(squadId); }
      /**
       * Navigates from a Cartographer row to its owning review.
       * @param {MouseEvent} e
       * @param {string} gid
       * @returns {void}
       */
      function gotoCartoGuardian(e, gid) { e.stopPropagation(); showTab("reviews", true); selectedGuardian = gid; revealedGuardianId = gid; syncHash(true); renderReviews(); renderReviewDetail(); }
      /**
       * Navigates from a Cartographer row to a specific task/cell/proof item.
       * @param {MouseEvent} e
       * @param {string} squadId
       * @param {string} kind
       * @param {number} ti
       * @param {number} si
       * @param {number} vi
       * @returns {void}
       */
      function gotoCartoItem(e, squadId, kind, ti, si, vi) { e.stopPropagation(); gotoSquadItem(squadId, kind, ti, si, vi); }
      // RAL-114: resolve a Cartographer row down to the most granular
      // task/cell/proof entity it actually references, so the ref chip's
      // second segment can link straight there instead of only the parent squad.
      // Proof-scoped rows carry task_idx/proof_scope/cell_idx/idx in their
      // payload already (from scheduler/store logging) and are resolved from
      // that alone; cell/task-scoped rows are resolved by matching the row's
      // task name and cell id against the currently loaded `squads` cache.
      // Returns null when the row can't be resolved (e.g. the owning squad isn't
      // loaded, or older events that only recorded a squad id) — callers fall
      // back to plain, non-linked text rather than a broken link.
      /**
       * Resolves a Cartographer row down to its most granular task/cell/proof entity, or null if unresolvable.
       * @param {CartographerRow} row
       * @returns {EntityRef|null}
       */
      function cartoResolveEntity(row) {
        if (!row.squad_id) return null;
        const p = row.payload || {};
        if (row.scope === "proof" && typeof p.task_idx === "number" && typeof p.idx === "number") {
          const isCellScope = p.proof_scope === "cell";
          const si = isCellScope && typeof p.cell_idx === "number" ? p.cell_idx : -1;
          return { kind: "proof", taskIdx: p.task_idx, cellIdx: si, proofIdx: p.idx, label: `proof #${p.idx}` };
        }
        const squad = findSquad(row.squad_id);
        if (!squad) return null;
        if (row.cell_id) {
          const ti = (squad.tasks || []).findIndex((t) => t.name === row.task);
          if (ti < 0) return null;
          const si = (squad.tasks[ti].cells || []).findIndex((s) => s.id === row.cell_id);
          if (si < 0) return null;
          return { kind: "cell", taskIdx: ti, cellIdx: si, proofIdx: -1, label: row.cell_id };
        }
        if (row.task) {
          const ti = (squad.tasks || []).findIndex((t) => t.name === row.task);
          if (ti < 0) return null;
          return { kind: "task", taskIdx: ti, cellIdx: -1, proofIdx: -1, label: row.task };
        }
        return null;
      }
      /**
       * Renders a Cartographer row's clickable squad/guardian/entity reference chip.
       * @param {CartographerRow} row
       * @returns {string}
       */
      function cartoRefChip(row) {
        const parts = [];
        if (row.squad_id) parts.push(`<span class="mono" style="cursor:pointer;text-decoration:underline" data-click="gotoCartoSquad" data-squad-id="${esc(row.squad_id)}" data-tip="Jump to squad ${esc(row.squad_id)}.">${esc(row.squad_id)}</span>`);
        if (row.guardian_id) parts.push(`<span class="mono" style="cursor:pointer;text-decoration:underline" data-click="gotoCartoGuardian" data-guardian-id="${esc(row.guardian_id)}" data-tip="Jump to review ${esc(row.guardian_id)}.">${esc(row.guardian_id)}</span>`);
        const target = cartoResolveEntity(row);
        if (target) {
          parts.push(`<span class="mono" style="cursor:pointer;text-decoration:underline" data-click="gotoCartoItem" data-squad-id="${esc(row.squad_id)}" data-kind="${esc(target.kind)}" data-ti="${target.taskIdx}" data-si="${target.cellIdx}" data-vi="${target.proofIdx}" data-tip="Jump straight to this ${esc(target.kind)}'s own detail pane — the exact entity this log entry is about.\nUse this instead of the squad segment when you want to inspect the specific task, cell, or proof step a log line refers to, without hunting for it inside the squad.">${esc(target.label)}</span>`);
        } else if (row.cell_id) {
          parts.push(`<span class="mono">${esc(row.cell_id)}</span>`);
        }
        return parts.length ? parts.join(" · ") : "—";
      }
      // Expansion state lives in cartoExpanded (not just the DOM), since every
      // refresh re-renders the whole table from scratch (RAL-113) — without
      // this, the class toggle below would be wiped out by the very next
      // renderCartographer().
      /**
       * Toggles a Cartographer row's raw-payload expansion.
       * @param {number} id
       * @returns {void}
       */
      function toggleCartoPayload(id) {
        const key = String(id);
        if (cartoExpanded.has(key)) cartoExpanded.delete(key); else cartoExpanded.add(key);
        const row = document.getElementById(`carto-payload-${id}`);
        if (row) row.classList.toggle("hidden");
        syncHash();
      }
      // Column-header sort (RAL-112): "time" is sorted server-side (see cartoFetch);
      // every other column is re-ordered client-side over whatever rows are on the
      // current page, since the API has no way to sort by an arbitrary field.
      /** @type {{[key: string]: number}} */
      const CARTO_LEVEL_RANK = { trace: 0, debug: 1, info: 2, warning: 3, error: 4 };
      /**
       * Computes a Cartographer row's sortable value for a given column.
       * @param {CartographerRow} r
       * @param {string} col
       * @returns {string|number}
       */
      function cartoSortValue(r, col) {
        switch (col) {
          case "level": return CARTO_LEVEL_RANK[r.level] ?? -1;
          case "source": return (r.source || "").toLowerCase();
          case "scope": return (r.scope || "").toLowerCase();
          case "task": return (r.task || "").toLowerCase();
          case "refs": return [r.squad_id, r.guardian_id, r.cell_id].filter(Boolean).join(" ").toLowerCase();
          case "message": return (r.message || "").toLowerCase();
          default: return "";
        }
      }
      /**
       * Client-side sorts a page of Cartographer rows by the active non-time column.
       * @param {CartographerRow[]|null} rows
       * @returns {CartographerRow[]|null}
       */
      function cartoSortRows(rows) {
        if (!rows || cartoFilter.sortCol === "time") return rows;
        const dir = cartoFilter.dir === "asc" ? 1 : -1;
        const col = cartoFilter.sortCol;
        return [...rows].sort((a, b) => {
          const av = cartoSortValue(a, col), bv = cartoSortValue(b, col);
          if (av < bv) return -1 * dir;
          if (av > bv) return 1 * dir;
          return 0;
        });
      }
      /**
       * Changes the Cartographer table's active sort column/direction.
       * @param {string} col
       * @returns {void}
       */
      function cartoSortBy(col) {
        if (cartoFilter.sortCol === col) cartoFilter.dir = cartoFilter.dir === "asc" ? "desc" : "asc";
        else { cartoFilter.sortCol = col; cartoFilter.dir = col === "time" ? "desc" : "asc"; }
        cartoFilter.offset = 0;
        pollCartographer();
      }
      /**
       * Builds a shareable deep-link URL for one Cartographer row.
       * @param {number} id
       * @returns {string}
       */
      function cartoRowUrl(id) { return `${location.origin}${location.pathname}#/cartographer?id=${id}`; }
      /**
       * Renders a copy-link button for one Cartographer row.
       * @param {number} id
       * @returns {string}
       */
      function cartoRowCopyBtn(id) {
        const tip = "Copy a link to this exact log entry.\nUse this to share one specific event with a teammate — unlike the filters above (which can match a different set of rows later as new events arrive or old ones age out), this link always reproduces this same row.\nIf this event is later pruned by Cartographer's retention policy, the link will stop resolving.";
        return `<button class="copy-btn" data-tip="${esc(tip)}" data-copy="${esc(cartoRowUrl(id))}" onclick="copyText(event)">🔗</button>`;
      }
      /**
       * Renders a Cartographer rows table, optionally with sortable headers.
       * @param {CartographerRow[]|null} [rows]
       * @param {boolean} [sortable]
       * @returns {string}
       */
      function cartoTableHtml(rows, sortable) {
        if (rows === null || rows === undefined) return `<div class="empty">Loading…</div>`;
        if (!rows.length) return `<div class="empty">No events.</div>`;
        const head = [
          { key: "time", label: "time" },
          { key: "level", label: "level" },
          { key: "source", label: "source" },
          { key: "scope", label: "scope" },
          { key: "task", label: "task" },
          { key: "refs", label: "refs" },
          { key: "message", label: "message" },
        ];
        const th = head.map((h) => {
          if (!sortable) return `<th style="text-align:left;padding:5px 10px;color:var(--muted);font-weight:500">${h.label}</th>`;
          const active = cartoFilter.sortCol === h.key;
          const arrow = active ? (cartoFilter.dir === "asc" ? " ▲" : " ▼") : "";
          const tip = "Sort the log by " + h.label + ".\nClick again to reverse direction.\nTime sorts across every matching row; other columns only re-order the rows on the current page.";
          return `<th style="text-align:left;padding:5px 10px;color:${active ? "var(--text)" : "var(--muted)"};font-weight:500;cursor:pointer;user-select:none" data-click="cartoSortBy" data-col="${esc(h.key)}" data-tip="${tip}">${h.label}${arrow}</th>`;
        }).join("") + `<th style="padding:5px 10px"></th>`;
        const body = rows.map((r) => `
          <tr data-click="toggleCartoPayload" data-id="${r.id}" style="cursor:pointer" data-tip="Click to expand the raw JSON payload recorded with this event.">
            <td style="padding:5px 10px;border-bottom:1px solid var(--border)">${new Date(r.at_ms).toLocaleString([], { month: "short", day: "numeric", hour: "2-digit", minute: "2-digit", second: "2-digit" })}</td>
            <td style="padding:5px 10px;border-bottom:1px solid var(--border)">${cartoLevelPill(r.level)}</td>
            <td style="padding:5px 10px;border-bottom:1px solid var(--border)">${esc(r.source)}</td>
            <td style="padding:5px 10px;border-bottom:1px solid var(--border)">${r.scope ? `<span class="pill p-pending">${esc(r.scope)}</span>` : "—"}</td>
            <td style="padding:5px 10px;border-bottom:1px solid var(--border)">${esc(r.task || "—")}</td>
            <td style="padding:5px 10px;border-bottom:1px solid var(--border)">${cartoRefChip(r)}</td>
            <td style="padding:5px 10px;border-bottom:1px solid var(--border)">${esc(r.message)}</td>
            <td style="padding:5px 10px;border-bottom:1px solid var(--border)" onclick="event.stopPropagation()">${cartoRowCopyBtn(r.id)}</td>
          </tr>
          <tr class="${cartoExpanded.has(String(r.id)) ? "" : "hidden"}" id="carto-payload-${r.id}"><td colspan="${head.length + 1}" style="padding:5px 10px;border-bottom:1px solid var(--border);background:var(--panel-2)">
            <pre style="white-space:pre-wrap;word-break:break-word;margin:0;font-size:12px">${esc(JSON.stringify(r.payload ?? {}, null, 2))}</pre>
          </td></tr>`).join("");
        return `<table style="width:100%;border-collapse:collapse;font-size:13px"><thead><tr>${th}</tr></thead><tbody>${body}</tbody></table>`;
      }
      /**
       * Fetches and renders the current page of the Cartographer tab.
       * @returns {Promise<void>}
       */
      async function pollCartographer() {
        const page = cartoSnapshotId ? await cartoFetchOne(cartoSnapshotId) : await cartoFetch(cartoFilter);
        cartoRows = page.rows || []; cartoTotal = page.total || 0;
        byId("conn").className = "dot on";
        renderCartographer();
      }
      /**
       * Renders the Cartographer tab's table, snapshot banner, and pagination info.
       * @returns {void}
       */
      function renderCartographer() {
        const el = document.getElementById("cartographer-body");
        if (!el) return;
        el.innerHTML = (cartoSnapshotId && !(cartoRows && cartoRows.length))
          ? `<div class="empty">This log entry (id ${esc(cartoSnapshotId)}) no longer exists — it may have been pruned by Cartographer's retention policy.</div>`
          : cartoTableHtml(cartoSortRows(cartoRows), !cartoSnapshotId);
        const banner = document.getElementById("carto-snapshot-banner");
        if (banner) banner.style.display = cartoSnapshotId ? "flex" : "none";
        const pager = document.getElementById("carto-pagination-row");
        if (pager) pager.style.display = cartoSnapshotId ? "none" : "flex";
        const info = document.getElementById("carto-page-info");
        if (!info) return;
        info.textContent = cartoTotal
          ? `${cartoFilter.offset + 1}–${Math.min(cartoFilter.offset + cartoFilter.limit, cartoTotal)} of ${cartoTotal}`
          : "0 of 0";
      }
      /**
       * Applies the Cartographer filter-bar inputs and re-polls.
       * @returns {void}
       */
      function cartoApplyFilters() {
        cartoSnapshotId = null;
        cartoFilter.q = /** @type {HTMLInputElement} */ (document.getElementById("carto-f-q")).value.trim();
        cartoFilter.source = /** @type {HTMLInputElement} */ (document.getElementById("carto-f-source")).value.trim();
        cartoFilter.scope = /** @type {HTMLInputElement} */ (document.getElementById("carto-f-scope")).value.trim();
        cartoFilter.level = /** @type {HTMLInputElement} */ (document.getElementById("carto-f-level")).value.trim();
        cartoFilter.squad_id = /** @type {HTMLInputElement} */ (document.getElementById("carto-f-squad")).value.trim();
        cartoFilter.guardian_id = /** @type {HTMLInputElement} */ (document.getElementById("carto-f-guardian")).value.trim();
        cartoFilter.cell_id = /** @type {HTMLInputElement} */ (document.getElementById("carto-f-cell")).value.trim();
        cartoFilter.offset = 0;
        syncHash();
        pollCartographer();
      }
      /**
       * Clears every Cartographer filter input and re-polls.
       * @returns {void}
       */
      function cartoClearFilters() {
        for (const id of ["carto-f-q", "carto-f-source", "carto-f-scope", "carto-f-squad", "carto-f-guardian", "carto-f-cell"]) {
          const el = /** @type {HTMLInputElement|null} */ (document.getElementById(id)); if (el) el.value = "";
        }
        const lvl = /** @type {HTMLInputElement|null} */ (document.getElementById("carto-f-level")); if (lvl) lvl.value = "";
        cartoSnapshotId = null;
        cartoFilter = { source: "", scope: "", level: "", squad_id: "", guardian_id: "", cell_id: "", q: "", limit: cartoFilter.limit, offset: 0, sortCol: cartoFilter.sortCol, dir: cartoFilter.dir };
        syncHash();
        pollCartographer();
      }
      /**
       * Leaves single-row snapshot view and returns to the filtered/paginated view.
       * @returns {void}
       */
      function cartoExitSnapshot() {
        cartoSnapshotId = null;
        cartoFilter.offset = 0;
        syncHash();
        pollCartographer();
      }
      /**
       * Pages the Cartographer table by `delta` pages.
       * @param {number} delta
       * @returns {void}
       */
      function cartoPage(delta) {
        if (cartoSnapshotId) return;
        cartoFilter.offset = Math.max(0, cartoFilter.offset + delta * cartoFilter.limit);
        pollCartographer();
      }
      // Modal-embedded Cartographer view (used by a squad's Logs "events" tab and
      // a review's Logs button) — same table, fixed filter, simple total count,
      // no pagination controls (small, focused audiences; see the full
      // Cartographer tab for the paginated global view).
      /** @type {{[key: string]: {rows: CartographerRow[], total: number}|null|undefined}} */
      let cartoModalCache = {};
      /**
       * Fetches and caches a modal-embedded Cartographer view's rows.
       * @param {string} cacheKey
       * @param {Partial<CartoFilter>} filter
       * @returns {Promise<void>}
       */
      async function loadCartoModalRows(cacheKey, filter) {
        cartoModalCache[cacheKey] = null; // loading
        try {
          cartoModalCache[cacheKey] = await cartoFetch(/** @type {CartoFilter} */ ({ ...filter, limit: 200, offset: 0, sortCol: "time", dir: "desc" }));
        } catch (_) {
          cartoModalCache[cacheKey] = { rows: [], total: 0 };
        }
      }
      /**
       * Opens the Cartographer log modal scoped to one review.
       * @param {string} gid
       * @returns {void}
       */
      function openReviewLogs(gid) {
        const g = guardians.find((x) => x.id === gid);
        const cacheKey = `guardian:${gid}`;
        const render = () => {
          const d = cartoModalCache[cacheKey];
          byId("modal-root").innerHTML = `<div class="modal-bg" onclick="if(event.target===this)closeModal()"><div class="modal" style="width:880px;max-width:94vw">
              <h2>Review log — ${esc(g ? g.name : gid)}</h2>
              <div style="max-height:60vh;overflow:auto">${cartoTableHtml(d ? d.rows : null)}</div>
              <div class="row" style="justify-content:space-between;margin-top:8px;align-items:center">
                <span style="color:var(--muted);font-size:12px">${d ? d.total : 0} total — see the Logs tab for filtering and pagination.</span>
                <button class="btn" onclick="closeModal()" data-tip="Close this popup.">Close</button>
              </div>
            </div></div>`;
        };
        cartoModalCache[cacheKey] = undefined;
        render();
        loadCartoModalRows(cacheKey, { guardian_id: gid }).then(render);
      }
      /** @type {{[gid: string]: PrStackView[]|null|undefined}} */
      let prStackModalCache = {};
      /**
       * Fetches and caches a review's past PR stacks (RAL-302).
       * @param {string} gid
       * @returns {Promise<void>}
       */
      async function loadPrStackModal(gid) {
        prStackModalCache[gid] = null; // loading
        try {
          const r = await fetch(`/api/guardians/${gid}/pull-request-stacks`);
          prStackModalCache[gid] = r.ok ? await r.json() : [];
        } catch (_) {
          prStackModalCache[gid] = [];
        }
      }
      /**
       * Renders one PR row within the "view past PR stacks" modal (RAL-302) --
       * a trimmed-down `prCard`: no live drift banner (this is history, not a
       * currently-open PR), but a `dropped` state gets a muted badge and its
       * `dropped_reason` surfaced as the tooltip.
       * @param {PullRequestView} p
       * @returns {string}
       */
      function prStackModalRow(p) {
        const link = p.pr_url
          ? `<a href="${esc(p.pr_url)}" target="_blank" rel="noopener" class="mono" style="color:var(--accent)" data-tip="Open this pull/merge request on ${esc(p.forge)}.">#${p.pr_number ?? "?"}</a>`
          : `<span class="mono">${esc(p.branch_alias)}</span>`;
        const dropped = p.state === "dropped";
        const tip = dropped && p.dropped_reason ? esc(p.dropped_reason) : `Pull/merge request submitted via ${esc(p.forge)}.`;
        return `<div style="border:1px solid var(--border);border-radius:6px;padding:6px 8px;margin-bottom:4px" data-tip="${tip}">
            <div class="row" style="justify-content:space-between;gap:6px">
              <span>${esc(p.forge)} ${link} <span class="mono" style="color:var(--muted);font-size:11px">${esc(p.branch_alias)} → ${esc(p.base_ref)}</span></span>
              <span class="badge" style="font-size:11px${dropped ? ";color:var(--muted);border-color:var(--muted)" : ""}">${esc(p.state)}</span>
            </div>
          </div>`;
      }
      /**
       * Renders the "view past PR stacks" modal's body from cached data.
       * @param {string} gid
       * @returns {string}
       */
      function prStackModalBody(gid) {
        const stacks = prStackModalCache[gid];
        if (stacks === null || stacks === undefined) return `<div class="empty">loading…</div>`;
        if (!stacks.length) return `<div class="empty">No PR stacks have been submitted for this review yet.</div>`;
        return stacks.map((s) => `<div style="margin-bottom:14px">
              <div style="color:var(--muted);font-size:12px;margin-bottom:4px">${new Date(s.submitted_at_ms).toLocaleString([], { month: "short", day: "numeric", year: "numeric", hour: "2-digit", minute: "2-digit" })}</div>
              ${s.prs.map(prStackModalRow).join("")}
            </div>`).join("");
      }
      /**
       * Opens the "view past PR stacks" modal for a review (RAL-302): every
       * PR stack ralphus has ever submitted for it, in any state, including
       * ones dropped because their linked PR merged on the forge while the
       * review was still mid-flight (RAL-300). Read-only history -- does not
       * resubmit or replay anything.
       * @param {string} gid
       * @returns {void}
       */
      function openReviewPrStacks(gid) {
        closeSquadMenu();
        const g = guardians.find((x) => x.id === gid);
        const render = () => {
          byId("modal-root").innerHTML = `<div class="modal-bg" onclick="if(event.target===this)closeModal()"><div class="modal" style="width:640px;max-width:94vw">
              <h2>Past PR stacks — ${esc(g ? g.name : gid)}</h2>
              <div style="max-height:60vh;overflow:auto">${prStackModalBody(gid)}</div>
              <div class="row" style="justify-content:flex-end;margin-top:8px">
                <button class="btn" onclick="closeModal()" data-tip="Close this popup.">Close</button>
              </div>
            </div></div>`;
        };
        prStackModalCache[gid] = null;
        render();
        loadPrStackModal(gid).then(render);
      }

