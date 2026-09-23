      // RALPHUS-STATUS-DROPDOWN:BEGIN
      // ---------- Shared Status dropdown component (RAL-475) ----------
      // One data-driven Status filter control used everywhere a view lets
      // users show/hide rows by lifecycle status: Squads, Tasks, Reviews,
      // Queue, and Worktree Retirement. Modeled on the project filter's
      // presentation and dismissal behavior (25-chrome.js,
      // RALPHUS-PROJECT-FILTER-MENU) -- a closed-by-default trigger button
      // that opens a floating `.ctx-menu` checkbox list, dismissed by an
      // outside click. Each calling view owns its own filter state and
      // render path; this component only owns the popup's markup,
      // positioning, and open/close lifecycle.
      //
      // Two modes:
      //   "multi"  -- N-of-many toggle (Squads/Tasks/Reviews/Queue/
      //              Retirement status filters). Checkbox per option plus
      //              All/None buttons. Stays open across toggles/All/None
      //              so several statuses can be changed in one interaction.
      //   "single" -- 1-of-many-or-none picker (reserved for RAL-474's PR
      //              Status menu; not wired to any view by this ticket).
      //              Picking an option (or "any") closes the menu.

      /**
       * @typedef {object} StatusDropdownOption
       * @property {string} value - the raw status/state/agent identifier
       * @property {string} label - human-facing label shown next to the dot (if any)
       * @property {string} [color] - a documented CSS color variable role, e.g. "--done"; omit for a dotless option (RAL-486, e.g. an agent name with no associated lifecycle color)
       */

      /**
       * @typedef {object} StatusDropdownConfig
       * @property {string} id - unique id for this dropdown instance; also its menu/trigger element id suffix
       * @property {string} label - trigger button text, e.g. "Status" or "PR Status"
       * @property {"multi"|"single"} mode
       * @property {StatusDropdownOption[]} options
       * @property {Set<string>} [selected] - multi mode: the currently visible/selected values
       * @property {string|null} [selectedValue] - single mode: the currently selected value, or null for "any"
       * @property {string} [anyLabel] - single mode: label for the "no selection" entry
       * @property {string} [itemNoun] - RAL-486: singular noun used in generic multi-mode tooltip wording ("every {noun}", "Select every {noun} for this view.", "Clear every {noun} for this view..."). Defaults to "status" so every pre-existing caller (Squads/Tasks/Reviews/Queue/Retirement status) keeps its current copy unchanged.
       * @property {(value: string, on: boolean) => void} [onToggle] - multi mode: fired when one option is checked/unchecked
       * @property {() => void} [onAll] - multi mode: fired by the "all" button
       * @property {() => void} [onNone] - multi mode: fired by the "none" button
       * @property {(value: string|null) => void} [onSelect] - single mode: fired when an option (or "any") is picked
       * @property {(value: string) => string} [optionTip] - per-option tooltip text; defaults to a generic show/hide tip
       */

      /** @type {Map<string, StatusDropdownConfig>} dropdown id -> its latest config, kept fresh by every render call */
      const statusDropdownRegistry = new Map();

      /**
       * Converts a raw snake_case status identifier into a human-facing
       * label, e.g. "merge_stopped" -> "Merge Stopped". Every view that
       * builds a Status dropdown's option list derives its labels this way,
       * so labels sort consistently and don't need hand-maintaining per view.
       * @param {string} raw
       * @returns {string}
       */
      function statusDropdownLabel(raw) {
        return raw.split("_").filter(Boolean).map((w) => w[0].toUpperCase() + w.slice(1)).join(" ");
      }

      /**
       * Builds a Status dropdown menu element's DOM id from its dropdown id.
       * @param {string} id
       * @returns {string}
       */
      function statusDropdownMenuId(id) {
        return `status-dropdown-menu-${id}`;
      }

      /**
       * Builds a Status dropdown trigger button's DOM id from its dropdown id.
       * @param {string} id
       * @returns {string}
       */
      function statusDropdownTriggerId(id) {
        return `status-dropdown-trigger-${id}`;
      }

      /**
       * Renders a colored status dot for one dropdown option, or nothing for
       * an option with no `color` (RAL-486, e.g. an agent name).
       * @param {StatusDropdownOption} opt
       * @returns {string}
       */
      function statusDropdownDot(opt) {
        return opt.color ? `<span class="dot" style="background:${cvar(opt.color)}"></span>` : "";
      }

      /**
       * Sorts a dropdown's options alphabetically by their displayed label,
       * using locale-aware comparison so labels sort the way a human reads
       * them, not by the raw underscored identifier underneath (e.g.
       * "Merge Stopped" sorts under M, not under the raw "merge_stopped").
       * @param {StatusDropdownOption[]} options
       * @returns {StatusDropdownOption[]}
       */
      function statusDropdownSortedOptions(options) {
        return options.slice().sort((a, b) => a.label.localeCompare(b.label));
      }

      /**
       * Builds the trigger button's inner label: the literal label, a
       * dropdown indicator, and (multi mode only, and only when the
       * selection isn't "everything") a compact selected-count badge.
       * Kept separate from `statusDropdownTriggerHtml` so it can be
       * refreshed live while the menu is open, without recreating the
       * button element (and losing its `aria-expanded` state).
       * @param {StatusDropdownConfig} config
       * @returns {string}
       */
      function statusDropdownTriggerLabelHtml(config) {
        let countBadge = "";
        if (config.mode === "multi") {
          const selected = config.selected || new Set();
          if (selected.size && selected.size < config.options.length) {
            countBadge = ` <span class="chip">${selected.size}/${config.options.length}</span>`;
          }
        }
        return `${esc(config.label)} ▾${countBadge}`;
      }

      /**
       * Builds the closed trigger button's tooltip text.
       * @param {StatusDropdownConfig} config
       * @returns {string}
       */
      function statusDropdownTriggerTip(config) {
        if (config.mode === "multi") {
          return `Filter by ${esc(config.label)}. Opens a menu of every ${esc(config.itemNoun || "status")} -- toggle entries, or use All/None, to change what's shown here.`;
        }
        const opt = config.options.find((o) => o.value === config.selectedValue);
        return `Filter by ${esc(config.label)}. Currently: ${esc(opt ? opt.label : (config.anyLabel || "Any"))}.`;
      }

      /**
       * Builds the closed trigger button's full HTML.
       * @param {StatusDropdownConfig} config
       * @returns {string}
       */
      function statusDropdownTriggerHtml(config) {
        const menuId = statusDropdownMenuId(config.id);
        return `<button type="button" class="btn" id="${esc(statusDropdownTriggerId(config.id))}" data-tip="${esc(statusDropdownTriggerTip(config))}" aria-haspopup="true" aria-expanded="false" aria-controls="${esc(menuId)}" onclick="statusDropdownToggleMenu('${esc(config.id)}', event)">${statusDropdownTriggerLabelHtml(config)}</button>`;
      }

      /**
       * Builds the open menu's row markup for multi-select mode: one
       * checkbox row per option (alphabetical by label) plus All/None
       * buttons in the header, top-right per the widget's convention.
       * @param {StatusDropdownConfig} config
       * @returns {string}
       */
      function statusDropdownMultiRowsHtml(config) {
        const selected = config.selected || new Set();
        const noun = config.itemNoun || "status";
        const options = statusDropdownSortedOptions(config.options);
        const rows = options.map((opt) => {
          const tip = config.optionTip ? config.optionTip(opt.value) : `Show or hide ${esc(opt.label)} entries.`;
          return `<div class="ctx-check ${selected.has(opt.value) ? "on" : ""}"><label style="display:flex;align-items:center;gap:6px;width:100%;margin:0;cursor:pointer" data-tip="${esc(tip)}" onclick="event.stopPropagation()"><input type="checkbox" ${selected.has(opt.value) ? "checked" : ""} onchange="statusDropdownToggleOption('${esc(config.id)}','${esc(opt.value)}',this.checked)">${statusDropdownDot(opt)}${esc(opt.label)}</label></div>`;
        }).join("");
        return `<div style="display:flex;justify-content:flex-end;gap:6px;padding:2px 6px 6px" onclick="event.stopPropagation()">
            <button type="button" class="chip" data-tip="Select every ${esc(noun)} for this view." onclick="statusDropdownSelectAll('${esc(config.id)}')">all</button>
            <button type="button" class="chip" data-tip="Clear every ${esc(noun)} for this view -- hides every row until you re-select one." onclick="statusDropdownSelectNone('${esc(config.id)}')">none</button>
          </div><div class="ctx-sep"></div>${rows}`;
      }

      /**
       * Builds the open menu's row markup for single-select mode: an "any"
       * entry plus one radio row per option (alphabetical by label).
       * @param {StatusDropdownConfig} config
       * @returns {string}
       */
      function statusDropdownSingleRowsHtml(config) {
        const options = statusDropdownSortedOptions(config.options);
        const name = `status-dropdown-radio-${config.id}`;
        const anyChecked = config.selectedValue == null;
        const anyRow = `<div class="ctx-check ${anyChecked ? "on" : ""}"><label style="display:flex;align-items:center;gap:6px;width:100%;margin:0;cursor:pointer" data-tip="Clear the selection -- show entries of any status." onclick="event.stopPropagation()"><input type="radio" name="${esc(name)}" ${anyChecked ? "checked" : ""} onchange="statusDropdownSelectSingle('${esc(config.id)}', null)">${esc(config.anyLabel || "Any")}</label></div>`;
        const rows = options.map((opt) => {
          const checked = config.selectedValue === opt.value;
          const tip = config.optionTip ? config.optionTip(opt.value) : `Show only ${esc(opt.label)} entries.`;
          return `<div class="ctx-check ${checked ? "on" : ""}"><label style="display:flex;align-items:center;gap:6px;width:100%;margin:0;cursor:pointer" data-tip="${esc(tip)}" onclick="event.stopPropagation()"><input type="radio" name="${esc(name)}" ${checked ? "checked" : ""} onchange="statusDropdownSelectSingle('${esc(config.id)}','${esc(opt.value)}')">${statusDropdownDot(opt)}${esc(opt.label)}</label></div>`;
        }).join("");
        return anyRow + rows;
      }

      /**
       * Renders a Status dropdown's trigger button into a container and
       * records its config as the current source of truth for that
       * dropdown id. Call this every time the owning view re-renders,
       * exactly like the per-view filter render functions it replaces
       * (`renderStatusFilters`, `renderTtStatusFilters`, ...).
       * @param {string} containerId
       * @param {StatusDropdownConfig} config
       * @returns {void}
       */
      function renderStatusDropdown(containerId, config) {
        statusDropdownRegistry.set(config.id, config);
        const el = byId(containerId);
        if (!el) return;
        const wasOpen = document.getElementById(statusDropdownMenuId(config.id)) != null;
        el.innerHTML = statusDropdownTriggerHtml(config);
        if (wasOpen) {
          const trigger = document.getElementById(statusDropdownTriggerId(config.id));
          if (trigger) trigger.setAttribute("aria-expanded", "true");
        }
        statusDropdownRefreshMenu(config.id);
      }

      /**
       * Regenerates an already-open dropdown menu's rows from its latest
       * registered config, so a toggle/All/None click updates the open
       * menu without closing it. A no-op if that menu isn't open.
       * @param {string} id
       * @returns {void}
       */
      function statusDropdownRefreshMenu(id) {
        const menu = document.getElementById(statusDropdownMenuId(id));
        if (!menu) return;
        const config = statusDropdownRegistry.get(id);
        if (!config) return;
        menu.innerHTML = config.mode === "single" ? statusDropdownSingleRowsHtml(config) : statusDropdownMultiRowsHtml(config);
      }

      /**
       * Refreshes a Status dropdown trigger button's label/badge in place
       * (without recreating the element), so a toggle/All/None click while
       * the menu is open keeps the trigger's selected-count badge current.
       * @param {string} id
       * @returns {void}
       */
      function statusDropdownRefreshTrigger(id) {
        const config = statusDropdownRegistry.get(id);
        if (!config) return;
        const trigger = document.getElementById(statusDropdownTriggerId(id));
        if (!trigger) return;
        trigger.innerHTML = statusDropdownTriggerLabelHtml(config);
        trigger.setAttribute("data-tip", statusDropdownTriggerTip(config));
      }

      /**
       * Opens or closes a Status dropdown's menu from its trigger button.
       * Any other open Status dropdown menu is closed first, so only one
       * is ever open at a time; clicking an already-open trigger closes it.
       * @param {string} id
       * @param {MouseEvent} e
       * @returns {void}
       */
      function statusDropdownToggleMenu(id, e) {
        e.preventDefault();
        e.stopPropagation();
        const existing = document.getElementById(statusDropdownMenuId(id));
        statusDropdownCloseAll();
        if (existing) return;
        const config = statusDropdownRegistry.get(id);
        if (!config) return;
        const menu = document.createElement("div");
        menu.className = "ctx-menu";
        menu.id = statusDropdownMenuId(id);
        menu.setAttribute("role", "menu");
        menu.innerHTML = config.mode === "single" ? statusDropdownSingleRowsHtml(config) : statusDropdownMultiRowsHtml(config);
        document.body.appendChild(menu);
        const trigger = /** @type {HTMLElement} */ (e.currentTarget);
        const r = trigger.getBoundingClientRect();
        menu.style.left = Math.min(r.left, window.innerWidth - 240) + "px";
        menu.style.top = Math.min(r.bottom + 4, window.innerHeight - 360) + "px";
        trigger.setAttribute("aria-expanded", "true");
        menu.addEventListener("keydown", statusDropdownMenuKeydown);
        const firstFocusable = /** @type {HTMLElement|null} */ (menu.querySelector("input, button"));
        if (firstFocusable) firstFocusable.focus();
      }

      /**
       * Closes every open Status dropdown menu and resets every trigger's
       * `aria-expanded` state. Registered once as the document-level
       * outside-click handler, mirroring the project filter's
       * `closeProjectFilterMenu` pattern.
       * @returns {void}
       */
      function statusDropdownCloseAll() {
        document.querySelectorAll(".ctx-menu[id^='status-dropdown-menu-']").forEach((m) => m.remove());
        document.querySelectorAll("[id^='status-dropdown-trigger-']").forEach((b) => b.setAttribute("aria-expanded", "false"));
      }
      document.addEventListener("click", statusDropdownCloseAll);

      /**
       * Handles Escape inside an open Status dropdown menu: closes it and
       * returns focus to its trigger button.
       * @param {KeyboardEvent} e
       * @returns {void}
       */
      function statusDropdownMenuKeydown(e) {
        if (e.key !== "Escape") return;
        const menu = /** @type {HTMLElement} */ (e.currentTarget);
        const id = menu.id.replace(/^status-dropdown-menu-/, "");
        statusDropdownCloseAll();
        const trigger = document.getElementById(statusDropdownTriggerId(id));
        if (trigger) trigger.focus();
      }

      /**
       * Multi-select mode: toggles one option in/out of the current selection.
       * @param {string} id
       * @param {string} value
       * @param {boolean} on
       * @returns {void}
       */
      function statusDropdownToggleOption(id, value, on) {
        const config = statusDropdownRegistry.get(id);
        if (!config || !config.onToggle) return;
        config.onToggle(value, on);
        statusDropdownRefreshMenu(id);
        statusDropdownRefreshTrigger(id);
      }

      /**
       * Multi-select mode: selects every option for this dropdown's view.
       * @param {string} id
       * @returns {void}
       */
      function statusDropdownSelectAll(id) {
        const config = statusDropdownRegistry.get(id);
        if (!config || !config.onAll) return;
        config.onAll();
        statusDropdownRefreshMenu(id);
        statusDropdownRefreshTrigger(id);
      }

      /**
       * Multi-select mode: clears every option for this dropdown's view.
       * @param {string} id
       * @returns {void}
       */
      function statusDropdownSelectNone(id) {
        const config = statusDropdownRegistry.get(id);
        if (!config || !config.onNone) return;
        config.onNone();
        statusDropdownRefreshMenu(id);
        statusDropdownRefreshTrigger(id);
      }

      /**
       * Single-select mode: picks one option (or `null` for "any"), then closes the menu.
       * @param {string} id
       * @param {string|null} value
       * @returns {void}
       */
      function statusDropdownSelectSingle(id, value) {
        const config = statusDropdownRegistry.get(id);
        if (!config || !config.onSelect) return;
        config.onSelect(value);
        statusDropdownCloseAll();
        const trigger = document.getElementById(statusDropdownTriggerId(id));
        if (trigger) trigger.focus();
      }
      // RALPHUS-STATUS-DROPDOWN:END
