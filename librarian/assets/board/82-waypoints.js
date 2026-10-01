      // ---------- Waypoints tab (RAL-400) ----------
      // A waypoint is a cross-squad coordination join point: a roster of squads
      // and/or reviews that must (block mode) or may (advisory mode) check in
      // before the waypoint's own work proceeds. This chunk owns the whole
      // "#/waypoints" tab: sidebar list + filters, detail pane (settings, roster,
      // delivery feed, bearings), the create/add-roster-entry modals, and the
      // "Add to waypoint…" context-menu entry points wired from squads/reviews/
      // the squad graph.

      /** Every waypoint lifecycle state, in sidebar filter-chip order. */
      const WAYPOINT_STATES = ["open", "closed"];

      /** Maps a RosterEntryView.delivery_status to the `docs/colors.md`-documented CSS variable used for its status dot. @type {{[key: string]: string}} */
      const WAYPOINT_DELIVERY_COLORS = { undelivered: "--muted", delivered: "--done", via_restack: "--teal", failed: "--failed" };

      /** @typedef {object} WaypointFilters
       * @property {string} q
       * @property {Set<string>} status
       * @property {Set<string>} projects */

      /**
       * Builds the default Waypoints sidebar filter state: every state visible, no project filter, empty search.
       * @returns {WaypointFilters}
       */
      function defaultWaypointFilters() {
        return { q: "", status: new Set(WAYPOINT_STATES), projects: new Set() };
      }

      /** @type {WaypointFilters} */
      let waypointFilters = defaultWaypointFilters();

      /**
       * Applies sidebar filters parsed out of a `#/waypoints?...` hash. `parseHash` runs in an earlier chunk than this
       * one and cannot touch `waypointFilters` directly — the first call happens while chunk 80 is still loading, before
       * this chunk exists — so it hands the raw values over and this applies them once the tab is actually shown.
       * A `null` field means that parameter was absent from the hash and keeps its default.
       * @param {{q: string|null, status: string[]|null, projects: string[]|null}|null|undefined} parsed
       * @returns {void}
       */
      function applyWaypointHashFilters(parsed) {
        waypointFilters = defaultWaypointFilters();
        if (!parsed) return;
        if (parsed.q !== null && parsed.q !== undefined) waypointFilters.q = parsed.q;
        if (parsed.status) waypointFilters.status = new Set(parsed.status);
        if (parsed.projects) waypointFilters.projects = new Set(parsed.projects);
      }
      /** @type {string|null} */
      let selectedWaypointId = null;
      /** @type {WaypointListEntry[]} */
      let waypoints = [];
      /** @type {WaypointDetail|null} */
      let waypointDetail = null;
      /** @type {BearingView[]} */
      let waypointBearings = [];
      /** @type {WaypointEventEntry[]} */
      let waypointDeliveries = [];
      /** Guards a `pollWaypoints` in-flight request against a newer one landing first — mirrors `reviewPollSeq`. */
      let waypointPollSeq = 0;

      /**
       * Handles input on the waypoint sidebar's free-text filter field.
       * @param {string} value
       * @returns {void}
       */
      function onWaypointFilter(value) {
        waypointFilters.q = value.toLowerCase();
        renderWaypoints();
        syncHash();
      }

      /**
       * Renders the waypoint sidebar's open/closed status filter dropdown.
       * @returns {void}
       */
      function renderWaypointStatusFilters() {
        renderStatusDropdown("waypoint-status-filters", {
          id: "waypoint",
          label: "Status",
          mode: "multi",
          options: [
            { value: "open", label: "Open", color: "--running" },
            { value: "closed", label: "Closed", color: "--done" },
          ],
          selected: waypointFilters.status,
          itemNoun: "waypoint",
          onToggle: toggleWaypointStatus,
          onAll: () => allWaypointStatus(true),
          onNone: () => allWaypointStatus(false),
        });
      }

      /**
       * Toggles one waypoint state on/off in the sidebar filter.
       * @param {string} s
       * @param {boolean} on
       * @returns {void}
       */
      function toggleWaypointStatus(s, on) {
        if (on) waypointFilters.status.add(s); else waypointFilters.status.delete(s);
        renderWaypoints();
        syncHash();
      }

      /**
       * Selects or clears every waypoint state in the sidebar filter.
       * @param {boolean} on
       * @returns {void}
       */
      function allWaypointStatus(on) {
        waypointFilters.status = on ? new Set(WAYPOINT_STATES) : new Set();
        renderWaypointStatusFilters();
        renderWaypoints();
        syncHash();
      }

      /**
       * Renders the waypoint sidebar's project filter button and active-filter chips.
       * @returns {void}
       */
      function renderWaypointProjectFilterChips() {
        const chips = [...waypointFilters.projects].sort().map((p) => `<span class="filter-chip"><span class="x" onclick="toggleWaypointProjectFilter('${esc(p)}',false)" data-tip="Remove this project from the waypoint filter.">✕</span>${esc(p)}</span>`).join("");
        byId("waypoint-project-filter").innerHTML = `<button type="button" class="btn" onclick="openWaypointProjectFilterMenu(event)" data-tip="Filter waypoints by project. No projects selected shows every project.">Project ▾</button>${chips}`
          + (waypointFilters.projects.size ? `<span class="chip" onclick="clearWaypointProjectFilter()" data-tip="Clear the waypoint project filter -- show every project again.">clear</span>` : "");
      }

      /**
       * Toggles one project name on/off in the waypoint sidebar's project filter.
       * @param {string} name
       * @param {boolean} on
       * @returns {void}
       */
      function toggleWaypointProjectFilter(name, on) {
        if (on) waypointFilters.projects.add(name); else waypointFilters.projects.delete(name);
        renderWaypoints();
        syncHash();
        const menu = document.getElementById("waypoint-project-filter-menu");
        if (menu) menu.innerHTML = waypointProjectFilterMenuRowsHtml();
      }

      /**
       * Clears the waypoint sidebar's project filter entirely.
       * @returns {void}
       */
      function clearWaypointProjectFilter() {
        waypointFilters.projects.clear();
        closeWaypointProjectFilterMenu();
        renderWaypoints();
        syncHash();
      }

      /**
       * Builds the checkbox rows for the waypoint project-filter popup menu.
       * @returns {string}
       */
      function waypointProjectFilterMenuRowsHtml() {
        const names = registeredProjectNames.slice().sort((a, b) => a.localeCompare(b));
        if (!names.length) return `<div style="color:var(--muted);cursor:default">No registered projects.</div>`;
        return names.map((name) => `<div class="ctx-check ${waypointFilters.projects.has(name) ? "on" : ""}"><label style="display:flex;align-items:center;gap:6px;width:100%;margin:0;cursor:pointer" onclick="event.stopPropagation()"><input type="checkbox" ${waypointFilters.projects.has(name) ? "checked" : ""} onchange="toggleWaypointProjectFilter('${esc(name)}',this.checked)">${esc(name)}</label></div>`).join("");
      }

      /**
       * Opens the waypoint sidebar's project-filter popup menu, closing the equivalent menu on any other tab first.
       * @param {MouseEvent} e
       * @returns {void}
       */
      function openWaypointProjectFilterMenu(e) {
        e.preventDefault(); e.stopPropagation();
        closeProjectFilterMenu(); ttCloseProjectFilterMenu(); triageCloseProjectFilterMenu();
        const existing = document.getElementById("waypoint-project-filter-menu");
        if (existing) { existing.remove(); return; }
        const menu = document.createElement("div");
        menu.className = "ctx-menu"; menu.id = "waypoint-project-filter-menu";
        menu.innerHTML = waypointProjectFilterMenuRowsHtml();
        document.body.appendChild(menu);
        const r = /** @type {HTMLElement} */ (e.currentTarget).getBoundingClientRect();
        menu.style.left = Math.min(r.left, window.innerWidth - 220) + "px";
        menu.style.top = Math.min(r.bottom + 4, window.innerHeight - 360) + "px";
      }

      /**
       * Closes the waypoint sidebar's project-filter popup menu, if open.
       * @returns {void}
       */
      function closeWaypointProjectFilterMenu() { const m = document.getElementById("waypoint-project-filter-menu"); if (m) m.remove(); }
      document.addEventListener("click", closeWaypointProjectFilterMenu);

      /**
       * Fetches the waypoint list from the daemon without touching selection/detail state.
       * @returns {Promise<void>}
       */
      async function refreshWaypointsList() {
        waypoints = await (await fetch("/api/waypoints")).json();
      }

      /**
       * Filters and sorts the waypoint list per the sidebar's active filters (newest first).
       * @returns {WaypointListEntry[]}
       */
      function visibleWaypoints() {
        return waypoints
          .filter((w) => waypointFilters.status.has(w.state))
          .filter((w) => !waypointFilters.projects.size || w.projects.some((p) => waypointFilters.projects.has(p)))
          .filter((w) => !waypointFilters.q || w.id.toLowerCase().includes(waypointFilters.q) || (w.label || "").toLowerCase().includes(waypointFilters.q))
          .slice()
          .sort((a, b) => b.created_at_ms - a.created_at_ms);
      }

      /**
       * Renders a waypoint's open/closed state badge with its own tooltip (not covered by `STATE_TOOLTIPS`).
       * @param {string} state
       * @returns {string}
       */
      function waypointStateBadge(state) {
        const tip = state === "closed"
          ? "This waypoint is closed -- every roster entry has cleared or it was closed manually. No further deliveries are expected."
          : "This waypoint is open -- at least one roster entry is still expected to check in.";
        return `<span class="pill p-${esc(state)}" data-tip="${esc(tip)}">${esc(state)}</span>`;
      }

      /**
       * Renders a roster entry's delivery-status dot using the `docs/colors.md`-documented color for that status.
       * @param {string} deliveryStatus
       * @returns {string}
       */
      function wdot(deliveryStatus) {
        const colorVar = Object.prototype.hasOwnProperty.call(WAYPOINT_DELIVERY_COLORS, deliveryStatus) ? WAYPOINT_DELIVERY_COLORS[deliveryStatus] : "--muted";
        return `<span class="dot" style="background:${cvar(colorVar)}" data-tip="Delivery status: ${esc(deliveryStatus)}"></span>`;
      }

      /**
       * Renders a roster entry's block/advisory mode badge.
       * @param {string} mode
       * @returns {string}
       */
      function waypointModeBadge(mode) {
        const tip = mode === "advisory"
          ? "Advisory mode -- this entry is informational only and never halts anything."
          : "Block mode -- this entry can halt the waypoint's in-flight cells until it checks in.";
        return `<span class="badge roster-${esc(mode)}" data-tip="${esc(tip)}">${esc(mode)}</span>`;
      }

      /**
       * Renders the waypoint list in the Waypoints tab's sidebar, matching the Reviews tab's sidebar conventions.
       * @returns {void}
       */
      function renderWaypoints() {
        const el = byId("waypoints");
        renderWaypointStatusFilters();
        renderWaypointProjectFilterChips();
        if (!waypoints.length) { el.innerHTML = `<div class="empty">No waypoints.</div>`; return; }
        const list = visibleWaypoints();
        if (!list.length) { el.innerHTML = `<div class="empty">No matching waypoints.</div>`; return; }
        el.innerHTML = list.map((w) => `<div class="wp-card ${w.id === selectedWaypointId ? "selected" : ""}" data-click="selectWaypoint" data-ctx="openWaypointMenu" data-waypoint-id="${esc(w.id)}" data-tip="Open this waypoint.\nRight-click for actions.">
          <div class="wp-card-top">
            <div class="wp-card-name">${esc(w.label || w.id)}</div>
            ${waypointStateBadge(w.state)}
          </div>
          <div class="wp-card-meta">
            ${renderWaypointSpark(w)}
            <span data-tip="How many squads and reviews this waypoint tracks.">${w.roster_count} roster${w.roster_count === 1 ? "" : "s"}</span>
            ${w.projects.length ? `<span class="wp-proj" data-tip="Projects inferred from the roster.">${esc(w.projects.join(", "))}</span>` : ""}
          </div>
        </div>`).join("");
      }

      /**
       * Renders a waypoint's roster as one segment per entry, coloured by that entry's delivery status — the same
       * vocabulary `wdot` uses. Progress read as shape rather than as a count, so a sidebar scan answers "how far
       * along is this one" without opening it. Falls back to a single muted segment for an empty roster.
       * @param {WaypointListEntry} w
       * @returns {string}
       */
      function renderWaypointSpark(w) {
        const counts = w.delivery_summary;
        /** @type {string[]} */
        const segments = [];
        for (const [status, n] of [["delivered", counts.delivered], ["via_restack", counts.via_restack], ["failed", counts.failed], ["undelivered", counts.undelivered]]) {
          for (let i = 0; i < Number(n); i += 1) segments.push(String(status));
        }
        if (!segments.length) return `<span class="wp-spark" data-tip="No roster entries yet."><i></i></span>`;
        const tip = `Roster delivery: ${counts.delivered} delivered, ${counts.via_restack} via restack, ${counts.failed} failed, ${counts.undelivered} undelivered.`;
        const bars = segments.map((s) => `<i style="background:${cvar(WAYPOINT_DELIVERY_COLORS[s] || "--muted")}"></i>`).join("");
        return `<span class="wp-spark" data-tip="${esc(tip)}">${bars}</span>`;
      }

      /**
       * Renders the waypoint's lifecycle as a rail: open → surveyed → delivered → closed, with the step it is
       * currently on marked. Each step is derived from real roster state rather than stored, so it cannot drift
       * from the data. Answers "what is this waypoint waiting on", which a delivered-count alone does not.
       * @param {WaypointDetail} w
       * @returns {string}
       */
      function renderWaypointPipeline(w) {
        const total = w.roster.length;
        const surveyed = w.roster.filter((e) => e.survey_verdict).length;
        const ds = w.delivery_summary;
        const reached = ds.delivered + ds.via_restack;
        const closed = w.state === "closed";
        const steps = [
          { label: "open", done: true, tip: "The waypoint exists and is tracking its roster." },
          { label: "surveyed", done: total > 0 && surveyed >= total, tip: `Relevance decided for ${surveyed} of ${total} roster entries.` },
          { label: "delivered", done: total > 0 && reached >= total, tip: `Guidance reached ${reached} of ${total} roster entries.` },
          { label: "closed", done: closed, tip: closed ? "Closed — no further deliveries are expected." : "Closes when every roster entry reaches a terminal state, or when closed by hand." },
        ];
        // A later step being reached implies the earlier ones: an entry added by
        // hand never gets a survey verdict, so "surveyed" would otherwise stay
        // pending behind a waypoint that has already delivered and closed.
        const furthest = steps.reduce((acc, s, i) => (s.done ? i : acc), 0);
        for (let i = 0; i < furthest; i += 1) steps[i].done = true;
        const nowIdx = steps.findIndex((s) => !s.done);
        return `<div class="wp-pipe">${steps.map((s, i) => {
          const cls = s.done ? "is-done" : (i === nowIdx ? "is-now" : "");
          const line = i < steps.length - 1 ? `<span class="wp-pline ${steps[i + 1].done || s.done ? "is-done" : ""}"></span>` : "";
          return `<span class="wp-step ${cls}" data-tip="${esc(s.tip)}"><span class="wp-pdot">${s.done ? "✓" : ""}</span>${esc(s.label)}</span>${line}`;
        }).join("")}</div>`;
      }

      /**
       * Selects a waypoint and shows its detail pane.
       * @param {string} id
       * @returns {void}
       */
      function selectWaypoint(id) {
        selectedWaypointId = id;
        syncHash(true);
        renderWaypoints();
        renderWaypointDetail();
      }

      /**
       * Polls the waypoint list, and when a waypoint is selected, its detail/bearings/delivery feed.
       * Mirrors `pollReviews`'s in-flight sequence-guard so a superseded response never clobbers a newer one.
       * @returns {Promise<void>}
       */
      async function pollWaypoints() {
        const seq = ++waypointPollSeq;
        try {
          // Which waypoint the detail pane will show is usually already known —
          // from the hash on a deep link, or from the current selection on a
          // refresh — so its three requests do not have to queue behind the
          // list. Only a first visit with no hash has to wait for the list to
          // learn which waypoint to open.
          const hashWant = (pendingHash && pendingHash.tab === "waypoints" && pendingHash.waypointId) || null;
          const knownId = hashWant || selectedWaypointId;
          const listPromise = fetch("/api/waypoints").then((r) => r.json());
          const earlyDetail = knownId ? fetchWaypointDetailBundle(knownId) : null;

          const fresh = await listPromise;
          if (seq !== waypointPollSeq) { console.debug("pollWaypoints: superseded, abandoning"); return; }
          waypoints = fresh;
          if (pendingHash && pendingHash.tab === "waypoints") {
            const want = pendingHash; pendingHash = null;
            if (want.waypointId) selectedWaypointId = want.waypointId;
          }
          if (!selectedWaypointId && waypoints.length) {
            const firstVisible = visibleWaypoints()[0];
            if (firstVisible) selectedWaypointId = firstVisible.id;
          }
          syncHash();
          renderWaypoints();
          if (selectedWaypointId) {
            const id = selectedWaypointId;
            // Reuse the in-flight bundle when the guess held; otherwise the
            // list picked a different waypoint and that one is fetched now.
            const bundle = (earlyDetail && knownId === id) ? await earlyDetail : await fetchWaypointDetailBundle(id);
            if (seq !== waypointPollSeq) { console.debug("pollWaypoints: superseded, abandoning"); return; }
            waypointDetail = bundle.detail;
            waypointBearings = bundle.bearings;
            waypointDeliveries = bundle.deliveries;
            renderWaypointDetail();
          }
          byId("conn").className = "dot on";
          markUpdated();
        } catch (e) { byId("conn").className = "dot off"; }
      }

      /**
       * Fetches one waypoint's detail, bearings and effect feed together. Split out so the poller can start it
       * before the sidebar list has landed, whenever the waypoint to show is already known.
       * @param {string} id
       * @returns {Promise<{detail: WaypointDetail|null, bearings: BearingView[], deliveries: WaypointEventEntry[]}>}
       */
      async function fetchWaypointDetailBundle(id) {
        const [detail, bearings, deliveries] = await Promise.all([
          fetch(`/api/waypoints/${id}`).then((r) => (r.ok ? r.json() : null)),
          fetch(`/api/waypoints/${id}/bearings`).then((r) => (r.ok ? r.json() : [])),
          fetch(`/api/waypoints/${id}/deliveries`).then((r) => (r.ok ? r.json() : [])),
        ]);
        return { detail, bearings, deliveries };
      }

      /**
       * Renders one roster entry row: kind icon, delivery dot, entity id (deep-linked), mode badge, and an expandable verdict/rationale.
       * @param {RosterEntryView} entry
       * @returns {string}
       */
      function renderRosterEntryRow(entry) {
        const icon = entry.kind === "review" ? "🔀" : "🧩";
        const gotoFn = entry.kind === "review" ? "gotoReview" : "gotoSquad";
        const idAttr = entry.kind === "review" ? `data-guardian-id="${esc(entry.entry_id)}"` : `data-squad-id="${esc(entry.entry_id)}"`;
        const link = `<a class="wp-entry-id" href="#" data-click="${gotoFn}" ${idAttr} data-tip="Open this ${esc(entry.kind)}'s own page.">${icon} ${esc(entry.entry_id)}</a>`;
        const verdict = entry.survey_verdict
          ? `<details class="wp-entry-why"><summary data-tip="Show the relevance verdict and rationale the survey recorded for this entry.">${esc(entry.survey_verdict)}</summary><div style="padding:4px 0 0 12px">${esc(entry.survey_rationale || "(no rationale recorded)")}</div></details>`
          : "";
        const staleBadge = entry.stale_at_ms
          ? ` <span class="badge" style="color:var(--stale);border-color:var(--stale)" data-tip="This work finished while the waypoint was still open and had judged it impacted, so it landed without the waypoint's changes and may be stale.\nNothing has been re-run automatically.\nTo re-run it carrying its prior findings and this waypoint's bearings: ralphus waypoint redo ${esc(entry.waypoint_id)} ${esc(entry.entry_id)}">stale</span>`
          : "";
        const removeBtn = `<button class="icon-btn" data-click="removeRosterEntry" data-waypoint-id="${esc(entry.waypoint_id)}" data-entry-id="${esc(entry.entry_id)}" data-tip="Remove this entry from the waypoint's roster.\nThis cannot be undone." style="font-size:11px;padding:1px 5px">✕</button>`;
        const toggleModeBtn = `<button class="icon-btn" data-click="toggleRosterEntryMode" data-waypoint-id="${esc(entry.waypoint_id)}" data-entry-id="${esc(entry.entry_id)}" data-mode="${entry.mode === "advisory" ? "block" : "advisory"}" data-tip="Switch this entry to ${entry.mode === "advisory" ? "block" : "advisory"} mode.\n${entry.mode === "advisory" ? "Block holds this work until the waypoint closes." : "Advisory releases it while still delivering the guidance."}" style="font-size:11px;padding:1px 5px">⇄</button>`;
        return `<div class="wp-entry">
          <div class="wp-entry-top">
            ${wdot(entry.delivery_status)}
            ${link}
            ${waypointModeBadge(entry.mode)}
            <span class="badge" style="color:var(--muted);border-color:var(--border)" data-tip="Whether this waypoint's guidance has reached this entry yet.">${esc(entry.delivery_status)}</span>${staleBadge}
            <span class="wp-entry-actions">${toggleModeBtn}${removeBtn}</span>
          </div>
          ${verdict}
        </div>`;
      }

      /**
       * Renders the delivery feed: a Cartographer-backed, chronological event list for the selected waypoint.
       * @returns {string}
       */
      function renderWaypointDeliveryFeed() {
        if (!waypointDeliveries.length) return `<div class="empty">Nothing has happened yet.</div>`;
        return waypointDeliveries.slice().sort((a, b) => b.at_ms - a.at_ms).map((ev) => `<div class="wp-feed-row">
          <span class="wp-feed-when" data-tip="When this effect was recorded.">${esc(new Date(ev.at_ms).toLocaleString())}</span>
          <span class="wp-feed-body">${esc(ev.message)}${renderWaypointEffectTarget(ev)}</span>
        </div>`).join("");
      }

      /**
       * Renders which entity a waypoint effect landed on, as deep links. This is what makes the feed a view of
       * what the waypoint did to each squad/cell/review rather than a flat message log — a halt is only meaningful
       * once you can see which cell it stopped. Returns an empty string for an effect with no entity refs.
       * @param {WaypointEventEntry} ev
       * @returns {string}
       */
      function renderWaypointEffectTarget(ev) {
        const parts = [];
        if (ev.squad_id) {
          const label = ev.cell_id ? `${ev.squad_id} / ${ev.task ? `${ev.task} / ` : ""}${ev.cell_id}` : ev.squad_id;
          parts.push(`<a href="#" data-click="gotoSquad" data-squad-id="${esc(ev.squad_id)}" data-tip="Open the squad this effect landed on.">🧩 ${esc(label)}</a>`);
        }
        if (ev.guardian_id) {
          parts.push(`<a href="#" data-click="gotoReview" data-guardian-id="${esc(ev.guardian_id)}" data-tip="Open the review this effect landed on.">🔀 ${esc(ev.guardian_id)}</a>`);
        }
        if (!parts.length) return "";
        const src = `<span class="wp-src" data-tip="The subsystem that recorded this effect.\nA 'waypoints' row is a survey decision; 'scheduler' and 'submit' rows are actions taken on it.">${esc(ev.source)}</span>`;
        return `<div class="wp-feed-refs">${src}${parts.join("")}</div>`;
      }

      /**
       * Navigates to the squad or review a bearing's entity URI points at (`squad:<id>` or `guardian:<id>`).
       * @param {string} entityUri
       * @returns {void}
       */
      function gotoEntityUri(entityUri) {
        const idx = entityUri.indexOf(":");
        if (idx < 0) return;
        const prefix = entityUri.slice(0, idx);
        const id = entityUri.slice(idx + 1);
        if (prefix === "squad") gotoSquad(id);
        else if (prefix === "guardian") gotoReview(id);
        else if (prefix === "waypoint") { showTab("waypoints", true); selectWaypoint(id); }
      }

      /**
       * Renders the bearings panel: the append-only chronicle of completed work reported against this waypoint.
       * @returns {string}
       */
      function renderWaypointBearings() {
        if (!waypointBearings.length) return `<div class="empty">No bearings reported yet.</div>`;
        return waypointBearings.slice().sort((a, b) => b.id - a.id).map((b) => {
          const link = b.entity_uri ? `<a href="#" data-click="gotoEntityUri" data-entity-uri="${esc(b.entity_uri)}" data-tip="Open the entity this bearing was reported against.">${esc(b.entity_uri)}</a>` : "";
          const commit = b.commit_id
            ? `<span class="mono" data-tip="A lead for narrowing an investigation, not an assertion that the base you are looking at already contains this commit.">${esc(b.commit_id.slice(0, 12))}${b.commit_summary ? ` — ${esc(b.commit_summary)}` : ""}</span>`
            : "";
          const refs = (link || commit) ? `<div class="wp-feed-refs">${commit}${link}</div>` : "";
          return `<div class="wp-feed-row">
            <span class="wp-feed-when" data-tip="When this bearing was appended. Bearings are append-only and never edited.">${esc(new Date(b.created_at_ms).toLocaleString())}</span>
            <span class="wp-feed-body">${esc(b.summary)}${refs}</span>
          </div>`;
        }).join("");
      }

      /**
       * Renders the Waypoints tab's detail pane: settings, state, inferred projects, roster, delivery feed, and bearings.
       * @returns {void}
       */
      function renderWaypointDetail() {
        const el = byId("waypoint-detail");
        if (!selectedWaypointId) { el.innerHTML = `<div class="empty">Select a waypoint.</div>`; return; }
        if (!waypointDetail || waypointDetail.id !== selectedWaypointId) { el.innerHTML = `<div class="empty">Loading waypoint…</div>`; return; }
        const w = waypointDetail;
        const entityUri = `waypoint:${w.id}`;
        const closeReopenBtn = w.state === "open"
          ? `<button class="btn" data-click="closeWaypoint" data-waypoint-id="${esc(w.id)}" data-tip="Close this waypoint manually.\nWho/when: the coordination is done even though some entries haven't formally checked in.\nClosing releases everything this waypoint holds.">■ Close</button>`
          : `<button class="btn" data-click="reopenWaypoint" data-waypoint-id="${esc(w.id)}" data-tip="Reopen this waypoint.\nWho/when: more roster entries need to check in after it was closed.\nReopening makes it hold its block-mode entries again.">▶ Reopen</button>`;
        el.innerHTML = `
          <div class="wp-cmdbar">
            <div class="wp-cmd-id">
              <div class="wp-cmd-name">${esc(w.label || w.id)}</div>
              <div class="wp-cmd-sub mono" data-tip="This waypoint's id. Use it with the ralphus waypoint commands.">${esc(w.id)}</div>
            </div>
            <div class="wp-cmd-actions">
              ${waypointStateBadge(w.state)}
              <button class="btn" data-click="toggleWatch" data-entity-uri="${esc(entityUri)}" data-tip="${isWatching(entityUri) ? "Stop receiving watcher notifications for this waypoint." : "Watch this waypoint and choose which mailbox priority tiers should notify you."}">${isWatching(entityUri) ? "◉ Unwatch" : "◎ Watch…"}</button>
              ${closeReopenBtn}
            </div>
          </div>
          ${renderWaypointPipeline(w)}
          ${renderWaypointSetup(w)}
          <div class="wp-prompt" data-tip="The coordination guidance this waypoint carries.\nIt is sent to the survey to decide which work is impacted, and delivered to the roster entries that are.">${esc(w.prompt)}</div>
          <div class="wp-section">
            <h4>Roster</h4>
            <span class="wp-section-rule"></span>
            <button class="btn sm" data-click="openAddRosterEntry" data-waypoint-id="${esc(w.id)}" data-tip="Add a squad or review to this waypoint's roster by id.\nWho/when: you know a piece of work needs to respect this waypoint and don't want to wait for the survey to find it.">＋ Add</button>
          </div>
          ${w.roster.length ? w.roster.map(renderRosterEntryRow).join("") : `<div class="empty">No roster entries.</div>`}
          <div class="wp-section">
            <h4>Effects</h4>
            <span class="wp-section-rule"></span>
          </div>
          ${renderWaypointDeliveryFeed()}
          <div class="wp-section">
            <h4>Bearings</h4>
            <span class="wp-section-rule"></span>
            <button class="btn sm" data-click="openAppendBearing" data-waypoint-id="${esc(w.id)}" data-tip="Append a bearing -- a permanent record of completed work for this waypoint.\nWho/when: use this once you've finished a piece of coordinated work and want other roster entries to see it happened.\nBearings are append-only; this cannot be undone or edited afterward.">＋ Add</button>
          </div>
          ${renderWaypointBearings()}
        `;
      }

      /**
       * Renders the waypoint's settings as a row of chips. These are short scalars — agent, model, whether advisory
       * entries are permitted, the inferred projects, the delivery rollup — and a key/value row each spent a screen
       * of height to say very little. The prompt is deliberately not a chip: it is prose and gets its own block.
       * @param {WaypointDetail} w
       * @returns {string}
       */
      function renderWaypointSetup(w) {
        const ds = w.delivery_summary;
        const parts = [
          ds.delivered ? `${ds.delivered} delivered` : "",
          ds.via_restack ? `${ds.via_restack} via restack` : "",
          ds.failed ? `${ds.failed} failed` : "",
          ds.undelivered ? `${ds.undelivered} undelivered` : "",
        ].filter(Boolean);
        /**
         * @param {string} k
         * @param {string} v
         * @param {string} tip
         * @param {boolean} [muted]
         * @returns {string}
         */
        const chip = (k, v, tip, muted) => `<span class="wp-chip ${muted ? "is-muted" : ""}" data-tip="${esc(tip)}"><span class="wp-chip-k">${esc(k)}</span>${esc(v)}</span>`;
        return `<div class="wp-setup">
          ${chip("agent", w.agent || "default", "The agent that runs this waypoint's relevance survey.\nAn API backend (claude, ollama) is called directly; a terminal agent (claude-code, codex) runs through the subprocess runner.", !w.agent)}
          ${chip("model", w.model || "default", "The model the survey agent runs as.", !w.model)}
          ${chip("advisory", w.allow_advisory ? "allowed" : "not allowed", "Whether roster entries may be set to advisory mode, which delivers the guidance without holding the work.", !w.allow_advisory)}
          ${chip("projects", w.projects.length ? w.projects.join(", ") : "none inferred", "Projects inferred by hopping through the roster's squads and reviews, resolved server-side.", !w.projects.length)}
          ${chip("delivery", parts.length ? parts.join(" · ") : "nothing yet", "Roster-entry counts by delivery status.", !parts.length)}
        </div>`;
      }

      /**
       * Removes one roster entry from a waypoint after confirmation, then refreshes the detail pane.
       * @param {string} waypointId
       * @param {string} entryId
       * @returns {Promise<void>}
       */
      async function removeRosterEntry(waypointId, entryId) {
        if (!confirm(`Remove roster entry "${entryId}" from this waypoint? This cannot be undone.`)) return;
        const resp = await del(`/api/waypoints/${waypointId}/roster/${entryId}`, { success: "Roster entry removed.", errorLabel: "remove roster entry" });
        if (resp.ok) { waypointDetail = await resp.json(); renderWaypointDetail(); }
      }

      /**
       * Flips a roster entry's mode between block and advisory, then refreshes the detail pane.
       * @param {string} waypointId
       * @param {string} entryId
       * @param {string} mode
       * @returns {Promise<void>}
       */
      async function toggleRosterEntryMode(waypointId, entryId, mode) {
        const resp = await patchJson(`/api/waypoints/${waypointId}/roster/${entryId}`, { mode }, { success: `Switched to ${mode} mode.`, errorLabel: "update roster entry mode" });
        if (resp.ok) { waypointDetail = await resp.json(); renderWaypointDetail(); }
      }

      /**
       * Closes a waypoint manually, then refreshes the detail pane and list.
       * @param {string} waypointId
       * @returns {Promise<void>}
       */
      async function closeWaypoint(waypointId) {
        const resp = await post(`/api/waypoints/${waypointId}/close`, undefined, { success: "Waypoint closed.", errorLabel: "close waypoint" });
        if (resp.ok) { waypointDetail = await resp.json(); await refreshWaypointsList(); renderWaypoints(); renderWaypointDetail(); }
      }

      /**
       * Reopens a closed waypoint, then refreshes the detail pane and list.
       * @param {string} waypointId
       * @returns {Promise<void>}
       */
      async function reopenWaypoint(waypointId) {
        const resp = await post(`/api/waypoints/${waypointId}/reopen`, undefined, { success: "Waypoint reopened.", errorLabel: "reopen waypoint" });
        if (resp.ok) { waypointDetail = await resp.json(); await refreshWaypointsList(); renderWaypoints(); renderWaypointDetail(); }
      }

      /**
       * Adds a roster entry to a waypoint (used by both the detail-pane "Add roster entry" flow and the "Add to waypoint…" context menu).
       * @param {string} waypointId
       * @param {string} kind
       * @param {string} entryId
       * @param {string} [mode]
       * @returns {Promise<void>}
       */
      async function addEntryToWaypoint(waypointId, kind, entryId, mode) {
        const body = mode ? { kind, entry_id: entryId, mode } : { kind, entry_id: entryId };
        const resp = await post(`/api/waypoints/${waypointId}/roster`, body, { success: `Added ${entryId} to waypoint.`, errorLabel: "add roster entry" });
        if (resp.ok) {
          if (selectedWaypointId === waypointId) { waypointDetail = await resp.json(); renderWaypointDetail(); }
          await refreshWaypointsList();
          renderWaypoints();
        }
      }

      /**
       * Opens the "Add roster entry" modal for a waypoint: pick a kind (squad/review) and type its id.
       * @param {string} waypointId
       * @returns {void}
       */
      function openAddRosterEntry(waypointId) {
        byId("modal-root").innerHTML = `<div class="modal-bg" onclick="if(event.target===this)closeModal()"><div class="modal">
          <h3 style="margin-top:0">Add roster entry</h3>
          <div class="kv-row"><span class="k">kind</span><span class="v">
            <select id="rw-kind" data-tip="Whether the id below is a squad id or a review (guardian) id.">
              <option value="squad">squad</option>
              <option value="review">review</option>
            </select>
          </span></div>
          <div class="kv-row"><span class="k">entry id</span><span class="v"><input type="text" id="rw-entry-id" placeholder="squad-... or guardian-..." style="width:100%" data-tip="The exact squad id or review id to add to this waypoint's roster."></span></div>
          <div class="kv-row"><span class="k">mode</span><span class="v">
            <select id="rw-mode" data-tip="Block mode can halt this entry's in-flight cells; advisory mode is informational only and never halts anything.">
              <option value="block">block</option>
              <option value="advisory">advisory</option>
            </select>
          </span></div>
          <div class="btn-row">
            <button class="btn" onclick="closeModal()" data-tip="Discard without adding a roster entry.">Cancel</button>
            <button class="btn primary" data-click="submitAddRosterEntry" data-waypoint-id="${esc(waypointId)}" data-tip="Add this roster entry to the waypoint.">Add</button>
          </div>
        </div></div>`;
      }

      /**
       * Reads the "Add roster entry" modal's fields and submits them, closing the modal on success.
       * @param {string} waypointId
       * @returns {Promise<void>}
       */
      async function submitAddRosterEntry(waypointId) {
        const kind = /** @type {HTMLSelectElement} */ (byId("rw-kind")).value;
        const entryId = /** @type {HTMLInputElement} */ (byId("rw-entry-id")).value.trim();
        const mode = /** @type {HTMLSelectElement} */ (byId("rw-mode")).value;
        if (!entryId) { notify("error", "Enter an entry id."); return; }
        await addEntryToWaypoint(waypointId, kind, entryId, mode);
        closeModal();
      }

      /**
       * Opens the "append a bearing" modal for a waypoint.
       * @param {string} waypointId
       * @returns {void}
       */
      function openAppendBearing(waypointId) {
        byId("modal-root").innerHTML = `<div class="modal-bg" onclick="if(event.target===this)closeModal()"><div class="modal">
          <h3 style="margin-top:0">Add bearing</h3>
          <div class="kv-row"><span class="k">summary</span><span class="v"><input type="text" id="bw-summary" style="width:100%" placeholder="concise summary of the work that just completed" data-tip="A short, permanent record of completed work. Bearings are append-only -- this cannot be edited or deleted afterward."></span></div>
          <div class="kv-row"><span class="k">entity uri</span><span class="v"><input type="text" id="bw-entity-uri" style="width:100%" placeholder="squad:... or guardian:... (optional)" data-tip="Optional link back to the entity this bearing was reported against."></span></div>
          <div class="kv-row"><span class="k">commit id</span><span class="v"><input type="text" id="bw-commit-id" style="width:100%" placeholder="(optional)" data-tip="Optional Git commit id -- a narrowing aid, not an assertion the currently-viewed base already contains it."></span></div>
          <div class="kv-row"><span class="k">commit summary</span><span class="v"><input type="text" id="bw-commit-summary" style="width:100%" placeholder="(optional)" data-tip="Optional one-line summary of the commit above."></span></div>
          <div class="btn-row">
            <button class="btn" onclick="closeModal()" data-tip="Discard without adding a bearing.">Cancel</button>
            <button class="btn primary" data-click="submitAppendBearing" data-waypoint-id="${esc(waypointId)}" data-tip="Append this bearing. This cannot be undone.">Add bearing</button>
          </div>
        </div></div>`;
      }

      /**
       * Reads the "append a bearing" modal's fields and submits them, closing the modal on success.
       * @param {string} waypointId
       * @returns {Promise<void>}
       */
      async function submitAppendBearing(waypointId) {
        const summary = /** @type {HTMLInputElement} */ (byId("bw-summary")).value.trim();
        if (!summary) { notify("error", "Enter a summary."); return; }
        const entityUri = /** @type {HTMLInputElement} */ (byId("bw-entity-uri")).value.trim();
        const commitId = /** @type {HTMLInputElement} */ (byId("bw-commit-id")).value.trim();
        const commitSummary = /** @type {HTMLInputElement} */ (byId("bw-commit-summary")).value.trim();
        const body = {
          producer_kind: "squad",
          producer_id: waypointId,
          summary,
          entity_uri: entityUri || null,
          commit_id: commitId || null,
          commit_summary: commitSummary || null,
        };
        const resp = await post(`/api/waypoints/${waypointId}/bearings`, body, { success: "Bearing added.", errorLabel: "add bearing" });
        if (resp.ok) {
          if (selectedWaypointId === waypointId) { waypointBearings = await (await fetch(`/api/waypoints/${waypointId}/bearings`)).json(); renderWaypointDetail(); }
          closeModal();
        }
      }

      /**
       * Opens the waypoint right-click context menu.
       * @param {MouseEvent} e
       * @param {string} id
       * @returns {void}
       */
      function openWaypointMenu(e, id) {
        e.preventDefault(); e.stopPropagation(); closeWaypointMenu();
        const w = waypoints.find((x) => x.id === id); if (!w) return;
        const entityUri = `waypoint:${id}`;
        const items = [
          `<div data-click="toggleWatch" data-entity-uri="${esc(entityUri)}" data-tip="${isWatching(entityUri) ? "Stop receiving watcher notifications for this waypoint." : "Watch this waypoint and choose which mailbox priority tiers should notify you."}">${isWatching(entityUri) ? "◉ Unwatch" : "◎ Watch…"}</div>`,
          `<div data-click="openAddRosterEntry" data-waypoint-id="${esc(id)}" data-tip="Add a squad or review to this waypoint's roster.">＋ Add roster entry</div>`,
          w.state === "open"
            ? `<div data-click="closeWaypoint" data-waypoint-id="${esc(id)}" data-tip="Close this waypoint manually.">■ Close</div>`
            : `<div data-click="reopenWaypoint" data-waypoint-id="${esc(id)}" data-tip="Reopen this waypoint.">▶ Reopen</div>`,
        ];
        const menu = document.createElement("div");
        menu.className = "ctx-menu"; menu.id = "waypoint-menu"; menu.innerHTML = items.join("");
        document.body.appendChild(menu);
        menu.style.left = Math.min(e.clientX, window.innerWidth - 180) + "px";
        menu.style.top = Math.min(e.clientY, window.innerHeight - 170) + "px";
      }

      /**
       * Closes the waypoint right-click context menu, if open.
       * @returns {void}
       */
      function closeWaypointMenu() { const m = document.getElementById("waypoint-menu"); if (m) m.remove(); }
      document.addEventListener("click", closeWaypointMenu);

      /**
       * Opens the "create waypoint" modal, optionally pre-seeding one roster entry (e.g. from "Add to waypoint… → New waypoint…").
       * @param {{kind: string, entry_id: string}} [seedEntry]
       * @returns {void}
       */
      function openCreateWaypoint(seedEntry) {
        const seedRow = seedEntry
          ? `<div class="kv-row"><span class="k">roster</span><span class="v mono">${esc(seedEntry.kind)}:${esc(seedEntry.entry_id)}</span></div>`
          : `<div class="kv-row"><span class="k">roster</span><span class="v"><input type="text" id="cw-roster-kind" placeholder="squad or review" style="width:90px" data-tip="Kind of the first roster entry: squad or review."> <input type="text" id="cw-roster-id" placeholder="entry id" style="width:220px" data-tip="Id of the first roster entry. A waypoint needs at least one roster entry to be created."></span></div>`;
        byId("modal-root").innerHTML = `<div class="modal-bg" onclick="if(event.target===this)closeModal()"><div class="modal">
          <h3 style="margin-top:0">Create waypoint</h3>
          <div class="kv-row"><span class="k">label</span><span class="v"><input type="text" id="cw-label" style="width:100%" placeholder="(optional)" data-tip="A short human-readable label for this waypoint."></span></div>
          <div class="kv-row"><span class="k">prompt</span><span class="v"><textarea id="cw-prompt" style="width:100%;min-height:60px" placeholder="what this waypoint is coordinating" data-tip="Required. Describes what this waypoint is coordinating -- shown (redacted) to roster entries."></textarea></span></div>
          <div class="kv-row"><span class="k">agent</span><span class="v"><input type="text" id="cw-agent" style="width:100%" placeholder="(default)" data-tip="Optional agent backend override for this waypoint's own work."></span></div>
          <div class="kv-row"><span class="k">model</span><span class="v"><input type="text" id="cw-model" style="width:100%" placeholder="(default)" data-tip="Optional model override for this waypoint's own work."></span></div>
          <div class="kv-row"><span class="k">advisory</span><span class="v"><label style="display:flex;align-items:center;gap:6px"><input type="checkbox" id="cw-allow-advisory" data-tip="Allow roster entries to be added in advisory mode (informational only, never halts anything).">allow advisory entries</label></span></div>
          ${seedRow}
          <div class="btn-row">
            <button class="btn" onclick="closeModal()" data-tip="Discard without creating a waypoint.">Cancel</button>
            <button class="btn primary" data-click="submitCreateWaypoint" data-seed-kind="${seedEntry ? esc(seedEntry.kind) : ""}" data-seed-entry-id="${seedEntry ? esc(seedEntry.entry_id) : ""}" data-tip="Create this waypoint.">Create</button>
          </div>
        </div></div>`;
      }

      /**
       * Reads the "create waypoint" modal's fields and submits them, closing the modal and selecting the new waypoint on success.
       * @param {string} seedKind
       * @param {string} seedEntryId
       * @returns {Promise<void>}
       */
      async function submitCreateWaypoint(seedKind, seedEntryId) {
        const prompt = /** @type {HTMLTextAreaElement} */ (byId("cw-prompt")).value.trim();
        if (!prompt) { notify("error", "Enter a prompt."); return; }
        let roster;
        if (seedEntryId) {
          roster = [{ kind: seedKind, entry_id: seedEntryId }];
        } else {
          const kind = /** @type {HTMLInputElement} */ (byId("cw-roster-kind")).value.trim().toLowerCase();
          const entryId = /** @type {HTMLInputElement} */ (byId("cw-roster-id")).value.trim();
          if (!kind || !entryId) { notify("error", "A waypoint needs at least one roster entry."); return; }
          roster = [{ kind, entry_id: entryId }];
        }
        const label = /** @type {HTMLInputElement} */ (byId("cw-label")).value.trim();
        const agent = /** @type {HTMLInputElement} */ (byId("cw-agent")).value.trim();
        const model = /** @type {HTMLInputElement} */ (byId("cw-model")).value.trim();
        const allowAdvisory = /** @type {HTMLInputElement} */ (byId("cw-allow-advisory")).checked;
        const body = { label: label || null, prompt, agent: agent || null, model: model || null, allow_advisory: allowAdvisory, roster };
        const resp = await post("/api/waypoints", body, { success: "Waypoint created.", errorLabel: "create waypoint" });
        if (resp.ok) {
          const created = await resp.json();
          closeModal();
          await refreshWaypointsList();
          selectWaypoint(created.id);
          showTab("waypoints", true);
        }
      }

      /**
       * Opens the "add to waypoint…" popup: pick an existing open waypoint or start a new one, for the given entity.
       * @param {MouseEvent} e
       * @param {string} kind
       * @param {string} entryId
       * @returns {Promise<void>}
       */
      async function openAddToWaypointMenu(e, kind, entryId) {
        e.preventDefault(); e.stopPropagation(); closeSquadMenu(); closeWaypointMenu();
        if (!waypoints.length) await refreshWaypointsList();
        const openWaypoints = waypoints.filter((w) => w.state === "open");
        const rows = openWaypoints.map((w) => `<div data-click="addEntryToWaypointFromMenu" data-kind="${esc(kind)}" data-entry-id="${esc(entryId)}" data-waypoint-id="${esc(w.id)}" data-tip="Add this ${esc(kind)} to \"${esc(w.label || w.id)}\"'s roster.">📍 ${esc(w.label || w.id)}</div>`).join("");
        const newRow = `<div data-click="openCreateWaypointFromMenu" data-kind="${esc(kind)}" data-entry-id="${esc(entryId)}" data-tip="Create a brand-new waypoint with this ${esc(kind)} as its first roster entry.">＋ New waypoint…</div>`;
        const menu = document.createElement("div");
        menu.className = "ctx-menu"; menu.id = "waypoint-menu";
        menu.innerHTML = (rows || `<div style="color:var(--muted);cursor:default;padding:6px 10px">No open waypoints.</div>`) + newRow;
        document.body.appendChild(menu);
        menu.style.left = Math.min(e.clientX, window.innerWidth - 220) + "px";
        menu.style.top = Math.min(e.clientY, window.innerHeight - 260) + "px";
      }

      /**
       * Delegated-handler wrapper: adds an entity to a waypoint from the "Add to waypoint…" popup, then closes it.
       * @param {string} kind
       * @param {string} entryId
       * @param {string} waypointId
       * @returns {Promise<void>}
       */
      async function addEntryToWaypointFromMenu(kind, entryId, waypointId) {
        closeWaypointMenu();
        await addEntryToWaypoint(waypointId, kind, entryId);
      }

      /**
       * Delegated-handler wrapper: opens the create-waypoint modal pre-seeded from the "Add to waypoint…" popup, then closes it.
       * @param {string} kind
       * @param {string} entryId
       * @returns {void}
       */
      function openCreateWaypointFromMenu(kind, entryId) {
        closeWaypointMenu();
        openCreateWaypoint({ kind, entry_id: entryId });
      }

      /**
       * Cell-graph-only entry point: adds the cell's owning squad as a waypoint roster entry (cell-level roster entries stay out of scope for now).
       * @param {MouseEvent} e
       * @param {string} squadId
       * @returns {Promise<void>}
       */
      async function openSetWaypointFromCell(e, squadId) {
        await openAddToWaypointMenu(e, "squad", squadId);
      }

      // Initial `#/waypoints…` routing, handled here rather than alongside every
      // other tab's in chunk 80. That branch runs while chunk 80 loads, and every
      // function it needs is defined in this chunk, which loads after it — so it
      // threw a ReferenceError and took the whole initial route down with it,
      // leaving every `#/waypoints` deep link on the default tab. Running it at
      // the end of this chunk is the point at which those functions exist.
      // `pendingHash` is deliberately left set, so `pollWaypoints` can still
      // apply `waypointId` once the list has loaded — the same handoff the
      // reviews and tasks tabs use.
      if (typeof pendingHash !== "undefined" && pendingHash && pendingHash.tab === "waypoints") {
        applyWaypointHashFilters(pendingHash.waypointHashFilters);
        renderWaypointStatusFilters();
        renderWaypointProjectFilterChips();
        showTab("waypoints");
      }
