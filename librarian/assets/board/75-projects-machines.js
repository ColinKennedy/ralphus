      // ---------- resource usage (RAL-11) ----------
      /**
       * Polls `/api/resources` and re-renders the Resources tab.
       * @returns {Promise<void>}
       */
      async function pollResources() {
        try {
          const d = await (await fetch("/api/resources")).json();
          byId("conn").className = "dot on";
          markUpdated();
          resources = d.resources || [];
          if (!userIsSelecting()) renderResources();
        } catch (e) { markUnreachable(); }
      }
      /**
       * @typedef {object} ResCol
       * @property {string} key
       * @property {string} label
       * @property {string} tip
       * @property {(r: ResourceEntry) => (string|number|null)} get
       */
      /** @type {ResCol[]} */
      const RES_COLS = [
        { key: "task", label: "Task", tip: "Task name and cell ID. Click to sort alphabetically.", get: (r) => (r.task_name || "") + " / " + (r.cell_id || "") },
        { key: "cpu", label: "CPU", tip: "CPU usage of the runner process and its children. Click to sort — highest first.", get: (r) => r.cpu_percent },
        { key: "ram", label: "RAM", tip: "RAM usage of the runner process and its children. Click to sort — highest first.", get: (r) => r.mem_bytes },
        { key: "gpu", label: "GPU", tip: "GPU memory usage. Only available if the process uses CUDA/ROCm. Click to sort — highest first.", get: (r) => r.gpu_mem_bytes },
      ];
      /**
       * Formats a percentage value, or "N/A" when null.
       * @param {number|null} v
       * @returns {string}
       */
      const fmtPct = (v) => v == null ? "N/A" : v.toFixed(1) + "%";
      /**
       * Formats a byte count in human-readable units, or "N/A" when null.
       * @param {number|null} v
       * @returns {string}
       */
      const fmtBytes = (v) => {
        if (v == null) return "N/A";
        const u = ["B", "KB", "MB", "GB", "TB"];
        let i = 0, n = v;
        while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
        return (i === 0 ? String(n) : n.toFixed(1)) + " " + u[i];
      };
      /**
       * Changes the Resources table's active sort column/direction.
       * @param {string} key
       * @returns {void}
       */
      function setResSort(key) {
        if (resSort.key === key) resSort.dir *= -1;
        else resSort = { key, dir: key === "task" ? 1 : -1 };
        renderResources();
      }
      /**
       * Sorts the current resource-usage snapshot by the active column.
       * @returns {ResourceEntry[]}
       */
      function sortedResources() {
        const col = RES_COLS.find((c) => c.key === resSort.key) || RES_COLS[1];
        return [...resources].sort((a, b) => {
          const va = col.get(a), vb = col.get(b);
          // Missing metrics (N/A) always sort to the bottom regardless of direction.
          if (va == null && vb == null) return 0;
          if (va == null) return 1;
          if (vb == null) return -1;
          const cmp = typeof va === "string" ? va.localeCompare(/** @type {string} */ (vb)) : /** @type {number} */ (va) - /** @type {number} */ (vb);
          return resSort.dir * cmp;
        });
      }
      /**
       * Renders the Resources tab's table.
       * @returns {void}
       */
      function renderResources() {
        const el = byId("resources");
        byId("res-summary").textContent =
          `${resources.length} running ${resources.length === 1 ? "task" : "tasks"}`;
        if (!resources.length) { el.innerHTML = `<div class="empty">No running tasks.</div>`; return; }
        /**
         * @param {string} key
         * @returns {string}
         */
        const arrow = (key) => resSort.key === key ? (resSort.dir < 0 ? " ↓" : " ↑") : "";
        const head = RES_COLS.map((c) =>
          `<th class="${resSort.key === c.key ? "sorted" : ""}" data-click="setResSort" data-col="${esc(c.key)}" data-tip="${esc(c.tip)}">${c.label}${arrow(c.key)}</th>`
        ).join("") + `<th data-tip="Jump to the cell in the Squads tab."></th>`;
        const rows = sortedResources().map((r) => `<tr>
            <td>${esc(r.task_name)} <span style="color:var(--muted)">/ ${esc(r.cell_id)}</span><div style="color:var(--muted);font-size:11px">${esc(r.squad_label || r.squad_id)} · pid ${r.pid}</div></td>
            <td>${fmtPct(r.cpu_percent)}</td>
            <td>${fmtBytes(r.mem_bytes)}</td>
            <td>${fmtBytes(r.gpu_mem_bytes)}</td>
            <td><button class="res-jump" data-click="jumpToTask" data-squad-id="${esc(r.squad_id)}" data-ti="${r.task_idx}" data-si="${r.cell_idx}" data-tip="Switch to the Squads tab and scroll to this exact running cell.">Go to task →</button></td>
          </tr>`).join("");
        el.innerHTML = `<table class="res-table"><thead><tr>${head}</tr></thead><tbody>${rows}</tbody></table>`;
      }
      // ---- Projects tab (RAL-100/RAL-101) ----────────────────────────────────
      // Validation runs once per tab-load (not on every refresh, unlike the rest
      // of the board) since each row check spawns a `git rev-parse` subprocess on
      // the daemon; refreshProjects() re-arms it for an explicit re-check.
      let projectsValidated = false;
      // ---- Machines tab (RAL-185) ----
      // The registry of machine providers a task's `machine = "<scheme>:<uri>"`
      // resolves against. Registering is an administrative action -- never
      // declarable in a task file, since a provider entry names a program the
      // daemon will run -- so this tab is where it is exposed, rather than
      // anywhere a task is authored.
      /**
       * Polls `/api/machines` and re-renders the Machines tab.
       * @returns {Promise<void>}
       */
      async function pollMachines() {
        try {
          const d = await (await fetch("/api/machines")).json();
          byId("conn").className = "dot on";
          markUpdated();
          machines = (d.machines || []).slice().sort(
            (/** @type {MachineProviderView} */ a, /** @type {MachineProviderView} */ b) => a.scheme.localeCompare(b.scheme));
          machineBuiltins = d.builtin || [];
          renderMachines();
        } catch (e) { markUnreachable(); }
      }
      /**
       * Renders one registered provider's table row.
       * @param {MachineProviderView} m
       * @returns {string}
       */
      function machineRowHtml(m) {
        const args = (m.args || []).join(" ");
        return `<tr>
            <td><span class="proj-name">${esc(m.scheme)}</span></td>
            <td>${esc(m.description || "")}</td>
            <td class="mono">${esc(m.program)}${args ? ` <span style="color:var(--muted)">${esc(args)}</span>` : ""}</td>
            <td>v${m.protocol_version}${m.supports_channel ? ` <span style="color:var(--teal)" data-tip="This provider reuses one long-lived process for many commands instead of being spawned per command.
Matters when reaching the machine is expensive; the daemon falls back to per-command spawns automatically if a channel cannot be opened.">channel</span>` : ""}</td>
            <td>${machineReachability(m)}</td>
            <td style="color:var(--muted)">${fmtProjCreated(m.created_at_ms)}</td>
            <td><button class="btn" data-click="checkMachine" data-scheme="${esc(m.scheme)}" data-tip="Probe this machine now via the provider's 'ping' verb and record the result.
Who/when: after registering a provider, or when a squad failed and you need to tell 'the work broke' apart from 'the machine is unplugged'.">Check</button>
              <button class="btn" data-click="removeMachine" data-scheme="${esc(m.scheme)}" data-tip="Deregister this provider.\nAny task submitted afterwards naming this scheme is rejected at submit; already-submitted squads are unaffected, since they resolved their machine when they were submitted.\nThis cannot be undone — you would have to register it again.">Remove</button></td>
          </tr>`;
      }
      /**
       * Renders the Machines tab: registered providers, built-in schemes, and the register form.
       * @returns {void}
       */
      function renderMachines() {
        byId("machine-summary").innerHTML =
          `${machines.length} registered ${machines.length === 1 ? "provider" : "providers"} `
          + `<button class="btn" style="margin-left:8px;padding:2px 8px;font-size:12px" onclick="pollMachines()" data-tip="Re-query the daemon for the registered machine provider list.">⟳ Refresh</button>`;
        const el = byId("machines");
        const err = machineError
          ? `<div class="empty" style="color:var(--failed);margin-bottom:8px">${esc(machineError)}</div>`
          : "";
        const builtin = machineBuiltins.length
          ? `<div style="color:var(--muted);font-size:12px;margin-bottom:12px" data-tip="Schemes that always resolve without a registry row.\n'local' is also the implicit default when a task never sets machine at all, so a task file that never mentions machine keeps running exactly where it always did.">Built-in (always available): ${machineBuiltins.map((/** @type {string} */ b) => `<span class="mono">${esc(b)}</span>`).join(", ")}</div>`
          : "";
        const table = machines.length
          ? `<table class="proj-table"><thead><tr>`
            + `<th data-tip="The scheme half of a machine value. Matched case-insensitively.">Scheme</th>`
            + `<th data-tip="Human-readable description, shown here and in submit-time error messages listing available providers.">Description</th>`
            + `<th data-tip="The program the daemon runs to reach machines under this scheme, plus any arguments always prepended before the verb.">Program</th>`
            + `<th data-tip="Provider-contract version this entry was registered against.\nA provider declaring a version this daemon does not implement is refused at dispatch rather than invoked and hoped for.">Contract</th>`
            + `<th data-tip="Result of the last explicit reachability probe (the provider's 'ping' verb).
Not polled: probing spawns the provider program, so a board refreshing every couple of seconds would turn it into steady load on a build farm.
'not checked' means no probe has run — deliberately distinct from reachable or unreachable, which would imply evidence that does not exist.">Reachable</th>`
            + `<th data-tip="When this provider was registered.">Registered</th><th></th>`
            + `</tr></thead><tbody>${machines.map((m) => machineRowHtml(m)).join("")}</tbody></table>`
          : `<div class="empty">No registered machine providers — every task runs on this daemon's own host.</div>`;
        const form = `<div style="margin-top:16px;padding-top:12px;border-top:1px solid var(--border)">
            <div style="margin-bottom:8px" data-tip="Register a provider program so tasks can reference its scheme.\nDeliberately an administrative action rather than something a task file declares: a provider entry names a program the daemon will run, so a task file that could both name and define one would make submitting a task equivalent to running arbitrary code.">Register a provider</div>
            <div class="row" style="gap:8px;flex-wrap:wrap">
              <input id="machine-scheme" type="text" placeholder="scheme (e.g. incredibuild)" style="width:200px" data-tip="Provider name — the left half of a machine value.\nLetters, digits, '_' and '-' only, at least two characters (a one-character scheme is rejected so a pasted Windows path never parses as a machine)." />
              <input id="machine-program" type="text" placeholder="program path" style="flex:1;min-width:260px" data-tip="Absolute path to the program the daemon invokes for this scheme.\nCalled as: program &lt;verb&gt; --uri &lt;uri&gt; [--handle H] [--since N] — see docs/machine-providers.md for the contract." />
              <input id="machine-desc" type="text" placeholder="description (optional)" style="width:220px" data-tip="Shown in this table and in submit-time error messages listing available providers." />
              <button class="primary" onclick="registerMachine()" data-tip="Register this provider with the daemon.\nRe-registering an existing scheme updates it in place rather than creating a duplicate.">Register</button>
            </div>
          </div>`;
        el.innerHTML = err + builtin + table + form;
      }
      /**
       * Renders a provider's last-known reachability as a colored chip.
       * @param {MachineProviderView} m
       * @returns {string}
       */
      function machineReachability(m) {
        if (m.last_check_ok === undefined || m.last_check_ok === null) {
          return `<span style="color:var(--muted)" data-tip="No reachability probe has run for this provider yet.
This is deliberately not the same as 'reachable' or 'unreachable' — nothing has been measured.">not checked</span>`;
        }
        const when = m.last_check_ms ? ` (${fmtProjCreated(m.last_check_ms)})` : "";
        const note = m.last_check_note ? `
${m.last_check_note}` : "";
        return m.last_check_ok
          ? `<span style="color:var(--done)" data-tip="This machine answered its last reachability probe${esc(when)}.${esc(note)}">reachable</span>`
          : `<span style="color:var(--failed)" data-tip="This machine did not answer its last reachability probe${esc(when)}.${esc(note)}
Work submitted against it will fail — fix the machine or deregister the provider.">unreachable</span>`;
      }
      /**
       * Probes one machine provider and refreshes the tab with the result.
       * @param {string} scheme
       * @returns {Promise<void>}
       */
      async function checkMachine(scheme) {
        try {
          const r = await fetch(`/api/machines/${encodeURIComponent(scheme)}/check`, { method: "POST" });
          machineError = r.ok ? "" : await responseError(r, "check failed");
        } catch (e) { machineError = "daemon unreachable"; }
        await pollMachines();
      }
      /**
       * Registers a machine provider from the Machines tab form.
       * @returns {Promise<void>}
       */
      async function registerMachine() {
        const scheme = /** @type {HTMLInputElement} */ (byId("machine-scheme")).value.trim();
        const program = /** @type {HTMLInputElement} */ (byId("machine-program")).value.trim();
        const description = /** @type {HTMLInputElement} */ (byId("machine-desc")).value.trim();
        if (!scheme || !program) { machineError = "Scheme and program are both required."; renderMachines(); return; }
        try {
          const r = await fetch("/api/machines", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ scheme, program, description }),
          });
          machineError = r.ok ? "" : await responseError(r, "register failed");
        } catch (e) { machineError = "daemon unreachable"; }
        await pollMachines();
      }
      /**
       * Deregisters a machine provider.
       * @param {string} scheme
       * @returns {Promise<void>}
       */
      async function removeMachine(scheme) {
        if (!confirm(`Deregister machine provider "${scheme}"?\n\nTasks submitted afterwards that name this scheme will be rejected.`)) return;
        try {
          const r = await fetch(`/api/machines/${encodeURIComponent(scheme)}`, { method: "DELETE" });
          machineError = r.ok ? "" : await responseError(r, "remove failed");
        } catch (e) { machineError = "daemon unreachable"; }
        await pollMachines();
      }

      /**
       * Polls the Triage endpoints (types, pools, schedules) and re-renders
       * the Triage tab (RAL-318).
       * @returns {Promise<void>}
       */
      async function pollTriage() {
        try {
          const [typesResp, poolsResp, schedulesResp, candidatesResp] = await Promise.all([
            fetch("/api/triage/types"),
            fetch("/api/triage/pools"),
            fetch("/api/triage/schedules"),
            fetch("/api/triage/candidates"),
          ]);
          const [typesData, poolsData, schedulesData, candidatesData] = await Promise.all([
            typesResp.json(),
            poolsResp.json(),
            schedulesResp.json(),
            candidatesResp.json(),
          ]);
          byId("conn").className = "dot on";
          markUpdated();
          triageTypes = (typesData.types || []).slice().sort(
            (/** @type {TriageTypeView} */ a, /** @type {TriageTypeView} */ b) => a.name.localeCompare(b.name));
          triagePools = poolsData.pools || [];
          triageSchedules = schedulesData.schedules || [];
          triageCandidates = candidatesData.candidates || [];
          renderTriage();
        } catch (e) { markUnreachable(); }
      }
      /**
       * Renders the Triage tab: the registered type registry, current pool
       * state with editable count thresholds, configured cron schedules,
       * and (at the bottom) the candidate list of cells that opted into
       * Triage but haven't been linked to a review yet (RAL-318). Wrapped in
       * `preserveUserState` (RAL-7) so a periodic refresh or a filter
       * keystroke on the candidate list's name search doesn't steal focus.
       * @returns {void}
       */
      function renderTriage() {
        byId("triage-summary").innerHTML =
          `${triageTypes.length} ${triageTypes.length === 1 ? "type" : "types"}, `
          + `${triagePools.length} pooled ${triagePools.length === 1 ? "key" : "keys"} `
          + `<button class="btn" style="margin-left:8px;padding:2px 8px;font-size:12px" onclick="pollTriage()" data-tip="Re-query the daemon for Triage types, pool state, schedules, and candidates.">⟳ Refresh</button>`;
        const err = triageError
          ? `<div class="empty" style="color:var(--failed);margin-bottom:8px">${esc(triageError)}</div>`
          : "";
        const el = byId("triage");
        preserveUserState(el, () => {
          el.innerHTML = err + renderTriageTypesSection() + renderTriagePoolsSection() + renderTriageSchedulesSection() + renderTriageCandidatesSection();
        });
      }
      /**
       * Renders the Triage type registry table plus its register form.
       * @returns {string}
       */
      function renderTriageTypesSection() {
        const rows = triageTypes.length
          ? triageTypes.map((t) => triageTypeRowHtml(t)).join("")
          : `<tr><td colspan="5" class="empty">No Triage types registered yet.</td></tr>`;
        const table = `<table class="proj-table"><thead><tr>`
          + `<th data-tip="The name a cell's inline triage_type value, or the Arbiter's own classification, resolves against.">Name</th>`
          + `<th data-tip="Short human-readable label shown alongside the name.">Label</th>`
          + `<th data-tip="Fed to the Arbiter's classification prompt alongside every other registered type's description -- write it so the Arbiter can tell this type apart from the others.">Description</th>`
          + `<th data-tip="When this type was registered.">Registered</th><th></th>`
          + `</tr></thead><tbody>${rows}</tbody></table>`;
        const form = `<div style="margin-top:16px;padding-top:12px;border-top:1px solid var(--border)">
            <div style="margin-bottom:8px" data-tip="Register a Triage type so a cell's inline triage_type can name it, or the Arbiter can classify into it.\nThe built-in \"unclassified\" type always exists and is what a classification failure (timeout, error, ambiguous result) permanently assigns -- single attempt, no retry.">Register a Triage type</div>
            <div class="row" style="gap:8px;flex-wrap:wrap">
              <input id="triage-type-name" type="text" placeholder="name (e.g. security)" style="width:180px" data-tip="Type name -- what a cell's inline triage_type value or the Arbiter's classification result names." />
              <input id="triage-type-label" type="text" placeholder="label" style="width:180px" data-tip="Short human-readable label shown in this table and in the board's Arbiter-created review badges." />
              <input id="triage-type-desc" type="text" placeholder="description" style="flex:1;min-width:260px" data-tip="Fed to the Arbiter's classification prompt -- write it so the Arbiter can tell this type apart from every other registered type." />
              <button class="primary" onclick="registerTriageType()" data-tip="Register this Triage type with the daemon.\nRe-registering an existing name updates its label/description in place rather than creating a duplicate.">Register</button>
            </div>
          </div>`;
        return `<div style="margin-bottom:24px">
            <div style="font-weight:600;margin-bottom:8px" data-tip="Every Triage type a cell's inline triage_type or the Arbiter's classification can resolve to.">Types</div>
            ${table}${form}
          </div>`;
      }
      /**
       * Renders one registered Triage type's table row.
       * @param {TriageTypeView} t
       * @returns {string}
       */
      function triageTypeRowHtml(t) {
        const isBuiltin = t.name === "unclassified";
        const action = isBuiltin
          ? `<span style="color:var(--muted)" data-tip="The built-in fallback Triage type assigned when classification fails, times out, or is ambiguous (single attempt, no retry). It always exists and can never be removed.">built-in</span>`
          : `<button class="btn" data-click="removeTriageType" data-name="${esc(t.name)}" data-tip="Deregister this Triage type.\nCells already pooled or classified under it keep that assignment -- only future classification/validation against this name is affected. This cannot be undone.">Remove</button>`;
        return `<tr>
            <td><span class="proj-name">${esc(t.name)}</span></td>
            <td>${esc(t.label || "—")}</td>
            <td>${esc(t.description || "—")}</td>
            <td style="color:var(--muted)">${fmtProjCreated(t.created_at_ms)}</td>
            <td>${action}</td>
          </tr>`;
      }
      /**
       * Registers (or updates) a Triage type from the register form's fields.
       * @returns {Promise<void>}
       */
      async function registerTriageType() {
        const name = /** @type {HTMLInputElement} */ (byId("triage-type-name")).value.trim();
        const label = /** @type {HTMLInputElement} */ (byId("triage-type-label")).value.trim();
        const description = /** @type {HTMLInputElement} */ (byId("triage-type-desc")).value.trim();
        if (!name) { triageError = "Type name is required."; renderTriage(); return; }
        try {
          const r = await fetch("/api/triage/types", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ name, label, description }),
          });
          triageError = r.ok ? "" : await responseError(r, "register failed");
        } catch (e) { triageError = "daemon unreachable"; }
        await pollTriage();
      }
      /**
       * Deregisters a Triage type. The daemon refuses this for the built-in
       * "unclassified" type.
       * @param {string} name
       * @returns {Promise<void>}
       */
      async function removeTriageType(name) {
        if (!confirm(`Deregister Triage type "${name}"?\n\nCells already pooled or classified under it keep that assignment -- only future classification/validation against this name is affected.`)) return;
        try {
          const r = await fetch(`/api/triage/types/${encodeURIComponent(name)}`, { method: "DELETE" });
          triageError = r.ok ? "" : await responseError(r, "remove failed");
        } catch (e) { triageError = "daemon unreachable"; }
        await pollTriage();
      }
      /**
       * Renders the pool-state table: current pooled-cell counts per
       * (project, triage type) key alongside an editable count threshold.
       * @returns {string}
       */
      function renderTriagePoolsSection() {
        const rows = triagePools.length
          ? triagePools.map((p) => triagePoolRowHtml(p)).join("")
          : `<tr><td colspan="4" class="empty">No cells pooled and no thresholds configured yet.</td></tr>`;
        const table = `<table class="proj-table"><thead><tr>`
          + `<th data-tip="The project a pooled cell's owning task declared.">Project</th>`
          + `<th data-tip="The Triage type these pooled cells classified into, or were inline-declared as.">Type</th>`
          + `<th data-tip="Cells currently pooled for this (project, type) key, waiting for a count threshold or cron schedule to drain them into one fresh review.">Pooled</th>`
          + `<th data-tip="Draining this pool creates a review once it holds this many cells -- 'first to fire wins' against this key's schedules, and the pool resets afterward. Blank means no count-based trigger; only this key's schedules (if any) can drain it.">Threshold</th>`
          + `</tr></thead><tbody>${rows}</tbody></table>`;
        return `<div style="margin-bottom:24px">
            <div style="font-weight:600;margin-bottom:8px" data-tip="Current pool state (RAL-318): cells opted into Triage are pooled per (project, triage type) until a count threshold or cron schedule drains them into one fresh review.\nTo configure a threshold before any cell has been pooled yet, use the ⋯ button on that project's row in the Projects tab instead.">Pools</div>
            ${table}
          </div>`;
      }
      /**
       * Renders one pool key's row, including its inline threshold editor.
       * @param {TriagePoolView} p
       * @returns {string}
       */
      function triagePoolRowHtml(p) {
        const thresholdVal = p.threshold === null || p.threshold === undefined ? "" : String(p.threshold);
        return `<tr>
            <td>${esc(p.project)}</td>
            <td><span class="proj-name">${esc(p.triage_type)}</span></td>
            <td>${p.count}</td>
            <td><div class="row" style="gap:4px">
              <input type="number" min="1" step="1" class="mono pool-threshold-input" value="${esc(thresholdVal)}" placeholder="none" style="width:70px;background:var(--bg);border:1px solid var(--border);color:var(--text);border-radius:4px;padding:2px 5px;font-size:12px" data-tip="Draining this pool creates a review once it holds this many cells. Clear the field and press Set to remove the count-based trigger." />
              <button class="btn" style="padding:2px 8px;font-size:11px" data-click="savePoolThreshold" data-project="${esc(p.project)}" data-triage-type="${esc(p.triage_type)}" data-tip="Save this pool's count threshold.">Set</button>
            </div></td>
          </tr>`;
      }
      /**
       * Reads this row's threshold input and saves it (or, if left blank,
       * clears any configured count threshold) for the given pool key.
       * @param {MouseEvent} e
       * @param {string} project
       * @param {string} triageType
       * @returns {Promise<void>}
       */
      async function savePoolThreshold(e, project, triageType) {
        const row = /** @type {HTMLElement|null} */ (/** @type {HTMLElement} */ (e.target).closest("tr"));
        const input = row ? /** @type {HTMLInputElement|null} */ (row.querySelector(".pool-threshold-input")) : null;
        const raw = input ? input.value.trim() : "";
        if (raw !== "" && (!/^\d+$/.test(raw) || Number(raw) < 1)) {
          triageError = "Threshold must be a whole number of at least 1, or blank to clear it.";
          renderTriage();
          return;
        }
        try {
          const r = await fetch("/api/triage/pools/threshold", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ project, triage_type: triageType, threshold: raw === "" ? null : Number(raw) }),
          });
          triageError = r.ok ? "" : await responseError(r, "set threshold failed");
        } catch (err) { triageError = "daemon unreachable"; }
        await pollTriage();
      }
      /**
       * Renders the configured cron schedules table plus its add-schedule form.
       * @returns {string}
       */
      function renderTriageSchedulesSection() {
        const rows = triageSchedules.length
          ? triageSchedules.map((s) => triageScheduleRowHtml(s)).join("")
          : `<tr><td colspan="8" class="empty">No schedules configured.</td></tr>`;
        const table = `<table class="proj-table"><thead><tr>`
          + `<th data-tip="The project half of the (project, triage type) pool key this schedule drains.">Project</th>`
          + `<th data-tip="The Triage type half of the pool key this schedule drains.">Type</th>`
          + `<th data-tip="Standard 5-field cron expression, evaluated in UTC.">Cron</th>`
          + `<th data-tip="The reference date this schedule's every-N-occurrences count is measured from -- lets 'every other Monday' or 'every 3 months' mean something a bare cron expression can't express alone. Shown in your local timezone; stored as UTC.">Anchor</th>`
          + `<th data-tip="Fires on every Nth occurrence of the cron expression counted from the anchor date, not every occurrence.">Every N</th>`
          + `<th data-tip="How many times this schedule has fired and drained its pool.">Fired</th>`
          + `<th data-tip="When this schedule was last evaluated by the daemon's scheduler tick, shown in your local timezone.">Last checked</th>`
          + `<th></th>`
          + `</tr></thead><tbody>${rows}</tbody></table>`;
        const form = `<div style="margin-top:16px;padding-top:12px;border-top:1px solid var(--border)">
            <div style="margin-bottom:8px" data-tip="Add a cron-style drain trigger for a (project, triage type) pool. A pool may have several active schedules at once (e.g. 'every other Monday' and 'every 3 months') plus an independent count threshold -- whichever fires first drains the pool and resets its counter/timer; no cell is double-counted across drains.">Add a schedule</div>
            <div class="row" style="gap:8px;flex-wrap:wrap">
              <input id="triage-sched-project" type="text" placeholder="project" style="width:160px" data-tip="The project half of the pool key this schedule drains." />
              <input id="triage-sched-type" type="text" placeholder="triage type" style="width:160px" data-tip="The Triage type half of the pool key this schedule drains." />
              <input id="triage-sched-cron" type="text" placeholder="cron expr (e.g. 0 0 * * MON)" style="width:220px" class="mono" data-tip="Standard 5-field cron expression, evaluated in UTC." />
              <input id="triage-sched-anchor" type="date" style="width:160px" data-tip="Reference date this schedule's every-N-occurrences count is measured from." />
              <input id="triage-sched-every-n" type="number" min="1" step="1" value="1" style="width:90px" data-tip="Fires on every Nth occurrence of the cron expression counted from the anchor date -- 1 means every occurrence." />
              <button class="primary" onclick="addTriageSchedule()" data-tip="Register this schedule.\nIt races the pool's count threshold and every other active schedule on the same key -- whichever fires first drains the pool.">Add</button>
            </div>
          </div>`;
        return `<div>
            <div style="font-weight:600;margin-bottom:8px" data-tip="Cron-style drain triggers (RAL-318). Each (project, triage type) pool can have several active schedules simultaneously, each anchored to its own reference date for interval parity.">Schedules</div>
            ${table}${form}
          </div>`;
      }
      /**
       * Renders one configured cron schedule entry's table row.
       * @param {TriageScheduleView} s
       * @returns {string}
       */
      function triageScheduleRowHtml(s) {
        return `<tr>
            <td>${esc(s.project)}</td>
            <td><span class="proj-name">${esc(s.triage_type)}</span></td>
            <td class="mono">${esc(s.cron_expr)}</td>
            <td style="color:var(--muted)">${fmtProjCreated(s.anchor_date_ms)}</td>
            <td>${s.every_n}</td>
            <td>${s.occurrence_count}</td>
            <td style="color:var(--muted)">${s.last_checked_ms ? fmtProjCreated(s.last_checked_ms) : "never"}</td>
            <td><button class="btn" data-click="removeTriageSchedule" data-id="${s.id}" data-tip="Remove this schedule.\nThe pool's count threshold (if any) and any other schedules on this key are unaffected. This cannot be undone.">Remove</button></td>
          </tr>`;
      }
      /**
       * Registers a new cron schedule entry from the add-a-schedule form.
       * @returns {Promise<void>}
       */
      async function addTriageSchedule() {
        const project = /** @type {HTMLInputElement} */ (byId("triage-sched-project")).value.trim();
        const triageType = /** @type {HTMLInputElement} */ (byId("triage-sched-type")).value.trim();
        const cronExpr = /** @type {HTMLInputElement} */ (byId("triage-sched-cron")).value.trim();
        const anchorDate = /** @type {HTMLInputElement} */ (byId("triage-sched-anchor")).value;
        const everyNRaw = /** @type {HTMLInputElement} */ (byId("triage-sched-every-n")).value.trim();
        if (!project || !triageType || !cronExpr || !anchorDate) {
          triageError = "Project, triage type, cron expression, and anchor date are all required.";
          renderTriage();
          return;
        }
        const anchorMs = new Date(anchorDate).getTime();
        const everyN = everyNRaw === "" ? 1 : Number(everyNRaw);
        try {
          const r = await fetch("/api/triage/schedules", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ project, triage_type: triageType, cron_expr: cronExpr, anchor_date_ms: anchorMs, every_n: everyN }),
          });
          triageError = r.ok ? "" : await responseError(r, "add schedule failed");
        } catch (e) { triageError = "daemon unreachable"; }
        await pollTriage();
      }
      /**
       * Removes a configured cron schedule entry.
       * @param {string} id
       * @returns {Promise<void>}
       */
      async function removeTriageSchedule(id) {
        if (!confirm("Remove this Triage schedule?\n\nThe pool's count threshold (if any) and any other schedules on this key are unaffected.")) return;
        try {
          const r = await fetch(`/api/triage/schedules/${encodeURIComponent(id)}`, { method: "DELETE" });
          triageError = r.ok ? "" : await responseError(r, "remove failed");
        } catch (e) { triageError = "daemon unreachable"; }
        await pollTriage();
      }
      /**
       * Computes the Triage tab's candidate list after applying the
       * type/project/name-search filters.
       * @returns {TriageCandidateView[]}
       */
      function visibleTriageCandidates() {
        const q = triageCandidateFilters.q.trim().toLowerCase();
        return triageCandidates.filter((c) =>
          (!triageCandidateFilters.type || c.triage_types.includes(triageCandidateFilters.type))
          && (!triageCandidateFilters.project || c.project === triageCandidateFilters.project)
          && (!q
            || c.task_name.toLowerCase().includes(q)
            || c.cell_id.toLowerCase().includes(q)
            || (c.cell_name || "").toLowerCase().includes(q)));
      }
      /**
       * Renders the small status badge for one candidate row describing its
       * relationship to Triage, not its own cell state: "Scheduled" (opted
       * in, not done yet -- nothing to sweep into a review), "Queued" (done
       * and viable, waiting on its pool's count threshold or a cron
       * schedule), or "Excluded (failed)" (done, but its proof failed --
       * permanently barred from ever being swept into an auto-review; see
       * `Store::drain_triage_pool`). Mirrors `triagePlaceholderSection()`'s
       * per-cell placeholder wording/color, compacted for a table cell.
       * @param {TriageCandidateView} c
       * @returns {string}
       */
      function triageCandidateStatusBadge(c) {
        if (c.status === "failed") {
          return `<span class="badge" style="color:var(--failed);border-color:var(--failed);font-size:11px" data-tip="This cell's proof failed -- a failed cell can never pass review, so it is permanently excluded from being swept into an automatic review.">✗ Excluded (failed)</span>`;
        }
        const done = c.status === "queued";
        const label = done ? "Queued" : "Scheduled";
        const icon = done ? "✓" : "⏳";
        const tip = done
          ? "This cell finished and passed, and is pooled, waiting for its Triage pool's count threshold or a cron schedule to fire."
          : "This cell opted into Triage but hasn't finished yet -- it can't be swept into an automatic review until it finishes.";
        return `<span class="badge" style="color:var(--arbiter);border-color:var(--arbiter);font-size:11px;opacity:${done ? "1" : "0.7"}" data-tip="${tip}">${icon} ${label}</span>`;
      }
      /**
       * Renders one Triage candidate's table row. The whole row is
       * clickable and jumps to the Squads tab with the owning squad/cell
       * selected, via the same `gotoSquadItem` navigation the rest of the
       * board uses.
       * @param {TriageCandidateView} c
       * @returns {string}
       */
      function triageCandidateRowHtml(c) {
        const cellLabel = c.cell_name || c.cell_id;
        return `<tr style="cursor:pointer" data-click="gotoTriageCandidate" data-squad-id="${esc(c.squad_id)}" data-ti="${c.task_idx}" data-si="${c.cell_idx}" data-tip="Jump to the Squads tab and select this cell.">
            <td>${triageCandidateStatusBadge(c)}</td>
            <td>${c.triage_types.map((t) => esc(t)).join(", ")}</td>
            <td>${esc(c.project)}</td>
            <td><div>${esc(c.task_name)}</div><div class="mono" style="font-size:11px;color:var(--muted)">${esc(cellLabel)}</div></td>
          </tr>`;
      }
      /**
       * Renders the Triage candidate list: every cell across every squad
       * that has opted into Triage and hasn't been linked to an actual
       * review yet, filterable by type/project/name.
       * @returns {string}
       */
      function renderTriageCandidatesSection() {
        const list = visibleTriageCandidates();
        const typeOptions = triageTypes.map((t) =>
          `<option value="${esc(t.name)}" ${triageCandidateFilters.type === t.name ? "selected" : ""}>${esc(t.name)}</option>`).join("");
        const projects = [...new Set(triageCandidates.map((c) => c.project))].sort();
        const projectOptions = projects.map((p) =>
          `<option value="${esc(p)}" ${triageCandidateFilters.project === p ? "selected" : ""}>${esc(p)}</option>`).join("");
        const rows = list.length
          ? list.map((c) => triageCandidateRowHtml(c)).join("")
          : `<tr><td colspan="4" class="empty">${triageCandidates.length ? "No candidates match these filters." : "No cells have opted into Triage yet."}</td></tr>`;
        const table = `<table class="proj-table"><thead><tr>`
          + `<th data-tip="Whether this cell can be swept into an automatic review yet: Scheduled (opted in, still pending/running/failed) or Queued (done, waiting on its pool's count threshold or a cron schedule).">Status</th>`
          + `<th data-tip="This cell's resolved Triage type(s).">Type</th>`
          + `<th data-tip="The project this cell's owning task belongs to.">Project</th>`
          + `<th data-tip="The task and cell this candidate belongs to. Click a row to jump to it on the Squads tab.">Task / Cell</th>`
          + `</tr></thead><tbody>${rows}</tbody></table>`;
        return `<div style="margin-top:24px;padding-top:12px;border-top:1px solid var(--border)">
            <div style="font-weight:600;margin-bottom:8px" data-tip="Every cell across every squad that has opted into Triage (triage = true) and hasn't been linked to an actual review yet -- decided at submission, before the cell even runs. A cell appears here the moment its Triage type(s) resolve, and drops off once its pool drains into a review.">Candidates (${triageCandidates.length})</div>
            <div class="row" style="gap:8px;flex-wrap:wrap;margin-bottom:8px">
              <select id="triage-cand-type" onchange="onTriageCandidateTypeFilter(this.value)" style="background:var(--bg);border:1px solid var(--border);color:var(--text);border-radius:4px;padding:2px 5px;font-size:12px" data-tip="Filter the candidate list down to one Triage type.">
                <option value="">All types</option>${typeOptions}
              </select>
              <select id="triage-cand-project" onchange="onTriageCandidateProjectFilter(this.value)" style="background:var(--bg);border:1px solid var(--border);color:var(--text);border-radius:4px;padding:2px 5px;font-size:12px" data-tip="Filter the candidate list down to one project.">
                <option value="">All projects</option>${projectOptions}
              </select>
              <input id="triage-cand-q" type="text" placeholder="filter by task/cell name…" value="${esc(triageCandidateFilters.q)}" oninput="onTriageCandidateNameFilter(this.value)" style="flex:1;min-width:200px;background:var(--bg);border:1px solid var(--border);color:var(--text);border-radius:4px;padding:2px 5px;font-size:12px" data-tip="Filter the candidate list by task or cell name/id." />
            </div>
            ${table}
          </div>`;
      }
      /**
       * Updates the candidate list's Triage-type filter and re-renders.
       * @param {string} v
       * @returns {void}
       */
      function onTriageCandidateTypeFilter(v) { triageCandidateFilters.type = v; renderTriage(); }
      /**
       * Updates the candidate list's project filter and re-renders.
       * @param {string} v
       * @returns {void}
       */
      function onTriageCandidateProjectFilter(v) { triageCandidateFilters.project = v; renderTriage(); }
      /**
       * Updates the candidate list's name-search filter and re-renders.
       * @param {string} v
       * @returns {void}
       */
      function onTriageCandidateNameFilter(v) { triageCandidateFilters.q = v; renderTriage(); }

      /**
       * Polls `/api/users` and re-renders the Users tab.
       * @returns {Promise<void>}
       */
      async function pollUsers() {
        try {
          const d = await (await fetch("/api/users")).json();
          byId("conn").className = "dot on";
          markUpdated();
          users = (d.users || []).slice().sort((/** @type {UserView} */ a, /** @type {UserView} */ b) => a.name.localeCompare(b.name));
          if (userEditState === null) renderUsers();
        } catch (e) { markUnreachable(); }
      }
      /**
       * Enters inline edit mode for a user row.
       * @param {string} name
       * @returns {void}
       */
      function startUserEdit(name) {
        const u = users.find((x) => x.name === name);
        if (!u) return;
        userEditState = name;
        userEditDraft = { name: u.name };
        userSaveError = "";
        renderUsers();
      }
      /**
       * Exits a user row's inline edit mode without saving.
       * @returns {void}
       */
      function cancelUserEdit() {
        userEditState = null;
        userEditDraft = {};
        userSaveError = "";
        renderUsers();
      }
      /**
       * Saves a user row's inline edit draft (renames the user).
       * @param {string} name
       * @returns {Promise<void>}
       */
      async function saveUserEdit(name) {
        const newName = (userEditDraft.name ?? "").trim();
        userSaveError = "";
        if (!newName) { userSaveError = "Name is required."; renderUsers(); return; }
        let resp;
        try {
          resp = await fetch(`/api/users/${encodeURIComponent(name)}/rename`, {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ name: newName }),
          });
        } catch (e) {
          userSaveError = "save failed: daemon unreachable";
          renderUsers();
          return;
        }
        if (!resp.ok) {
          userSaveError = await responseError(resp, "save failed");
          renderUsers();
          return;
        }
        userEditState = null;
        userEditDraft = {};
        await pollUsers();
      }
      /**
       * Renders one registered user's table row (read-only or inline-edit form).
       * @param {UserView} u
       * @returns {string}
       */
      function userRowHtml(u) {
        if (userEditState === u.name) {
          const d = userEditDraft;
          return `<tr class="proj-edit-row">
              <td><input type="text" value="${esc(d.name)}" oninput="userEditDraft.name=this.value" data-tip="Placeholder identity name — no format restrictions beyond non-empty." /></td>
              <td style="color:var(--muted)">${fmtProjCreated(u.created_at_ms)}</td>
              <td></td>
              <td>
                <div class="row" style="gap:6px;flex-wrap:nowrap">
                  <button class="btn primary" data-click="saveUserEdit" data-name="${esc(u.name)}" data-tip="Save this rename.\nA request already claiming the old name keeps working under the old name until it re-claims the new one.">Save</button>
                  <button class="btn" onclick="cancelUserEdit()" data-tip="Discard your changes and exit edit mode.">Cancel</button>
                </div>
                ${userSaveError ? `<div class="verr">${esc(userSaveError)}</div>` : ""}
              </td>
            </tr>`;
        }
        return `<tr>
            <td><span class="proj-name proj-field" data-name="${esc(u.name)}" ondblclick="startUserEdit(this.dataset.name)" data-tip="Double-click to rename this placeholder user.">${esc(u.name)}</span></td>
            <td style="color:var(--muted)">${fmtProjCreated(u.created_at_ms)}</td>
            <td data-tip="RAL-332: a UI-level convenience gate, not a real security boundary -- see the disclaimer above.">${u.is_admin ? "✓ Admin" : "—"}</td>
            <td class="row" style="gap:6px;flex-wrap:nowrap">
              <button class="btn" data-click="removeUser" data-name="${esc(u.name)}" data-tip="Remove this placeholder user.\nGrants/revokes nothing by itself — it just stops the name from being selectable as a default_user or explicit identity.\nThis cannot be undone — you would have to add it again.">Remove</button>
              <button class="btn squadbtn" data-click="openUserMenu" data-name="${esc(u.name)}" data-tip="More actions — edit this user's profile as admin, or grant/revoke their admin flag.">⋯</button>
            </td>
          </tr>`;
      }
      /**
       * Renders the Users tab: the admin disclaimer, registered users, and the add-user form.
       * @returns {void}
       */
      function renderUsers() {
        byId("user-summary").innerHTML =
          `${users.length} registered ${users.length === 1 ? "user" : "users"} `
          + `<button class="btn" style="margin-left:8px;padding:2px 8px;font-size:12px" onclick="pollUsers()" data-tip="Re-query the daemon for the registered user list.">⟳ Refresh</button>`;
        const el = byId("users");
        const err = userError
          ? `<div class="empty" style="color:var(--failed);margin-bottom:8px">${esc(userError)}</div>`
          : "";
        const table = users.length
          ? `<table class="proj-table"><thead><tr>`
            + `<th data-tip="Placeholder identity name — what a caller claims via X-Ralphus-User, or a default_user config value, names. Double-click a row to rename.">Name</th>`
            + `<th data-tip="When this user was registered.">Registered</th>`
            + `<th data-tip="Whether this user's admin flag (RAL-332) is set.">Admin</th><th></th>`
            + `</tr></thead><tbody>${users.map((u) => userRowHtml(u)).join("")}</tbody></table>`
          : `<div class="empty">No registered users yet.</div>`;
        const form = `<div style="margin-top:16px;padding-top:12px;border-top:1px solid var(--border)">
            <div style="margin-bottom:8px" data-tip="Register a placeholder identity by name.\nThis is not authentication — it grants no permissions, and any caller can claim a registered name. TODO: replace with user auth once RAL-252 is done.">Add a user</div>
            <div class="row" style="gap:8px;flex-wrap:wrap">
              <input id="user-name" type="text" placeholder="name (e.g. colin)" style="width:200px" data-tip="Placeholder identity name — no format restrictions beyond non-empty." />
              <button class="primary" onclick="addUser()" data-tip="Register this user with the daemon.\nRe-registering an existing name is a no-op.">Add</button>
            </div>
          </div>`;
        el.innerHTML = err + table + form;
      }
      /**
       * Opens the per-user "..." menu (RAL-332): Edit Profile (visit-as) and
       * the admin-flag toggle. Mirrors `openSquadMenu`/`openReviewMenu`'s
       * floating-menu pattern (`closeSquadMenu` already closes this one too --
       * same shared `#squad-menu` element and outside-click handler).
       * @param {MouseEvent} e
       * @param {string} name
       * @returns {void}
       */
      function openUserMenu(e, name) {
        e.preventDefault(); e.stopPropagation(); closeSquadMenu();
        const u = users.find((x) => x.name === name); if (!u) return;
        const items = [
          `<div data-click="editUserProfile" data-name="${esc(name)}" data-tip="Open this user's Preferences page (RAL-329) -- their theme setting doesn't move, but their hidden squads/reviews are read and can be re-enabled on their behalf.\nWho/when: troubleshooting what a user has hidden, without asking them to check themselves.\nDoes not swap your own session/filters -- ends the moment you navigate away. Logged to Cartographer for auditability.">👤 Edit Profile</div>`,
          u.is_admin
            ? `<div data-click="toggleUserAdmin" data-name="${esc(name)}" data-admin="0" data-tip="Revoke this user's admin flag.\nThis is a UI-level convenience gate, not a hard security boundary (RAL-252 isn't done yet) -- see the disclaimer on this tab.">🔓 Remove Admin</div>`
            : `<div data-click="toggleUserAdmin" data-name="${esc(name)}" data-admin="1" data-tip="Grant this user the admin flag -- shows them the Machines/Triage/Projects/Users/Secrets tabs and lets them do the same for others.\nThis is a UI-level convenience gate, not a hard security boundary (RAL-252 isn't done yet) -- see the disclaimer on this tab.">🔑 Make Admin</div>`,
        ];
        const menu = document.createElement("div");
        menu.className = "ctx-menu"; menu.id = "squad-menu"; menu.innerHTML = items.join("");
        document.body.appendChild(menu);
        menu.style.left = Math.min(e.clientX, window.innerWidth - 180) + "px";
        menu.style.top = Math.min(e.clientY, window.innerHeight - 90) + "px";
      }
      /**
       * "Edit Profile" (RAL-332): opens RAL-329's Preferences page scoped to
       * `name` instead of the admin's own identity, and records the visit to
       * Cartographer. Fire-and-forget on the audit call -- a failed log
       * write must not block navigation.
       * @param {string} name
       * @returns {void}
       */
      function editUserProfile(name) {
        closeSquadMenu();
        fetch(`/api/users/${encodeURIComponent(name)}/visit`, { method: "POST" }).catch(() => {});
        prefsViewingAs = name;
        showTab("prefs", true);
      }
      /**
       * Grants or revokes a user's admin flag (RAL-332).
       * @param {string} name
       * @param {string} admin - "1" or "0" (from the menu item's dataset)
       * @returns {Promise<void>}
       */
      async function toggleUserAdmin(name, admin) {
        closeSquadMenu();
        const isAdmin = admin === "1";
        if (!confirm(`${isAdmin ? "Grant" : "Revoke"} admin for "${name}"?\nAdmin shows the Machines/Triage/Projects/Users/Secrets tabs and lets them manage other users -- a UI-level convenience gate, not a hard security boundary.`)) return;
        try {
          const r = await fetch(`/api/users/${encodeURIComponent(name)}/admin`, {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ is_admin: isAdmin }),
          });
          userError = r.ok ? "" : await responseError(r, "admin change failed");
        } catch (e) { userError = "daemon unreachable"; }
        await pollUsers();
      }
      /**
       * Registers a user from the Users tab form.
       * @returns {Promise<void>}
       */
      async function addUser() {
        const name = /** @type {HTMLInputElement} */ (byId("user-name")).value.trim();
        if (!name) { userError = "Name is required."; renderUsers(); return; }
        try {
          const r = await fetch("/api/users", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ name }),
          });
          userError = r.ok ? "" : await responseError(r, "add failed");
        } catch (e) { userError = "daemon unreachable"; }
        await pollUsers();
      }
      /**
       * Removes a registered user.
       * @param {string} name
       * @returns {Promise<void>}
       */
      async function removeUser(name) {
        if (!confirm(`Remove user "${name}"?`)) return;
        try {
          const r = await fetch(`/api/users/${encodeURIComponent(name)}`, { method: "DELETE" });
          userError = r.ok ? "" : await responseError(r, "remove failed");
        } catch (e) { userError = "daemon unreachable"; }
        await pollUsers();
      }

      /**
       * Polls `/api/secret-env-names` and re-renders the Secrets tab.
       * @returns {Promise<void>}
       */
      async function pollSecretEnvNames() {
        try {
          const d = await (await fetch("/api/secret-env-names")).json();
          byId("conn").className = "dot on";
          markUpdated();
          secretEnvNames = (d.names || []).slice().sort((/** @type {SecretEnvNameView} */ a, /** @type {SecretEnvNameView} */ b) => a.name.localeCompare(b.name));
          renderSecretEnvNames();
        } catch (e) { markUnreachable(); }
      }
      /**
       * Renders one secret env-var name's table row.
       * @param {SecretEnvNameView} s
       * @returns {string}
       */
      function secretEnvNameRowHtml(s) {
        return `<tr>
            <td><span class="proj-name">${esc(s.name)}</span></td>
            <td style="color:var(--muted)">${fmtProjCreated(s.created_at_ms)}</td>
            <td>
              <button class="btn" data-click="renameSecretEnvName" data-name="${esc(s.name)}" data-tip="Rename this secret env-var name.\nThe new name takes over redaction coverage immediately; the old name stops being treated as secret.">Rename</button>
              <button class="btn" data-click="removeSecretEnvName" data-name="${esc(s.name)}" data-tip="Stop treating this env-var name as secret.\nValues already redacted in existing pane text/terminal logs stay redacted — this only affects future dispatches.\nThis cannot be undone — you would have to add it again.">Remove</button>
            </td>
          </tr>`;
      }
      /**
       * Renders the Secrets tab: registered secret env-var names and the add form.
       * @returns {void}
       */
      function renderSecretEnvNames() {
        byId("secret-env-name-summary").innerHTML =
          `${secretEnvNames.length} secret env-var ${secretEnvNames.length === 1 ? "name" : "names"} `
          + `<button class="btn" style="margin-left:8px;padding:2px 8px;font-size:12px" onclick="pollSecretEnvNames()" data-tip="Re-query the daemon for the secret env-var name list.">⟳ Refresh</button>`;
        const el = byId("secrets");
        const err = secretEnvNameError
          ? `<div class="empty" style="color:var(--failed);margin-bottom:8px">${esc(secretEnvNameError)}</div>`
          : "";
        const table = secretEnvNames.length
          ? `<table class="proj-table"><thead><tr>`
            + `<th data-tip="Env-var name — a cell/proof step's resolved env is checked against this list by exact name match.">Name</th>`
            + `<th data-tip="When this name was registered.">Registered</th><th></th>`
            + `</tr></thead><tbody>${secretEnvNames.map((s) => secretEnvNameRowHtml(s)).join("")}</tbody></table>`
          : `<div class="empty">No secret env-var names registered yet.</div>`;
        const form = `<div style="margin-top:16px;padding-top:12px;border-top:1px solid var(--border)">
            <div style="margin-bottom:8px" data-tip="Register an env-var name as secret.\nA matching variable's resolved value is scrubbed from durable pane text and terminal logs wherever it shows up, additive to the existing from_env-sourced agent-profile secrets (RAL-264).">Add a secret env-var name</div>
            <div class="row" style="gap:8px;flex-wrap:wrap">
              <input id="secret-env-name-input" type="text" placeholder="name (e.g. STRIPE_SECRET_KEY)" style="width:240px" data-tip="Must be a valid environment-variable identifier: starts with a letter or underscore, then letters/digits/underscores." />
              <button class="primary" onclick="addSecretEnvName()" data-tip="Register this name with the daemon.\nAdding a name that's already registered is rejected, not merged.">Add</button>
            </div>
          </div>`;
        el.innerHTML = err + table + form;
      }
      /**
       * Registers a secret env-var name from the Secrets tab form.
       * @returns {Promise<void>}
       */
      async function addSecretEnvName() {
        const name = /** @type {HTMLInputElement} */ (byId("secret-env-name-input")).value.trim();
        if (!name) { secretEnvNameError = "Name is required."; renderSecretEnvNames(); return; }
        try {
          const r = await fetch("/api/secret-env-names", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ name }),
          });
          secretEnvNameError = r.ok ? "" : await responseError(r, "add failed");
        } catch (e) { secretEnvNameError = "daemon unreachable"; }
        await pollSecretEnvNames();
      }
      /**
       * Renames a registered secret env-var name.
       * @param {string} name
       * @returns {Promise<void>}
       */
      async function renameSecretEnvName(name) {
        const newName = prompt(`Rename secret env-var name "${name}" to:`, name);
        if (!newName || newName.trim() === "" || newName.trim() === name) return;
        try {
          const r = await fetch(`/api/secret-env-names/${encodeURIComponent(name)}/rename`, {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ name: newName.trim() }),
          });
          secretEnvNameError = r.ok ? "" : await responseError(r, "rename failed");
        } catch (e) { secretEnvNameError = "daemon unreachable"; }
        await pollSecretEnvNames();
      }
      /**
       * Removes a registered secret env-var name.
       * @param {string} name
       * @returns {Promise<void>}
       */
      async function removeSecretEnvName(name) {
        if (!confirm(`Stop treating "${name}" as a secret env-var name?`)) return;
        try {
          const r = await fetch(`/api/secret-env-names/${encodeURIComponent(name)}`, { method: "DELETE" });
          secretEnvNameError = r.ok ? "" : await responseError(r, "remove failed");
        } catch (e) { secretEnvNameError = "daemon unreachable"; }
        await pollSecretEnvNames();
      }

      /**
       * Headers for a Preferences-tab request to `/api/hidden` -- empty for
       * the normal "your own preferences" case, or an explicit
       * `X-Ralphus-User` override while an admin is viewing another user's
       * page via "Edit Profile" (RAL-332). This is the one place in the
       * board that ever sends that header on purpose.
       * @returns {{[key: string]: string}}
       */
      function prefsUserHeaders() {
        return prefsViewingAs ? { "X-Ralphus-User": prefsViewingAs } : {};
      }
      /**
       * @returns {string}
       */
      function prefsUserName() { return prefsViewingAs || currentUserName || ""; }
      /**
       * @param {string} entityUri
       * @returns {boolean}
       */
      function isWatching(entityUri) { return watches.some((w) => w.entity_uri === entityUri); }
      /**
       * @param {string} entityUri
       * @returns {string}
       */
      function watchersHtml(entityUri) {
        const rows = entityWatchers.get(entityUri) || [];
        const names = rows.map((w) => w.user_name).join(", ") || "Nobody yet";
        return `<details onclick="event.stopPropagation()"><summary data-tip="Show everyone watching this entity. The list refreshes through the board's live update cycle.">${rows.length} watcher${rows.length === 1 ? "" : "s"}</summary><span style="color:var(--muted);font-size:11px">${esc(names)}</span></details>`;
      }
      /**
       * @param {string} entityUri
       * @returns {Promise<void>}
       */
      async function toggleWatch(entityUri) {
        closeSquadMenu();
        if (isWatching(entityUri)) {
          await fetch(`/api/watches/${entityUri}`, { method: "DELETE", headers: prefsUserHeaders() });
        } else {
          const answer = prompt("Notification tiers (comma-separated): urgent, high, normal", "urgent,high,normal");
          if (answer === null) return;
          const notify_tiers = answer.split(",").map((x) => x.trim().toLowerCase()).filter((x) => ["urgent", "high", "normal"].includes(x));
          if (!notify_tiers.length) { alert("Choose at least one of urgent, high, normal."); return; }
          await fetch("/api/watches", { method: "POST", headers: { ...prefsUserHeaders(), "Content-Type": "application/json" }, body: JSON.stringify({ entity_uri: entityUri, notify_tiers }) });
        }
        await pollWatches();
        tick();
      }
      /**
       * @param {boolean} enabled
       * @returns {Promise<void>}
       */
      async function setAutoWatch(enabled) {
        const name = prefsUserName();
        if (!name) return;
        const r = await fetch(`/api/users/${encodeURIComponent(name)}/preferences`, { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify({ auto_follow: enabled, default_notify_tiers: defaultWatchTiers }) });
        if (r.ok) autoWatch = enabled;
        renderPrefs();
      }
      /**
       * @returns {Promise<void>}
       */
      async function pollWatches() {
        try {
          const own = await fetch("/api/watches", { headers: prefsUserHeaders() });
          if (own.ok) watches = (await own.json()).watches || [];
          const uris = new Set(watches.map((w) => w.entity_uri));
          if (selectedSquadId) uris.add(`squad:${selectedSquadId}`);
          if (selectedGuardian) uris.add(`guardian:${selectedGuardian}`);
          const pairs = await Promise.all([...uris].map(async (uri) => {
            const r = await fetch(`/api/watches/${uri}`);
            return /** @type {[string, WatchView[]]} */ ([uri, r.ok ? ((await r.json()).watches || []) : []]);
          }));
          entityWatchers = new Map(pairs);
          const name = prefsUserName();
          if (name) {
            const p = await fetch(`/api/users/${encodeURIComponent(name)}/preferences`);
            if (p.ok) {
              const preferences = await p.json();
              autoWatch = !!preferences.auto_follow;
              defaultWatchTiers = preferences.default_notify_tiers || defaultWatchTiers;
            }
          }
        } catch (_) { /* transient -- the next live refresh retries */ }
      }
      /**
       * Polls `GET /api/hidden` for the current (or, under RAL-332's "Edit
       * Profile", visited) user's hidden squads/reviews (RAL-328/RAL-329),
       * plus `GET /api/tasks`/`GET /api/guardians` to resolve their display names, and
       * re-renders the Preferences tab.
       * @returns {Promise<void>}
       */
      async function pollPrefs() {
        try {
          await pollWatches();
          const [hiddenResp, tasksResp, guardiansResp] = await Promise.all([
            fetch("/api/hidden", { headers: prefsUserHeaders() }),
            fetch("/api/tasks"),
            fetch("/api/guardians"),
          ]);
          byId("conn").className = "dot on";
          markUpdated();
          if (hiddenResp.ok) {
            const d = await hiddenResp.json();
            hiddenItems = d.hidden || [];
            hiddenError = "";
          } else {
            hiddenItems = [];
            hiddenError = await responseError(hiddenResp, "could not load hidden items");
          }
          if (tasksResp.ok) {
            const t = await tasksResp.json();
            hiddenSquadNames = new Map((t.squads || []).map((/** @type {SquadView} */ s) => [s.id, s.label || s.id]));
          }
          if (guardiansResp.ok) {
            const g = await guardiansResp.json();
            hiddenGuardianNames = new Map((g || []).map((/** @type {GuardianView} */ x) => [x.id, x.name || x.id]));
          }
        } catch (e) {
          markUnreachable();
          hiddenError = "daemon unreachable";
        }
        renderPrefs();
      }
      /**
       * Updates the Preferences tab's free-text hidden-item filter and re-renders.
       * @param {string} v
       * @returns {void}
       */
      function onHiddenFilter(v) { hiddenFilters.q = v.toLowerCase(); renderPrefs(); syncHash(); }
      /**
       * Toggles whether a `HiddenItem.kind` is shown in the Preferences tab's list.
       * @param {string} kind
       * @param {boolean} on
       * @returns {void}
       */
      function toggleHiddenType(kind, on) {
        on ? hiddenFilters.type.add(kind) : hiddenFilters.type.delete(kind);
        renderPrefs(); syncHash();
      }
      /**
       * The display name for one hidden item, falling back to its raw id when its
       * owning squad/review hasn't loaded (or no longer resolves a name).
       * @param {HiddenItem} h
       * @returns {string}
       */
      function hiddenItemName(h) {
        if (h.kind === "squad") return hiddenSquadNames.get(h.squad_id || "") || h.squad_id || "";
        return hiddenGuardianNames.get(h.guardian_id || "") || h.guardian_id || "";
      }
      /**
       * `hiddenItems`, filtered by the current search text and type checkboxes.
       * @returns {HiddenItem[]}
       */
      function filteredHiddenItems() {
        return hiddenItems.filter((h) => {
          if (!hiddenFilters.type.has(h.kind)) return false;
          if (!hiddenFilters.q) return true;
          const id = h.kind === "squad" ? (h.squad_id || "") : (h.guardian_id || "");
          return id.toLowerCase().includes(hiddenFilters.q) || hiddenItemName(h).toLowerCase().includes(hiddenFilters.q);
        });
      }
      /**
       * Renders one hidden item's table row.
       * @param {HiddenItem} h
       * @returns {string}
       */
      function hiddenItemRowHtml(h) {
        const id = (h.kind === "squad" ? h.squad_id : h.guardian_id) || "";
        const whose = prefsViewingAs ? `${prefsViewingAs}'s` : "your own";
        return `<tr>
            <td>${h.kind === "squad" ? "Squad" : "Review"}</td>
            <td><span class="proj-name">${esc(hiddenItemName(h))}</span></td>
            <td style="color:var(--muted)">${fmtProjCreated(h.hidden_at_ms)}</td>
            <td><button class="btn" data-click="unhidePrefItem" data-kind="${esc(h.kind)}" data-id="${esc(id)}" data-tip="Re-enable this ${h.kind} in ${esc(whose)} views.\nOnly affects ${esc(whose)} view — nobody else's visibility changes.">Unhide</button></td>
          </tr>`;
      }
      /**
       * Renders the Preferences tab: the theme setting and the hidden-items list/filters.
       * @returns {void}
       */
      function renderPrefs() {
        const banner = byId("prefs-visit-banner");
        if (prefsViewingAs) {
          banner.style.display = "";
          banner.innerHTML = `<div style="margin-bottom:16px;padding:8px 12px;border:1px solid var(--accent);border-radius:6px" data-tip="You are viewing this page as another user via Edit Profile (RAL-332).\nWho/when: an admin action for troubleshooting or managing another user's hidden items on their behalf.\nThis does not swap your own session or filters -- it ends the moment you leave this tab, and every visit is logged to Cartographer for auditability.">Viewing <b>${esc(prefsViewingAs)}</b>'s preferences as admin. <button class="btn" onclick="showTab('users', true)" data-tip="Return to the Users tab -- also ends this Edit Profile view.">← Back to Users</button></div>`;
        } else {
          banner.style.display = "none";
          banner.innerHTML = "";
        }
        const whose = prefsViewingAs ? `${esc(prefsViewingAs)}'s` : "your";
        const resolvedAs = !prefsViewingAs && currentUserName
          ? ` <span style="color:var(--muted)" data-tip="Resolved from the X-Ralphus-User header, falling back to [daemon].default_user.">(resolved as ${esc(currentUserName)})</span>`
          : "";
        /** @type {HTMLInputElement} */ (byId("auto-watch")).checked = autoWatch;
        const watched = watches.map((w) => {
          const [kind, id] = w.entity_uri.split(":", 2);
          const squad = squads.find((s) => s.id === id);
          const review = guardians.find((g) => g.id === id);
          const name = kind === "squad" ? (squad?.label || id) : (review?.name || id);
          const status = kind === "squad" ? (squad?.state || "unknown") : (review?.status || "unknown");
          return `<tr><td>${kind === "squad" ? "Squad" : "Review"}</td><td>${esc(name)}</td><td>${pill(status)}</td><td>${esc(w.notify_tiers.join(", "))}</td><td><button class="btn" data-click="toggleWatch" data-entity-uri="${esc(w.entity_uri)}" data-tip="Stop watching this item. Existing mailbox messages are retained.">Unwatch</button></td></tr>`;
        }).join("");
        byId("watched-items").innerHTML = watched
          ? `<table class="proj-table"><thead><tr><th>Type</th><th>Name</th><th>Status</th><th>Tiers</th><th></th></tr></thead><tbody>${watched}</tbody></table>`
          : `<div class="empty">Nothing watched.</div>`;
        byId("prefs-summary").innerHTML =
          `${hiddenItems.length} hidden item${hiddenItems.length === 1 ? "" : "s"} in ${whose} view${resolvedAs} `
          + `<button class="btn" style="margin-left:8px;padding:2px 8px;font-size:12px" onclick="pollPrefs()" data-tip="Re-query the daemon for ${whose} hidden items.">⟳ Refresh</button>`;
        /** @type {HTMLInputElement} */ (byId("hidden-filter")).value = hiddenFilters.q;
        /** @type {HTMLInputElement} */ (byId("hidden-filter-squad")).checked = hiddenFilters.type.has("squad");
        /** @type {HTMLInputElement} */ (byId("hidden-filter-review")).checked = hiddenFilters.type.has("review");
        const el = byId("hidden-items");
        if (hiddenError) {
          el.innerHTML = `<div class="empty" style="color:var(--failed)">${esc(hiddenError)}</div>`;
          return;
        }
        const filtered = filteredHiddenItems();
        const table = filtered.length
          ? `<table class="proj-table"><thead><tr>`
            + `<th data-tip="Squad or review.">Type</th>`
            + `<th data-tip="Name — hover the id elsewhere in the board to find it again.">Name</th>`
            + `<th data-tip="When you hid this item.">Hidden</th><th></th>`
            + `</tr></thead><tbody>${filtered.map((h) => hiddenItemRowHtml(h)).join("")}</tbody></table>`
          : `<div class="empty">${hiddenItems.length ? "No hidden items match your filter." : "Nothing hidden."}</div>`;
        el.innerHTML = table;
      }
      /**
       * Re-enables one hidden squad/review for the current (or visited,
       * RAL-332) user.
       * @param {string} kind
       * @param {string} id
       * @returns {Promise<void>}
       */
      async function unhidePrefItem(kind, id) {
        const path = kind === "squad" ? `/api/hidden/squads/${encodeURIComponent(id)}` : `/api/hidden/reviews/${encodeURIComponent(id)}`;
        try {
          const r = await fetch(path, { method: "DELETE", headers: prefsUserHeaders() });
          hiddenError = r.ok ? "" : await responseError(r, "unhide failed");
        } catch (e) { hiddenError = "daemon unreachable"; }
        await pollPrefs();
      }

      /**
       * Polls `/api/projects`, re-validates once per tab-load, and re-renders the Projects tab.
       * @returns {Promise<void>}
       */
      async function pollProjects() {
        try {
          const d = await (await fetch("/api/projects")).json();
          byId("conn").className = "dot on";
          markUpdated();
          projects = (d.projects || []).slice().sort((/** @type {ProjectView} */ a, /** @type {ProjectView} */ b) => a.name.localeCompare(b.name));
          if (!projectsValidated) { projectsValidated = true; await validateAllProjects(); }
          if (projectEditState === null) renderProjects();
        } catch (e) { markUnreachable(); }
      }
      /**
       * Forces the Projects tab to re-validate every row on the next poll.
       * @returns {void}
       */
      function refreshProjects() { projectsValidated = false; pollProjects(); }
      /**
       * Re-checks one registered project's on-disk path/vcs validity.
       * @param {string} name
       * @returns {Promise<void>}
       */
      async function validateOneProject(name) {
        try {
          const r = await (await fetch(`/api/projects/${encodeURIComponent(name)}/validate`)).json();
          projectErrors[name] = r.valid ? null : (r.message || "invalid");
        } catch (_) { /* transient fetch failure — leave the prior status in place */ }
      }
      /**
       * Re-validates every registered project in parallel.
       * @returns {Promise<void>}
       */
      async function validateAllProjects() {
        await Promise.all(projects.map((p) => validateOneProject(p.name)));
        if (projectEditState === null) renderProjects();
      }
      /**
       * Formats a project's registration timestamp for display.
       * @param {number} ms
       * @returns {string}
       */
      function fmtProjCreated(ms) {
        return ms ? new Date(ms).toLocaleString([], { month: "short", day: "numeric", year: "numeric", hour: "2-digit", minute: "2-digit" }) : "—";
      }
      /**
       * Enters inline edit mode for a project row.
       * @param {string} name
       * @returns {void}
       */
      function startProjectEdit(name) {
        const p = projects.find((x) => x.name === name);
        if (!p) return;
        projectEditState = name;
        projectEditDraft = { description: p.description, path: p.path, vcs: p.vcs, clone_url: p.clone_url || "" };
        projectSaveError = "";
        projectSaveWarnings = [];
        renderProjects();
      }
      /**
       * Exits a project row's inline edit mode without saving.
       * @returns {void}
       */
      function cancelProjectEdit() {
        projectEditState = null;
        projectEditDraft = {};
        projectSaveError = "";
        renderProjects();
      }
      /**
       * Saves a project row's inline edit draft.
       * @param {string} name
       * @returns {Promise<void>}
       */
      async function saveProjectEdit(name) {
        const draft = projectEditDraft;
        projectSaveError = "";
        projectSaveWarnings = [];
        const trimmedUrl = (draft.clone_url || "").trim();
        const original = projects.find((x) => x.name === name);
        // The field being left blank clears a previously registered clone
        // URL; an omitted `url` would otherwise leave it untouched (RAL-355).
        const urlFields = trimmedUrl
          ? { url: trimmedUrl }
          : original && original.clone_url
            ? { clear_clone_url: true }
            : {};
        const body = { name, description: draft.description, path: draft.path, vcs: draft.vcs, ...urlFields };
        let resp;
        try {
          resp = await post("/api/projects", body);
        } catch (e) {
          projectSaveError = "save failed: daemon unreachable";
          renderProjects();
          return;
        }
        if (!resp.ok) {
          let msg = `save failed (${resp.status})`;
          try { const body = await resp.json(); if (body && body.error && body.error.message) msg = body.error.message; } catch (_) {}
          projectSaveError = msg;
          renderProjects();
          return;
        }
        try { const respBody = await resp.json(); projectSaveWarnings = respBody.warnings || []; } catch (_) {}
        const idx = projects.findIndex((x) => x.name === name);
        if (idx >= 0) projects[idx] = { ...projects[idx], description: draft.description ?? projects[idx].description, path: draft.path ?? projects[idx].path, vcs: draft.vcs ?? projects[idx].vcs, clone_url: trimmedUrl || undefined };
        projectEditState = null;
        projectEditDraft = {};
        renderProjects();
        await validateOneProject(name);
        renderProjects();
      }
      /**
       * Renders one project's row (read-only or inline-edit form).
       * @param {ProjectView} p
       * @returns {string}
       */
      function projectRowHtml(p) {
        const err = projectErrors[p.name];
        const nameCell = err
          ? `<span class="proj-name proj-error-name" data-tip="Validation failed for this project:\n${esc(err)}">⚠ ${esc(p.name)}</span>`
          : `<span class="proj-name">${esc(p.name)}</span>`;
        if (projectEditState === p.name) {
          const d = projectEditDraft;
          return `<tr class="proj-edit-row${err ? " proj-error" : ""}">
              <td>${nameCell}</td>
              <td><input type="text" value="${esc(d.description)}" oninput="projectEditDraft.description=this.value" data-tip="Human-readable description, also used for fuzzy project lookup by name/description." /></td>
              <td><input type="text" value="${esc(d.path)}" oninput="projectEditDraft.path=this.value" data-tip="Absolute filesystem path to the project's git repository root.\nMust exist on-disk and be a git working tree — checked when you click Save." /></td>
              <td><input type="text" value="${esc(d.clone_url)}" placeholder="(none)" oninput="projectEditDraft.clone_url=this.value" data-tip="Clone URL a machine provider uses to provision this project on another machine (RAL-355).\nAccepts any form git accepts (ssh://, git@host:path, https://, ...).\nLeave blank and Save to clear a previously registered URL.\nA blank clone URL will make remote work on this project fail until one is set." /></td>
              <td><select onchange="projectEditDraft.vcs=this.value" data-tip="Version control kind. Only \"git\" is implemented today.">
                    <option value="git" ${d.vcs === "git" ? "selected" : ""}>git</option>
                  </select></td>
              <td style="color:var(--muted)">${fmtProjCreated(p.created_at_ms)}</td>
              <td>
                <div class="row" style="gap:6px;flex-wrap:nowrap">
                  <button class="btn primary" data-click="saveProjectEdit" data-name="${esc(p.name)}" data-tip="Save these changes.\nIf the path does not exist or is not a git repository, the save is rejected and the reason is shown here.">Save</button>
                  <button class="btn" onclick="cancelProjectEdit()" data-tip="Discard your changes and exit edit mode.">Cancel</button>
                </div>
                ${projectSaveError ? `<div class="verr">${esc(projectSaveError)}</div>` : ""}
              </td>
            </tr>`;
        }
        return `<tr class="${err ? "proj-error" : ""}">
            <td>${nameCell}</td>
            <td><span class="proj-field" data-name="${esc(p.name)}" ondblclick="startProjectEdit(this.dataset.name)" data-tip="Double-click to edit the description.">${esc(p.description) || "—"}</span></td>
            <td><span class="proj-field mono" data-name="${esc(p.name)}" ondblclick="startProjectEdit(this.dataset.name)" data-tip="Double-click to edit the path.\nMust exist on-disk and be a git working tree.">${esc(p.path)}</span></td>
            <td><span class="proj-field mono" data-name="${esc(p.name)}" ondblclick="startProjectEdit(this.dataset.name)" data-tip="Double-click to edit. Clone URL a machine provider uses to provision this project on another machine (RAL-355).\nBlank means remote work on this project will fail until one is set.">${p.clone_url ? esc(p.clone_url) : `<span style="color:var(--muted)">(none)</span>`}</span></td>
            <td><span class="proj-field" data-name="${esc(p.name)}" ondblclick="startProjectEdit(this.dataset.name)" data-tip="Double-click to edit. Only \"git\" is implemented today.">${esc(p.vcs)}</span></td>
            <td style="color:var(--muted)">${fmtProjCreated(p.created_at_ms)}</td>
            <td><button class="btn" data-click="openProjectTriageThresholds" data-name="${esc(p.name)}" style="padding:2px 8px;font-size:12px" data-tip="Configure auto-review (Triage) thresholds for this project -- e.g. \"once 4 bug fixes for this project are recorded, make a review\".\nOnly registered Triage types (see the Triage tab) can be picked here.">⋯</button></td>
          </tr>`;
      }
      /**
       * Renders the Projects tab's table.
       * @returns {void}
       */
      function renderProjects() {
        const el = byId("projects");
        byId("proj-summary").innerHTML =
          `${projects.length} registered ${projects.length === 1 ? "project" : "projects"} `
          + `<button class="btn" style="margin-left:8px;padding:2px 8px;font-size:12px" onclick="refreshProjects()" data-tip="Re-query the daemon for the registered project list and re-validate every row's path.\nUseful after fixing a broken path or repository outside of ralphus.">⟳ Refresh</button>`;
        if (!projects.length) { el.innerHTML = `<div class="empty">No registered projects.</div>`; return; }
        const warningBanner = projectSaveWarnings.length
          ? `<div class="verr" style="margin-bottom:8px">${projectSaveWarnings.map((w) => `<span class="vwarn">⚠ ${esc(w)}</span>`).join("<br/>")}</div>`
          : "";
        const head = `<th data-tip="Unique project name, set at registration. Cannot be renamed here.">Name</th>`
          + `<th data-tip="Human-readable description, also used for fuzzy project lookup by name/description.">Description</th>`
          + `<th data-tip="Absolute filesystem path to the project's git repository root.">Path</th>`
          + `<th data-tip="Clone URL a machine provider uses to provision this project on another machine (RAL-355). Blank means remote work on this project will fail until one is set.">Clone URL</th>`
          + `<th data-tip="Version control kind. Only \"git\" is implemented today.">VCS</th>`
          + `<th data-tip="When this project was registered.">Registered</th>`
          + `<th></th>`;
        const rows = projects.map((p) => projectRowHtml(p)).join("");
        el.innerHTML = `${warningBanner}<table class="proj-table"><thead><tr>${head}</tr></thead><tbody>${rows}</tbody></table>`;
      }
      /**
       * Opens the "auto-review thresholds" popup for one project (RAL-318) --
       * lets you configure "every N cells of type X" pool-drain thresholds
       * for that project without leaving the Projects tab. Refreshes the
       * Triage tab's own `triageTypes`/`triagePools` state first so the
       * popup reflects the latest registered types and pool counts.
       *
       * The daemon resolves this registered project's name to its actual
       * Triage pool key server-side (falling back to a normalized worktree
       * path only for cells under an unregistered project), so this popup
       * only ever needs to pass `projectName` through as-is.
       * @param {string} projectName
       * @returns {Promise<void>}
       */
      async function openProjectTriageThresholds(projectName) {
        projectTriageModalProject = projectName;
        projectTriageModalError = "";
        await pollTriage();
        renderProjectTriageModal();
      }
      /**
       * Closes the open Projects-tab Triage-threshold popup.
       * @returns {void}
       */
      function closeProjectTriageModal() {
        projectTriageModalProject = null;
        closeModal();
      }
      // RALPHUS-TRIAGE-POOL-MATCH:BEGIN
      /**
       * Best-effort, browser-side normalization (RAL-374) for comparing a
       * Triage pool key against a project's registered path: slash direction
       * and case folded away. This is a UI hint only -- the daemon
       * (`crate::triage::pool_key_for_path`) remains the single source of
       * truth for what a pool key actually resolves to, since only it can
       * canonicalize a path against the real filesystem (symlinks, drive
       * substitutions, etc). This just needs to be close enough to stop an
       * orphaned pool row from silently vanishing from this popup.
       * @param {string} value
       * @returns {string}
       */
      function normalizeTriagePoolPathHint(value) {
        return String(value || "").trim().replace(/\\/g, "/").replace(/\/+$/, "").toLowerCase();
      }
      /**
       * Splits `triagePools` into the rows that exactly match `projectName`
       * (everything this popup showed before RAL-374) and the rows that
       * don't match by name but whose key looks like a path variant of
       * `projectPath` -- i.e. an orphaned pool this popup would otherwise
       * hide entirely, e.g. a threshold set via the CLI's `<project>`
       * positional argument before the daemon resolved paths server-side,
       * or a pre-fix row the once-per-restart `repair_triage_pool_keys` pass
       * hasn't reached yet.
       * @param {TriagePoolView[]} triagePools
       * @param {string} projectName
       * @param {string|undefined} projectPath
       * @returns {{matched: TriagePoolView[], orphaned: TriagePoolView[]}}
       */
      function partitionTriagePoolsForProject(triagePools, projectName, projectPath) {
        const matched = [];
        const orphaned = [];
        const normalizedProjectPath = projectPath ? normalizeTriagePoolPathHint(projectPath) : "";
        for (const p of triagePools) {
          if (p.project === projectName) {
            matched.push(p);
          } else if (normalizedProjectPath && normalizeTriagePoolPathHint(p.project) === normalizedProjectPath) {
            orphaned.push(p);
          }
        }
        return { matched, orphaned };
      }
      // RALPHUS-TRIAGE-POOL-MATCH:END
      /**
       * Renders the open Projects-tab Triage-threshold popup for
       * `projectTriageModalProject`: one row per Triage type this project
       * already has a pool or threshold for, plus a picker to add a
       * threshold for a type it doesn't have one for yet. Also surfaces
       * (RAL-374) any pool that looks orphaned under a raw path key instead
       * of this project's registered name, so a real accumulating pool
       * never silently reads as "No thresholds configured yet".
       * @returns {void}
       */
      function renderProjectTriageModal() {
        const projectName = projectTriageModalProject;
        if (!projectName) return;
        const thisProject = projects.find((pr) => pr.name === projectName);
        const { matched: rows, orphaned } = partitionTriagePoolsForProject(
          triagePools,
          projectName,
          thisProject ? thisProject.path : undefined,
        );
        const rowsHtml = rows.length
          ? rows.map((p) => {
              const thresholdVal = p.threshold === null || p.threshold === undefined ? "" : String(p.threshold);
              return `<tr>
                  <td><span class="proj-name">${esc(p.triage_type)}</span></td>
                  <td>${p.count}</td>
                  <td><div class="row" style="gap:4px">
                    <input type="number" min="1" step="1" class="mono ptm-threshold-input" id="ptm-threshold-${esc(p.triage_type)}" value="${esc(thresholdVal)}" placeholder="none" style="width:70px;background:var(--bg);border:1px solid var(--border);color:var(--text);border-radius:4px;padding:2px 5px;font-size:12px" data-tip="Draining this pool creates a review once it holds this many ${esc(p.triage_type)} cells for ${esc(projectName)}. Clear the field and press Set to remove the count-based trigger." />
                    <button class="btn" style="padding:2px 8px;font-size:11px" data-click="saveProjectTriageThreshold" data-triage-type="${esc(p.triage_type)}" data-tip="Save this threshold.">Set</button>
                  </div></td>
                </tr>`;
            }).join("")
          : `<tr><td colspan="3" class="empty">No thresholds configured yet.</td></tr>`;
        const usedTypes = new Set(rows.map((p) => p.triage_type));
        const addableTypes = triageTypes.filter((t) => !usedTypes.has(t.name));
        const addSection = addableTypes.length
          ? `<div class="row" style="gap:8px;flex-wrap:wrap;margin-top:12px">
              <select id="ptm-add-type" style="width:180px" data-tip="Which registered Triage type this threshold applies to. Register new types on the Triage tab.">
                ${addableTypes.map((t) => `<option value="${esc(t.name)}">${esc(t.label || t.name)}</option>`).join("")}
              </select>
              <input type="number" min="1" step="1" id="ptm-add-threshold" placeholder="count" style="width:90px" data-tip="Draining this pool creates a review once it holds this many cells of this type for this project." />
              <button class="btn primary" onclick="addProjectTriageThreshold()" data-tip="Configure this pool's count threshold, even before any cell of this type has been pooled yet for this project.">Set</button>
            </div>`
          : `<div class="empty" style="margin-top:12px">Every registered Triage type already has a threshold row above.</div>`;
        const err = projectTriageModalError ? `<div class="verr" style="margin-top:8px">${esc(projectTriageModalError)}</div>` : "";
        const orphanedBanner = orphaned.length
          ? `<div class="verr" style="margin-bottom:8px" data-tip="These rows are stored under a raw filesystem-path key instead of this project's registered name, most likely from a threshold set before the daemon resolved the path to a name, or the CLI's <project> argument given a path. They already count toward the total pooled/threshold for this project's cells -- restart the daemon to merge them into the rows below via the automatic repair pass, or inspect them directly on the general Triage tab.">
              ⚠ ${orphaned.length} orphaned ${orphaned.length === 1 ? "pool" : "pools"} for this project ${orphaned.length === 1 ? "is" : "are"} stored under a raw path key and not shown below:
              <ul style="margin:4px 0 0 18px">
                ${orphaned.map((o) => `<li><code>${esc(o.project)}</code> — ${esc(o.triage_type)}: pooled ${o.count}, threshold ${o.threshold === null || o.threshold === undefined ? "none" : o.threshold}</li>`).join("")}
              </ul>
            </div>`
          : "";
        byId("modal-root").innerHTML = `
          <div class="modal-bg" onclick="if(event.target===this)closeProjectTriageModal()"><div class="modal" style="width:560px;max-width:94vw">
            <h2>Auto-review thresholds — ${esc(projectName)}</h2>
            <div class="k" style="margin-bottom:8px" data-tip="How many Triage-classified cells of a given type must accumulate for this project before the Arbiter automatically drains that pool into a fresh review. This races any cron schedules configured for the same (project, type) key on the Triage tab -- whichever fires first drains the pool.">How many cells of a given Triage type must accumulate for ${esc(projectName)} before a review is created automatically.</div>
            ${orphanedBanner}
            <table class="proj-table"><thead><tr>
              <th>Type</th><th data-tip="Cells currently pooled for this project + type, waiting for this threshold (or a cron schedule) to fire.">Pooled</th><th data-tip="Draining this pool creates a review once it holds this many cells.">Threshold</th>
            </tr></thead><tbody>${rowsHtml}</tbody></table>
            ${addSection}
            ${err}
            <div class="btn-row">
              <button class="btn" onclick="closeProjectTriageModal()" data-tip="Close this popup.">Close</button>
            </div>
          </div></div>`;
      }
      /**
       * Saves one row's threshold value from the open Projects-tab
       * Triage-threshold popup.
       * @param {MouseEvent} e
       * @param {string} triageType
       * @returns {Promise<void>}
       */
      async function saveProjectTriageThreshold(e, triageType) {
        const projectName = projectTriageModalProject;
        if (!projectName) return;
        const input = /** @type {HTMLInputElement|null} */ (document.getElementById(`ptm-threshold-${triageType}`));
        const raw = input ? input.value.trim() : "";
        if (raw !== "" && (!/^\d+$/.test(raw) || Number(raw) < 1)) {
          projectTriageModalError = "Threshold must be a whole number of at least 1, or blank to clear it.";
          renderProjectTriageModal();
          return;
        }
        try {
          const r = await fetch("/api/triage/pools/threshold", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ project: projectName, triage_type: triageType, threshold: raw === "" ? null : Number(raw) }),
          });
          projectTriageModalError = r.ok ? "" : await responseError(r, "set threshold failed");
        } catch (err) { projectTriageModalError = "daemon unreachable"; }
        await pollTriage();
        renderProjectTriageModal();
      }
      /**
       * Adds a threshold row to the open Projects-tab Triage-threshold popup
       * for a Triage type the current project doesn't have one for yet.
       * @returns {Promise<void>}
       */
      async function addProjectTriageThreshold() {
        const projectName = projectTriageModalProject;
        if (!projectName) return;
        const typeSel = /** @type {HTMLSelectElement|null} */ (document.getElementById("ptm-add-type"));
        const thresholdInput = /** @type {HTMLInputElement|null} */ (document.getElementById("ptm-add-threshold"));
        const triageType = typeSel ? typeSel.value : "";
        const raw = thresholdInput ? thresholdInput.value.trim() : "";
        if (!triageType || !/^\d+$/.test(raw) || Number(raw) < 1) {
          projectTriageModalError = "Pick a Triage type and enter a whole number of at least 1.";
          renderProjectTriageModal();
          return;
        }
        try {
          const r = await fetch("/api/triage/pools/threshold", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ project: projectName, triage_type: triageType, threshold: Number(raw) }),
          });
          projectTriageModalError = r.ok ? "" : await responseError(r, "set threshold failed");
        } catch (err) { projectTriageModalError = "daemon unreachable"; }
        await pollTriage();
        renderProjectTriageModal();
      }

      // ---------- Fork registrations popup (RAL-338) ----------
      /**
       * Polls `/api/project-forks` -- every registered fork row, across
       * every project and user.
       * @returns {Promise<void>}
       */
      async function pollProjectForks() {
        try {
          const d = await (await fetch("/api/project-forks")).json();
          projectForks = d.forks || [];
        } catch (_) { /* transient fetch failure -- leave prior rows in place */ }
      }
      /**
       * Opens the "fork registrations" popup for one project (RAL-338) --
       * lets you register/edit/remove per-user (and the project-wide
       * default) fork rows without leaving the Projects tab. Refreshes both
       * the fork list and the user registry (to flag a row naming a user
       * who's since been removed) before rendering.
       * @param {string} projectName
       * @returns {Promise<void>}
       */
      async function openProjectForksModal(projectName) {
        projectForksModalProject = projectName;
        projectForksModalError = "";
        projectForksAddDraft = { user: "", fork_url: "", remote_name: "", fork_owner: "" };
        projectForksEditUser = null;
        projectForksEditDraft = {};
        await Promise.all([pollProjectForks(), pollUsers()]);
        renderProjectForksModal();
      }
      /**
       * Closes the open Projects-tab fork-registrations popup.
       * @returns {void}
       */
      function closeProjectForksModal() {
        projectForksModalProject = null;
        projectForksEditUser = null;
        projectForksEditDraft = {};
        closeModal();
      }
      /**
       * Enters inline edit mode for one fork row inside the open popup.
       * @param {string} user - "" for the project-wide default row.
       * @returns {void}
       */
      function startProjectForkEdit(user) {
        const projectName = projectForksModalProject;
        const row = projectForks.find((f) => f.project === projectName && f.user === user);
        if (!row) return;
        projectForksEditUser = user;
        projectForksEditDraft = { fork_url: row.fork_url, remote_name: row.remote_name, fork_owner: row.fork_owner };
        projectForksModalError = "";
        renderProjectForksModal();
      }
      /**
       * Exits a fork row's inline edit mode without saving.
       * @returns {void}
       */
      function cancelProjectForkEdit() {
        projectForksEditUser = null;
        projectForksEditDraft = {};
        renderProjectForksModal();
      }
      /**
       * Saves a fork row's inline edit draft (`PATCH .../forks[/{user}]`).
       * @param {string} user - "" for the project-wide default row.
       * @returns {Promise<void>}
       */
      async function saveProjectForkEdit(user) {
        const projectName = projectForksModalProject;
        if (!projectName) return;
        const draft = projectForksEditDraft;
        const path = user
          ? `/api/projects/${encodeURIComponent(projectName)}/forks/${encodeURIComponent(user)}`
          : `/api/projects/${encodeURIComponent(projectName)}/forks`;
        try {
          const r = await fetch(path, {
            method: "PATCH",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify(draft),
          });
          if (!r.ok) { projectForksModalError = await responseError(r, "save failed"); renderProjectForksModal(); return; }
        } catch (_) { projectForksModalError = "daemon unreachable"; renderProjectForksModal(); return; }
        projectForksEditUser = null;
        projectForksEditDraft = {};
        await pollProjectForks();
        renderProjectForksModal();
      }
      /**
       * Registers a new fork row from the open popup's "add a fork" form
       * (`POST .../forks`). `user` blank registers the project-wide default
       * row; `remote_name`/`fork_owner` blank let the daemon default them
       * (`fork`/`fork-<user>`, and an auto-derived GitHub owner).
       * @returns {Promise<void>}
       */
      async function addProjectFork() {
        const projectName = projectForksModalProject;
        if (!projectName) return;
        const draft = projectForksAddDraft;
        if (!draft.fork_url || !draft.fork_url.trim()) {
          projectForksModalError = "A fork clone URL is required.";
          renderProjectForksModal();
          return;
        }
        /** @type {{user: string, fork_url: string, remote_name?: string, fork_owner?: string}} */
        const body = { user: draft.user || "", fork_url: draft.fork_url.trim() };
        if (draft.remote_name && draft.remote_name.trim()) body.remote_name = draft.remote_name.trim();
        if (draft.fork_owner && draft.fork_owner.trim()) body.fork_owner = draft.fork_owner.trim();
        try {
          const r = await post(`/api/projects/${encodeURIComponent(projectName)}/forks`, body);
          if (!r.ok) { projectForksModalError = await responseError(r, "add failed"); renderProjectForksModal(); return; }
        } catch (_) { projectForksModalError = "daemon unreachable"; renderProjectForksModal(); return; }
        projectForksAddDraft = { user: "", fork_url: "", remote_name: "", fork_owner: "" };
        projectForksModalError = "";
        await pollProjectForks();
        renderProjectForksModal();
      }
      /**
       * Removes a fork row after confirmation (`DELETE .../forks[/{user}]`).
       * @param {string} user - "" for the project-wide default row.
       * @returns {Promise<void>}
       */
      async function removeProjectFork(user) {
        const projectName = projectForksModalProject;
        if (!projectName) return;
        if (!confirm(`Remove the ${user ? `"${user}"` : "default"} fork registration for "${projectName}"? This cannot be undone.`)) return;
        const path = user
          ? `/api/projects/${encodeURIComponent(projectName)}/forks/${encodeURIComponent(user)}`
          : `/api/projects/${encodeURIComponent(projectName)}/forks`;
        try {
          const r = await del(path);
          if (!r.ok) { projectForksModalError = await responseError(r, "remove failed"); renderProjectForksModal(); return; }
        } catch (_) { projectForksModalError = "daemon unreachable"; renderProjectForksModal(); return; }
        await pollProjectForks();
        renderProjectForksModal();
      }
      /**
       * Renders one fork row (read-only or inline-edit form) inside the
       * open Projects-tab fork-registrations popup. A row naming a `user`
       * no longer present in the user registry is marked, not hidden
       * (RAL-338: fork rows deliberately survive user deletion).
       * @param {ForkRecord} f
       * @returns {string}
       */
      function projectForkRowHtml(f) {
        const isDefault = f.user === "";
        const unregistered = !isDefault && !users.some((u) => u.name === f.user);
        const userCell = isDefault
          ? `<span style="color:var(--muted)" data-tip="Used whenever no user-specific fork row applies for whoever is submitting.">(default)</span>`
          : unregistered
          ? `<span style="color:var(--ignored)" data-tip="This fork is registered for user \"${esc(f.user)}\", who is no longer in the user registry.\nThe row still applies if that name is ever used again -- it was not deleted, only flagged.">⚠ ${esc(f.user)}</span>`
          : esc(f.user);
        if (projectForksEditUser === f.user) {
          const d = projectForksEditDraft;
          return `<tr class="proj-edit-row">
              <td>${userCell}</td>
              <td><input type="text" value="${esc(d.fork_url || "")}" oninput="projectForksEditDraft.fork_url=this.value" data-tip="The fork's own clone URL (any form git accepts)." /></td>
              <td><input type="text" value="${esc(d.remote_name || "")}" oninput="projectForksEditDraft.remote_name=this.value" data-tip="Local git remote name ralphus creates/updates automatically before the first fork-mode push." /></td>
              <td><input type="text" value="${esc(d.fork_owner || "")}" oninput="projectForksEditDraft.fork_owner=this.value" data-tip="GitHub owner/org login the fork lives under (needed to build the cross-repository PR's \"owner:branch\" head). Leave blank for GitLab, which addresses cross-project MRs by numeric project id instead." /></td>
              <td>
                <div class="row" style="gap:6px;flex-wrap:nowrap">
                  <button class="btn primary" data-click="saveProjectForkEdit" data-user="${esc(f.user)}" data-tip="Save these changes.">Save</button>
                  <button class="btn" onclick="cancelProjectForkEdit()" data-tip="Discard your changes and exit edit mode.">Cancel</button>
                </div>
              </td>
            </tr>`;
        }
        return `<tr>
            <td>${userCell}</td>
            <td class="mono">${esc(f.fork_url)}</td>
            <td class="mono">${esc(f.remote_name)}</td>
            <td class="mono">${f.fork_owner ? esc(f.fork_owner) : `<span style="color:var(--muted)">(none)</span>`}</td>
            <td>
              <div class="row" style="gap:6px;flex-wrap:nowrap">
                <button class="btn" data-click="startProjectForkEdit" data-user="${esc(f.user)}" style="padding:2px 8px;font-size:12px" data-tip="Edit this fork registration.">Edit</button>
                <button class="btn" data-click="removeProjectFork" data-user="${esc(f.user)}" style="padding:2px 8px;font-size:12px" data-tip="Remove this fork registration.\nThis cannot be undone.">Remove</button>
              </div>
            </td>
          </tr>`;
      }
      /**
       * Renders the open Projects-tab fork-registrations popup for
       * `projectForksModalProject`: one row per registered fork (the
       * project-wide default plus any per-user rows), plus a form to
       * register a new one.
       * @returns {void}
       */
      function renderProjectForksModal() {
        const projectName = projectForksModalProject;
        if (!projectName) return;
        const rows = projectForks
          .filter((f) => f.project === projectName)
          .slice()
          .sort((a, b) => (a.user === "" ? -1 : b.user === "" ? 1 : a.user.localeCompare(b.user)));
        const rowsHtml = rows.length
          ? rows.map((f) => projectForkRowHtml(f)).join("")
          : `<tr><td colspan="5" class="empty">No forks registered for this project yet.</td></tr>`;
        const d = projectForksAddDraft;
        const err = projectForksModalError ? `<div class="verr" style="margin-top:8px">${esc(projectForksModalError)}</div>` : "";
        byId("modal-root").innerHTML = `
          <div class="modal-bg" onclick="if(event.target===this)closeProjectForksModal()"><div class="modal" style="width:720px;max-width:96vw">
            <h2>Fork registrations — ${esc(projectName)}</h2>
            <div class="k" style="margin-bottom:8px" data-tip="A fork is the writable repository review branches are pushed to when the acting user can't push directly to this project's own repository (RAL-338). The default (project-wide) row applies whenever no user-specific row exists for whoever is submitting.">Registered forks for ${esc(projectName)}, including the project-wide default (used when no user-specific row applies).</div>
            <table class="proj-table"><thead><tr>
              <th data-tip="Which user this fork row applies to. \"(default)\" is the project-wide fallback row.">User</th>
              <th data-tip="The fork's own clone URL.">Fork URL</th>
              <th data-tip="Local git remote name ralphus creates/updates automatically before the first fork-mode push.">Remote name</th>
              <th data-tip="GitHub owner/org login the fork lives under. Blank for GitLab.">Owner</th>
              <th></th>
            </tr></thead><tbody>${rowsHtml}</tbody></table>
            <div class="row" style="gap:8px;flex-wrap:wrap;margin-top:12px">
              <input type="text" placeholder="user (blank = default)" value="${esc(d.user || "")}" oninput="projectForksAddDraft.user=this.value" style="width:140px" data-tip="Leave blank to register/replace the project-wide default row." />
              <input type="text" placeholder="fork clone URL" value="${esc(d.fork_url || "")}" oninput="projectForksAddDraft.fork_url=this.value" style="width:220px" data-tip="Required. The fork's own clone URL (any form git accepts)." />
              <input type="text" placeholder="remote name (optional)" value="${esc(d.remote_name || "")}" oninput="projectForksAddDraft.remote_name=this.value" style="width:150px" data-tip="Defaults to \"fork\" for the default row, else \"fork-<user>\"." />
              <input type="text" placeholder="owner (optional)" value="${esc(d.fork_owner || "")}" oninput="projectForksAddDraft.fork_owner=this.value" style="width:120px" data-tip="GitHub owner/org login. Auto-derived from the URL when it looks like a GitHub host; leave blank for GitLab." />
              <button class="btn primary" onclick="addProjectFork()" data-tip="Register this fork. Replaces any existing row for the same user (or the default row, if user is blank).">Add / Replace</button>
            </div>
            ${err}
            <div class="btn-row">
              <button class="btn" onclick="closeProjectForksModal()" data-tip="Close this popup.">Close</button>
            </div>
          </div></div>`;
      }

      // Jump straight to a task/cell in the Tasks tab and scroll to it.
      /**
       * Switches to the Tasks tab and scrolls to a specific task or cell.
       * @param {string} squadId
       * @param {number} ti
       * @param {number} si
       * @param {"task"|"cell"} [kind]
       * @returns {void}
       */
      function jumpToTask(squadId, ti, si, kind = "cell") {
        const taskIdx = +ti;
        const cellIdx = +si;
        gotoSquadItem(squadId, kind, taskIdx, cellIdx, -1);
        if (!findSquad(squadId)) {
          pendingHash = { tab: "squads", squadId, sel: `${kind}:${taskIdx}:${cellIdx}` };
        }
        const scrollId = kind === "task" ? `tk-${taskIdx}` : `n-${taskIdx}-${cellIdx}`;
        setTimeout(() => document.getElementById(scrollId)?.scrollIntoView({ behavior: "smooth", block: "center" }), 120);
      }

