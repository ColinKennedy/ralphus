      // ---------- Waypoints tab (RAL-400) ----------
      // A waypoint is a cross-squad coordination join point: a affected of squads
      // and/or reviews that must (block mode) or may (advisory mode) check in
      // before the waypoint's own work proceeds. This chunk owns the whole
      // "#/waypoints" tab: sidebar list + filters, detail pane (settings, affected,
      // delivery feed, bearings), the create/add-affected-entry modals, and the
      // "Add to waypoint…" context-menu entry points wired from squads/reviews/
      // the squad graph.

      /** Every waypoint lifecycle state, in sidebar filter-chip order. */
      const WAYPOINT_STATES = ["open", "closed"];

      /** Maps a AffectedEntryView.delivery_status to the `docs/colors.md`-documented CSS variable used for its status dot. @type {{[key: string]: string}} */
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
      /**
       * @typedef {object} WaypointFormDraft
       * @property {string} mode - "create" or "edit".
       * @property {string} id - the waypoint being edited; empty when creating.
       * @property {string} label
       * @property {string} prompt
       * @property {string} agent
       * @property {string} model
       * @property {boolean} allowAdvisory
       * @property {Set<string>} picked - `kind:id` affected keys, create-only.
       * @property {string} pickerKind - which affected tab is showing.
       * @property {string} pickerQuery - the affected picker's filter text.
       * @property {string} cwd - scopes the agent list, like the review edit modal does.
       * @property {{prompt: string, agent: string, model: string, allowAdvisory: boolean}} [original] - edit-only: the
       *   survey-affecting values the form opened with, so saving can tell whether re-judging is even on the table.
       */
      /**
       * @type {WaypointFormDraft|null} The open create/edit dialog's state. Held as a draft rather than read off the
       * DOM so the dialog can re-render when the agent and affected lists arrive without discarding what was typed —
       * the same reason the review edit modal keeps one.
       */
      let waypointFormDraft = null;
      /** @type {{id: string, name: string}[]} Squads offered by the create dialog's affected picker, fetched when it opens. */
      let createWaypointSquads = [];
      /** @type {{id: string, name: string}[]} Reviews offered by the create dialog's affected picker, fetched when it opens. */
      let createWaypointReviews = [];
      /** Whether the detail pane's guidance block is expanded. Collapsed by default: a waypoint's guidance can run
       * to many lines, and it should not push the affected and activity below the fold on every open. */
      let waypointGuidanceExpanded = false;
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
          ? "This waypoint is closed -- every affected entry has cleared or it was closed manually. No further deliveries are expected."
          : "This waypoint is open -- at least one affected entry is still expected to check in.";
        return `<span class="pill p-${esc(state)}" data-tip="${esc(tip)}">${esc(state)}</span>`;
      }

      /**
       * Renders a affected entry's delivery-status dot using the `docs/colors.md`-documented color for that status.
       * @param {string} deliveryStatus
       * @returns {string}
       */
      function wdot(deliveryStatus) {
        const colorVar = Object.prototype.hasOwnProperty.call(WAYPOINT_DELIVERY_COLORS, deliveryStatus) ? WAYPOINT_DELIVERY_COLORS[deliveryStatus] : "--muted";
        return `<span class="dot" style="background:${cvar(colorVar)}" data-tip="Delivery status: ${esc(deliveryStatus)}"></span>`;
      }

      /**
       * Renders a affected entry's block/advisory mode badge.
       * @param {string} mode
       * @returns {string}
       */
      function waypointModeBadge(mode) {
        const tip = mode === "advisory"
          ? "Advisory mode -- this entry is informational only and never halts anything."
          : "Block mode -- this entry can halt the waypoint's in-flight cells until it checks in.";
        return `<span class="badge affected-${esc(mode)}" data-tip="${esc(tip)}">${esc(mode)}</span>`;
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
            <span data-tip="How many squads and reviews this waypoint lands on.
Its roster -- the work that must land for it to be carried out -- is counted separately, on the waypoint itself.">${w.affected_count} affected</span>
            ${w.projects.length ? `<span class="wp-proj" data-tip="Projects inferred from the affected.">${esc(w.projects.join(", "))}</span>` : ""}
          </div>
        </div>`).join("");
      }

      /**
       * Renders a waypoint's affected as one segment per entry, coloured by that entry's delivery status — the same
       * vocabulary `wdot` uses. Progress read as shape rather than as a count, so a sidebar scan answers "how far
       * along is this one" without opening it. Falls back to a single muted segment for an empty affected.
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
        if (!segments.length) return `<span class="wp-spark" data-tip="No affected entries yet."><i></i></span>`;
        const tip = `Affected delivery: ${counts.delivered} delivered, ${counts.via_restack} via restack, ${counts.failed} failed, ${counts.undelivered} undelivered.`;
        const bars = segments.map((s) => `<i style="background:${cvar(WAYPOINT_DELIVERY_COLORS[s] || "--muted")}"></i>`).join("");
        return `<span class="wp-spark" data-tip="${esc(tip)}">${bars}</span>`;
      }

      /**
       * Renders the waypoint's lifecycle as a rail: open → surveyed → delivered → closed, with the step it is
       * currently on marked. Each step is derived from real affected state rather than stored, so it cannot drift
       * from the data. Answers "what is this waypoint waiting on", which a delivered-count alone does not.
       * @param {WaypointDetail} w
       * @returns {string}
       */
      function renderWaypointPipeline(w) {
        const total = w.affected.length;
        const surveyed = w.affected.filter((e) => e.survey_verdict).length;
        const ds = w.delivery_summary;
        const reached = ds.delivered + ds.via_restack;
        const closed = w.state === "closed";
        const steps = [
          { label: "open", done: true, tip: "The waypoint exists and is tracking its affected." },
          { label: "surveyed", done: total > 0 && surveyed >= total, tip: `Relevance decided for ${surveyed} of ${total} affected entries.` },
          { label: "delivered", done: total > 0 && reached >= total, tip: `Guidance reached ${reached} of ${total} affected entries.` },
          { label: "closed", done: closed, tip: closed ? "Closed — no further deliveries are expected." : "Closes when every affected entry reaches a terminal state, or when closed by hand." },
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
        // Selecting used to change which waypoint the pane *claims* to show
        // without fetching it, so `renderWaypointDetail` fell through to its
        // "Loading waypoint…" branch — `waypointDetail` still held the previous
        // waypoint — and stayed there until the next `pollWaypoints` tick. SSE
        // only ticks on daemon activity, so on an idle daemon that was the 60s
        // fallback interval: up to a minute of "Loading" for data that answers
        // in milliseconds.
        void loadWaypointDetail(id);
      }

      /**
       * Expands or collapses the detail pane's guidance block.
       * @returns {void}
       */
      function toggleWaypointGuidance() {
        waypointGuidanceExpanded = !waypointGuidanceExpanded;
        renderWaypointDetail();
      }

      /**
       * Fetches and renders one waypoint's detail pane immediately, for a selection that must not wait for the next
       * poll. Takes a `waypointPollSeq` ticket like `pollWaypoints` does, so whichever request was issued last is the
       * one that renders — a fast click through several waypoints settles on the one actually selected, not on
       * whichever response happens to land last.
       * @param {string} id
       * @returns {Promise<void>}
       */
      async function loadWaypointDetail(id) {
        const seq = ++waypointPollSeq;
        try {
          const bundle = await fetchWaypointDetailBundle(id);
          if (seq !== waypointPollSeq) { console.debug("loadWaypointDetail: superseded, abandoning"); return; }
          waypointDetail = bundle.detail;
          waypointBearings = bundle.bearings;
          waypointDeliveries = bundle.deliveries;
          renderWaypointDetail();
        } catch (e) {
          console.debug("loadWaypointDetail failed", e);
        }
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
       * Renders one affected entry row: kind icon, delivery dot, entity id (deep-linked), mode badge, and an expandable verdict/rationale.
       * @param {AffectedEntryView} entry
       * @returns {string}
       */
      function renderAffectedEntryRow(entry) {
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
        const removeBtn = `<button class="icon-btn" data-click="removeAffectedEntry" data-waypoint-id="${esc(entry.waypoint_id)}" data-entry-id="${esc(entry.entry_id)}" data-tip="Remove this entry from the waypoint's affected list.\nThis cannot be undone." style="font-size:11px;padding:1px 5px">✕</button>`;
        const toggleModeBtn = `<button class="icon-btn" data-click="toggleAffectedEntryMode" data-waypoint-id="${esc(entry.waypoint_id)}" data-entry-id="${esc(entry.entry_id)}" data-mode="${entry.mode === "advisory" ? "block" : "advisory"}" data-tip="Switch this entry to ${entry.mode === "advisory" ? "block" : "advisory"} mode.\n${entry.mode === "advisory" ? "Block holds this work until the waypoint closes." : "Advisory releases it while still delivering the guidance."}" style="font-size:11px;padding:1px 5px">⇄</button>`;
        return `<div class="wp-entry">
          <div class="wp-entry-top">
            ${wdot(entry.delivery_status)}
            ${link}
            ${waypointModeBadge(entry.mode)}
            <span class="badge" style="color:var(--muted);border-color:var(--border)" data-tip="Whether this waypoint's guidance has reached this entry yet.">${esc(entry.delivery_status)}</span>${staleBadge}${renderBearingDecisionBadge(entry)}
            <span class="wp-entry-actions">${toggleModeBtn}${removeBtn}</span>
          </div>
          ${verdict}
        </div>`;
      }

      /**
       * One row of the waypoint's completion list.
       *
       * Deliberately thinner than an affected row: a roster entry carries no survey verdict, no delivery status and
       * no answer, because none apply. It is not work the waypoint lands on, it is work the waypoint consists of.
       * All it has to do is finish.
       * @param {RosterEntryView} entry
       * @returns {string}
       */
      function renderRosterEntryRow(entry) {
        const icon = entry.kind === "review" ? "\u{1F500}" : "\u{1F9E9}";
        const gotoFn = entry.kind === "review" ? "gotoReview" : "gotoSquad";
        const idAttr = entry.kind === "review" ? `data-guardian-id="${esc(entry.entry_id)}"` : `data-squad-id="${esc(entry.entry_id)}"`;
        const link = `<a class="wp-entry-id" href="#" data-click="${gotoFn}" ${idAttr} data-tip="Open this ${esc(entry.kind)}'s own page.">${icon} ${esc(entry.entry_id)}</a>`;
        const state = entry.terminal
          ? `<span class="badge" style="color:var(--done);border-color:var(--done)" data-tip="This has finished. Once every roster entry has, the waypoint's own work has landed.">landed</span>`
          : `<span class="badge" style="color:var(--queued);border-color:var(--queued)" data-tip="This has not finished yet, so the waypoint's own work has not landed \u2014 which is what holds its block-mode affected entries.">outstanding</span>`;
        const note = entry.note ? `<div class="wp-entry-note">${esc(entry.note)}</div>` : "";
        const removeBtn = `<button class="icon-btn" data-click="removeRosterEntry" data-waypoint-id="${esc(w0().id)}" data-entry-id="${esc(entry.entry_id)}" data-tip="Drop this from the completion list.\nRemoving the last outstanding entry can complete the waypoint's first phase.\nThis cannot be undone." style="font-size:11px;padding:1px 5px">\u2715</button>`;
        return `<div class="wp-entry">
          <div class="wp-entry-top">
            ${link}
            ${state}
            <span class="wp-entry-actions">${removeBtn}</span>
          </div>
          ${note}
        </div>`;
      }

      /**
       * The waypoint currently open in the detail pane. A roster row needs its id for the remove action, and a
       * roster entry -- unlike an affected one -- does not carry its own `waypoint_id`.
       * @returns {WaypointDetail}
       */
      function w0() {
        return /** @type {WaypointDetail} */ (waypointDetail);
      }

      /**
       * The answer this entry gave the waypoint, as a badge.
       *
       * A waypoint cannot tell whether its guidance was acted on by watching the work stop, so an entry says so
       * itself. Deciding *not* to act is a real answer and reads as one here; what reads as outstanding is silence,
       * which for a block-mode entry is also what is still holding it.
       * @param {AffectedEntryView} entry
       * @returns {string}
       */
      function renderBearingDecisionBadge(entry) {
        const decision = entry.bearing_decision;
        if (!decision) {
          // Only worth flagging where the absence costs something.
          if (entry.mode !== "block") return "";
          return ` <span class="badge" style="color:var(--queued);border-color:var(--queued)" data-tip="This entry has not answered the waypoint yet, and is blocking because of it.\nIt answers by ending its run with a RALPHUS_BEARING: line.">awaiting answer</span>`;
        }
        const color = decision === "accepted" ? "--done" : decision === "rejected" ? "--failed" : "--queued";
        const why = decision === "accepted"
          ? "This entry took the waypoint's guidance up in its work."
          : decision === "rejected"
            ? "This entry considered the guidance and deliberately did not act on it. That is a valid answer, and it no longer blocks."
            : "The guidance applies here, but this entry is not acting on it now.";
        return ` <span class="badge" style="color:var(${color});border-color:var(${color})" data-tip="${why}">${esc(decision)}</span>`;
      }

      /**
       * Renders the delivery feed: a Cartographer-backed, chronological event list for the selected waypoint.
       * @returns {string}
       */
      function renderWaypointDeliveryFeed() {
        if (!waypointDeliveries.length) return `<div class="empty">Nothing has happened to this waypoint yet.</div>`;
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
          // Who published it, as a link. The producer is the squad or review
          // that did the work, so this is the jump to the worktree it was done
          // in — previously the row named the work without saying whose it was.
          const producer = b.producer_kind === "review"
            ? `<a href="#" data-click="gotoReview" data-guardian-id="${esc(b.producer_id)}" data-tip="Open the review that published this bearing, and its worktree.">🔀 ${esc(b.producer_id)}</a>`
            : `<a href="#" data-click="gotoSquad" data-squad-id="${esc(b.producer_id)}" data-tip="Open the squad that published this bearing, and its worktree.">🧩 ${esc(b.producer_id)}</a>`;
          const link = b.entity_uri ? `<a href="#" data-click="gotoEntityUri" data-entity-uri="${esc(b.entity_uri)}" data-tip="Open the specific entity this bearing points at.">${esc(b.entity_uri)}</a>` : "";
          const commit = b.commit_id
            ? `<span class="mono" data-tip="A lead for narrowing an investigation, not an assertion that the base you are looking at already contains this commit.">${esc(b.commit_id.slice(0, 12))}${b.commit_summary ? ` — ${esc(b.commit_summary)}` : ""}</span>`
            : "";
          const refs = `<div class="wp-feed-refs">${producer}${commit}${link}</div>`;
          return `<div class="wp-feed-row">
            <span class="wp-feed-when" data-tip="When this bearing was appended. Bearings are append-only and never edited.">${esc(new Date(b.created_at_ms).toLocaleString())}</span>
            <span class="wp-feed-body">${esc(b.summary)}${refs}</span>
          </div>`;
        }).join("");
      }

      /**
       * Renders the Waypoints tab's detail pane: settings, state, inferred projects, affected, delivery feed, and bearings.
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
          : `<button class="btn" data-click="reopenWaypoint" data-waypoint-id="${esc(w.id)}" data-tip="Reopen this waypoint.\nWho/when: more affected entries need to check in after it was closed.\nReopening makes it hold its block-mode entries again.">▶ Reopen</button>`;
        el.innerHTML = `
          <div class="wp-cmdbar">
            <div class="wp-cmd-id">
              <div class="wp-cmd-name">${esc(w.label || w.id)}</div>
              <div class="wp-cmd-sub mono" data-tip="This waypoint's id. Use it with the ralphus waypoint commands.">${esc(w.id)}</div>
            </div>
            <div class="wp-cmd-actions">
              ${waypointStateBadge(w.state)}
              <button class="btn" data-click="openEditWaypoint" data-waypoint-id="${esc(w.id)}" data-tip="Change this waypoint's label, guidance, survey agent/model, and whether advisory entries are allowed.\nApplies to future surveys and deliveries; entries already decided keep their verdict.">✎ Edit details</button>
              <button class="btn" data-click="toggleWatch" data-entity-uri="${esc(entityUri)}" data-tip="${isWatching(entityUri) ? "Stop receiving watcher notifications for this waypoint." : "Watch this waypoint and choose which mailbox priority tiers should notify you."}">${isWatching(entityUri) ? "◉ Unwatch" : "◎ Watch…"}</button>
              ${closeReopenBtn}
            </div>
          </div>
          ${renderWaypointPipeline(w)}
          ${renderWaypointSetup(w)}
          <div class="wp-guidance">
            <div class="wp-guidance-head">
              <span class="wp-guidance-label" data-tip="The coordination guidance this waypoint carries.\nIt is sent to the survey to decide which work is impacted, and delivered to the affected entries that are.">Guidance</span>
              <button class="btn sm" data-click="toggleWaypointGuidance" data-tip="${waypointGuidanceExpanded ? "Collapse the guidance back to a few lines." : "Show the full guidance. A waypoint's guidance can run long, so it is clamped by default."}">${waypointGuidanceExpanded ? "Collapse" : "Expand"}</button>
            </div>
            <div class="wp-prompt ${waypointGuidanceExpanded ? "is-open" : ""}">${esc(w.prompt)}</div>
          </div>
          <div class="wp-section">
            <h4>Roster</h4>
            <span class="wp-section-rule"></span>
            <button class="btn sm" data-click="openAddRosterEntry" data-waypoint-id="${esc(w.id)}" data-tip="Add a squad or review to this waypoint's completion list -- the work whose landing IS this waypoint being carried out.\nNothing is added here automatically: what must be true for this to be done is a statement of intent, not something the survey can discover.">＋ Add</button>
          </div>
          ${(w.roster || []).length ? w.roster.map(renderRosterEntryRow).join("") : `<div class="empty">No roster entries \u2014 nothing specific has to land for this waypoint to be carried out.</div>`}
          <div class="wp-section">
            <h4>Affected</h4>
            <span class="wp-section-rule"></span>
            <button class="btn sm" data-click="openAddAffectedEntry" data-waypoint-id="${esc(w.id)}" data-tip="Add a squad or review to this waypoint's affected by id.\nWho/when: you know a piece of work needs to respect this waypoint and don't want to wait for the survey to find it.">＋ Add</button>
          </div>
          ${w.affected.length ? w.affected.map(renderAffectedEntryRow).join("") : `<div class="empty">No affected entries.</div>`}
          <div class="wp-section">
            <h4>Activity log</h4>
            <span class="wp-section-rule"></span>
            <a class="btn sm" href="#/cartographer?scope=waypoint&q=${esc(w.id)}" data-tip="Open this in the Logs tab, where it can be filtered, searched and paged further back.\nThis section is the same log, narrowed to the rows that name this waypoint.">Open in Logs</a>
          </div>
          ${renderWaypointDeliveryFeed()}
          <div class="wp-section">
            <h4>Bearings</h4>
            <span class="wp-section-rule"></span>
            <button class="btn sm" data-click="openAppendBearing" data-waypoint-id="${esc(w.id)}" data-tip="Append a bearing -- a permanent record of completed work for this waypoint.\nWho/when: use this once you've finished a piece of coordinated work and want other affected entries to see it happened.\nBearings are append-only; this cannot be undone or edited afterward.">＋ Add</button>
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
          ${chip("advisory", w.allow_advisory ? "allowed" : "not allowed", "Whether affected entries may be set to advisory mode, which delivers the guidance without holding the work.", !w.allow_advisory)}
          ${chip("projects", w.projects.length ? w.projects.join(", ") : "none inferred", "Projects inferred by hopping through the affected's squads and reviews, resolved server-side.", !w.projects.length)}
          ${chip("delivery", parts.length ? parts.join(" · ") : "nothing yet", "Affected-entry counts by delivery status.", !parts.length)}
        </div>`;
      }

      /**
       * Removes one affected entry from a waypoint after confirmation, then refreshes the detail pane.
       * @param {string} waypointId
       * @param {string} entryId
       * @returns {Promise<void>}
       */
      async function removeAffectedEntry(waypointId, entryId) {
        if (!confirm(`Remove affected entry "${entryId}" from this waypoint? This cannot be undone.`)) return;
        const resp = await del(`/api/waypoints/${waypointId}/affected/${entryId}`, { success: "Affected entry removed.", errorLabel: "remove affected entry" });
        if (resp.ok) { waypointDetail = await resp.json(); renderWaypointDetail(); }
      }

      /**
       * Flips a affected entry's mode between block and advisory, then refreshes the detail pane.
       * @param {string} waypointId
       * @param {string} entryId
       * @param {string} mode
       * @returns {Promise<void>}
       */
      async function toggleAffectedEntryMode(waypointId, entryId, mode) {
        const resp = await patchJson(`/api/waypoints/${waypointId}/affected/${entryId}`, { mode }, { success: `Switched to ${mode} mode.`, errorLabel: "update affected entry mode" });
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
       * Adds a affected entry to a waypoint (used by both the detail-pane "Add affected entry" flow and the "Add to waypoint…" context menu).
       * @param {string} waypointId
       * @param {string} kind
       * @param {string} entryId
       * @param {string} [mode]
       * @returns {Promise<void>}
       */
      async function addEntryToWaypoint(waypointId, kind, entryId, mode) {
        const body = mode ? { kind, entry_id: entryId, mode } : { kind, entry_id: entryId };
        const resp = await post(`/api/waypoints/${waypointId}/affected`, body, { success: `Added ${entryId} to waypoint.`, errorLabel: "add affected entry" });
        if (resp.ok) {
          if (selectedWaypointId === waypointId) { waypointDetail = await resp.json(); renderWaypointDetail(); }
          await refreshWaypointsList();
          renderWaypoints();
        }
      }

      /**
       * Opens the "add a roster entry" dialog -- the completion list, not the affected one.
       *
       * No mode and no survey here: a roster entry is work the waypoint consists of, so the only thing asked of it
       * is that it finish. The note is for whoever reads this later wondering why this particular squad is what
       * "done" means.
       * @param {string} waypointId
       * @returns {void}
       */
      function openAddRosterEntry(waypointId) {
        byId("modal-root").innerHTML = `<div class="modal-bg" onclick="if(event.target===this)closeModal()"><div class="modal">
          <h3 style="margin-top:0">Add roster entry</h3>
          <p class="wp-rs-lead">The roster is what must land for this waypoint to be carried out. Its block-mode affected entries stay held until every roster entry here has finished.</p>
          <div class="kv-row"><span class="k">entry id</span><span class="v"><input type="text" id="rw-goal-id" placeholder="squad-... or guardian-..." style="width:100%" data-tip="The exact squad id or review id whose landing IS this waypoint being carried out.\nWhich kind it is comes from the id itself."></span></div>
          <div class="kv-row"><span class="k">note</span><span class="v"><input type="text" id="rw-goal-note" placeholder="why this is what done means" style="width:100%" data-tip="Optional. Why this entry is on the completion list, for whoever reads it later."></span></div>
          <div class="btn-row">
            <button class="btn" onclick="closeModal()" data-tip="Discard without adding a roster entry.">Cancel</button>
            <button class="btn primary" data-click="submitAddRosterEntry" data-waypoint-id="${esc(waypointId)}" data-tip="Add this to the waypoint's completion list.">Add</button>
          </div>
        </div></div>`;
      }

      /**
       * Sends the "add roster entry" dialog.
       * @param {string} waypointId
       * @returns {Promise<void>}
       */
      async function submitAddRosterEntry(waypointId) {
        const entryId = /** @type {HTMLInputElement} */ (byId("rw-goal-id")).value.trim();
        if (!entryId) { notify("error", "A roster entry needs an id."); return; }
        const note = /** @type {HTMLInputElement} */ (byId("rw-goal-note")).value.trim();
        // The id says which kind it is, the same way the remove route infers it.
        const kind = entryId.startsWith("guardian-") ? "review" : "squad";
        const resp = await post(`/api/waypoints/${waypointId}/roster`, { kind, entry_id: entryId, note: note || null },
          { success: "Added to the roster.", errorLabel: "add roster entry" });
        if (resp.ok) { closeModal(); waypointDetail = await resp.json(); renderWaypointDetail(); }
      }

      /**
       * Drops one entry from the waypoint's completion list. Removing the last outstanding one can complete the
       * waypoint's first phase, which is why it re-renders rather than only removing a row.
       * @param {string} waypointId
       * @param {string} entryId
       * @returns {Promise<void>}
       */
      async function removeRosterEntry(waypointId, entryId) {
        if (!confirm("Drop this from the waypoint's completion list?")) return;
        const resp = await del(`/api/waypoints/${waypointId}/roster/${entryId}`,
          { success: "Removed from the roster.", errorLabel: "remove roster entry" });
        if (resp.ok) { waypointDetail = await resp.json(); await refreshWaypointsList(); renderWaypoints(); renderWaypointDetail(); }
      }

      /**
       * Opens the "Add affected entry" modal for a waypoint: pick a kind (squad/review) and type its id.
       * @param {string} waypointId
       * @returns {void}
       */
      function openAddAffectedEntry(waypointId) {
        byId("modal-root").innerHTML = `<div class="modal-bg" onclick="if(event.target===this)closeModal()"><div class="modal">
          <h3 style="margin-top:0">Add affected entry</h3>
          <div class="kv-row"><span class="k">kind</span><span class="v">
            <select id="rw-kind" data-tip="Whether the id below is a squad id or a review (guardian) id.">
              <option value="squad">squad</option>
              <option value="review">review</option>
            </select>
          </span></div>
          <div class="kv-row"><span class="k">entry id</span><span class="v"><input type="text" id="rw-entry-id" placeholder="squad-... or guardian-..." style="width:100%" data-tip="The exact squad id or review id to add to this waypoint's affected list."></span></div>
          <div class="kv-row"><span class="k">mode</span><span class="v">
            <select id="rw-mode" data-tip="Block mode can halt this entry's in-flight cells; advisory mode is informational only and never halts anything.">
              <option value="block">block</option>
              <option value="advisory">advisory</option>
            </select>
          </span></div>
          <div class="btn-row">
            <button class="btn" onclick="closeModal()" data-tip="Discard without adding an affected entry.">Cancel</button>
            <button class="btn primary" data-click="submitAddAffectedEntry" data-waypoint-id="${esc(waypointId)}" data-tip="Add this affected entry to the waypoint.">Add</button>
          </div>
        </div></div>`;
      }

      /**
       * Reads the "Add affected entry" modal's fields and submits them, closing the modal on success.
       * @param {string} waypointId
       * @returns {Promise<void>}
       */
      async function submitAddAffectedEntry(waypointId) {
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
          `<div data-click="openAddAffectedEntry" data-waypoint-id="${esc(id)}" data-tip="Add a squad or review to this waypoint's affected list.">＋ Add affected entry</div>`,
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
       * Opens the "create waypoint" modal, optionally pre-seeding one affected entry (e.g. from "Add to waypoint… → New waypoint…").
       * @param {{kind: string, entry_id: string}} [seedEntry]
       * @returns {void}
       */
      function openCreateWaypoint(seedEntry) {
        waypointFormDraft = {
          mode: "create",
          id: "",
          label: "",
          prompt: "",
          agent: "",
          model: "",
          allowAdvisory: false,
          picked: new Set(seedEntry ? [`${seedEntry.kind}:${seedEntry.entry_id}`] : []),
          pickerKind: "squad",
          pickerQuery: "",
          cwd: "",
        };
        renderWaypointFormModal();
        const draft = waypointFormDraft;
        void loadCreateWaypointCandidates().then(() => {
          if (waypointFormDraft === draft) renderWaypointFormModal();
        });
        preloadAgentSelect("", () => waypointFormDraft === draft, renderWaypointFormModal);
      }

      /**
       * Opens the same form against an existing waypoint, for editing its settings.
       *
       * Edit and create are one dialog on one draft rather than two that drift: the fields are identical, and the
       * only differences are the title, which button saves, and that the affected picker is create-only — an existing
       * waypoint's affected is edited in place on the detail pane, where each entry has its own mode and verdict.
       * @param {string} waypointId
       * @returns {void}
       */
      function openEditWaypoint(waypointId) {
        const w = waypointDetail && waypointDetail.id === waypointId ? waypointDetail : null;
        if (!w) { notify("error", "Open the waypoint first."); return; }
        const cwd = w.projects && w.projects[0] ? w.projects[0] : "";
        waypointFormDraft = {
          mode: "edit",
          id: w.id,
          label: w.label || "",
          prompt: w.prompt || "",
          agent: w.agent || "",
          model: w.model || "",
          allowAdvisory: !!w.allow_advisory,
          picked: new Set(),
          pickerKind: "squad",
          pickerQuery: "",
          cwd,
          // What the form opened with. Saving only offers a re-survey when one
          // of the survey's own inputs actually moved -- renaming a waypoint
          // should never put held work back in the classifier's queue.
          original: {
            prompt: w.prompt || "",
            agent: w.agent || "",
            model: w.model || "",
            allowAdvisory: !!w.allow_advisory,
          },
        };
        renderWaypointFormModal();
        const draft = waypointFormDraft;
        preloadAgentSelect(cwd, () => waypointFormDraft === draft, renderWaypointFormModal);
      }

      /**
       * Records one form field as it is typed, so a re-render (after the agent list or the affected lists land) keeps
       * what was already entered. The review edit modal uses the same draft-and-rerender shape for the same reason.
       * @param {string} field
       * @param {string|boolean} value
       * @returns {void}
       */
      function onWaypointFormField(field, value) {
        if (!waypointFormDraft) return;
        /** @type {{[k: string]: any}} */ (waypointFormDraft)[field] = value;
      }

      /**
       * Renders the create/edit waypoint dialog from the current draft.
       * @returns {void}
       */
      function renderWaypointFormModal() {
        const d = waypointFormDraft;
        if (!d) return;
        const editing = d.mode === "edit";
        const affectedSection = editing
          ? ""
          : `<div class="wp-field">
              <label>Affected</label>
              <div class="wp-picked" id="cw-picked" data-tip="The squads and reviews this waypoint tracks. Click one to remove it."></div>
              <div class="wp-picker">
                <div class="wp-picker-tabs" id="cw-picker-tabs"></div>
                <div class="wp-picker-search">
                  <input type="text" id="cw-picker-q" value="${esc(d.pickerQuery)}" placeholder="filter by name or id…" oninput="onCreateWaypointPickerQuery(this.value)" data-tip="Narrow the list below by name or id.">
                </div>
                <div class="wp-picker-list" id="cw-picker-list"></div>
              </div>
              <span class="wp-hint">At least one is required. Hidden squads and reviews are not listed.</span>
            </div>`;
        byId("modal-root").innerHTML = `<div class="modal-bg" onclick="if(event.target===this)closeModal()"><div class="modal" style="width:620px;max-width:94vw">
          <h3 style="margin-top:0">${editing ? "Waypoint details" : "Create waypoint"}</h3>
          <div class="wp-form">
            <div class="wp-field">
              <label for="cw-label">Label</label>
              <input type="text" id="cw-label" value="${esc(d.label)}" oninput="onWaypointFormField('label',this.value)" placeholder="optional, e.g. rename greet to salute" data-tip="A short human-readable name. Shown wherever this waypoint appears; the id is used when it has none.">
            </div>
            <div class="wp-field">
              <label for="cw-prompt">Guidance</label>
              <textarea id="cw-prompt" oninput="onWaypointFormField('prompt',this.value)" placeholder="What is changing, and which work has to account for it?" data-tip="Required. This is what the survey reads to decide which work is impacted, and what gets delivered to the entries that are.\nWrite it so someone who has not seen the change can tell whether their own work touches it.">${esc(d.prompt)}</textarea>
              <span class="wp-hint">Read by the survey to decide impact, and delivered to the work it affects.</span>
            </div>
            <div class="wp-field-row">
              <div class="wp-field">
                <label for="cw-agent">Survey agent</label>
                ${renderAgentSelectHtml("cw-agent", d.cwd, d.agent, "onCreateWaypointAgentChange", "", "The agent that decides which work this waypoint affects.\nAn API backend (claude, ollama) is called directly; a terminal agent (claude-code, codex, pi) runs through the subprocess runner.")}
              </div>
              <div class="wp-field">
                <label for="cw-model">Model</label>
                <input type="text" id="cw-model" value="${esc(d.model)}" oninput="onWaypointFormField('model',this.value)" placeholder="default" data-tip="Optional model override for the survey agent. Leave empty to use that agent's own default.">
              </div>
            </div>
            ${affectedSection}
            <div class="wp-field">
              <label class="wp-check" data-tip="Lets the survey mark an entry advisory: it receives the guidance but is never held.\nOff means every impacted entry blocks until this waypoint closes."><input type="checkbox" id="cw-allow-advisory" ${d.allowAdvisory ? "checked" : ""} onchange="onWaypointFormField('allowAdvisory',this.checked)"> Allow advisory entries</label>
            </div>
          </div>
          <div class="btn-row">
            <button class="btn" onclick="closeModal()" data-tip="Discard without ${editing ? "saving" : "creating a waypoint"}.">Cancel</button>
            <button class="btn primary" data-click="${editing ? "submitEditWaypoint" : "submitCreateWaypoint"}" data-tip="${editing ? "Save these settings. They apply to future surveys and deliveries; entries already decided keep their verdict." : "Create this waypoint. It starts holding its block-mode entries immediately."}">${editing ? "Save" : "Create"}</button>
          </div>
        </div></div>`;
        if (!editing) renderCreateWaypointPicker();
      }

      /**
       * Records the survey agent chosen in the form. The shared agent `<select>` dispatches through a named global,
       * so this is its handler rather than a value read at submit time.
       * @param {string} value
       * @returns {void}
       */
      function onCreateWaypointAgentChange(value) {
        onWaypointFormField("agent", value);
      }
      // Reached only as a string handed to `renderAgentSelectHtml`, which no
      // parser can follow -- same reason `onNtAgentChange` carries one.
      void onCreateWaypointAgentChange;

      /**
       * Switches the affected picker between squads and reviews.
       * @param {string} kind
       * @returns {void}
       */
      function setCreateWaypointPickerKind(kind) {
        if (waypointFormDraft) waypointFormDraft.pickerKind = kind;
        renderCreateWaypointPicker();
      }

      /**
       * Filters the affected picker list as the search box is typed into.
       * @param {string} value
       * @returns {void}
       */
      function onCreateWaypointPickerQuery(value) {
        if (waypointFormDraft) waypointFormDraft.pickerQuery = value.toLowerCase();
        renderCreateWaypointPicker();
      }

      /**
       * Adds or removes one affected candidate. Keyed `kind:id` so a squad and a review can never collide.
       * @param {string} kind
       * @param {string} entryId
       * @param {boolean} on
       * @returns {void}
       */
      function toggleCreateWaypointPick(kind, entryId, on) {
        if (!waypointFormDraft) return;
        const key = `${kind}:${entryId}`;
        if (on) waypointFormDraft.picked.add(key); else waypointFormDraft.picked.delete(key);
        renderCreateWaypointPicker();
      }

      /**
       * Candidates for the affected picker, from the dialog's own fetch.
       * @returns {{[kind: string]: {id: string, name: string}[]}}
       */
      function createWaypointCandidates() {
        return { squad: createWaypointSquads, review: createWaypointReviews };
      }

      /**
       * Loads the squads and reviews the affected picker offers, when the dialog opens.
       *
       * Deliberately its own fetch rather than reading the shared `squads`/`guardians` globals: those are populated
       * by the Squads and Reviews tabs' own polls, so opening this dialog from the Waypoints tab showed whatever
       * those tabs had last left behind — in practice an empty review list, because nothing had visited that tab.
       * Fetching here also means the lists are only pulled when someone actually opens the picker.
       *
       * Hidden squads and reviews are dropped: a hidden entity is one the user deliberately removed from view, so
       * offering it here would quietly reintroduce it.
       * @returns {Promise<void>}
       */
      async function loadCreateWaypointCandidates() {
        const [taskIndex, reviewIndex] = await Promise.all([
          fetch("/api/tasks").then((r) => (r.ok ? r.json() : { squads: [] })).catch(() => ({ squads: [] })),
          fetch("/api/guardian-index").then((r) => (r.ok ? r.json() : [])).catch(() => []),
        ]);
        createWaypointSquads = (taskIndex.squads || [])
          .filter((/** @type {SquadView} */ sq) => !hiddenSquadIds.has(sq.id))
          .map((/** @type {SquadView} */ sq) => ({
            id: sq.id,
            name: sq.label || (sq.tasks && sq.tasks[0] ? sq.tasks[0].name : "") || sq.id,
          }));
        createWaypointReviews = (reviewIndex || [])
          .filter((/** @type {{id: string}} */ g) => !hiddenGuardianIds.has(g.id))
          .map((/** @type {{id: string, name?: string}} */ g) => ({ id: g.id, name: g.name || g.id }));
      }

      /**
       * Renders the affected picker's tabs, list and selected chips. Called on open and after every change, so the
       * selected set and the list's checkboxes can never disagree.
       * @returns {void}
       */
      function renderCreateWaypointPicker() {
        const d = waypointFormDraft;
        if (!d || !document.getElementById("cw-picker-list")) return;
        const all = createWaypointCandidates();
        byId("cw-picker-tabs").innerHTML = [["squad", "Squads"], ["review", "Reviews"]].map(([kind, label]) =>
          `<button type="button" class="wp-picker-tab ${d.pickerKind === kind ? "on" : ""}" onclick="setCreateWaypointPickerKind('${kind}')" data-tip="Pick ${label.toLowerCase()} for this waypoint's affected list.">${label}<span class="n">${all[kind].length}</span></button>`).join("");

        const q = d.pickerQuery;
        const rows = (all[d.pickerKind] || [])
          .filter((/** @type {{id: string, name: string}} */ c) => !q || c.id.toLowerCase().includes(q) || c.name.toLowerCase().includes(q));
        byId("cw-picker-list").innerHTML = rows.length
          ? rows.map((/** @type {{id: string, name: string}} */ c) => {
              const on = d.picked.has(`${d.pickerKind}:${c.id}`);
              return `<label class="wp-pick-row" data-tip="${esc(c.id)}">
                <input type="checkbox" ${on ? "checked" : ""} onchange="toggleCreateWaypointPick('${esc(d.pickerKind)}','${esc(c.id)}',this.checked)">
                <span class="wp-pick-name">${esc(c.name)}</span>
                <span class="wp-pick-id">${esc(c.id)}</span>
              </label>`;
            }).join("")
          : `<div class="empty" style="padding:10px">${q ? "Nothing matches that." : "None available."}</div>`;

        byId("cw-picked").innerHTML = [...d.picked].sort().map((key) => {
          const [kind, ...rest] = key.split(":");
          const id = rest.join(":");
          return `<span class="wp-chip" onclick="toggleCreateWaypointPick('${esc(kind)}','${esc(id)}',false)" data-tip="Remove this entry from the affected."><span class="wp-chip-k">${esc(kind)}</span>${esc(id)} ✕</span>`;
        }).join("");
      }

      /**
       * Creates a waypoint from the dialog's draft, then opens it.
       * @returns {Promise<void>}
       */
      async function submitCreateWaypoint() {
        const d = waypointFormDraft;
        if (!d) return;
        if (!d.prompt.trim()) { notify("error", "Enter the guidance this waypoint carries."); return; }
        const affected = [...d.picked].map((key) => {
          const [kind, ...rest] = key.split(":");
          return { kind, entry_id: rest.join(":") };
        });
        if (!affected.length) { notify("error", "Pick at least one squad or review for the affected."); return; }
        const body = { label: d.label.trim() || null, prompt: d.prompt.trim(), agent: d.agent || null, model: d.model.trim() || null, allow_advisory: d.allowAdvisory, affected };
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
       * Saves edited settings back to an existing waypoint, then refreshes its detail pane.
       * @returns {Promise<void>}
       */
      async function submitEditWaypoint() {
        const d = waypointFormDraft;
        if (!d) return;
        if (!d.prompt.trim()) { notify("error", "A waypoint needs guidance."); return; }
        // Only the survey's own inputs warrant re-judging. A label is not one.
        const o = d.original || { prompt: "", agent: "", model: "", allowAdvisory: false };
        const changedSurveyInputs = [
          o.prompt !== d.prompt.trim() ? "guidance" : "",
          o.agent !== (d.agent || "") ? "survey agent" : "",
          o.model !== d.model.trim() ? "model" : "",
          o.allowAdvisory !== d.allowAdvisory ? "advisory policy" : "",
        ].filter(Boolean);
        if (!changedSurveyInputs.length) { await saveWaypointSettings(d, false); return; }
        const resp = await fetch(`/api/waypoints/${d.id}/resurvey-preview`);
        if (!resp.ok) { notify("error", "Could not work out what re-running would affect."); return; }
        /** @type {ResurveyPreview} */
        const preview = await resp.json();
        renderResurveyConfirm(preview, changedSurveyInputs);
      }

      /**
       * Writes the draft's settings back, optionally re-queueing every daemon-enrolled affected entry for the survey.
       * @param {WaypointFormDraft} d
       * @param {boolean} resurvey
       * @returns {Promise<void>}
       */
      async function saveWaypointSettings(d, resurvey) {
        const body = { label: d.label.trim() || null, prompt: d.prompt.trim(), agent: d.agent || null, model: d.model.trim() || null, allow_advisory: d.allowAdvisory, resurvey };
        const resp = await patchJson(`/api/waypoints/${d.id}`, body, { success: resurvey ? "Saved; re-running the survey." : "Waypoint updated.", errorLabel: "update waypoint" });
        if (resp.ok) {
          closeModal();
          await refreshWaypointsList();
          void loadWaypointDetail(d.id);
        }
      }

      /**
       * Confirms a settings save that changed what the survey judges on, naming every entry it would re-run against.
       * Saving without re-running is offered too: the new guidance applies to work surveyed from here on, and
       * already-decided entries keep the verdict they were given.
       * @param {ResurveyPreview} preview
       * @param {string[]} changed - Which survey inputs the edit moved, for the explanation line.
       * @returns {void}
       */
      function renderResurveyConfirm(preview, changed) {
        const targets = preview.targets || [];
        const explicit = preview.held_explicit || [];
        const reheld = targets.filter((t) => t.will_be_held_until_judged).length;
        // `will_be_held_until_judged` only means anything for an entry that is
        // actually being re-judged. Showing it on the left-alone list would
        // say the opposite of what that list is for.
        /**
         * @param {ResurveyTarget} t
         * @param {boolean} rerunning - Whether this row is in the re-run list, so the hold badge means something.
         * @returns {string}
         */
        const row = (t, rerunning) => {
          const name = t.label ? `${esc(t.label)} <span class="wp-rs-id">${esc(t.entry_id)}</span>` : esc(t.entry_id);
          const verdict = t.current_verdict ? esc(t.current_verdict) : "not yet judged";
          return `<div class="wp-rs-row">
            <span class="wp-rs-kind">${esc(t.kind)}</span>
            <span class="wp-rs-name">${name}</span>
            <span class="wp-rs-now" data-tip="What the survey decided last time. A re-run replaces it.">${verdict}</span>
            ${rerunning && t.will_be_held_until_judged ? `<span class="wp-rs-hold" data-tip="This entry blocks while it has no verdict, so it is held again from the moment you save until the survey reaches it.">held until re-judged</span>` : ""}
          </div>`;
        };
        byId("modal-root").innerHTML = `<div class="modal-bg" onclick="if(event.target===this)closeModal()"><div class="modal" style="width:600px;max-width:94vw">
          <h3>Re-run the survey?</h3>
          <p class="wp-rs-lead">You changed the ${esc(changed.join(" and "))}, which is what the survey judges on. Its existing verdicts were reached against the old settings.</p>
          ${targets.length ? `<div class="wp-rs-head">Would re-run against ${targets.length} ${targets.length === 1 ? "entry" : "entries"}</div><div class="wp-rs-list">${targets.map((t) => row(t, true)).join("")}</div>` : `<div class="wp-rs-head">Nothing to re-run — this waypoint has no entries the survey owns.</div>`}
          ${reheld ? `<p class="wp-rs-warn" data-tip="The gate treats an entry with no verdict as uncleared, so it blocks until the survey reaches it again.">${reheld} of these ${reheld === 1 ? "is" : "are"} held the moment you save, until the survey re-judges ${reheld === 1 ? "it" : "them"} — including any the old settings had released.</p>` : ""}
          ${explicit.length ? `<div class="wp-rs-head">Left alone — ${explicit.length} declared by hand</div><div class="wp-rs-list muted">${explicit.map((t) => row(t, false)).join("")}</div>` : ""}
          <div class="btn-row" style="margin-top:14px;justify-content:flex-end">
            <button class="btn" data-click="closeModal" data-tip="Go back to the form. Nothing is saved.">Cancel</button>
            <button class="btn" data-click="saveWaypointWithoutResurvey" data-tip="Save the new settings, but leave every existing verdict alone.\nThe new guidance still applies to work the survey judges from here on.">Save without re-running</button>
            <button class="btn primary" data-click="saveWaypointAndResurvey" data-tip="Save, then put every entry above back in the survey's queue to be judged against the new settings." ${targets.length ? "" : "disabled"}>Save and re-run</button>
          </div>
        </div></div>`;
      }

      /**
       * Confirm-dialog action: save the edited settings and re-queue the survey.
       * @returns {Promise<void>}
       */
      async function saveWaypointAndResurvey() {
        if (waypointFormDraft) await saveWaypointSettings(waypointFormDraft, true);
      }

      /**
       * Confirm-dialog action: save the edited settings and leave existing verdicts alone.
       * @returns {Promise<void>}
       */
      async function saveWaypointWithoutResurvey() {
        if (waypointFormDraft) await saveWaypointSettings(waypointFormDraft, false);
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
        const rows = openWaypoints.map((w) => `<div data-click="addEntryToWaypointFromMenu" data-kind="${esc(kind)}" data-entry-id="${esc(entryId)}" data-waypoint-id="${esc(w.id)}" data-tip="Add this ${esc(kind)} to \"${esc(w.label || w.id)}\"'s affected.">📍 ${esc(w.label || w.id)}</div>`).join("");
        const newRow = `<div data-click="openCreateWaypointFromMenu" data-kind="${esc(kind)}" data-entry-id="${esc(entryId)}" data-tip="Create a brand-new waypoint with this ${esc(kind)} as its first affected entry.">＋ New waypoint…</div>`;
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
       * Cell-graph-only entry point: adds the cell's owning squad as a waypoint affected entry (cell-level affected entries stay out of scope for now).
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
