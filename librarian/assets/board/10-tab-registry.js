      // ---- Projects tab (RAL-100/RAL-101) ----
      /** @type {MachineProviderView[]} registered machine providers (RAL-185) */
      let machines = [];
      /** @type {string[]} schemes that always resolve without a registry row */
      let machineBuiltins = [];
      /** Message from the last failed register/remove, shown inline above the table. */
      let machineError = "";
      // ---- Triage tab (RAL-318) ----
      /** @type {TriageTypeView[]} registered Triage types */
      let triageTypes = [];
      /** @type {TriagePoolView[]} current pool state, one row per (project, triage_type) with at least one pooled cell */
      let triagePools = [];
      /** @type {TriageScheduleView[]} every schedule across every pool key */
      let triageSchedules = [];
      /** @type {TriageCandidateView[]} every cell that opted into Triage and hasn't been linked to a review yet, across every squad */
      let triageCandidates = [];
      /**
       * Default (empty) filters for the Triage tab's candidate list.
       * @returns {{type: string, project: string, q: string}}
       */
      function defaultTriageCandidateFilters() { return { type: "", project: "", q: "" }; }
      /** Client-side filters for the Triage tab's candidate list -- empty string means "no filter" for each field. */
      let triageCandidateFilters = defaultTriageCandidateFilters();
      /** Message from the last failed Triage action, shown inline above the tab's tables. */
      let triageError = "";
      /** @type {string|null} the project (display name) scoped by the open Projects-tab "auto-review thresholds" popup, or null when closed. */
      let projectTriageModalProject = null;
      /** Message from the last failed action inside the open Projects-tab Triage-threshold popup. */
      let projectTriageModalError = "";
      // ---- Fork registrations popup state (RAL-338) ----
      /** @type {ForkRecord[]} one row per registered fork, across every project and user. */
      let projectForks = [];
      /** @type {string|null} project (display name) scoped by the open Projects-tab "fork registrations" popup, or null when closed. */
      let projectForksModalProject = null;
      /** Message from the last failed action inside the open Projects-tab fork-registrations popup. */
      let projectForksModalError = "";
      /** @type {{user: string, fork_url: string, remote_name: string, fork_owner: string}} draft for the "add a fork" row. */
      let projectForksAddDraft = { user: "", fork_url: "", remote_name: "", fork_owner: "" };
      /** @type {string|null} user of the fork row currently in inline edit mode, or null. */
      let projectForksEditUser = null;
      /** @type {{fork_url?: string, remote_name?: string, fork_owner?: string}} draft for the fork row being edited. */
      let projectForksEditDraft = {};
      /** @type {ProjectView[]} */
      let projects = [];
      /** @type {{[key: string]: string|null}} name -> validation error message, or null/absent if valid/unknown */
      let projectErrors = {};
      /** @type {string|null} name of the project row currently in edit mode, or null */
      let projectEditState = null;
      /** @type {{description?: string, path?: string, vcs?: string, clone_url?: string}} draft for the row being edited */
      let projectEditDraft = {};
      let projectSaveError = "";   // message from the last failed save, shown inline in the edit row
      /** @type {string[]} non-blocking warnings (e.g. an inline credential) from the last successful save */
      let projectSaveWarnings = [];
      // ---- Users tab (placeholder identity registry; TODO: replace with user auth once RAL-252 is done) ----
      /** @type {UserView[]} */
      let users = [];
      /** Message from the last failed add/remove, shown inline above the table. */
      let userError = "";
      /** @type {string|null} name of the user row currently in edit mode, or null */
      let userEditState = null;
      /** @type {{name?: string}} draft for the row being edited */
      let userEditDraft = {};
      let userSaveError = "";   // message from the last failed rename, shown inline in the edit row
      /**
       * RAL-324: one layer's contribution to a variable in a resolved-env view.
       * @typedef {object} EnvLayerValue
       * @property {string} layer - Human label of the layer, e.g. "squad", "cell", "review build step".
       * @property {string|null} value - The layer's (already redacted) value; null when this layer tombstones the variable away.
       * @property {boolean} redacted - Whether `value` is the masked placeholder rather than the real value.
       * @property {boolean} effective - Whether this is the layer that won.
       */
      /**
       * RAL-324: one resolved environment variable.
       * @typedef {object} EnvVarRow
       * @property {string} name - The variable's name, never masked.
       * @property {string} value - The winning value, already masked if `redacted`.
       * @property {boolean} redacted - Whether the Secrets tab lists `name`, so `value` is a placeholder.
       * @property {string} source - Label of the layer the winning value came from.
       * @property {EnvLayerValue[]} layers - Every layer that mentions `name`, lowest-precedence first.
       */
      /**
       * RAL-324: one surface's whole resolved environment, as served by the
       * `GET` twin of that surface's `POST .../env` route.
       * @typedef {object} EnvView
       * @property {string} scope - Machine-readable scope id, e.g. "cell", "review-build".
       * @property {string} label - Human label naming the specific surface.
       * @property {string[]} layers - Every layer feeding this surface, lowest-precedence first.
       * @property {EnvVarRow[]} vars - The resolved variables, alphabetical by name.
       * @property {number} redacted_count - How many of `vars` had their value masked.
       * @property {string} note - Why an unregistered secret-looking variable is still shown in full.
       * @property {string|null} [warning] - Set when a layer could not be resolved and was left out.
       */

      // ---- RAL-324: read-only resolved-env viewer ----
      //
      // Every surface that takes environment variables gets a "🔎 Resolved
      // env" button next to its override editor. The daemon resolves the
      // layers and masks the value of any name registered in the Secrets tab
      // before serializing (`ralphus_daemon::env_view`), so this popup and
      // `ralphus <noun> env` show byte-identical values and neither can
      // become a bypass for the other. Strictly read-only -- setting and
      // unsetting stay with the override editors above it.

      /**
       * The "🔎 Resolved env" button markup for one surface. `apiPath` is the
       * same path that surface's overrides are set on; the daemon serves the
       * read-only view as its `GET` twin.
       * @param {string} apiPath
       * @param {string} label - Human scope name used in the tooltip, e.g. "this cell".
       * @returns {string}
       */
      function envViewerBtn(apiPath, label) {
        return `<button class="btn" data-click="openEnvViewer" data-api-path="${esc(apiPath)}" data-tip="Show every environment variable ${esc(label)} actually resolves to, with the layer each value came from.\nWho/when: before a retry, to check what a variable will really be — the override table above shows only this one layer, not what it inherits.\nRead-only: values of names registered in the Secrets tab are masked, and this popup cannot change anything.">🔎 Resolved env</button>`;
      }
      /**
       * Renders the body of the resolved-env popup for a loaded view.
       * @param {EnvView} view
       * @returns {string}
       */
      function envViewerBody(view) {
        const rows = (view.vars || []).map((v) => {
          const value = v.redacted
            ? `<span class="mono" style="color:var(--muted)" data-tip="This variable's name is registered in the Secrets tab, so its value is never sent to the board or the CLI.\nRemove it there if you need to read the value.">${esc(v.value)}</span>`
            : `<span class="mono">${esc(v.value)}</span>`;
          const shadowed = (v.layers || []).filter((l) => !l.effective);
          const provenance = shadowed.length
            ? ` <span style="color:var(--muted);font-size:10px" data-tip="${esc(shadowed.map((l) => `${l.layer} = ${l.value === null ? "(unset)" : l.value}`).join("\n"))}">shadows ${shadowed.length}</span>`
            : "";
          return `<tr><td class="mono">${esc(v.name)}</td><td style="word-break:break-all">${value}</td><td style="color:var(--muted)">${esc(v.source)}${provenance}</td></tr>`;
        }).join("");
        const table = rows
          ? `<table class="env-table"><thead><tr>`
            + `<th data-tip="The environment variable's name. Names are never masked.">name</th>`
            + `<th data-tip="The value this surface resolves to. A name registered in the Secrets tab shows a placeholder instead.">value</th>`
            + `<th data-tip="Which layer's value won, and how many lower layers it shadows (hover the count to see them).">from</th>`
            + `</tr></thead><tbody>${rows}</tbody></table>`
          : `<div class="empty">No environment variables resolve here.</div>`;
        const warning = view.warning
          ? `<div class="warn" style="margin:8px 0 0">${esc(view.warning)}</div>`
          : "";
        return `<div class="kv-row"><span class="k">surface</span><span class="v">${esc(view.label || "")}</span></div>
          <div class="kv-row"><span class="k">layers</span><span class="v mono" style="font-size:11px" data-tip="The precedence order, lowest first — a later layer's value for the same name wins.">${esc((view.layers || []).join("  <  "))}</span></div>
          <div style="max-height:52vh;overflow:auto;margin-top:8px">${table}</div>
          ${warning}
          <div style="color:var(--muted);font-size:11px;margin-top:8px">${esc(view.note || "")}</div>`;
      }
      /**
       * Opens the read-only resolved-env popup for the surface at `apiPath`.
       * @param {string} apiPath
       * @returns {void}
       */
      function openEnvViewer(apiPath) {
        const shell = (/** @type {string} */ body) => {
          byId("modal-root").innerHTML = `<div class="modal-bg" onclick="if(event.target===this)closeModal()"><div class="modal" style="width:760px;max-width:94vw">
              <h2 data-tip="Read-only: this popup lists what the surface resolves to and cannot set or unset anything.\nUse the 'environment overrides' section behind it to change a value.">Resolved environment variables</h2>
              ${body}
              <div class="row" style="justify-content:flex-end;margin-top:8px">
                <button class="btn" onclick="closeModal()" data-tip="Close this popup. Nothing was changed.">Close</button>
              </div>
            </div></div>`;
        };
        shell(`<div class="empty">Loading…</div>`);
        fetch(apiPath).then((r) => {
          if (!r.ok) throw new Error(`HTTP ${r.status}`);
          return r.json();
        }).then((/** @type {EnvView} */ view) => shell(envViewerBody(view)))
          .catch((/** @type {Error} */ err) => shell(`<div class="warn">Could not load the resolved environment: ${esc(String(err.message || err))}</div>`));
      }

      // ---- Secrets tab (RAL-281: user-configurable secret env-var name list) ----
      /** @type {SecretEnvNameView[]} */
      let secretEnvNames = [];
      /** Message from the last failed add/rename/remove, shown inline above the table. */
      let secretEnvNameError = "";
      // ---- Preferences tab (RAL-329: per-user hidden squads/reviews, built on RAL-328) ----
      /** The two `HiddenItem.kind` values. */
      const HIDDEN_KINDS = ["squad", "review"];
      /**
       * @returns {{q: string, type: Set<string>}}
       */
      function defaultHiddenFilters() { return { q: "", type: new Set(HIDDEN_KINDS) }; }
      /** @type {HiddenItem[]} */
      let hiddenItems = [];
      /** @type {WatchView[]} */
      let watches = [];
      /** @type {Map<string, WatchView[]>} */
      let entityWatchers = new Map();
      let autoWatch = true;
      /** @type {string[]} */
      let defaultWatchTiers = ["urgent", "high", "normal"];
      /** Message from the last failed load/unhide, shown inline above the table. */
      let hiddenError = "";
      let hiddenFilters = defaultHiddenFilters();
      /** @type {Map<string, string>} squad id -> display label, from the last `GET /api/tasks`. */
      let hiddenSquadNames = new Map();
      /** @type {Map<string, string>} guardian id -> display name, from the last `GET /api/guardians`. */
      let hiddenGuardianNames = new Map();
      /** @type {MailboxMessageView[]} full personal-mailbox history (RAL-401), including already-read messages, for the Preferences tab's Message history section. */
      let mailboxHistory = [];
      /** Message from the last failed message-history load, shown inline above the table. */
      let mailboxHistoryError = "";
      /** @type {Map<string, AgentOptionsCacheEntry>} cwd -> agents + default fetched from GET /api/agents, cached for the page's lifetime. */
      const agentOptionsByCwd = new Map();
      // ---- Admin gating (RAL-332) ----
      // UI-level convenience gate only -- see `crate::users`'s module doc
      // comment on the daemon side. Refreshed every tick so a promotion/
      // demotion made elsewhere is reflected without a manual reload.
      /** @type {string|null} the caller's own resolved identity, from `GET /api/whoami`. */
      let currentUserName = null;
      let currentUserIsAdmin = false;
      // Whether `pollWhoAmI` has completed at least once. Before that, a
      // deep link into an admin-only tab (e.g. a bookmarked `#/users`) is
      // let through optimistically rather than bounced to Tasks -- there is
      // no admin answer yet to bounce on, and an actual admin's bookmark
      // must not misfire just because the very first fetch hasn't landed.
      let whoAmIResolved = false;
      /** Tab names only ever shown to an admin (client-side hide -- the daemon enforces this server-side too). */
      const ADMIN_ONLY_TABS = ["machines", "triage", "projects", "users", "secrets", "worktree-retirement"];
      // RAL-332 "Edit Profile": the target user name an admin is viewing
      // RAL-329's Preferences page as, or null for "viewing your own". Set by
      // `editUserProfile`, cleared the moment the admin navigates off the
      // Preferences tab -- never persisted, never swaps the admin's own
      // session/filters.
      /** @type {string|null} */
      let prefsViewingAs = null;
      /**
       * Whether a global keyboard shortcut should stay dormant because the
       * user is typing into a text control.
       * @param {EventTarget|null} target
       * @returns {boolean}
       */
      function isTypingShortcutTarget(target) {
        const el = target instanceof HTMLElement ? target : null;
        return !!el && /input|textarea/i.test(el.tagName || "");
      }
      /**
       * Returns every task whose name contains the current search query.
       * Empty query means "show everything."
       * @param {string} query
       * @returns {GotoSearchResult[]}
       */
      function gotoSearchResults(query) {
        const needle = String(query || "").trim().toLowerCase();
        /** @type {GotoSearchResult[]} */
        const out = [];
        squads.forEach((squad) => {
          // RAL-331: a hidden squad's tasks are excluded from go-to search
          // by default too, same "hidden unless specifically surfaced" rule
          // as the sidebar.
          if (hiddenSquadIds.has(squad.id) && !filters.showHidden && squad.id !== revealedSquadId) return;
          squad.tasks.forEach((task, taskIdx) => {
            if (needle && !task.name.toLowerCase().includes(needle)) return;
            out.push({
              squadId: squad.id,
              taskIdx,
              taskName: task.name,
              label: `${squad.id} > ${task.name}`,
            });
          });
        });
        return out;
      }
      /**
       * Keeps the overlay's selected result index within the currently-visible range.
       * @param {number} resultCount
       * @returns {void}
       */
      function clampGotoSearchSelection(resultCount) {
        if (resultCount <= 0) { gotoSearchSelected = -1; return; }
        if (gotoSearchSelected < 0) gotoSearchSelected = 0;
        if (gotoSearchSelected >= resultCount) gotoSearchSelected = resultCount - 1;
      }
      /**
       * Opens the cross-squad task go-to search overlay (RAL-253).
       * @returns {void}
       */
      function openGotoSearch() {
        gotoSearchQuery = "";
        gotoSearchSelected = 0;
        byId("modal-root").innerHTML = `
          <div class="modal-bg" onclick="if(event.target===this)closeModal()"><div class="modal goto-search-modal">
            <h2>Go To Task</h2>
            <input type="text" id="goto-search-input" class="goto-search-input" placeholder="search tasks across every squad…" oninput="gotoSearchInput(this.value)" onkeydown="gotoSearchInputKeydown(event)" data-tip="Filter every task by substring match on task name.\nWho/when: use this when you know part of a task's name and want the owning squad shown beside it.\nArrow keys move the selection · Enter opens it." />
            <div id="goto-search-summary" class="goto-search-summary"></div>
            <div id="goto-search-list" class="goto-search-list" data-tip="Matching tasks across every squad, shown as squad-id > task-name so duplicate names stay unambiguous.\nClick once to select a row; double-click or press Enter to open it."></div>
            <div class="btn-row">
              <button class="btn" onclick="closeModal()" data-tip="Close task go-to search without navigating anywhere.">Cancel</button>
              <button class="btn primary" onclick="gotoSelectedTaskSearchResult()" data-tip="Jump to the selected task on the Squads tab.\nWho/when: use this after narrowing the list to the task you want.">Go To</button>
            </div>
          </div></div>`;
        const input = /** @type {HTMLInputElement|null} */ (document.getElementById("goto-search-input"));
        if (input) {
          input.value = gotoSearchQuery;
          input.focus();
        }
        renderGotoSearchResults();
      }
      /**
       * Re-renders the go-to search result list and summary.
       * @returns {void}
       */
      function renderGotoSearchResults() {
        const list = document.getElementById("goto-search-list");
        const summary = document.getElementById("goto-search-summary");
        if (!list || !summary) return;
        const results = gotoSearchResults(gotoSearchQuery);
        clampGotoSearchSelection(results.length);
        summary.textContent = `${results.length} matching task${results.length === 1 ? "" : "s"} across ${squads.length} squad${squads.length === 1 ? "" : "s"}.`;
        list.innerHTML = results.length
          ? results.map((result, index) => `<button type="button" id="goto-search-row-${index}" class="goto-search-row${index === gotoSearchSelected ? " selected" : ""}" onclick="selectGotoSearchResult(${index})" ondblclick="gotoSelectedTaskSearchResult()" data-tip="Open ${esc(result.label)} on the Squads tab.\nWho/when: use this when several squads contain similarly named work and you need the exact owner shown.">${esc(result.label)}</button>`).join("")
          : `<div class="empty" style="margin:0;padding:20px" data-tip="No current task names contain this substring.\nWho/when: clear or broaden the query to search across every squad again.">No matching tasks.</div>`;
        scrollGotoSearchSelectionIntoView();
      }
      /**
       * Applies a new query to the go-to search overlay.
       * @param {string} value
       * @returns {void}
       */
      function gotoSearchInput(value) {
        gotoSearchQuery = value;
        gotoSearchSelected = 0;
        renderGotoSearchResults();
      }
      /**
       * Selects one visible search result row.
       * @param {number} index
       * @returns {void}
       */
      function selectGotoSearchResult(index) {
        gotoSearchSelected = index;
        renderGotoSearchResults();
      }
      /**
       * Keeps the selected search result visible while keyboard navigation moves it.
       * @returns {void}
       */
      function scrollGotoSearchSelectionIntoView() {
        if (gotoSearchSelected < 0) return;
        document.getElementById(`goto-search-row-${gotoSearchSelected}`)?.scrollIntoView({ block: "nearest" });
      }
      /**
       * Keyboard navigation for the go-to search input.
       * @param {KeyboardEvent} event
       * @returns {void}
       */
      function gotoSearchInputKeydown(event) {
        const results = gotoSearchResults(gotoSearchQuery);
        if (!results.length && event.key !== "Enter") return;
        if (event.key === "ArrowDown") {
          event.preventDefault();
          gotoSearchSelected = Math.min(results.length - 1, Math.max(0, gotoSearchSelected + 1));
          renderGotoSearchResults();
        } else if (event.key === "ArrowUp") {
          event.preventDefault();
          gotoSearchSelected = Math.max(0, gotoSearchSelected - 1);
          renderGotoSearchResults();
        } else if (event.key === "Enter") {
          event.preventDefault();
          gotoSelectedTaskSearchResult();
        }
      }
      /**
       * Navigates to the currently selected go-to search hit.
       * @returns {void}
       */
      function gotoSelectedTaskSearchResult() {
        const results = gotoSearchResults(gotoSearchQuery);
        clampGotoSearchSelection(results.length);
        const hit = results[gotoSearchSelected] || null;
        if (!hit) return;
        closeModal();
        jumpToTask(hit.squadId, hit.taskIdx, -1, "task");
      }
      /**
       * @returns {{q: string, sort: string, dir: number, status: Set<string>, showHidden: boolean}}
       */
      function defaultTaskFilters() { return { q: "", sort: "date", dir: -1, status: new Set(STATES), showHidden: false }; }
      /**
       * @typedef {object} TaskTabFilters
       * @property {string} q
       * @property {string} sort - one of TASK_TAB_SORTS
       * @property {number} dir - 1 (asc) or -1 (desc)
       * @property {Set<string>} status
       * @property {boolean} showHidden - include tasks belonging to hidden squads
       * @property {boolean} needsMe - RAL-362 §5: only rows the "needs me" predicate matches
       * @property {boolean} groupBySquad
       */
      /**
       * @returns {TaskTabFilters}
       */
      // RAL-362 §2: the default view sorts by squad ascending -- deliberately
      // *not* needs-me-first, so a first-time visitor sees the whole board
      // grouped the way the Squads tab already is, before opting into any
      // narrower filter.
      function defaultTaskTabFilters() { return { q: "", sort: "squad", dir: 1, status: new Set(STATES), showHidden: false, needsMe: false, groupBySquad: false }; }
      /**
       * @typedef {object} TaskTabSel
       * @property {"task"|"cell"|null} kind
       * @property {string|null} squadId
       * @property {number} taskIdx
       * @property {number} cellIdx - -1 when `kind === "task"`
       */
      /** @type {TaskTabSel} what's shown in the Tasks tab's details pane */
      let taskTabSel = { kind: null, squadId: null, taskIdx: -1, cellIdx: -1 };
      /**
       * RAL-350: the Tasks tab's row multi-selection -- `"<squadId>:<taskIdx>"`
       * keys, independent of `taskTabSel` (the details-pane single selection)
       * and of the current view filter. A filter never mutates this set; it
       * only narrows which of these keys a bulk action (the row meatball menu)
       * reaches at the moment it's invoked -- see `ttVisibleSelectedRows`.
       * @type {Set<string>}
       */
      let ttSel = new Set();
      /** @type {string|null} anchor key for shift-range multi-selection among the Tasks tab's currently visible/filtered rows (RAL-350), mirrors `anchorId`/`guardianAnchorId`. */
      let ttSelAnchor = null;
      /** @type {string[]} squad ids scoped by the currently open Tasks-tab row meatball menu's Hide/Unhide action (RAL-350) -- one or every squad among the selected, currently-visible rows. */
      let _ttRowMenuSquadIds = [];
      /** @type {TaskTabFilters} */
      let taskTabFilters = defaultTaskTabFilters();
      /** @type {Set<string>} `"<squadId>:<taskIdx>"` keys of expanded Tasks-tab rows (RAL-362 §4) */
      let taskTabExpanded = new Set();
      // RALPHUS-TASK-TAB-COLUMNS:BEGIN
      /**
       * RAL-362 §2: the Tasks tab's column model -- data, not markup. Drives
       * both the CSS-grid template (`--grid`, built by `taskTabGridTemplate`)
       * and the header's sort-caret/meatball-menu/resize behavior. Column
       * order here is the default order; `hideable` columns can be toggled
       * off via the meatball menu (state in `taskTabHiddenCols`).
       * @typedef {object} TaskTabColumn
       * @property {string} key
       * @property {string} label
       * @property {number} width - default px width
       * @property {number} min - minimum resize px width
       * @property {boolean} flex - true if this column's grid track is `1fr` (still resizable, `width` is just its initial basis)
       * @property {boolean} sortable
       * @property {boolean} hideable
       * @property {boolean} groupable - whether `groupBySquad` headers key off this column
       * @property {"left"|"right"} align
       */
      /** @type {TaskTabColumn[]} */
      const TASK_TAB_COLUMNS = [
        { key: "sel", label: "", width: 26, min: 26, flex: false, sortable: false, hideable: false, groupable: false, align: "left" },
        { key: "star", label: "", width: 24, min: 24, flex: false, sortable: false, hideable: false, groupable: false, align: "left" },
        { key: "name", label: "Task", width: 320, min: 160, flex: true, sortable: true, hideable: false, groupable: false, align: "left" },
        { key: "squad", label: "Squad", width: 150, min: 90, flex: false, sortable: true, hideable: false, groupable: true, align: "left" },
        { key: "cells", label: "Cells", width: 150, min: 90, flex: false, sortable: false, hideable: true, groupable: false, align: "left" },
        { key: "review", label: "Review / PR", width: 170, min: 120, flex: false, sortable: false, hideable: true, groupable: false, align: "right" },
        { key: "time", label: "Time", width: 100, min: 80, flex: false, sortable: true, hideable: true, groupable: false, align: "right" },
        { key: "tokens", label: "Tokens", width: 90, min: 70, flex: false, sortable: true, hideable: true, groupable: false, align: "right" },
        { key: "cache", label: "Cache", width: 130, min: 90, flex: false, sortable: true, hideable: true, groupable: false, align: "right" },
        { key: "cost", label: "Cost", width: 90, min: 70, flex: false, sortable: true, hideable: true, groupable: false, align: "right" },
        { key: "meatball", label: "", width: 28, min: 28, flex: false, sortable: false, hideable: false, groupable: false, align: "left" },
      ];
      /** RAL-362 §2: sort keys the toolbar's sort chips accept; must line up with the `sortable` columns above. */
      const TASK_TAB_SORTS = TASK_TAB_COLUMNS.filter((c) => c.sortable).map((c) => c.key);
      /** RAL-362 §2: "Default view: all columns except Cache/Cost" -- everything else hideable starts shown. */
      const TASK_TAB_DEFAULT_HIDDEN_COLS = ["cache", "cost"];
      /**
       * Builds the CSS-grid `grid-template-columns` value for the visible
       * subset of `TASK_TAB_COLUMNS`, honoring any per-column widths the user
       * has dragged (`widths`, keyed by column `key`).
       * @param {TaskTabColumn[]} columns
       * @param {Set<string>} hiddenCols
       * @param {{[key: string]: number}} widths
       * @returns {string}
       */
      function taskTabGridTemplate(columns, hiddenCols, widths) {
        return columns
          .filter((c) => !hiddenCols.has(c.key))
          .map((c) => {
            const w = widths[c.key] || c.width;
            return c.flex ? `minmax(${c.min}px, 1fr)` : `${Math.max(w, c.min)}px`;
          })
          .join(" ");
      }
      // RALPHUS-TASK-TAB-COLUMNS:END
      /** @type {Set<string>} hidden `TASK_TAB_COLUMNS` keys (RAL-362 §2 meatball menu), persisted to `TASK_TAB_COLS_LS_KEY` */
      let taskTabHiddenCols = new Set(TASK_TAB_DEFAULT_HIDDEN_COLS);
      /** @type {{[key: string]: number}} per-column dragged widths, keyed by `TaskTabColumn.key`, persisted to `TASK_TAB_WIDTHS_LS_KEY` */
      let taskTabColWidths = {};
      /** @type {"duration"|"start"} RAL-362 §3: the Time column's display mode, from its own meatball-menu options. */
      let taskTabTimeMode = "duration";
      /** @type {PrIndexRow[]} cached `GET /api/pull-requests/index` rows (RAL-362 §1) -- refetched every Tasks-tab poll. */
      let taskTabPrIndex = [];
      /** @type {{id:string,user_name:string,entity_uri:string,notify_tiers:string[],created_at_ms:number}[]} the acting user's own watches (RAL-362 §5), refreshed once per tick alongside `hiddenSquadIds`/`hiddenGuardianIds`. */
      let taskTabWatches = [];
      /**
       * `task:<squadId>:<taskIdx>` and `cell:<squadId>:<taskIdx>:<cellIdx>`
       * entity URIs the user has explicitly un-starred despite being covered
       * by a squad- or task-level watch (RAL-362 §5 "Un-watching such a task
       * needs an explicit mechanism"). There is no server-side representation
       * for "exception to a watch cascade", so this is tracked as a
       * client-side muted set, persisted to `localStorage` per acting user --
       * a deliberately lightweight stand-in for a real negative-watch record,
       * since the daemon's watches table has no notion of one.
       * @type {Set<string>}
       */
      let taskTabMutedTasks = new Set();
      /** @type {{squadId:string,taskIdx:number}|null} the task the on-demand PR drift/feedback check (RAL-362 §5) was last run for. */
      let taskTabPrCheckFor = null;
      /** @type {{loading:boolean, sync?:PrSyncStatus, comments?:PrCommentItem[], error?:string}|null} result of the on-demand PR check above. */
      let taskTabPrCheckResult = null;
      /** localStorage keys the Tasks tab persists under (RAL-362 §7) -- namespaced so they never collide with the Squads tab's `SPLIT_CFG` keys. */
      const TT_LS = {
        hiddenCols: "ralphus-tt-hidden-cols",
        widths: "ralphus-tt-col-widths",
        sort: "ralphus-tt-sort",
        dir: "ralphus-tt-dir",
        groupBy: "ralphus-tt-group-by",
        timeMode: "ralphus-tt-time-mode",
        muted: "ralphus-tt-muted-tasks",
      };
      /**
       * Loads every Tasks-tab persisted preference from `localStorage`
       * (RAL-362 §7) -- called once at startup, after every function/state
       * it touches is defined.
       * @returns {void}
       */
      function loadTaskTabPrefs() {
        try {
          const hidden = localStorage.getItem(TT_LS.hiddenCols);
          if (hidden) taskTabHiddenCols = new Set(JSON.parse(hidden));
          const widths = localStorage.getItem(TT_LS.widths);
          if (widths) taskTabColWidths = JSON.parse(widths);
          const sort = localStorage.getItem(TT_LS.sort);
          if (sort && TASK_TAB_SORTS.includes(sort)) taskTabFilters.sort = sort;
          const dir = localStorage.getItem(TT_LS.dir);
          if (dir) taskTabFilters.dir = dir === "-1" ? -1 : 1;
          taskTabFilters.groupBySquad = localStorage.getItem(TT_LS.groupBy) === "1";
          const timeMode = localStorage.getItem(TT_LS.timeMode);
          if (timeMode === "start" || timeMode === "duration") taskTabTimeMode = timeMode;
          const muted = localStorage.getItem(TT_LS.muted);
          if (muted) taskTabMutedTasks = new Set(JSON.parse(muted));
        } catch (e) { /* corrupt/unavailable localStorage -- fall back to defaults */ }
      }
      /**
       * Persists the current hidden-columns set (RAL-362 §7).
       * @returns {void}
       */
      function saveTaskTabHiddenCols() { localStorage.setItem(TT_LS.hiddenCols, JSON.stringify([...taskTabHiddenCols])); }
      /**
       * Persists the current per-column widths (RAL-362 §7).
       * @returns {void}
       */
      function saveTaskTabColWidths() { localStorage.setItem(TT_LS.widths, JSON.stringify(taskTabColWidths)); }
      /**
       * Persists the current sort key/direction (RAL-362 §7).
       * @returns {void}
       */
      function saveTaskTabSort() { localStorage.setItem(TT_LS.sort, taskTabFilters.sort); localStorage.setItem(TT_LS.dir, String(taskTabFilters.dir)); }
      /**
       * Persists the current group-by-squad toggle (RAL-362 §7).
       * @returns {void}
       */
      function saveTaskTabGroupBy() { localStorage.setItem(TT_LS.groupBy, taskTabFilters.groupBySquad ? "1" : "0"); }
      /**
       * Persists the current Time-column display mode (RAL-362 §7).
       * @returns {void}
       */
      function saveTaskTabTimeMode() { localStorage.setItem(TT_LS.timeMode, taskTabTimeMode); }
      /**
       * Persists the current muted task/cell set (RAL-362 §5/§7).
       * @returns {void}
       */
      function saveTaskTabMuted() { localStorage.setItem(TT_LS.muted, JSON.stringify([...taskTabMutedTasks])); }
      /** RAL-318: the two possible `GuardianView.origin` values -- a review is either authored explicitly or created automatically by the Arbiter/Triage subsystem. */
      // RAL-362 §8: the Tasks tab's decision logic -- needsMe predicate, sort
      // comparators, filter predicate, usage aggregation, review/PR picking,
      // and virtualization-window maths -- kept pure and DOM/fetch-free (same
      // discipline as RALPHUS-USAGE-SUMMARY above) so test/board-task-tab-logic.mjs
      // can slice the real shipped source out and run it under `node --test`.
      // RALPHUS-TASK-TAB-LOGIC:BEGIN
      /**
       * @typedef {object} TtUsage
       * @property {number} tokensIn
       * @property {number} tokensOut
       * @property {number} cacheCreate
       * @property {number} cacheRead
       * @property {number} cost
       * @property {boolean} anyCost - true once at least one constituent reported a nonzero cost_usd
       * @property {boolean} estimated - true once any constituent has cost_is_estimated
       */
      /**
       * @typedef {object} TtUsageItem
       * @property {number} [tokens_in]
       * @property {number} [tokens_out]
       * @property {number} [cache_creation_tokens]
       * @property {number} [cache_read_tokens]
       * @property {number} [cost_usd]
       * @property {boolean} [cost_is_estimated]
       */
      /**
       * @typedef {object} TtRow
       * @property {string} key - `"<squadId>:<taskIdx>"`
       * @property {string} squadId
       * @property {string|null} squadLabel
       * @property {string} squadState
       * @property {number} taskIdx
       * @property {TaskView} task
       * @property {string} name
       * @property {string} state
       * @property {CellView[]} cells
       * @property {TtReview[]} reviews
       * @property {{review: TtReview, count: number}|null} reviewBadge
       * @property {PrIndexRow[]} prs
       * @property {{pr: PrIndexRow, count: number}|null} prPick
       * @property {TtUsage} usage
       * @property {number|null} startedAtMs
       * @property {number|null} finishedAtMs
       * @property {number|null} sortTimeMs
       * @property {{watched: boolean, inherited: boolean}} watch
       * @property {boolean} needsMe
       * @property {string|null} needsMeReason
       */
      /**
       * @typedef {object} TtDisplayItem
       * @property {"task"|"cell"|"group"} type
       * @property {TtRow} [row]
       * @property {CellView} [cell]
       * @property {string} [squadId]
       * @property {TtRow[]} [rows]
       */
      /**
       * Sums tokens/cache/cost across a flat list of usage-bearing items
       * (cells and/or proof steps) -- the shared reducer behind
       * {@link ttTaskUsage}/{@link ttCellUsage}. `anyCost` distinguishes "no
       * backend reported a cost" from "reported exactly $0" per RAL-362 §3 --
       * only the former renders as a dash.
       * @param {TtUsageItem[]} items
       * @returns {TtUsage}
       */
      function ttUsageOf(items) {
        const u = { tokensIn: 0, tokensOut: 0, cacheCreate: 0, cacheRead: 0, cost: 0, anyCost: false, estimated: false };
        for (const it of items) {
          u.tokensIn += it.tokens_in || 0;
          u.tokensOut += it.tokens_out || 0;
          u.cacheCreate += it.cache_creation_tokens || 0;
          u.cacheRead += it.cache_read_tokens || 0;
          if (it.cost_usd) { u.cost += it.cost_usd; u.anyCost = true; }
          if (it.cost_is_estimated) u.estimated = true;
        }
        return u;
      }
      /**
       * A cell's own usage-bearing items: itself plus its cell-scope proof steps.
       * @param {CellView} cell
       * @returns {TtUsageItem[]}
       */
      function ttCellUsageItems(cell) { return [cell, .../** @type {TtUsageItem[]} */ (cell.proof || [])]; }
      /**
       * RAL-362 §3 cost-columns table: a task's aggregate is the sum over its
       * cells, its cells' proof steps, and its own task-scope proof steps --
       * `TaskView` itself carries no usage fields.
       * @param {TaskView} task
       * @returns {TtUsageItem[]}
       */
      function ttTaskUsageItems(task) {
        /** @type {TtUsageItem[]} */
        const items = [.../** @type {TtUsageItem[]} */ (task.proof || [])];
        for (const c of task.cells || []) items.push(...ttCellUsageItems(c));
        return items;
      }
      /**
       * @param {TaskView} task
       * @returns {TtUsage}
       */
      function ttTaskUsage(task) { return ttUsageOf(ttTaskUsageItems(task)); }
      /**
       * @param {CellView} cell
       * @returns {TtUsage}
       */
      function ttCellUsage(cell) { return ttUsageOf(ttCellUsageItems(cell)); }
      /**
       * Formats the Tokens column's "in / out" pair, or a dash when the task
       * consumed none.
       * @param {TtUsage} u
       * @returns {string}
       */
      function ttFmtTokens(u) { return (u.tokensIn || u.tokensOut) ? `${u.tokensIn} / ${u.tokensOut}` : "–"; }
      /**
       * Formats the Cache column's "write / read" pair (RAL-362 §3 -- deliberately
       * not "in / out", since both figures are input-side), or a dash when zero.
       * @param {TtUsage} u
       * @returns {string}
       */
      function ttFmtCache(u) { return (u.cacheCreate || u.cacheRead) ? `${u.cacheCreate} / ${u.cacheRead}` : "–"; }
      /**
       * Formats the Cost column: a dash when no constituent reported a real
       * cost figure (never "$0.00" -- RAL-362 §3), else "$0.42", prefixed
       * "~" once any constituent's figure is a mid-run estimate.
       * @param {TtUsage} u
       * @returns {string}
       */
      function ttFmtCost(u) { return u.anyCost ? `${u.estimated ? "~" : ""}$${u.cost.toFixed(2)}` : "–"; }
      /**
       * @typedef {object} TtReview
       * @property {string} id
       * @property {string} name
       * @property {string} status
       * @property {string} origin
       * @property {string[]} branches - every distinct branch this task's cells submitted under this review
       */
      /**
       * Unions a task's per-cell `reviews` refs into one row per review id
       * (RAL-362 Risks: "Review linkage is per-cell, not per-task" -- a task's
       * reviews are the union over its cells). Counts/badges must be scoped
       * to exactly this union, never inherited from the squad or siblings.
       * @param {TaskView} task
       * @returns {TtReview[]}
       */
      function ttTaskReviews(task) {
        /** @type {Map<string, TtReview>} */
        const map = new Map();
        for (const c of task.cells || []) {
          for (const r of c.reviews || []) {
            let entry = map.get(r.id);
            if (!entry) { entry = { id: r.id, name: r.name, status: r.status, origin: r.origin, branches: [] }; map.set(r.id, entry); }
            entry.status = r.status;
            if (r.branch && !entry.branches.includes(r.branch)) entry.branches.push(r.branch);
          }
        }
        return [...map.values()];
      }
      /** RAL-362 §3: review statuses in most-attention-wanting-first order, for picking which review a multi-review task's badge is tinted by. */
      const TT_REVIEW_ATTENTION_RANK = ["merge_failed", "merge_stopped", "in_review", "collecting", "merging", "proof_pending", "conflict_resolved", "approved", "deployed", "cancelled"];
      /**
       * Picks the single most-attention-wanting review out of a task's own
       * review union, for the row's Review badge.
       * @param {TtReview[]} reviews
       * @returns {{review: TtReview, count: number}|null}
       */
      function ttPickReviewBadge(reviews) {
        if (!reviews.length) return null;
        const rank = (/** @type {string} */ s) => { const i = TT_REVIEW_ATTENTION_RANK.indexOf(s); return i === -1 ? TT_REVIEW_ATTENTION_RANK.length : i; };
        let best = reviews[0];
        for (const r of reviews.slice(1)) if (rank(r.status) < rank(best.status)) best = r;
        return { review: best, count: reviews.length };
      }
      /**
       * Every `PrIndexRow` submitted from any cell of task `(squadId, taskIdx)`
       * -- the client-side half of RAL-362 §1's server-side join (matched on
       * `source_task_idx` alone, deliberately ignoring `source_cell_idx`,
       * since a task's PRs span every one of its cells).
       * @param {PrIndexRow[]} prIndex
       * @param {string} squadId
       * @param {number} taskIdx
       * @returns {PrIndexRow[]}
       */
      function ttPrsForTask(prIndex, squadId, taskIdx) {
        return prIndex.filter((p) => p.source_squad_id === squadId && p.source_task_idx === taskIdx);
      }
      /**
       * Picks the task's PR badge: earliest-created (lowest `created_at_ms`,
       * ties broken by lowest `pr_number`) -- RAL-362 §3.
       * @param {PrIndexRow[]} prs
       * @returns {{pr: PrIndexRow, count: number}|null}
       */
      function ttPickTaskPr(prs) {
        if (!prs.length) return null;
        const sorted = [...prs].sort((a, b) => (a.created_at_ms - b.created_at_ms) || ((a.pr_number ?? 0) - (b.pr_number ?? 0)));
        return { pr: sorted[0], count: sorted.length };
      }
      /** RAL-362 §3: PR-state color roles, reusing existing status hues (docs/colors.md) rather than inventing new ones -- "open" reads as in-flight/awaiting-action like `in_review`, "merged" like `done`, "closed" like `cancelled`, "dropped" like `failed` (the review determined the branch couldn't be carried). */
      /** @type {{[state: string]: string}} */
      const TT_PR_COLORS = { open: "--accent", merged: "--done", closed: "--cancelled", dropped: "--failed" };
      /** RAL-395: CI/CD status color roles for an *open* PR, reusing the same status hues `docs/colors.md` already documents for `done`/`failed`/`pending` rather than inventing new ones -- see the "PR CI/CD status" subsection there. */
      /** @type {{[status: string]: string}} */
      const TT_PR_CI_COLORS = { passing: "--done", failing: "--failed", pending: "--pending" };
      /**
       * The color role for a PR chip/badge (RAL-395): once a PR is no longer
       * `open` (merged/closed/dropped), its CI status is moot -- keep
       * `TT_PR_COLORS`' lifecycle coloring. While `open`, prefer the polled
       * CI status (a distinct color for failing vs. passing vs. not-yet-known)
       * over the flat "in-flight" accent color, so a reviewer sees red/green
       * without opening the PR.
       * @param {{state: string, ci_status?: string|null}} pr
       * @returns {string}
       */
      function ttPrColorVar(pr) {
        if (pr.state === "open" && pr.ci_status) {
          return TT_PR_CI_COLORS[pr.ci_status] || TT_PR_COLORS.open;
        }
        return TT_PR_COLORS[pr.state] || "--muted";
      }
      /**
       * The task-level entity URI a squad/task watch is filed under (RAL-362
       * §5), matching `crate::entity_uri::EntityUri`'s `Display` grammar.
       * @param {string} squadId
       * @param {number} taskIdx
       * @returns {string}
       */
      function ttTaskEntityUri(squadId, taskIdx) { return `task:${squadId}:${taskIdx}`; }
      /**
       * The squad-level entity URI that covers every task underneath it
       * (RAL-362 §5, `EntityUri::covers`).
       * @param {string} squadId
       * @returns {string}
       */
      function ttSquadEntityUri(squadId) { return `squad:${squadId}`; }
      /**
       * The cell-level entity URI a cell watch is filed under, matching
       * `crate::entity_uri::EntityUri`'s `Display` grammar.
       * @param {string} squadId
       * @param {number} taskIdx
       * @param {number} cellIdx
       * @returns {string}
       */
      function ttCellEntityUri(squadId, taskIdx, cellIdx) { return `cell:${squadId}:${taskIdx}:${cellIdx}`; }
      /**
       * Resolves a task's effective watch state against the user's raw watch
       * list: explicit beats inherited, and an inherited (squad-level) watch
       * is suppressed by an explicit local mute (RAL-362 §5 -- "un-watching a
       * squad-covered task" has no server-side representation, so it's
       * tracked as a client-side muted set instead).
       * @param {{entity_uri:string}[]} watches
       * @param {Set<string>} mutedUris
       * @param {string} squadId
       * @param {number} taskIdx
       * @returns {{watched: boolean, inherited: boolean}}
       */
      function ttEffectiveWatch(watches, mutedUris, squadId, taskIdx) {
        const taskUri = ttTaskEntityUri(squadId, taskIdx);
        if (watches.some((w) => w.entity_uri === taskUri)) return { watched: true, inherited: false };
        const squadUri = ttSquadEntityUri(squadId);
        const inherited = watches.some((w) => w.entity_uri === squadUri) && !mutedUris.has(taskUri);
        return { watched: inherited, inherited };
      }
      /**
       * Resolves a cell's effective watch state, layering its own
       * explicit watch/mute on top of its owning task's effective watch
       * (itself possibly inherited from the squad) -- same explicit-beats-
       * inherited, mute-suppresses-inherited rules as {@link ttEffectiveWatch},
       * one level down.
       * @param {{entity_uri:string}[]} watches
       * @param {Set<string>} mutedUris
       * @param {string} squadId
       * @param {number} taskIdx
       * @param {number} cellIdx
       * @returns {{watched: boolean, inherited: boolean}}
       */
      function ttEffectiveCellWatch(watches, mutedUris, squadId, taskIdx, cellIdx) {
        const cellUri = ttCellEntityUri(squadId, taskIdx, cellIdx);
        if (watches.some((w) => w.entity_uri === cellUri)) return { watched: true, inherited: false };
        if (mutedUris.has(cellUri)) return { watched: false, inherited: false };
        const taskWatch = ttEffectiveWatch(watches, mutedUris, squadId, taskIdx);
        return { watched: taskWatch.watched, inherited: taskWatch.watched };
      }
      /**
       * RAL-362 §5: maps each "needs me" trigger condition to the
       * `MailboxPriority` tier it's gated by. The daemon does not yet emit a
       * real mailbox message for the review-based triggers (only cell/proof
       * failures do, always at `urgent` -- see `scheduler.rs`'s
       * `enqueue_cell_failure_mailbox`), so this assigns the same tier
       * ordering the mailbox already uses (urgent > high > normal) to keep
       * "needs me" consistent with how notify tiers read everywhere else.
       */
      /** @type {{[key: string]: string}} */
      const TT_NEEDS_ME_TIER = { failed: "urgent", in_review: "high", approved_no_pr: "normal" };
      /**
       * Whether an effectively-watched task's watch tiers permit a given
       * trigger to count toward "needs me" (RAL-362 §5).
       * @param {string[]} notifyTiers
       * @param {string} trigger - key of {@link TT_NEEDS_ME_TIER}
       * @returns {boolean}
       */
      function ttTierAllows(notifyTiers, trigger) {
        return !notifyTiers || !notifyTiers.length || notifyTiers.includes(TT_NEEDS_ME_TIER[trigger]);
      }
      /**
       * RAL-362 §5 "needs me" predicate: WATCHED and blocked on the user --
       * failed, awaiting the user's review approval, or approved with no PR
       * submitted. Unwatched work never nags, and PR-side signals (drift,
       * un-actioned feedback) are deliberately excluded (they need per-PR
       * network calls -- see Phase 5, offered on-demand instead).
       * @param {TaskView} task
       * @param {{watched:boolean}} effectiveWatch
       * @param {string[]} notifyTiers
       * @param {TtReview[]} reviews
       * @param {number} prCount
       * @returns {{needs: boolean, reason: string|null}}
       */
      function ttTaskNeedsMe(task, effectiveWatch, notifyTiers, reviews, prCount) {
        if (!effectiveWatch.watched) return { needs: false, reason: null };
        if (task.state === "failed" && ttTierAllows(notifyTiers, "failed")) {
          return { needs: true, reason: "This task failed." };
        }
        const inReview = reviews.find((r) => r.status === "in_review");
        if (inReview && ttTierAllows(notifyTiers, "in_review")) {
          return { needs: true, reason: `Review "${inReview.name}" is in_review, awaiting your approval.` };
        }
        const approved = reviews.find((r) => r.status === "approved");
        if (approved && !prCount && ttTierAllows(notifyTiers, "approved_no_pr")) {
          return { needs: true, reason: `Review "${approved.name}" is approved with no PR submitted yet.` };
        }
        return { needs: false, reason: null };
      }
      /**
       * Compares two Tasks-tab rows for the active sort key. Tokens/Cache
       * sort by the SUM of their displayed pair (RAL-362 §3) even though both
       * numbers stay visible in the cell.
       * @param {TtRow} a
       * @param {TtRow} b
       * @param {string} sortKey
       * @returns {number}
       */
      function ttCompareRows(a, b, sortKey) {
        switch (sortKey) {
          case "name": return a.name.localeCompare(b.name);
          case "squad": return (a.squadLabel || a.squadId).localeCompare(b.squadLabel || b.squadId) || (a.taskIdx - b.taskIdx);
          case "time": return (a.sortTimeMs ?? -1) - (b.sortTimeMs ?? -1);
          case "tokens": return (a.usage.tokensIn + a.usage.tokensOut) - (b.usage.tokensIn + b.usage.tokensOut);
          case "cache": return (a.usage.cacheCreate + a.usage.cacheRead) - (b.usage.cacheCreate + b.usage.cacheRead);
          case "cost": return a.usage.cost - b.usage.cost;
          default: return 0;
        }
      }
      /**
       * Whether a built row survives the toolbar's filters (RAL-362 §2):
       * name substring, status set, hidden-squad inclusion, and "needs me".
       * @param {TtRow} row
       * @param {TaskTabFilters} filters
       * @param {Set<string>} hiddenSquadIds
       * @param {Set<string>} needsMeKeys
       * @returns {boolean}
       */
      function ttRowMatchesFilters(row, filters, hiddenSquadIds, needsMeKeys) {
        if (filters.q && !row.name.toLowerCase().includes(filters.q)) return false;
        if (!filters.status.has(row.state)) return false;
        if (!filters.showHidden && hiddenSquadIds.has(row.squadId)) return false;
        if (filters.needsMe && !needsMeKeys.has(row.key)) return false;
        return true;
      }
      /**
       * Aggregates the Tokens/Cache/Cost columns across a group of rows, for
       * a group-by-squad header (RAL-362 §3) -- covers only the rows passed
       * in (i.e. already post-filter).
       * @param {TtRow[]} rows
       * @returns {TtUsage}
       */
      function ttGroupAggregate(rows) {
        const acc = { tokensIn: 0, tokensOut: 0, cacheCreate: 0, cacheRead: 0, cost: 0, anyCost: false, estimated: false };
        for (const r of rows) {
          acc.tokensIn += r.usage.tokensIn; acc.tokensOut += r.usage.tokensOut;
          acc.cacheCreate += r.usage.cacheCreate; acc.cacheRead += r.usage.cacheRead;
          if (r.usage.anyCost) { acc.cost += r.usage.cost; acc.anyCost = true; }
          if (r.usage.estimated) acc.estimated = true;
        }
        return acc;
      }
      /**
       * Computes the visible window for a virtualized list (RAL-362 §2): the
       * first/last indices to actually render, with a small overscan on each
       * side so fast scrolling doesn't flash empty rows.
       * @param {number} scrollTop
       * @param {number} viewportH
       * @param {number} rowH
       * @param {number} totalRows
       * @param {number} overscan
       * @returns {{first: number, last: number}}
       */
      function ttVisibleRange(scrollTop, viewportH, rowH, totalRows, overscan) {
        const first = Math.max(0, Math.floor(scrollTop / rowH) - overscan);
        const count = Math.ceil(viewportH / rowH) + overscan * 2;
        return { first, last: Math.min(totalRows, first + count) };
      }
      /**
       * Flattens filtered/sorted task rows into the virtualized list's
       * display items -- task rows, their expanded cell sub-rows, and (when
       * grouping) one group-header item per squad (RAL-362 §2/§4).
       * @param {TtRow[]} rows - already filtered + sorted
       * @param {boolean} groupBySquad
       * @param {Set<string>} expandedKeys
       * @returns {TtDisplayItem[]}
       */
      function ttBuildDisplayList(rows, groupBySquad, expandedKeys) {
        /** @type {TtDisplayItem[]} */
        const items = [];
        const pushTask = (/** @type {TtRow} */ r) => {
          items.push({ type: "task", row: r });
          if (expandedKeys.has(r.key)) for (const cell of r.cells) items.push({ type: "cell", row: r, cell });
        };
        if (!groupBySquad) { rows.forEach(pushTask); return items; }
        /** @type {Map<string, TtRow[]>} */
        const groups = new Map();
        for (const r of rows) {
          if (!groups.has(r.squadId)) groups.set(r.squadId, []);
          /** @type {TtRow[]} */ (groups.get(r.squadId)).push(r);
        }
        for (const [squadId, groupRows] of groups) {
          items.push({ type: "group", squadId, rows: groupRows });
          groupRows.forEach(pushTask);
        }
        return items;
      }
      // RALPHUS-TASK-TAB-LOGIC:END

