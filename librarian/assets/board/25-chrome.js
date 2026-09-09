      // ---------- theme ----------
      // RAL-257: the toggle swaps inline SVG icons instead of a Unicode glyph.
      // Each icon uses `currentColor`, so it inherits the .icon-btn
      // `color: var(--text)` and stays legible in either theme: the flashbang
      // (light theme) renders dark-on-light, the night-vision goggles (dark
      // theme) render light-on-dark.
      const FLASHBANG_ICON = `<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="5.5" y="8.5" width="13" height="12.5" rx="2.8"/><line x1="5.5" y1="11.5" x2="18.5" y2="11.5"/><path d="M7.5 8.5a4.5 3.5 0 0 1 9 0"/><circle cx="16" cy="4.4" r="1.7"/><line x1="14.7" y1="6" x2="12.6" y2="7.6"/></svg>`;
      const NIGHTVISION_ICON = `<svg viewBox="0 0 24 24" width="16" height="16" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><circle cx="8" cy="13" r="4.4"/><circle cx="16" cy="13" r="4.4"/><circle cx="8" cy="13" r="1.9"/><circle cx="16" cy="13" r="1.9"/><line x1="12" y1="11.8" x2="12" y2="14.2"/><path d="M7.6 8.7c-2.5-1.2-4.2-1.2-4.5-2"/><path d="M16.4 8.7c2.5-1.2 4.2-1.2 4.5-2"/></svg>`;
      /**
       * Applies a theme ("light"|"dark") to the document root and updates the toggle button icon.
       * @param {string} t
       * @returns {void}
       */
      function applyTheme(t) {
        document.documentElement.dataset.theme = t;
        const icon = t === "light" ? FLASHBANG_ICON : NIGHTVISION_ICON;
        byId("theme-btn").innerHTML = icon;
        byId("prefs-theme-btn").innerHTML = icon;
      }
      /**
       * Flips the current theme and persists the choice to localStorage.
       * @returns {void}
       */
      function toggleTheme() {
        const t = document.documentElement.dataset.theme === "light" ? "dark" : "light";
        localStorage.setItem("ralphus-theme", t); applyTheme(t);
      }
      applyTheme(localStorage.getItem("ralphus-theme") || "dark");

      // ---------- resizable panes (RAL-12) ----------
      // The squads-page sidebar/details columns and the tasks-page details column
      // are var-driven grid tracks; a thin splitter beside each drags its width.
      // Widths persist in localStorage.
      /**
       * @typedef {object} SplitCfgEntry
       * @property {string} key
       * @property {number} def
       * @property {number} min
       * @property {number} [max]
       */
      /** @type {{[key: string]: SplitCfgEntry}} */
      const SPLIT_CFG = {
        "--sidebar-w": { key: "ralphus-sidebar-w", def: 280, min: 180, max: 520 },
        // RAL-111: no max here — the details pane can be dragged arbitrarily wide.
        // Its effective ceiling is computed dynamically by detailsMaxW() instead,
        // so the center graph pane never collapses below MIN_CENTER_W.
        "--details-w": { key: "ralphus-details-w", def: 340, min: 240 },
        // Same idea for the flat Tasks tab's details pane, whose ceiling is
        // computed by taskDetailsMaxW() so the table pane never collapses below MIN_CENTER_W.
        "--task-details-w": { key: "ralphus-task-details-w", def: 340, min: 240 },
      };
      const MIN_CENTER_W = 240; // guard rail for the middle graph pane, matching --sidebar-w's existing min-width pattern
      /**
       * Computes the details pane's dynamic max width so the center graph pane never collapses below MIN_CENTER_W.
       * @returns {number}
       */
      function detailsMaxW() {
        const sidebarW = parseFloat(getComputedStyle(document.documentElement).getPropertyValue("--sidebar-w")) || SPLIT_CFG["--sidebar-w"].def;
        return Math.max(SPLIT_CFG["--details-w"].min, window.innerWidth - sidebarW - 12 - MIN_CENTER_W);
      }
      /**
       * Computes the Tasks tab details pane's dynamic max width so the table pane never collapses below MIN_CENTER_W.
       * @returns {number}
       */
      function taskDetailsMaxW() {
        return Math.max(SPLIT_CFG["--task-details-w"].min, window.innerWidth - 12 - MIN_CENTER_W);
      }
      /**
       * Resolves a pane's effective max width for the current viewport.
       * @param {string} varName
       * @param {SplitCfgEntry} c
       * @returns {number}
       */
      function paneMax(varName, c) { return varName === "--details-w" ? detailsMaxW() : varName === "--task-details-w" ? taskDetailsMaxW() : (c.max ?? Infinity); }
      /** @type {{el: Element, varName: string, invert: boolean, startX: number, startW: number, min: number}|null} */
      let splitDrag = null;
      /**
       * Restores persisted pane widths from localStorage on load.
       * @returns {void}
       */
      function applyPaneWidths() {
        for (const [v, c] of Object.entries(SPLIT_CFG)) {
          const saved = parseFloat(localStorage.getItem(c.key) ?? "");
          if (!Number.isNaN(saved)) {
            document.documentElement.style.setProperty(v, Math.max(c.min, Math.min(paneMax(v, c), saved)) + "px");
          }
        }
      }
      /**
       * Drag-move handler for an active pane splitter.
       * @param {MouseEvent} e
       * @returns {void}
       */
      function onSplitMove(e) {
        if (!splitDrag) return;
        const { varName, invert, startX, startW, min } = splitDrag;
        const delta = (e.clientX - startX) * (invert ? -1 : 1);
        const w = Math.max(min, Math.min(paneMax(varName, SPLIT_CFG[varName]), startW + delta));
        document.documentElement.style.setProperty(varName, w + "px");
      }
      /**
       * Drag-end handler that persists the final pane width.
       * @returns {void}
       */
      function onSplitUp() {
        if (!splitDrag) return;
        const c = SPLIT_CFG[splitDrag.varName];
        const w = parseFloat(getComputedStyle(document.documentElement).getPropertyValue(splitDrag.varName)) || c.def;
        localStorage.setItem(c.key, String(Math.round(w)));
        splitDrag.el.classList.remove("dragging");
        document.body.style.userSelect = "";
        document.body.style.cursor = "";
        window.removeEventListener("mousemove", onSplitMove);
        window.removeEventListener("mouseup", onSplitUp);
        splitDrag = null;
      }
      /**
       * Wires up mousedown-drag behavior on every `.splitter` element in the squads and tasks pages.
       * @returns {void}
       */
      function initSplitters() {
        applyPaneWidths();
        document.querySelectorAll("#squads-page .splitter, #tasks-page .splitter").forEach((sp0) => {
          const el = /** @type {HTMLElement} */ (sp0);
          el.addEventListener("mousedown", (/** @type {MouseEvent} */ e) => {
            e.preventDefault();
            const varName = el.dataset.var ?? "";
            const c = SPLIT_CFG[varName];
            const startW = parseFloat(getComputedStyle(document.documentElement).getPropertyValue(varName)) || c.def;
            splitDrag = { el, varName, invert: el.dataset.invert === "1", startX: e.clientX, startW, min: c.min };
            el.classList.add("dragging");
            document.body.style.userSelect = "none";
            document.body.style.cursor = "col-resize";
            window.addEventListener("mousemove", onSplitMove);
            window.addEventListener("mouseup", onSplitUp);
          });
        });
      }
      initSplitters();

      // ---------- tabs ----------
      const TABS = ["squads", "tasks", "queue", "reviews", "resources", "cartographer", "projects", "machines", "triage", "users", "secrets", "worktree-retirement", "prefs"];
      /**
       * Switches the active top-level tab, updates its URL hash, and re-renders.
       * @param {string} name
       * @param {boolean} [push]
       * @returns {void}
       */
      function showTab(name, push = false) {
        // RAL-332: a non-admin never lands on an admin-only tab, even via a
        // direct #/users-style hash -- the daemon enforces the real gate
        // server-side; this just keeps the client from rendering a page
        // whose data it can no longer fetch.
        if (ADMIN_ONLY_TABS.includes(name) && whoAmIResolved && !currentUserIsAdmin) name = "squads";
        // RAL-332 "Edit Profile": viewing another user's Preferences ends the
        // instant the admin navigates anywhere else -- never persisted.
        if (tab === "prefs" && name !== "prefs") prefsViewingAs = null;
        tab = name;
        // A freshness stamp belongs to the tab that fetched it -- carrying one
        // over from the previous tab reads as "this tab just refreshed" when
        // it didn't.
        byId("updated").textContent = "—";
        for (const t of TABS) {
          const page = byId(t + "-page");
          page.style.display = name === t ? (t === "resources" || t === "queue" || t === "cartographer" || t === "projects" || t === "machines" || t === "triage" || t === "users" || t === "secrets" || t === "worktree-retirement" || t === "prefs" ? "block" : "grid") : "none";
          byId("tab-" + t).classList.toggle("active", name === t);
        }
        // Entering the Queue tab is a deliberate action, so refresh it once on
        // entry even though periodic auto-update is off by default.
        if (name === "queue") queueLoaded = false;
        syncHash(push);
        tick();
      }
      /**
       * Shows or hides the admin-only tab buttons (RAL-332) to match
       * `currentUserIsAdmin`. A UI-level convenience only -- the daemon
       * enforces the real gate server-side on every endpoint those tabs use.
       * @returns {void}
       */
      function applyAdminTabVisibility() {
        for (const t of ADMIN_ONLY_TABS) {
          byId("tab-" + t).style.display = currentUserIsAdmin ? "" : "none";
        }
      }
      /**
       * Polls `GET /api/whoami` (RAL-332) for the caller's own resolved
       * identity and admin flag, and applies admin-only tab visibility.
       * Called from `tick()` regardless of tab, so a promotion/demotion made
       * elsewhere (e.g. another admin's browser) shows up without reload.
       * @returns {Promise<void>}
       */
      async function pollWhoAmI() {
        try {
          const res = await fetch("/api/whoami");
          if (!res.ok) return;
          /** @type {WhoAmI} */
          const d = await res.json();
          currentUserName = d.name;
          currentUserIsAdmin = d.is_admin;
          whoAmIResolved = true;
        } catch (e) { /* transient -- the next tick retries */ }
        applyAdminTabVisibility();
        // A non-admin whose admin flag was just revoked mid-session should
        // not stay parked on a page it can no longer fetch.
        if (whoAmIResolved && !currentUserIsAdmin && ADMIN_ONLY_TABS.includes(tab)) showTab("squads", true);
      }

      // ---------- hash routing ----------
      // RAL-188 §C.8: the board *produces* the ralphus URI form and accepts it
      // alongside the legacy positional one:
      //
      //   produced   #/tasks?q=&sort=&dir=&status=&sel=ralphus:/SQUAD[…]/CELL[…]?id=squad-…
      //              #/reviews?q=&status=&sel=ralphus:/REVIEW[…]?id=guardian-…
      //   accepted   #/tasks/<squad>?sel=kind:ti:si[:vi]&…   (legacy, still linked to from elsewhere)
      //              #/reviews/<id>?q=&status=             (legacy)
      //
      // `sel` is always written **last** and read as "everything to the end of
      // the hash", because a ralphus URI legitimately contains `?` and `&`
      // (`…?id=squad-1`). That keeps the URI raw and legible in the address bar
      // instead of percent-encoding it into noise — the entire point of the
      // scheme. `[`, `]`, `?` and `/` are all legal raw in a URL fragment, and
      // any character that isn't survives round-tripping as a `%XX` escape
      // `uriDecodeLabel` reads back.
      /** Matches the `sel=` parameter and everything after it, so an embedded `?id=…&…` stays intact. */
      // RALPHUS-HASH-SEL:BEGIN
      const HASH_SEL_RE = /[?&]sel=/;
      /**
       * @typedef {object} ParsedHash
       * @property {string} tab
       * @property {string|null} [guardianId]
       * @property {{[key: string]: string}} [cartoQuery]
       * @property {string|null} [squadId]
       * @property {string|null} [sel] legacy `kind:ti:si[:vi]` selector
       * @property {ParsedUri|null} [uri] RAL-188 selection, when the hash carried one
       */
      /**
       * Splits a raw hash into `[everything-before-sel, sel-value-or-null]`.
       * @param {string} raw
       * @returns {[string, string|null]}
       */
      function splitHashSel(raw) {
        const at = raw.search(HASH_SEL_RE);
        return at < 0 ? [raw, null] : [raw.slice(0, at), raw.slice(at + 5)];
      }
      // RALPHUS-HASH-SEL:END
      /**
       * Joins a base route, a filter query string, and a trailing raw `sel=` value.
       * @param {string} base
       * @param {string} qs
       * @param {string} selUri
       * @returns {string}
       */
      function hashWithSel(base, qs, selUri) {
        const parts = [qs, selUri ? `sel=${selUri}` : ""].filter(Boolean);
        return parts.length ? `${base}?${parts.join("&")}` : base;
      }
      /** @type {ParsedHash|null} the hash-derived route waiting to be applied once its data has loaded */
      let pendingHash = null;
      /**
       * Rewrites `location.hash` to reflect the current tab/selection/filter state.
       * @param {boolean} [push]
       * @returns {void}
       */
      function syncHash(push = false) {
        let url;
        if (tab === "resources") {
          url = "#/resources";
        } else if (tab === "queue") {
          url = "#/queue";
        } else if (tab === "cartographer") {
          // RAL-112: every filter control is part of the URL so a link reproduces
          // the same view; a snapshot (?id=) link ignores the live filters, since
          // its whole point is to survive them changing later.
          // RAL-113: the set of expanded rows rides along in both cases, so a
          // shared link (or the back/forward buttons) also reproduces which rows
          // were open, not just which rows matched the filter.
          if (cartoSnapshotId) {
            const p = new URLSearchParams();
            p.set("id", cartoSnapshotId);
            if (cartoExpanded.size) p.set("expanded", [...cartoExpanded].join(","));
            url = `#/cartographer?${p.toString()}`;
          } else {
            const p = new URLSearchParams();
            for (const k of /** @type {(keyof CartoFilter)[]} */ (["q", "source", "scope", "level", "squad_id", "guardian_id", "cell_id"])) {
              if (cartoFilter[k]) p.set(k, String(cartoFilter[k]));
            }
            if (cartoExpanded.size) p.set("expanded", [...cartoExpanded].join(","));
            const qs = p.toString();
            url = qs ? `#/cartographer?${qs}` : "#/cartographer";
          }
        } else if (tab === "projects") {
          url = "#/projects";
        } else if (tab === "machines") {
          url = "#/machines";
        } else if (tab === "triage") {
          url = "#/triage";
        } else if (tab === "users") {
          url = "#/users";
        } else if (tab === "secrets") {
          url = "#/secrets";
        } else if (tab === "prefs") {
          const p = new URLSearchParams();
          if (hiddenFilters.q) p.set("q", hiddenFilters.q);
          if (hiddenFilters.type.size !== HIDDEN_KINDS.length) p.set("type", [...hiddenFilters.type].join(","));
          const qs = p.toString();
          url = qs ? `#/prefs?${qs}` : "#/prefs";
        } else if (tab === "reviews") {
          const p = new URLSearchParams();
          if (reviewFilters.q) p.set("q", reviewFilters.q);
          if (reviewFilters.status.size !== GUARDIAN_STATES.length) p.set("status", [...reviewFilters.status].join(","));
          if (reviewResolverDefaulted) p.set("resolver", [...reviewFilters.resolver].join(","));
          if (reviewFilters.showHidden) p.set("hidden", "1");
          // The review's *name* is what makes the link legible; `?id=` keeps it
          // resolvable after a rename (§C.3). Falls back to the legacy
          // `#/reviews/<id>` path only when the guardian hasn't loaded yet.
          const guardian = selectedGuardian ? findGuardian(selectedGuardian) : null;
          const selUri = guardian
            ? `ralphus:${URI_SEGMENT_SEPARATOR_TOKEN}${uriSegment("REVIEW", guardian.name || guardian.id, 0)}${URI_QUERY_OPEN_TOKEN}id${URI_QUERY_ASSIGN_TOKEN}${uriEncodeQueryValue(guardian.id)}`
            : "";
          const base = selectedGuardian && !guardian ? `#/reviews/${selectedGuardian}` : "#/reviews";
          url = hashWithSel(base, p.toString(), selUri);
        } else if (tab === "tasks") {
          const p = new URLSearchParams();
          if (taskTabFilters.q) p.set("q", taskTabFilters.q);
          if (taskTabFilters.sort !== "squad") p.set("sort", taskTabFilters.sort);
          if (taskTabFilters.dir !== 1) p.set("dir", "desc");
          if (taskTabFilters.status.size !== STATES.length) p.set("status", [...taskTabFilters.status].join(","));
          if (taskTabFilters.showHidden) p.set("hidden", "1");
          if (taskTabFilters.needsMe) p.set("needsme", "1");
          if (taskTabFilters.groupBySquad) p.set("group", "1");
          if (taskTabExpanded.size) p.set("expanded", [...taskTabExpanded].join(","));
          const squad = taskTabSel.squadId ? findSquad(taskTabSel.squadId) : null;
          const task = squad && taskTabSel.kind ? (squad.tasks || [])[taskTabSel.taskIdx] : null;
          const selUri = squad && task ? taskTabSelectionUri(squad.id, task, taskTabSel) : "";
          url = hashWithSel("#/tasks", p.toString(), selUri);
        } else {
          const p = new URLSearchParams();
          if (filters.q) p.set("q", filters.q);
          if (filters.sort !== "date") p.set("sort", filters.sort);
          if (filters.dir !== -1) p.set("dir", "asc");
          if (filters.status.size !== STATES.length) p.set("status", [...filters.status].join(","));
          if (filters.showHidden) p.set("hidden", "1");
          const squad = selectedSquadId ? findSquad(selectedSquadId) : null;
          if (squad) {
            url = hashWithSel("#/squads", p.toString(), squadSelectionUri(squad, sel));
          } else {
            // The squad isn't in `squads` yet (first paint, or a hash applied
            // before the first poll) so there are no names to build a URI
            // from — emit the legacy positional form until it loads.
            if (sel.kind) { const vi = sel.kind === "proof" ? `:${sel.proofIdx ?? -1}` : ""; p.set("sel", `${sel.kind}:${sel.taskIdx}:${sel.cellIdx}${vi}`); }
            const base = selectedSquadId ? `#/squads/${selectedSquadId}` : "#/squads";
            const qs = p.toString();
            url = qs ? `${base}?${qs}` : base;
          }
        }
        if (!_inPopstate && push) history.pushState(null, "", url);
        else history.replaceState(null, "", url);
      }
      /**
       * Parses `location.hash` into a route descriptor, and side-effects `filters` for the tasks-tab case.
       * @returns {ParsedHash|null}
       */
      function parseHash() {
        const raw = location.hash.replace(/^#\/?/, "");
        if (!raw) return null;
        if (raw.startsWith("reviews")) {
          const [head, selValue] = splitHashSel(raw);
          const [path, query] = head.split("?");
          const p = new URLSearchParams(query || "");
          reviewFilters = defaultReviewFilters();
          const pq = p.get("q"); if (pq !== null) reviewFilters.q = pq.toLowerCase();
          const pstatus = p.get("status"); if (pstatus !== null) reviewFilters.status = new Set(pstatus.split(",").filter(Boolean));
          const presolver = p.get("resolver");
          reviewResolverDefaulted = presolver !== null;
          if (presolver !== null) reviewFilters.resolver = new Set(presolver.split(",").filter(Boolean));
          reviewFilters.showHidden = p.get("hidden") === "1";
          // `?id=` is authoritative; the REVIEW[...] label is the human-facing
          // half and is resolved against the loaded list in `pollReviews`.
          const uri = looksLikeUri(selValue) ? parseRalphusUri(/** @type {string} */ (selValue)) : null;
          const reviewName = uri && uri.segments[0] ? uri.segments[0].name : null;
          return { tab: "reviews", guardianId: (uri && uri.query.id) || path.split("/")[1] || null, uri, sel: reviewName };
        }
        if (raw.startsWith("resources")) return { tab: "resources" };
        if (raw.startsWith("queue")) return { tab: "queue" };
        if (raw.startsWith("cartographer")) {
          const cartoQuery = Object.fromEntries(new URLSearchParams(raw.split("?")[1] || "").entries());
          return { tab: "cartographer", cartoQuery };
        }
        if (raw.startsWith("projects")) return { tab: "projects" };
        if (raw.startsWith("machines")) return { tab: "machines" };
        if (raw.startsWith("triage")) return { tab: "triage" };
        if (raw.startsWith("users")) return { tab: "users" };
        if (raw.startsWith("secrets")) return { tab: "secrets" };
        if (raw.startsWith("prefs")) {
          const [, query] = raw.split("?");
          const p = new URLSearchParams(query || "");
          hiddenFilters = defaultHiddenFilters();
          const pq = p.get("q"); if (pq !== null) hiddenFilters.q = pq.toLowerCase();
          const ptype = p.get("type"); if (ptype !== null) hiddenFilters.type = new Set(ptype.split(",").filter(Boolean));
          return { tab: "prefs" };
        }
        if (raw.startsWith("squads")) return parseSquadsHashBody(raw);
        if (raw.startsWith("tasks")) {
          return isLegacySquadsTasksHash(raw) ? parseSquadsHashBody(raw) : parseTasksTabHash(raw);
        }
        return null;
      }
      // RALPHUS-LEGACY-TASKS-HASH:BEGIN
      /**
       * Whether a `#tasks...` hash is really an old (pre-RAL-362) squad-viewer
       * link -- back when the squad viewer itself lived at `#/tasks` -- rather
       * than a link into the new flat Tasks tab. Recognized by any of the
       * three shapes the old viewer ever emitted: a positional squad id in the
       * path, a bare legacy `kind:ti:si[:vi]` selector, or a URI selector whose
       * first segment addresses a SQUAD (see `squadSelectionUri`). The new
       * Tasks tab never emits any of these -- its own selector URIs start
       * with a TASK segment (see `taskTabSelectionUri`).
       * @param {string} raw
       * @returns {boolean}
       */
      function isLegacySquadsTasksHash(raw) {
        const [head, selValue] = splitHashSel(raw);
        const [path] = head.split("?");
        const seg = path.split("/");
        if (seg[1]) return true;
        if (selValue && !looksLikeUri(selValue) && /^(task|cell|proof):/.test(selValue)) return true;
        if (selValue && looksLikeUri(selValue)) {
          const uri = parseRalphusUri(selValue);
          if (uri && uri.segments[0] && uri.segments[0].kind === "SQUAD") return true;
        }
        return false;
      }
      // RALPHUS-LEGACY-TASKS-HASH:END
      /**
       * Parses the squad-viewer's hash body -- shared by its own `#/squads`
       * route and by legacy `#/tasks...` links from before RAL-362 renamed it.
       * Side-effects `filters` for the squads-tab case.
       * @param {string} raw
       * @returns {ParsedHash}
       */
      function parseSquadsHashBody(raw) {
        const [head, selValue] = splitHashSel(raw);
        const [path, query] = head.split("?");
        const seg = path.split("/");
        const p = new URLSearchParams(query || "");
        filters = defaultTaskFilters();
        const pq = p.get("q"); if (pq !== null) filters.q = pq.toLowerCase();
        const psort = p.get("sort"); if (psort) filters.sort = psort;
        if (p.get("dir") === "asc") filters.dir = 1;
        const pstatus = p.get("status"); if (pstatus !== null) filters.status = new Set(pstatus.split(",").filter(Boolean));
        filters.showHidden = p.get("hidden") === "1";
        // A URI carries its own squad reference (`?id=`, else the SQUAD[...] label);
        // the legacy form kept it in the path. Both land in `squadId`/`uri` and are
        // decoded against the loaded squad in `pollTasks`.
        const uri = looksLikeUri(selValue) ? parseRalphusUri(/** @type {string} */ (selValue)) : null;
        const uriSquadId = uri ? uri.query.id || (uri.segments[0] && uri.segments[0].name) : null;
        return { tab: "squads", squadId: uriSquadId || seg[1] || null, uri, sel: uri ? null : selValue };
      }
      /**
       * Parses the new flat Tasks tab's hash -- its own filter/selection
       * scheme, disjoint from the squad viewer's (see `isLegacySquadsTasksHash`).
       * Side-effects `taskTabFilters`/`taskTabExpanded` for the tasks-tab case.
       * @param {string} raw
       * @returns {ParsedHash}
       */
      function parseTasksTabHash(raw) {
        const [head, selValue] = splitHashSel(raw);
        const [, query] = head.split("?");
        const p = new URLSearchParams(query || "");
        taskTabFilters = defaultTaskTabFilters();
        const pq = p.get("q"); if (pq !== null) taskTabFilters.q = pq.toLowerCase();
        const psort = p.get("sort"); if (psort) taskTabFilters.sort = psort;
        if (p.get("dir") === "desc") taskTabFilters.dir = -1;
        const pstatus = p.get("status"); if (pstatus !== null) taskTabFilters.status = new Set(pstatus.split(",").filter(Boolean));
        taskTabFilters.showHidden = p.get("hidden") === "1";
        taskTabFilters.needsMe = p.get("needsme") === "1";
        taskTabFilters.groupBySquad = p.get("group") === "1";
        const pexpanded = p.get("expanded"); if (pexpanded !== null) taskTabExpanded = new Set(pexpanded.split(",").filter(Boolean));
        const uri = looksLikeUri(selValue) ? parseRalphusUri(/** @type {string} */ (selValue)) : null;
        return { tab: "tasks", uri, sel: uri ? null : selValue };
      }

      // ---------- filters ----------
      /**
       * Renders the sidebar's per-state show/hide checkboxes.
       * @returns {void}
       */
      function renderStatusFilters() {
        const el = byId("status-filters");
        /** @type {HTMLInputElement} */ (byId("filter")).value = filters.q;
        el.innerHTML = STATES.map((s) => `<label data-tip="Show or hide ${s} squads.">${sdot(s)}<input type="checkbox" ${filters.status.has(s) ? "checked" : ""} data-state="${esc(s)}" onchange="toggleStatus(this.dataset.state,this.checked)">${s}</label>`).join("")
          + `<span class="chip" onclick="allStatus(true)" data-tip="Show squads of every status.">all</span><span class="chip" onclick="allStatus(false)" data-tip="Hide all squads — clear the status filter entirely.">none</span>`;
        /** @type {HTMLInputElement} */ (byId("show-hidden-squads")).checked = filters.showHidden;
      }
      /**
       * Toggles the Tasks sidebar's "show hidden" filter (RAL-331) -- off by
       * default, so a hidden squad stays out of the list until opted back in.
       * @param {boolean} on
       * @returns {void}
       */
      function toggleShowHiddenSquads(on) { filters.showHidden = on; renderSquads(); syncHash(); }
      /**
       * Toggles one status in/out of the visible-squads filter.
       * @param {string} s
       * @param {boolean} on
       * @returns {void}
       */
      function toggleStatus(s, on) { on ? filters.status.add(s) : filters.status.delete(s); renderSquads(); syncHash(); }
      /**
       * Shows or hides all squads regardless of status.
       * @param {boolean} on
       * @returns {void}
       */
      function allStatus(on) { filters.status = on ? new Set(STATES) : new Set(); renderStatusFilters(); renderSquads(); syncHash(); }
      /**
       * Applies a free-text squad-name/id filter.
       * @param {string} v
       * @returns {void}
       */
      function onFilter(v) { filters.q = v.toLowerCase(); renderSquads(); syncHash(); }
      /**
       * Renders the Reviews sidebar's guardian-status checkboxes and syncs its text filter input.
       * @returns {void}
       */
      function renderReviewStatusFilters() {
        const el = byId("review-status-filters");
        /** @type {HTMLInputElement} */ (byId("review-filter")).value = reviewFilters.q;
        el.innerHTML = GUARDIAN_STATES.map((s) => `<label data-tip="Show or hide ${s} reviews.">${gdot(s)}<input type="checkbox" ${reviewFilters.status.has(s) ? "checked" : ""} data-state="${esc(s)}" onchange="toggleReviewStatus(this.dataset.state,this.checked)">${s}</label>`).join("")
          + `<span class="chip" onclick="allReviewStatus(true)" data-tip="Show reviews of every status.">all</span><span class="chip" onclick="allReviewStatus(false)" data-tip="Hide all reviews — clear the status filter entirely.">none</span>`;
        /** @type {HTMLInputElement} */ (byId("show-hidden-reviews")).checked = reviewFilters.showHidden;
      }
      /**
       * Toggles the Reviews sidebar's "show hidden" filter (RAL-331) -- off by
       * default, so a hidden review stays out of the list until opted back in.
       * @param {boolean} on
       * @returns {void}
       */
      function toggleShowHiddenReviews(on) { reviewFilters.showHidden = on; renderReviews(); syncHash(); }
      /**
       * Toggles one guardian status in/out of the visible-reviews filter.
       * @param {string} s
       * @param {boolean} on
       * @returns {void}
       */
      function toggleReviewStatus(s, on) { on ? reviewFilters.status.add(s) : reviewFilters.status.delete(s); renderReviews(); syncHash(); }
      /**
       * Shows or hides all reviews regardless of guardian status.
       * @param {boolean} on
       * @returns {void}
       */
      function allReviewStatus(on) { reviewFilters.status = on ? new Set(GUARDIAN_STATES) : new Set(); renderReviewStatusFilters(); renderReviews(); syncHash(); }
      /**
       * Returns a guardian's resolver-agent name: its own stored value, else
       * the effective `[review].default_resolver_agent` if already cached
       * for its project (see `agentOptionsByCwd`), else `"ollama"`.
       * @param {GuardianView} g
       * @returns {string}
       */
      function resolverOf(g) {
        const cwd = g.git_root || (g.projects && g.projects[0]) || "";
        const cached = agentOptionsByCwd.get(cwd);
        return g.resolver_agent || (cached && cached.agents.length ? cached.defaultAgent : "ollama");
      }
      /**
       * Returns the distinct resolver-agent values present in the currently-loaded reviews, sorted.
       * @returns {string[]}
       */
      function reviewResolverOptions() { return [...new Set(guardians.map(resolverOf))].sort(); }
      /**
       * Until the user (or the URL) has picked an explicit resolver selection, keeps
       * `reviewFilters.resolver` synced to every resolver currently in use — so newly-seen
       * resolvers are filtered-in by default instead of silently hidden. Called from both
       * `renderReviewResolverFilters` and `visibleGuardians`, since the latter can run first
       * (e.g. picking the default selected review on initial load).
       * @returns {void}
       */
      function syncReviewResolverDefault() {
        if (!reviewResolverDefaulted) reviewFilters.resolver = new Set(reviewResolverOptions());
      }
      /**
       * Renders the Reviews sidebar's resolver-agent checkboxes.
       * @returns {void}
       */
      function renderReviewResolverFilters() {
        const el = byId("review-resolver-filters");
        syncReviewResolverDefault();
        const options = reviewResolverOptions();
        el.innerHTML = options.map((r) => `<label data-tip="Show or hide reviews resolved by ${esc(r)}."><input type="checkbox" ${reviewFilters.resolver.has(r) ? "checked" : ""} onchange="toggleReviewResolver('${esc(r)}',this.checked)">${esc(r)}</label>`).join("")
          + (options.length ? `<span class="chip" onclick="allReviewResolver(true)" data-tip="Show reviews from every resolver.">all</span><span class="chip" onclick="allReviewResolver(false)" data-tip="Hide all reviews — clear the resolver filter entirely.">none</span>` : "");
      }
      /**
       * Toggles one resolver agent in/out of the visible-reviews filter.
       * @param {string} r
       * @param {boolean} on
       * @returns {void}
       */
      function toggleReviewResolver(r, on) {
        reviewResolverDefaulted = true;
        on ? reviewFilters.resolver.add(r) : reviewFilters.resolver.delete(r);
        renderReviews(); syncHash();
      }
      /**
       * Shows or hides all reviews regardless of resolver agent.
       * @param {boolean} on
       * @returns {void}
       */
      function allReviewResolver(on) {
        reviewResolverDefaulted = true;
        reviewFilters.resolver = on ? new Set(reviewResolverOptions()) : new Set();
        renderReviewResolverFilters(); renderReviews(); syncHash();
      }
      /**
       * Returns a guardian's provenance, defaulting to "explicit" for reviews
       * the daemon hasn't stamped an origin on (older data, or a daemon build
       * that predates RAL-318).
       * @param {GuardianView} g
       * @returns {string}
       */
      function originOf(g) { return g.origin || "explicit"; }
      /**
       * Renders the Reviews sidebar's Explicit/Arbiter provenance checkboxes
       * (RAL-318). Unlike the resolver-agent filter, this is a fixed
       * two-value domain, not one discovered from the currently-loaded
       * reviews, so it needs no auto-sync step.
       * @returns {void}
       */
      function renderReviewOriginFilters() {
        const el = byId("review-origin-filters");
        const label = (/** @type {string} */ o) => o === "arbiter" ? "⚙ Arbiter" : "Explicit";
        el.innerHTML = REVIEW_ORIGINS.map((o) => `<label data-tip="${o === "arbiter" ? "Show or hide reviews the Arbiter/Triage subsystem created automatically from a pooled threshold or schedule." : "Show or hide explicitly-authored reviews (an authored [[review]] block, or any other normal creation path)."}"><input type="checkbox" ${reviewFilters.origin.has(o) ? "checked" : ""} data-origin="${esc(o)}" onchange="toggleReviewOrigin(this.dataset.origin,this.checked)">${label(o)}</label>`).join("");
      }
      /**
       * Toggles one review origin in/out of the visible-reviews filter.
       * @param {string} o
       * @param {boolean} on
       * @returns {void}
       */
      function toggleReviewOrigin(o, on) {
        on ? reviewFilters.origin.add(o) : reviewFilters.origin.delete(o);
        renderReviews(); syncHash();
      }
      /**
       * Applies a free-text review-name/id filter.
       * @param {string} v
       * @returns {void}
       */
      function onReviewFilter(v) { reviewFilters.q = v.toLowerCase(); renderReviews(); syncHash(); }
      /**
       * Computes the Reviews sidebar list after guardian-status/text filtering.
       * @returns {GuardianView[]}
       */
      function visibleGuardians() {
        syncReviewResolverDefault();
        return guardians.filter((g) => reviewFilters.status.has(g.status)
          && reviewFilters.resolver.has(resolverOf(g))
          && reviewFilters.origin.has(originOf(g))
          && (g.id.toLowerCase().includes(reviewFilters.q) || g.name.toLowerCase().includes(reviewFilters.q))
          // RAL-331: a hidden review is a personal view preference, excluded
          // by default -- except the one just navigated to directly (§reveal).
          && (reviewFilters.showHidden || !hiddenGuardianIds.has(g.id) || g.id === revealedGuardianId));
      }
      /**
       * Changes the sidebar's squad sort key.
       * @param {string} k
       * @returns {void}
       */
      function setSort(k) { filters.sort = k; renderSortChips(); renderSquads(); syncHash(); }
      /**
       * Flips the sidebar's squad sort direction.
       * @returns {void}
       */
      function toggleDir() { filters.dir *= -1; renderSortChips(); renderSquads(); syncHash(); }
      /**
       * Updates the sort-chip UI to reflect the active sort key/direction.
       * @returns {void}
       */
      function renderSortChips() {
        document.querySelectorAll("[data-sort]").forEach((c) => c.classList.toggle("active", /** @type {HTMLElement} */ (c).dataset.sort === filters.sort));
        byId("dir-chip").textContent = filters.dir < 0 ? "↓" : "↑";
      }
      /**
       * Computes the sidebar's squad list after status/text filtering and sorting.
       * @returns {SquadView[]}
       */
      function visibleSquads() {
        let list = squads.filter((r) => filters.status.has(r.state)
          && (r.id.toLowerCase().includes(filters.q) || (r.label || "").toLowerCase().includes(filters.q))
          // RAL-331: a hidden squad is a personal view preference, excluded
          // by default -- except the one just navigated to directly (§reveal).
          && (filters.showHidden || !hiddenSquadIds.has(r.id) || r.id === revealedSquadId));
        list.sort((a, b) => filters.sort === "name"
          ? filters.dir * (a.label || a.id).localeCompare(b.label || b.id)
          : filters.dir * (a.created_at_ms - b.created_at_ms));
        return list;
      }

      // ---------- sidebar ----------
      /**
       * Renders the squad list in the sidebar.
       * @returns {void}
       */
      function renderSquads() {
        const el = byId("squads");
        const list = visibleSquads();
        if (!list.length) { el.innerHTML = `<div class="empty">No squads.</div>`; return; }
        el.innerHTML = list.map((r) => `
          <div class="squad-item ${(r.id === selectedSquadId || multiSel.has(r.id)) ? "selected" : ""}" data-click="onSquadClick" data-ctx="openSquadMenu" data-squad-id="${esc(r.id)}">
            <div class="squad-row">
              <span class="rid" data-tip="Full squad name: ${esc(r.label || r.id)}\nShown here in case the name above is truncated to make room for the status.">${hiddenSquadIds.has(r.id) ? `<span data-tip="You've hidden this squad from your own view.\nIt's shown now because \"show hidden\" is on, or you navigated to it directly.\nA personal preference — it does not affect what other users see.">🙈</span> ` : ""}${esc(r.label || r.id)}</span>
              <span class="meta"${isDowntimeWaiting(r) ? ` data-tip="${WAITING_TIP}"` : ""}>${sdot(squadDisplayState(r))}<span>${squadDisplayState(r)}</span>${r.state === "running" && r.started_at_ms ? `<span class="squad-dur" data-running="1" data-started="${r.started_at_ms}">${fmtDuration(Date.now() - r.started_at_ms)}</span>` : ""}</span>
              <span class="squad-actions">
                ${r.state === "queued" ? `<button class="btn squadbtn" data-click="activateSquad" data-squad-id="${esc(r.id)}" data-tip="Activate this queued squad — it was staged with hold=true and is waiting to be scheduled.">▶ Run</button>` : ""}
                <button class="btn squadbtn" data-click="openSquadMenu" data-squad-id="${esc(r.id)}" data-tip="Squad actions — rename, hide, retry, restart, cancel, delete, or view logs for this squad.">⋯</button>
              </span>
            </div>
          </div>`).join("");
      }
      /**
       * Selects a squad and shows its details pane.
       * @param {string} id
       * @returns {void}
       */
      function selectSquad(id) { clearNodeMultiSel(); selectedSquadId = id; revealedSquadId = id; sel = { kind: "squad", taskIdx: 0, cellIdx: 0 }; editing = false; renderAll(); syncHash(); }
      /**
       * Promotes a held (queued) squad to pending so the scheduler can pick it up.
       * @param {MouseEvent} e
       * @param {string} id
       * @returns {Promise<void>}
       */
      async function activateSquad(e, id) { e.stopPropagation(); await post(`/api/squads/${id}/activate`); tick(); }

      // ---------- multi-select + context menu ----------
      /**
       * Handles a click on a squad row: plain select, ctrl/cmd toggle, or shift range-select.
       * @param {MouseEvent} e
       * @param {string} id
       * @returns {void}
       */
      function onSquadClick(e, id) {
        if (e.shiftKey && anchorId) {
          const vis = visibleSquads().map((r) => r.id);
          const a = vis.indexOf(anchorId), b = vis.indexOf(id);
          if (a >= 0 && b >= 0) { const lo = Math.min(a, b), hi = Math.max(a, b); multiSel = new Set(vis.slice(lo, hi + 1)); }
        } else if (e.ctrlKey || e.metaKey) {
          if (multiSel.has(id)) multiSel.delete(id); else multiSel.add(id);
          anchorId = id;
        } else {
          multiSel = new Set([id]); anchorId = id;
        }
        selectedSquadId = id; sel = { kind: "squad", taskIdx: 0, cellIdx: 0 }; editing = false;
        renderAll(); syncHash(true);
      }
      /**
       * Opens the squad right-click/⋯ context menu with actions valid for its current state.
       * @param {MouseEvent} e
       * @param {string} id
       * @returns {void}
       */
      function openSquadMenu(e, id) {
        e.preventDefault(); e.stopPropagation(); closeSquadMenu();
        const r = findSquad(id); if (!r) return;
        const items = [`<div data-click="renameSquad" data-squad-id="${esc(id)}" data-tip="Rename this squad — changes the display label only.">✎ Rename</div>`];
        const squadUri = `squad:${id}`;
        items.push(`<div data-click="toggleWatch" data-entity-uri="${esc(squadUri)}" data-tip="${isWatching(squadUri) ? "Stop receiving watcher notifications for this squad." : "Watch this whole squad and choose which mailbox priority tiers should notify you."}">${isWatching(squadUri) ? "◉ Unwatch" : "◎ Watch…"}</div>`);
        const menuBatchSize = (multiSel.has(id) && multiSel.size > 1) ? multiSel.size : 0;
        if (menuBatchSize) {
          items.push(`<div data-click="hideSquadMenuItem" data-squad-id="${esc(id)}" data-tip="Hide all ${menuBatchSize} selected squads from your own view — they stay fully intact and keep running/counting normally.\nWho/when: use this to declutter your list of squads you don't need to watch right now.\nA personal preference — it never affects what other users see, and can be undone any time via \"show hidden\".">🙈 Hide ${menuBatchSize}</div>`);
          items.push(`<div data-click="unhideSquadMenuItem" data-squad-id="${esc(id)}" data-tip="Show all ${menuBatchSize} selected squads again in your own view, if hidden.\nWho/when: use this to undo an earlier hide across a whole selection.\nA personal preference — it never affects what other users see.">👁 Unhide ${menuBatchSize}</div>`);
        } else {
          items.push(hiddenSquadIds.has(id)
            ? `<div data-click="unhideSquadMenuItem" data-squad-id="${esc(id)}" data-tip="Show this squad again in your own view.\nWho/when: use this to undo an earlier hide.\nA personal preference — it never affects what other users see.">👁 Unhide</div>`
            : `<div data-click="hideSquadMenuItem" data-squad-id="${esc(id)}" data-tip="Hide this squad from your own view — it stays fully intact and keeps running/counting normally.\nWho/when: use this to declutter your list of squads you don't need to watch right now.\nA personal preference — it never affects what other users see, and can be undone any time via \"show hidden\".">🙈 Hide</div>`);
        }
        if (r.state === "queued") items.push(`<div data-click="squadMenuActivate" data-squad-id="${esc(id)}" data-tip="Start this queued squad immediately — it was held with hold=true and is waiting to be scheduled.">▶ Run</div>`);
        if (["done", "failed", "cancelled"].includes(r.state)) items.push(`<div data-click="retrySquad" data-squad-id="${esc(id)}" data-tip="Re-run with the same parameters.\nA succeeded squad will prompt for extra confirmation since it may duplicate side effects.">↻ Retry</div>`);
        if (["done", "failed", "cancelled"].includes(r.state)) items.push(`<div data-click="restartSquad" data-squad-id="${esc(id)}" data-tip="Re-run this squad and mark all downstream squads as dirty so they re-run too.\nThis cannot be undone.">⟳ Restart + downstream</div>`);
        items.push(`<div class="danger" data-click="cancelSquad" data-squad-id="${esc(id)}" data-tip="Cancel this squad and every squad downstream of it — stops all in-flight task, cell, and proof agents, and permanently locks them out of ever being picked up again (even if already done or failed).\nUse this to stop a stuck or unwanted squad, including one that already finished.\nShows a preview of every squad that will be cancelled before confirming.\nThis cannot be undone.">■ Cancel Squad</div>`);
        items.push(`<div data-click="openAddDependencyDialogFor" data-squad-id="${esc(id)}" data-tip="Make ${multiSel.has(id) && multiSel.size > 1 ? "every selected squad" : "this squad"} wait for another squad to finish before it can be scheduled.\nSearch for the target squad by ID or name, then confirm.\nThe target squad itself is not modified.">🔗 Add Dependency</div>`);
        items.push(`<div data-click="openStatusPickerForSquadMenuItem" data-squad-id="${esc(id)}" data-tip="Manually override this squad's status — any valid state can be set regardless of current state.\nA confirmation dialog will appear before applying.\nThis cannot be undone for terminal states (done/failed/cancelled).">⚙ Set Status</div>`);
        items.push(`<div class="danger" data-click="deleteSquad" data-squad-id="${esc(id)}" data-tip="Delete this squad and all its data permanently.\nThis cannot be undone.">🗑 Delete</div>`);
        if (!["pending","queued"].includes(r.state)) items.push(`<div data-click="openLogsFromSquadMenu" data-squad-id="${esc(id)}" data-tip="View squad logs — events, task states, cell timings, and proof output.">📄 Logs</div>`);
        const menu = document.createElement("div");
        menu.className = "ctx-menu"; menu.id = "squad-menu"; menu.innerHTML = items.join("");
        document.body.appendChild(menu);
        menu.style.left = Math.min(e.clientX, window.innerWidth - 180) + "px";
        menu.style.top = Math.min(e.clientY, window.innerHeight - 170) + "px";
      }
      /**
       * Closes the squad context menu, if open.
       * @returns {void}
       */
      function closeSquadMenu() { const m = document.getElementById("squad-menu"); if (m) m.remove(); }
      document.addEventListener("click", closeSquadMenu);
      /**
       * Runs a simple squad-menu POST action (e.g. activate) and refreshes the board.
       * @param {string} id
       * @param {string} act
       * @returns {Promise<void>}
       */
      async function squadMenuAct(id, act) { closeSquadMenu(); await post(`/api/squads/${id}/${act}`); tick(); }
      /**
       * Prompts for and applies a new display label for a squad.
       * @param {string} id
       * @returns {void}
       */
      function renameSquad(id) {
        closeSquadMenu();
        const r = findSquad(id); if (!r) return;
        const name = prompt("Rename squad label:", r.label || "");
        if (name === null) return;
        post(`/api/squads/${id}/edit`, { kind: "squad", label: name }).then(() => tick());
      }
      // CCTL-101: retry re-runs with the same params. A succeeded squad gets a
      // higher-friction confirmation (type-to-confirm) to guard against
      // accidental re-runs of successful work; a failed/cancelled one just confirms.
      /**
       * Re-runs a terminal squad with its existing parameters, with extra confirmation if it already succeeded.
       * @param {string} id
       * @returns {Promise<void>}
       */
      async function retrySquad(id) {
        closeSquadMenu();
        const r = findSquad(id); if (!r) return;
        if (r.state === "done") {
          const ans = prompt(`This squad SUCCEEDED. Re-running it may duplicate side effects.\nType "retry" to confirm re-running "${r.label || id}".`);
          if ((ans || "").trim().toLowerCase() !== "retry") return;
        } else if (!confirm(`Retry "${r.label || id}"? It will re-run with the same parameters.`)) {
          return;
        }
        await post(`/api/squads/${id}/retry`);
        tick();
      }
      // RAL-19/RAL-104: restart re-runs the squad and dirties every squad that
      // depends on it, so the whole downstream chain re-runs once this one
      // finishes again. Shows a dry-run preview of that impact first.
      /**
       * Restarts a whole squad (with a downstream-impact preview first).
       * @param {string} id
       * @returns {Promise<void>}
       */
      async function restartSquad(id) {
        closeSquadMenu();
        const r = findSquad(id); if (!r) return;
        await showRestartPreview(`Restart "${r.label || id}"?`, `/api/squads/${id}/restart/preview`, `/api/squads/${id}/restart`);
      }
      // Restart a single cell and everything downstream of it within the squad
      // (upstream cells stay done and are skipped on re-run). Dependent squads
      // are dirtied too. Shows a dry-run preview first (RAL-104).
      /**
       * Restarts one cell and everything downstream of it within the squad.
       * @param {string} id
       * @param {number} ti
       * @param {number} si
       * @returns {Promise<void>}
       */
      async function restartCell(id, ti, si) {
        const r = findSquad(id); if (!r) return;
        await showRestartPreview("Restart this cell and everything downstream of it?", `/api/squads/${id}/cells/${ti}/${si}/restart/preview`, `/api/squads/${id}/cells/${ti}/${si}/restart`);
      }
      /**
       * Restarts one task from its first cell and dirties downstream squads,
       * after showing the same dry-run preview as the daemon endpoint.
       * @param {string} id
       * @param {number} ti
       * @returns {Promise<void>}
       */
      async function restartTask(id, ti) {
        const r = findSquad(id); if (!r) return;
        const t = r.tasks[ti]; if (!t) return;
        await showRestartPreview(`Restart task "${t.name}"?`, `/api/squads/${id}/tasks/${ti}/restart/preview`, `/api/squads/${id}/tasks/${ti}/restart`);
      }
      // RAL-174: free-form context a human can attach to a restart, carried
      // forward to the restarted cell via the Ghost system. Shared by
      // every restart modal (showRestartPreview and the lighter-weight
      // proof-restart prompt) so the textarea/checkbox markup and its
      // tooltips exist in exactly one place.
      const RESTART_NOTE_TIP = "Optional free-form context for the restarted cell, e.g. 'you most recently tried X but hit issue Y' or 'you were stopped midway through X'.\nWho/when: anyone restarting a squad/cell/task who has situational knowledge the agent doesn't.\nDelivered via the Ghost system as a one-time note (RAL-174) -- it does not accumulate across future restarts like the agent's own notes do.";
      const RESTART_NOTE_APPLY_ALL_TIP = "Off (default): the note only reaches the exact cell/task being restarted.\nOn: the note also reaches every cell this restart cascades to (every downstream cell it resets to Pending along with the target).\nWho/when: check this when the note is relevant to the whole cascade, not just the immediate target.";
      /**
       * Builds the shared restart-note textarea + "Apply To All Children"
       * checkbox markup used by every restart confirmation modal (RAL-174).
       * @returns {string}
       */
      function restartNoteFieldsHtml() {
        return `
          <div style="margin-top:12px">
            <label for="restart-note-input" style="font-size:12px;color:var(--muted);display:block;margin-bottom:4px" data-tip="${RESTART_NOTE_TIP}">Note for the restarted cell (optional)</label>
            <textarea id="restart-note-input" rows="3" placeholder="e.g. you were stopped midway through the migration; the schema change is already applied" style="width:100%;box-sizing:border-box;resize:vertical;font-family:inherit;font-size:12px;padding:6px;background:var(--bg);color:var(--text);border:1px solid var(--border);border-radius:4px" data-tip="${RESTART_NOTE_TIP}"></textarea>
            <label style="display:flex;align-items:center;gap:6px;font-size:12px;color:var(--muted);margin-top:6px;cursor:pointer" data-tip="${RESTART_NOTE_APPLY_ALL_TIP}">
              <input type="checkbox" id="restart-note-apply-all" data-tip="${RESTART_NOTE_APPLY_ALL_TIP}">
              Apply To All Children
            </label>
          </div>`;
      }
      // RAL-104: fetch the non-mutating downstream-impact preview and show it in
      // a scrollable modal before the user confirms the actual (mutating)
      // restart. previewUrl and restartUrl are backed by the same server-side
      // computation, so the preview can never drift from what the restart does.
      /**
       * @typedef {object} RestartPreview
       * @property {{task_name: string, cell_id: string}[]} [cells]
       * @property {{idx: number, name: string}[]} [tasks]
       * @property {{id: string, label: string|null}[]} [dirtied_squads]
       */
      /**
       * Fetches a restart's dry-run impact preview and shows a confirmation modal.
       * @param {string} title
       * @param {string} previewUrl
       * @param {string} restartUrl
       * @returns {Promise<void>}
       */
      async function showRestartPreview(title, previewUrl, restartUrl) {
        /** @type {RestartPreview} */
        let impact;
        try {
          const resp = await post(previewUrl);
          if (!resp.ok) { alert("Failed to compute restart preview."); return; }
          impact = await resp.json();
        } catch (_) { alert("Failed to compute restart preview: network error."); return; }
        const cellRows = (impact.cells || []).map((s) =>
          `<div style="display:flex;gap:8px;padding:4px 0;border-bottom:1px solid var(--border);font-size:12px">
            <span style="flex:1">${esc(s.task_name)} · ${esc(s.cell_id)}</span>
          </div>`
        ).join("");
        const dirtiedRows = (impact.dirtied_squads || []).map((dr) =>
          `<div style="display:flex;gap:8px;padding:4px 0;border-bottom:1px solid var(--border);font-size:12px">
            <span style="flex:1">${esc(dr.label || dr.id)}</span>
            <span class="badge" style="font-size:11px" data-tip="A separate squad that depends on this one.\nIt will be reset to Pending and re-run once this one finishes again.">dependent squad</span>
          </div>`
        ).join("");
        const cellCount = (impact.cells || []).length;
        const taskCount = (impact.tasks || []).length;
        const dirtiedCount = (impact.dirtied_squads || []).length;
        byId("modal-root").innerHTML = `
          <div class="modal-bg" onclick="if(event.target===this)closeModal()"><div class="modal" style="width:640px;max-width:94vw">
            <h2>${esc(title)}</h2>
            <p style="font-size:13px;margin:0 0 10px;color:var(--muted)">This will reset ${cellCount} cell${cellCount === 1 ? "" : "s"} across ${taskCount} task${taskCount === 1 ? "" : "s"} to Pending${dirtiedCount ? `, and dirty ${dirtiedCount} dependent squad${dirtiedCount === 1 ? "" : "s"} so ${dirtiedCount === 1 ? "it re-runs" : "they re-run"} too` : ""}.</p>
            <div data-tip="Everything this restart will dirty, computed the same way the restart itself applies it.\nScroll to see the full list." style="max-height:280px;overflow:auto;border-top:1px solid var(--border)">${cellRows}${dirtiedRows}</div>
            ${restartNoteFieldsHtml()}
            <p style="font-size:12px;color:var(--failed);margin:10px 0 2px">This cannot be undone.</p>
            <div class="btn-row">
              <button class="btn" onclick="closeModal()" data-tip="Cancel — do not restart anything.">Cancel</button>
              <button class="btn primary" data-click="confirmRestart" data-restart-url="${esc(restartUrl)}" data-tip="Proceed with the restart described above.\nThis cannot be undone.">⟳ Restart</button>
            </div>
          </div></div>`;
      }
      /**
       * Confirms and executes a previously-previewed restart, forwarding any
       * optional restart note (RAL-174) typed into the modal's textarea.
       * @param {string} restartUrl
       * @returns {Promise<void>}
       */
      async function confirmRestart(restartUrl) {
        const noteEl = /** @type {HTMLTextAreaElement|null} */ (document.getElementById("restart-note-input"));
        const applyAllEl = /** @type {HTMLInputElement|null} */ (document.getElementById("restart-note-apply-all"));
        const note = (noteEl?.value || "").trim();
        closeModal();
        await post(restartUrl, note ? { note, apply_to_all: !!applyAllEl?.checked } : undefined);
        tick();
      }
      // RAL-116: cancel this squad and every squad transitively dependent on it.
      // Always available regardless of current state — even a terminal
      // (done/failed/already-cancelled) squad can be cancelled, so it is
      // permanently locked out of ever being picked up again. Shows a
      // dry-run preview of the full cascade first, mirroring restart's
      // preview (RAL-104): the preview and the real cancel share one
      // server-side computation, so they can never drift apart.
      /**
       * Cancels a squad and its downstream dependents (with a preview first).
       * @param {string} id
       * @returns {Promise<void>}
       */
      async function cancelSquad(id) {
        closeSquadMenu();
        const r = findSquad(id); if (!r) return;
        await showCancelPreview(`Cancel "${r.label || id}"?`, `/api/squads/${id}/cancel/preview`, `/api/squads/${id}/cancel`);
      }
      /**
       * @typedef {object} CancelPreview
       * @property {{id: string, label: string|null}[]} [squads]
       */
      /**
       * Fetches a cancel's dry-run impact preview and shows a confirmation modal.
       * @param {string} title
       * @param {string} previewUrl
       * @param {string} cancelUrl
       * @returns {Promise<void>}
       */
      async function showCancelPreview(title, previewUrl, cancelUrl) {
        /** @type {CancelPreview} */
        let impact;
        try {
          const resp = await post(previewUrl);
          if (!resp.ok) { alert("Failed to compute cancel preview."); return; }
          impact = await resp.json();
        } catch (_) { alert("Failed to compute cancel preview: network error."); return; }
        const squadRows = (impact.squads || []).map((r) =>
          `<div style="display:flex;gap:8px;padding:4px 0;border-bottom:1px solid var(--border);font-size:12px">
            <span style="flex:1">${esc(r.label || r.id)}</span>
          </div>`
        ).join("");
        const squadCount = (impact.squads || []).length;
        byId("modal-root").innerHTML = `
          <div class="modal-bg" onclick="if(event.target===this)closeModal()"><div class="modal" style="width:640px;max-width:94vw">
            <h2>${esc(title)}</h2>
            <p style="font-size:13px;margin:0 0 10px;color:var(--muted)">This will cancel ${squadCount} squad${squadCount === 1 ? "" : "s"} (this squad plus every squad downstream of it) — stopping every in-flight task, cell, and proof agent and permanently locking each one out of ever being picked up again.</p>
            <div data-tip="Every squad this cancellation will stop, computed the same way the cancel itself applies it.\nScroll to see the full list." style="max-height:280px;overflow:auto;border-top:1px solid var(--border)">${squadRows}</div>
            <p style="font-size:12px;color:var(--failed);margin:10px 0 2px">This cannot be undone.</p>
            <div class="btn-row">
              <button class="btn" onclick="closeModal()" data-tip="Close without cancelling anything.">Cancel</button>
              <button class="btn danger" data-click="confirmCancelSquad" data-cancel-url="${esc(cancelUrl)}" data-tip="Proceed with cancelling every squad listed above.\nThis cannot be undone.">■ Cancel Squad</button>
            </div>
          </div></div>`;
      }
      /**
       * Confirms and executes a previously-previewed cancel.
       * @param {string} cancelUrl
       * @returns {Promise<void>}
       */
      async function confirmCancelSquad(cancelUrl) {
        closeModal();
        await post(cancelUrl);
        tick();
      }
      // Attach an interactive terminal to a task cell's live tmux cell
      // (RAL-102 — replaces the old `claude --resume` spawn). Fails gracefully
      // via the alert below when the underlying tmux cell isn't currently
      // running (e.g. the cell already finished).
      /**
       * Spawns a resumed cell terminal on the daemon host, attached to the
       * cell's live tmux cell.
       * @param {string} squadId
       * @param {number} ti
       * @param {number} si
       * @returns {Promise<void>}
       */
      async function openTerminal(squadId, ti, si) {
        try {
          const resp = await fetch(`/api/squads/${squadId}/cells/${ti}/${si}/open-terminal`, {method:'POST', headers: traceHeaders()});
          if (!resp.ok) {
            const e = await resp.json().catch(() => ({}));
            alert(`Failed to open terminal: ${((e.error || {}).message) || 'unknown error'}`);
          }
        } catch (_) {
          alert('Failed to open terminal: network error');
        }
      }
      /**
       * Spawns a proof step's terminal on the daemon host, attached to the
       * proof step's live tmux cell.
       * @param {string} squadId
       * @param {number} taskIdx
       * @param {string} scope
       * @param {number} cellIdx
       * @param {number} proofIdx
       * @returns {Promise<void>}
       */
      async function openProofTerminal(squadId, taskIdx, scope, cellIdx, proofIdx) {
        try {
          const resp = await fetch(`/api/squads/${squadId}/proofs/${taskIdx}/${scope}/${cellIdx}/${proofIdx}/open-terminal`, {method:'POST', headers: traceHeaders()});
          if (!resp.ok) {
            const e = await resp.json().catch(() => ({}));
            alert(`Failed to open terminal: ${((e.error || {}).message) || 'unknown error'}`);
          }
        } catch (_) {
          alert('Failed to open terminal: network error');
        }
      }
      // mode is "open" (attach to the resolver's live tmux cell), "worktree"
      // (plain shell in the review worktree directory — unrelated to tmux,
      // available even before any resolver cell exists), or "agent" (resume
      // the real `claude` CLI on the resolver's own conversation).
      /**
       * Spawns a guardian branch's resolver terminal on the daemon host.
       * @param {string} guardianId
       * @param {string} branchId
       * @param {string} mode
       * @returns {Promise<void>}
       */
      async function openGuardianBranchTerminal(guardianId, branchId, mode) {
        try {
          const resp = await fetch(`/api/guardians/${guardianId}/branches/${branchId}/open-terminal?mode=${mode}`, {method:'POST'});
          if (!resp.ok) {
            const e = await resp.json().catch(() => ({}));
            alert(`Failed to open terminal: ${((e.error || {}).message) || 'unknown error'}`);
          }
        } catch (_) {
          alert('Failed to open terminal: network error');
        }
      }
      // mode is "open" (attach to the manual-checks generation's live tmux
      // cell) or "agent" (resume the real `claude` CLI on that generation's
      // own conversation, if it ran under claude-code).
      /**
       * Spawns a review's manual-checks generation terminal on the daemon host.
       * @param {string} guardianId
       * @param {string} mode
       * @returns {Promise<void>}
       */
      async function openGuardianManualChecksTerminal(guardianId, mode) {
        try {
          const resp = await fetch(`/api/guardians/${guardianId}/manual-checks/open-terminal?mode=${mode}`, {method:'POST'});
          if (!resp.ok) {
            const e = await resp.json().catch(() => ({}));
            alert(`Failed to open terminal: ${((e.error || {}).message) || 'unknown error'}`);
          }
        } catch (_) {
          alert('Failed to open terminal: network error');
        }
      }
      /**
       * Resumes the real CLI agent (Claude Code or Codex, whichever the
       * cell ran under) on a task cell's own conversation, in a new
       * terminal window — unlike `openTerminal`, this is an actual
       * interactive agent cell, not a re-attach to the runner's tmux wrapper.
       * @param {string} squadId
       * @param {number} ti
       * @param {number} si
       * @returns {Promise<void>}
       */
      async function openAgentTerminal(squadId, ti, si) {
        try {
          const resp = await fetch(`/api/squads/${squadId}/cells/${ti}/${si}/open-terminal?mode=agent`, {method:'POST', headers: traceHeaders()});
          if (!resp.ok) {
            const e = await resp.json().catch(() => ({}));
            alert(`Failed to open agent: ${((e.error || {}).message) || 'unknown error'}`);
          }
        } catch (_) {
          alert('Failed to open agent: network error');
        }
      }
      /**
       * Hands a detached cell back to unattended execution (RAL-288 Stage
       * 6), continuing the exact same agent conversation.
       * @param {string} squadId
       * @param {number} ti
       * @param {number} si
       * @returns {Promise<void>}
       */
      async function resumeAutomation(squadId, ti, si) {
        try {
          const resp = await fetch(`/api/squads/${squadId}/cells/${ti}/${si}/resume-automation`, {method:'POST', headers: traceHeaders()});
          if (resp.ok) {
            showInfoToast('Resuming automation on this cell.');
            tick();
          } else {
            const e = await resp.json().catch(() => ({}));
            alert(`Failed to resume automation: ${((e.error || {}).message) || 'unknown error'}`);
          }
        } catch (_) {
          alert('Failed to resume automation: network error');
        }
      }
      /**
       * Resumes the real CLI agent on a proof step's own conversation,
       * in a new terminal window — see `openAgentTerminal`'s doc comment.
       * @param {string} squadId
       * @param {number} taskIdx
       * @param {string} scope
       * @param {number} cellIdx
       * @param {number} proofIdx
       * @returns {Promise<void>}
       */
      async function openAgentProofTerminal(squadId, taskIdx, scope, cellIdx, proofIdx) {
        try {
          const resp = await fetch(`/api/squads/${squadId}/proofs/${taskIdx}/${scope}/${cellIdx}/${proofIdx}/open-terminal?mode=agent`, {method:'POST', headers: traceHeaders()});
          if (!resp.ok) {
            const e = await resp.json().catch(() => ({}));
            alert(`Failed to open agent: ${((e.error || {}).message) || 'unknown error'}`);
          }
        } catch (_) {
          alert('Failed to open agent: network error');
        }
      }
      /**
       * Toggles a terminal actions dropdown ("Open Terminal Log" / "Open Agent")
       * open or closed, keyed the same way as the matching peek box.
       * @param {string} key
       * @returns {void}
       */
      function toggleTerminalMenu(key) {
        terminalMenuOpen[key] = !terminalMenuOpen[key];
        if (sel.kind) renderDetails();
        if (selectedGuardian) renderReviewDetail();
      }
      /**
       * Closes a terminal actions dropdown — called before running one of its
       * items, mirroring `runSingleManualCheck`'s close-then-act pattern.
       * @param {string} key
       * @returns {void}
       */
      function closeTerminalMenu(key) {
        terminalMenuOpen[key] = false;
        if (sel.kind) renderDetails();
        if (selectedGuardian) renderReviewDetail();
      }
      /**
       * Renders the "▾" dropdown toggle plus its menu (when open) for the
       * terminal actions button row — mirrors the manual-checks dropdown
       * pattern (see `toggleManualMenu`).
       * @param {string} key
       * @param {string} itemsHtml
       * @returns {string}
       */
      function terminalMenuHtml(key, itemsHtml) {
        const open = !!terminalMenuOpen[key];
        return `<button class="btn" data-click="toggleTerminalMenu" data-key="${esc(key)}" style="border-left:none;border-radius:0 6px 6px 0;padding:4px 8px" data-tip="More terminal options.">▾</button>`
          + (open ? `<div style="position:absolute;top:100%;left:0;background:var(--bg);color:var(--text);border:1px solid var(--border);border-radius:6px;min-width:220px;z-index:50;box-shadow:0 4px 12px rgba(0,0,0,.4);padding:4px 0;margin-top:2px">${itemsHtml}</div>` : "");
      }
      /**
       * Renders one item in a terminal actions dropdown menu. `clickAction`
       * is a CLICK_HANDLERS key (each of which closes the dropdown itself
       * before acting, mirroring the old `closeTerminalMenu('${key}');${onclick}`
       * pattern) and `clickData` supplies that handler's dataset fields
       * beyond `key` (which is always included).
       * @param {string} key
       * @param {string} label
       * @param {string} clickAction
       * @param {{[k: string]: string|number}} clickData
       * @param {string} tip
       * @param {boolean} [disabled]
       * @returns {string}
       */
      function terminalMenuItem(key, label, clickAction, clickData, tip, disabled) {
        if (disabled) {
          return `<div style="padding:6px 12px;font-size:12px;color:var(--muted);white-space:nowrap" data-tip="${tip}">${label}</div>`;
        }
        const attrs = Object.entries(clickData).map(([k, v]) => ` data-${k.replace(/[A-Z]/g, (m) => "-" + m.toLowerCase())}="${esc(String(v))}"`).join("");
        return `<div data-click="${esc(clickAction)}" data-key="${esc(key)}"${attrs} style="padding:6px 12px;cursor:pointer;font-size:12px;white-space:nowrap" data-tip="${tip}" onmouseover="this.style.background='var(--panel-2)'" onmouseout="this.style.background=''">${label}</div>`;
      }

      /**
       * Renders the "Open Agent" terminal-menu item with an accurate
       * enabled/disabled reason, rather than a single blanket "Not available
       * yet" tooltip (RAL-288 Stage 6). While running, this is enabled only
       * where `detachSupported` is passed as `true` -- currently just the
       * plain task-cell call site, whose `open-terminal?mode=agent` endpoint
       * (`server::detach_and_open_agent`) cleanly detaches the running cell
       * first (never a false "Done"), waits for it to genuinely stop, then
       * opens the real interactive CLI -- the actual harness, not an
       * imitation -- inside a tmux session that survives closing this
       * window. Backend-agnostic: claude, codex, and pi all resume the same
       * way. The guardian branch-resolver and manual-checks-generation call
       * sites do NOT pass this flag -- their `mode=agent` endpoints still
       * unconditionally call the old finished-only resume flow, which would
       * start a second agent process racing the live one on the same
       * conversation/worktree; leave them on the old "wait until it
       * finishes" behavior until they get the same detach-aware treatment.
       * @param {string} key
       * @param {string} clickAction
       * @param {{[k: string]: string|number}} clickData
       * @param {boolean} isRunning
       * @param {string} [agentSessionId]
       * @param {string} [agent]
       * @param {string} [machine] - RAL-185/RAL-288: `undefined` means the
       *   daemon's own host. A remote machine has no detach channel yet,
       *   so this earns its own distinct disabled reason while running,
       *   rather than folding into the generic "still running" one.
       * @param {boolean} [detachSupported] - Whether this call site's
       *   backend endpoint actually supports detaching a still-running
       *   cell (see above). Defaults to `false`.
       * @returns {string}
       */
      function openAgentMenuItem(key, clickAction, clickData, isRunning, agentSessionId, agent, machine, detachSupported) {
        if (isRunning && machine) {
          return terminalMenuItem(key, "Open Agent", "", {},
            `Not available — this is running on the remote machine ${JSON.stringify(machine)}, which has no detach channel yet (RAL-288). Live attach only works for cells on the daemon's own host so far.`,
            true);
        }
        if (isRunning && detachSupported && agentSessionId) {
          return terminalMenuItem(key, "Open Agent", clickAction, clickData,
            "Detach this cell and open the real interactive agent, in a new terminal — the actual CLI, not a relay. Stays open even if you close this window.\nWho/when: you want to steer or check in on a cell while it's still working.\nAutomation pauses while you're in there. Use \"Resume Automation\" when you're done to hand it back.");
        }
        if (isRunning) {
          return terminalMenuItem(key, "Open Agent", "", {},
            "Not available yet — no agent session id has been recorded for this cell yet; it may have only just started.",
            true);
        }
        if (!agentSessionId) {
          const tip = agent === undefined
            ? "Not available yet — no resumable agent session has been recorded."
            : `Not available — ${JSON.stringify(agent)} has no resumable agent session to attach to.`;
          return terminalMenuItem(key, "Open Agent", "", {}, tip, true);
        }
        return terminalMenuItem(key, "Open Agent", clickAction, clickData,
          "Resume the real CLI agent, attached to this exact conversation, in a new terminal.\nWho/when: you want to keep working with the agent interactively, picking up right where it left off.\nOnly offered once it has finished running — see this tooltip if it's still disabled.");
      }

      /**
       * Renders the "Resume Automation" terminal-menu item (RAL-288 Stage
       * 6) -- only ever shown for a plain task cell that's still marked
       * running but has a recorded agent session (i.e. was detached at some
       * point). Hands the cell back to unattended execution, continuing the
       * exact same conversation rather than starting fresh. Safe to offer
       * even if the cell is actually still live (not detached): the daemon
       * rejects with a clear error in that case rather than racing it.
       * @param {string} key
       * @param {string} clickAction
       * @param {{[k: string]: string|number}} clickData
       * @param {boolean} isRunning
       * @param {string} [agentSessionId]
       * @returns {string}
       */
      function resumeAutomationMenuItem(key, clickAction, clickData, isRunning, agentSessionId) {
        if (!isRunning || !agentSessionId) return "";
        return terminalMenuItem(key, "Resume Automation", clickAction, clickData,
          "Hand this cell back to unattended execution, continuing the exact same conversation you were just in.\nWho/when: you're done steering it in the real agent terminal and want automation to pick back up.\nIf the cell is actually still live (not detached), this is safely rejected instead of racing it.");
      }

