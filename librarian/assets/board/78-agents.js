      // ---- Agents tab (RAL-473: DB-backed agent profiles + built-in backend command overrides) ----

      /** Every backend a profile's `backend` field may select — mirrors `agent_profiles::PROFILE_BACKENDS`. */
      const AGENT_PROFILE_BACKENDS = ["claude", "claude-code", "codex", "pi", "ollama", "anthropic", "raw"];
      /** The one backend that takes a per-profile executable — mirrors `agent_profiles::RAW_BACKEND`. */
      const AGENT_RAW_BACKEND = "raw";
      /** Backends with no invoked command to override at all — mirrors `agent_profiles::is_native_backend`. */
      const AGENT_NATIVE_BACKENDS = ["claude", "anthropic", "ollama"];
      /** Backends with a global, admin-editable command override — mirrors `agent_profiles::command_overridable_backend`. */
      const AGENT_OVERRIDABLE_BACKENDS = ["claude-code", "codex", "pi"];
      /** Maps an overridable backend name to its Health tab catalog check id (RAL-485) — the same `GET /api/health/report` result both tabs render, so they can never disagree.
       * @type {Record<string, string>} */
      const AGENT_BACKEND_HEALTH_CHECK_ID = { "claude-code": "claude-command", "codex": "codex-command", "pi": "pi-command" };
      /** @type {Map<string, {id: string, status: string, detail: string}>} Keyed by health-catalog check id, from the daemon's cached sweep report (RAL-485). */
      let agentBackendHealth = new Map();

      /**
       * Polls `/api/health/report` (the same cached daemon sweep the Health
       * tab reads) and extracts the built-in backends' health (RAL-485), so
       * the Agents tab's claude-code/codex/pi rows show the identical
       * pass/fail/skip status without triggering a live re-check on every
       * poll tick.
       * @returns {Promise<void>}
       */
      async function pollAgentBackendHealth() {
        try {
          const d = await (await fetch("/api/health/report")).json();
          const checks = /** @type {{id: string, status: string, detail: string}[]} */ (d.checks || []);
          agentBackendHealth = new Map(checks.map((c) => [c.id, c]));
          renderAgentBackendCommands();
        } catch (e) {
          // Supplementary badge only -- a failed fetch here just means no
          // badge renders; pollAgentBackendCommands already surfaces a
          // genuinely unreachable daemon via markUnreachable().
        }
      }

      /**
       * Re-runs the daemon's Free-tier health sweep immediately
       * (`POST /api/health/report/refresh`) and re-polls this tab's backend
       * health, so an explicit Refresh or a command Save/Reset shows an
       * up-to-date status right away instead of waiting for the next hourly
       * sweep (RAL-485).
       * @returns {Promise<void>}
       */
      async function refreshAgentBackendHealth() {
        try {
          await fetch("/api/health/report/refresh", { method: "POST" });
        } catch (e) {
          // Best-effort -- pollAgentBackendHealth below still shows whatever
          // the daemon has cached even if this particular refresh failed.
        }
        await pollAgentBackendHealth();
      }

      /**
       * Polls `/api/agent-profiles` and re-renders the profile list.
       * @returns {Promise<void>}
       */
      async function pollAgentProfiles() {
        try {
          const d = await (await fetch("/api/agent-profiles")).json();
          byId("conn").className = "dot on";
          markUpdated();
          agentProfiles = (d.profiles || []).slice().sort((/** @type {AgentProfileView} */ a, /** @type {AgentProfileView} */ b) => a.name.localeCompare(b.name));
          renderAgentProfiles();
        } catch (e) {
          markUnreachable();
        }
      }

      /**
       * Polls `/api/agent-backend-commands` and re-renders the backend command override table.
       * @returns {Promise<void>}
       */
      async function pollAgentBackendCommands() {
        try {
          const d = await (await fetch("/api/agent-backend-commands")).json();
          byId("conn").className = "dot on";
          markUpdated();
          agentBackendCommands = d.commands || [];
          renderAgentBackendCommands();
        } catch (e) {
          markUnreachable();
        }
      }

      /**
       * Reloads the agent profile list, the backend command overrides, and
       * (RAL-485) forces an immediate re-check of the built-in backends'
       * health rather than waiting for the next hourly sweep -- this is the
       * Agents tab's own explicit "Refresh" action.
       * @returns {Promise<void>}
       */
      async function refreshAgentsTab() {
        await Promise.all([pollAgentBackendCommands(), pollAgentProfiles(), refreshAgentBackendHealth()]);
      }

      /**
       * Refreshes the Agents tab's header summary line.
       * @returns {void}
       */
      function renderAgentsSummary() {
        const el = byId("agent-profile-summary");
        if (!el) return;
        const overrideCount = agentBackendCommands.length;
        el.innerHTML =
          `${agentProfiles.length} agent ${agentProfiles.length === 1 ? "profile" : "profiles"}, `
          + `${overrideCount} backend command ${overrideCount === 1 ? "override" : "overrides"} `
          + `<button class="btn" style="margin-left:8px;padding:2px 8px;font-size:12px" onclick="refreshAgentsTab()" data-tip="Re-query the daemon for agent profiles and backend command overrides.">⟳ Refresh</button>`;
      }

      /**
       * Renders a built-in backend's health badge (RAL-485), sourced from
       * the same `GET /api/health/report` cache the Health tab renders --
       * so the two tabs can never disagree about a claude-code/codex/pi
       * row's pass/fail/skip status. A missing entry (no sweep has
       * completed yet) shows a muted "not checked" placeholder rather than
       * a false pass or fail.
       * @param {string} backend
       * @returns {string}
       */
      function agentBackendHealthBadgeHtml(backend) {
        const checkId = AGENT_BACKEND_HEALTH_CHECK_ID[backend];
        const check = checkId ? agentBackendHealth.get(checkId) : undefined;
        if (!check) {
          return `<span class="dot" style="background:${cvar("--muted")}"></span> <span style="color:var(--muted)" data-tip="No health sweep has completed yet for this backend.">not checked</span>`;
        }
        return `<span data-tip="${esc(check.detail)}">${healthStatusBadge(check.status)}</span>`;
      }

      /**
       * Renders one built-in backend's command-override row: a read-only
       * summary, or (once editing starts) an inline edit form with its
       * blast-radius list of affected profiles.
       * @param {string} backend
       * @returns {string}
       */
      function agentBackendCommandRowHtml(backend) {
        const override = agentBackendCommands.find((c) => c.backend === backend);
        if (agentBackendCommandEditing !== backend) {
          return `<tr>
            <td class="mono">${esc(backend)}</td>
            <td>${agentBackendHealthBadgeHtml(backend)}</td>
            <td>${override ? `<span class="mono">${esc(override.command)}</span>` : `<span style="color:var(--muted)">(built-in default)</span>`}</td>
            <td style="color:var(--muted)">${override ? fmtProjCreated(override.updated_at_ms) : "—"}</td>
            <td>
              <button class="btn" onclick="startAgentBackendCommandEdit('${backend}')" data-tip="Edit ${esc(backend)}'s invoked command.\nThis is a global override -- every agent profile selecting the ${esc(backend)} backend picks it up on its next run, no daemon restart needed. The affected-profile list is shown once you start editing.">Edit</button>
              ${override ? `<button class="btn" onclick="resetAgentBackendCommand('${backend}')" data-tip="Reset ${esc(backend)}'s command back to its built-in default.\nEvery agent profile using ${esc(backend)} picks up the default on its next run.\nThis cannot be undone -- you would have to re-enter the override.">Reset</button>` : ""}
            </td>
          </tr>`;
        }
        const blast = agentBackendCommandBlastRadius;
        const blastHtml = blast
          ? `<div style="color:var(--muted);font-size:11px;margin-top:4px">${blast.length ? `Affects ${blast.length} profile${blast.length === 1 ? "" : "s"}: ${blast.map((p) => esc(p.name)).join(", ")}` : "Affects no profiles yet."}</div>`
          : `<div style="color:var(--muted);font-size:11px;margin-top:4px">Loading affected profiles…</div>`;
        return `<tr>
          <td class="mono">${esc(backend)}</td>
          <td>${agentBackendHealthBadgeHtml(backend)}</td>
          <td colspan="2">
            <input id="agent-backend-command-input" type="text" class="mono" style="width:100%" value="${esc(override ? override.command : "")}" placeholder="(built-in default command)" data-tip="The command invoked for every agent profile selecting the ${esc(backend)} backend. Applies globally on Save." />
            ${blastHtml}
          </td>
          <td>
            <button class="btn primary" onclick="saveAgentBackendCommand('${backend}')" data-tip="Save this command override for ${esc(backend)}. Applies immediately to every profile using this backend.">Save</button>
            <button class="btn" onclick="cancelAgentBackendCommandEdit()" data-tip="Discard this edit without saving.">Cancel</button>
          </td>
        </tr>`;
      }

      /**
       * Enters inline edit mode for a backend's command override and loads its blast-radius list.
       * @param {string} backend
       * @returns {void}
       */
      function startAgentBackendCommandEdit(backend) {
        agentBackendCommandEditing = backend;
        agentBackendCommandBlastRadius = null;
        agentBackendCommandsError = "";
        renderAgentBackendCommands();
        fetch(`/api/agent-backend-commands/${encodeURIComponent(backend)}/profiles`)
          .then((r) => (r.ok ? r.json() : Promise.reject(new Error(String(r.status)))))
          .then((/** @type {{profiles: AgentProfileView[]}} */ d) => {
            if (agentBackendCommandEditing !== backend) return;
            agentBackendCommandBlastRadius = d.profiles || [];
            renderAgentBackendCommands();
          })
          .catch(() => {});
      }

      /**
       * Exits a backend command's inline edit mode without saving.
       * @returns {void}
       */
      function cancelAgentBackendCommandEdit() {
        agentBackendCommandEditing = null;
        agentBackendCommandBlastRadius = null;
        renderAgentBackendCommands();
      }

      /**
       * Saves a backend's command override from the inline edit form.
       * @param {string} backend
       * @returns {Promise<void>}
       */
      async function saveAgentBackendCommand(backend) {
        const input = /** @type {HTMLInputElement} */ (byId("agent-backend-command-input"));
        const command = input.value.trim();
        if (!command) {
          agentBackendCommandsError = "Command is required.";
          renderAgentBackendCommands();
          return;
        }
        try {
          const r = await fetch(`/api/agent-backend-commands/${encodeURIComponent(backend)}`, {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ command }),
          });
          agentBackendCommandsError = r.ok ? "" : await responseError(r, "save failed");
        } catch (e) {
          agentBackendCommandsError = "daemon unreachable";
        }
        agentBackendCommandEditing = null;
        agentBackendCommandBlastRadius = null;
        await Promise.all([pollAgentBackendCommands(), refreshAgentBackendHealth()]);
      }

      /**
       * Resets a backend's command override to its built-in default.
       * @param {string} backend
       * @returns {Promise<void>}
       */
      async function resetAgentBackendCommand(backend) {
        if (!confirm(`Reset "${backend}"'s command override to the built-in default?\nThis cannot be undone.`)) return;
        try {
          const r = await fetch(`/api/agent-backend-commands/${encodeURIComponent(backend)}`, { method: "DELETE" });
          agentBackendCommandsError = r.ok ? "" : await responseError(r, "reset failed");
        } catch (e) {
          agentBackendCommandsError = "daemon unreachable";
        }
        await Promise.all([pollAgentBackendCommands(), refreshAgentBackendHealth()]);
      }

      /**
       * Renders the built-in backend command-override table (`#agent-backend-commands`).
       * Always shows all three overridable backends regardless of which ones
       * the daemon reports an override for -- `GET /api/agent-backend-commands`
       * only returns rows that exist; an absent backend still runs its
       * compiled-in default, shown here as "(built-in default)".
       * @returns {void}
       */
      function renderAgentBackendCommands() {
        renderAgentsSummary();
        const el = byId("agent-backend-commands");
        const err = agentBackendCommandsError
          ? `<div class="empty" style="color:var(--failed);margin-bottom:8px">${esc(agentBackendCommandsError)}</div>`
          : "";
        const table = `<table class="proj-table"><thead><tr>
            <th data-tip="Built-in backend name. Only claude-code, codex, and pi have a global, admin-editable invoked-command override -- claude/anthropic/ollama are native backends with nothing to override, and raw takes a per-profile executable instead of a shared command.">Backend</th>
            <th data-tip="This backend's health, from the same daemon sweep the Health tab shows (RAL-485). A single-executable-token command is checked for disk/PATH accessibility and executability (Pi also version-checked); a complex, shell-routed command is shown as \"skip\" without being executed.">Health</th>
            <th data-tip="The command invoked for every agent profile selecting this backend. \"(built-in default)\" means no override is stored.">Invoked command</th>
            <th data-tip="When this override was last saved.">Updated</th>
            <th></th>
          </tr></thead><tbody>${AGENT_OVERRIDABLE_BACKENDS.map((b) => agentBackendCommandRowHtml(b)).join("")}</tbody></table>`;
        el.innerHTML = `<div style="margin-bottom:8px" data-tip="Global, per-backend invoked-command overrides (RAL-473).\nEditing one here affects every agent profile that selects this backend, immediately and without a daemon restart -- check each row's blast-radius list before saving.">Built-in backend command overrides</div>${err}${table}`;
      }

      /**
       * Renders one agent profile's read-only table row.
       * @param {AgentProfileView} p
       * @returns {string}
       */
      function agentProfileRowHtml(p) {
        const envSummary = p.env.length ? `${p.env.length} row${p.env.length === 1 ? "" : "s"}` : `<span style="color:var(--muted)">none</span>`;
        return `<tr>
            <td><span class="proj-name">${esc(p.name)}</span></td>
            <td class="mono">${esc(p.backend)}</td>
            <td>${p.model ? esc(p.model) : `<span style="color:var(--muted)">—</span>`}</td>
            <td>${envSummary}</td>
            <td style="color:var(--muted)">${fmtProjCreated(p.updated_at_ms)}</td>
            <td>
              <button class="btn" data-click="editAgentProfile" data-name="${esc(p.name)}" data-tip="Edit this agent profile's backend, model, and environment table.\nNothing is sent to the daemon until you click Save Agent.">Edit</button>
              <button class="btn" data-click="deleteAgentProfile" data-name="${esc(p.name)}" data-tip="Delete this agent profile.\nBlocked while any stored squad or review still references it by name -- you'll be shown what's referencing it and offered a forced delete.\nThis cannot be undone.">Delete</button>
            </td>
          </tr>`;
      }

      /**
       * Renders the "still referenced" banner for a blocked delete, with a
       * Force Delete / Cancel pair, or an empty string when no delete is blocked.
       * @returns {string}
       */
      function agentProfileDeleteBlockedHtml() {
        if (!agentProfileDeleteBlocked || !agentProfileDeletePendingName) return "";
        const refs = agentProfileDeleteBlocked;
        const name = agentProfileDeletePendingName;
        const parts = [];
        if (refs.squad_ids.length) parts.push(`${refs.squad_ids.length} squad${refs.squad_ids.length === 1 ? "" : "s"} (${refs.squad_ids.map((id) => esc(id)).join(", ")})`);
        if (refs.guardian_ids.length) parts.push(`${refs.guardian_ids.length} review${refs.guardian_ids.length === 1 ? "" : "s"} (${refs.guardian_ids.map((id) => esc(id)).join(", ")})`);
        return `<div class="empty" style="color:var(--failed);margin-bottom:8px;text-align:left">
            "${esc(name)}" is still referenced by ${parts.length ? parts.join(" and ") : "other stored records"}.
            <button class="btn" data-click="forceDeleteAgentProfile" data-name="${esc(name)}" data-tip="Delete \"${esc(name)}\" anyway, despite the references above.\nThis cannot be undone.">Force Delete</button>
            <button class="btn" onclick="cancelForceDeleteAgentProfile()" data-tip="Cancel this delete and keep \"${esc(name)}\".">Cancel</button>
          </div>`;
      }

      /**
       * Opens the "+ New Agent" create form.
       * @returns {void}
       */
      function openNewAgentProfileForm() {
        agentProfileForm = { editingName: null, name: "", backend: AGENT_PROFILE_BACKENDS[0], executable: "", model: "", env: [], thinkingCapable: null };
        agentProfileFormError = "";
        renderAgentProfiles();
      }

      /**
       * Opens the edit form pre-filled from an existing profile.
       * @param {string} name
       * @returns {void}
       */
      function editAgentProfile(name) {
        const p = agentProfiles.find((x) => x.name === name);
        if (!p) return;
        agentProfileForm = {
          editingName: p.name,
          name: p.name,
          backend: p.backend,
          executable: p.executable || "",
          model: p.model || "",
          env: p.env.map((e) => ({ key: e.key, kind: e.kind, value: e.value })),
          thinkingCapable: p.thinking_capable,
        };
        agentProfileFormError = "";
        renderAgentProfiles();
      }

      /**
       * Closes the create/edit form without saving.
       * @returns {void}
       */
      function cancelAgentProfileForm() {
        agentProfileForm = null;
        agentProfileFormError = "";
        renderAgentProfiles();
      }

      /**
       * Adds a blank env row to the open profile form.
       * @returns {void}
       */
      function addAgentProfileEnvRow() {
        if (!agentProfileForm) return;
        agentProfileForm.env.push({ key: "", kind: "set", value: "" });
        renderAgentProfiles();
      }

      /**
       * Removes an env row from the open profile form.
       * @param {number} index
       * @returns {void}
       */
      function removeAgentProfileEnvRow(index) {
        if (!agentProfileForm || !agentProfileForm.env[index]) return;
        agentProfileForm.env.splice(index, 1);
        renderAgentProfiles();
      }

      /**
       * Stages an env row's key without re-rendering, preserving input focus.
       * @param {number} index
       * @param {string} value
       * @returns {void}
       */
      function onAgentProfileEnvKeyInput(index, value) {
        if (!agentProfileForm || !agentProfileForm.env[index]) return;
        agentProfileForm.env[index].key = value;
      }

      /**
       * Stages an env row's value without re-rendering, preserving input focus.
       * @param {number} index
       * @param {string} value
       * @returns {void}
       */
      function onAgentProfileEnvValueInput(index, value) {
        if (!agentProfileForm || !agentProfileForm.env[index]) return;
        agentProfileForm.env[index].value = value;
      }

      /**
       * Stages an env row's Set/Link kind and re-renders (the value placeholder differs by kind).
       * @param {number} index
       * @param {string} value
       * @returns {void}
       */
      function onAgentProfileEnvKindChange(index, value) {
        if (!agentProfileForm || !agentProfileForm.env[index]) return;
        agentProfileForm.env[index].kind = value === "link" ? "link" : "set";
        renderAgentProfiles();
      }

      /**
       * Stages the profile form's name field without re-rendering.
       * @param {string} value
       * @returns {void}
       */
      function onAgentProfileNameInput(value) {
        if (agentProfileForm) agentProfileForm.name = value;
      }

      /**
       * Stages the profile form's executable field without re-rendering.
       * @param {string} value
       * @returns {void}
       */
      function onAgentProfileExecutableInput(value) {
        if (agentProfileForm) agentProfileForm.executable = value;
      }

      /**
       * Stages the profile form's model field without re-rendering.
       * @param {string} value
       * @returns {void}
       */
      function onAgentProfileModelInput(value) {
        if (agentProfileForm) agentProfileForm.model = value;
      }

      /**
       * Stages the profile form's backend and re-renders (the executable field's visibility depends on it).
       * @param {string} value
       * @returns {void}
       */
      function onAgentProfileBackendChange(value) {
        if (!agentProfileForm) return;
        agentProfileForm.backend = value;
        renderAgentProfiles();
      }

      /**
       * Stages the profile form's thinking-capability override (RAL-516) and re-renders.
       * @param {string} value - "inherit" | "on" | "off"
       * @returns {void}
       */
      function onAgentProfileThinkingCapableChange(value) {
        if (!agentProfileForm) return;
        agentProfileForm.thinkingCapable = value === "on" ? true : value === "off" ? false : null;
        renderAgentProfiles();
      }

      /**
       * Renders the staged create/edit profile form, or an empty string when it's closed.
       * @returns {string}
       */
      function renderAgentProfileForm() {
        const form = agentProfileForm;
        if (!form) return "";
        const isRaw = form.backend === AGENT_RAW_BACKEND;
        const isNative = AGENT_NATIVE_BACKENDS.includes(form.backend);
        const executableField = isRaw
          ? `<div class="row" style="gap:8px;margin-top:8px">
              <label style="width:110px" data-tip="Executable invoked directly for this profile.\nOnly the \"raw\" backend takes a per-profile executable -- every other backend's command is a global override edited above.">Executable</label>
              <input type="text" class="mono" style="flex:1" value="${esc(form.executable)}" oninput="onAgentProfileExecutableInput(this.value)" placeholder="/path/to/agent-cli" data-tip="Required for the raw backend -- the literal command line invoked for this profile." />
            </div>`
          : "";
        const executableNote = !isRaw
          ? `<div style="color:var(--muted);font-size:11px;margin-top:4px">${isNative ? "Native backend -- no invoked command to override." : `Invoked command is a global override for the "${esc(form.backend)}" backend -- edit it in the table above, not per-profile.`}</div>`
          : "";
        const envRows = form.env.map((row, i) => `<div class="row" style="gap:6px;margin-top:6px;align-items:center">
            <input type="text" class="mono" style="width:200px" placeholder="KEY" value="${esc(row.key)}" oninput="onAgentProfileEnvKeyInput(${i},this.value)" data-tip="Environment variable name." />
            <select onchange="onAgentProfileEnvKindChange(${i},this.value)" data-tip="Set -- the literal value below.\nLink -- resolve another env var by name (this profile's own other entries first, then the daemon process environment). A Link's resolved value is never returned by the API.">
              <option value="set" ${row.kind === "set" ? "selected" : ""}>Set</option>
              <option value="link" ${row.kind === "link" ? "selected" : ""}>Link</option>
            </select>
            <input type="text" class="mono" style="flex:1" placeholder="${row.kind === "link" ? "other env var name" : "value"}" value="${esc(row.value)}" oninput="onAgentProfileEnvValueInput(${i},this.value)" data-tip="${row.kind === "link" ? "The name of another environment variable to resolve this key from." : "The literal value for this key."}" />
            <button class="btn" style="padding:2px 8px;font-size:11px" data-click="removeAgentProfileEnvRow" data-i="${i}" data-tip="Remove this row. Applies on Save Agent.">✕</button>
          </div>`).join("");
        const backendDefaultThinking = form.backend === "pi";
        const thinkingCapableValue = form.thinkingCapable === true ? "on" : form.thinkingCapable === false ? "off" : "inherit";
        const thinkingCapableField = `<div class="row" style="gap:8px;margin-top:8px">
              <label style="width:110px" data-tip="RAL-516: whether a Live View pane running this profile shows the \"Show Thinking\" checkbox at all.\nInherit uses this backend's own built-in default; Always/Never explicitly overrides it.">Show Thinking</label>
              <select style="flex:1" onchange="onAgentProfileThinkingCapableChange(this.value)" data-tip="Use when a custom command wraps a backend that actually emits thinking output but isn't recognized by name (or vice versa) -- this only controls whether the checkbox appears, not what it does once shown.">
                <option value="inherit" ${thinkingCapableValue === "inherit" ? "selected" : ""}>Inherit (${backendDefaultThinking ? "shows checkbox" : "no checkbox"} for "${esc(form.backend)}")</option>
                <option value="on" ${thinkingCapableValue === "on" ? "selected" : ""}>Always show checkbox</option>
                <option value="off" ${thinkingCapableValue === "off" ? "selected" : ""}>Never show checkbox</option>
              </select>
            </div>`;
        const err = agentProfileFormError ? `<div class="verr" style="margin-top:8px">${esc(agentProfileFormError)}</div>` : "";
        return `<div style="margin-top:16px;padding-top:12px;border-top:1px solid var(--border)">
            <div style="margin-bottom:8px" data-tip="${form.editingName ? `Editing "${esc(form.editingName)}".` : "Create a new agent profile."} Nothing is sent to the daemon until Save Agent.">${form.editingName ? `Edit agent: ${esc(form.editingName)}` : "New agent"}</div>
            <div class="row" style="gap:8px">
              <label style="width:110px" data-tip="Unique profile name, referenced from a task/cell's agent field.${form.editingName ? " Cannot be changed once created." : ""}">Name</label>
              <input type="text" class="mono" style="flex:1" value="${esc(form.name)}" ${form.editingName ? "disabled" : ""} oninput="onAgentProfileNameInput(this.value)" placeholder="my-agent" data-tip="Unique profile name.${form.editingName ? " Cannot be changed once created -- delete and recreate to rename." : ""}" />
            </div>
            <div class="row" style="gap:8px;margin-top:8px">
              <label style="width:110px" data-tip="Which backend this profile invokes. claude-code/codex/pi's command is a global override edited above; claude/anthropic/ollama are native; raw takes a per-profile executable.">Backend</label>
              <select style="flex:1" onchange="onAgentProfileBackendChange(this.value)" data-tip="Backend this profile invokes.">
                ${AGENT_PROFILE_BACKENDS.map((b) => `<option value="${esc(b)}" ${form.backend === b ? "selected" : ""}>${esc(b)}</option>`).join("")}
              </select>
            </div>
            ${executableField}
            ${executableNote}
            <div class="row" style="gap:8px;margin-top:8px">
              <label style="width:110px" data-tip="Default model for this profile, if the backend takes one. Leave blank to use the backend's own default.">Model</label>
              <input type="text" class="mono" style="flex:1" value="${esc(form.model)}" oninput="onAgentProfileModelInput(this.value)" placeholder="(backend default)" data-tip="Optional default model string passed to the backend." />
            </div>
            ${thinkingCapableField}
            <div style="margin-top:12px">
              <div style="font-size:12px;color:var(--muted);margin-bottom:4px" data-tip="Ordered Set/Link environment table for this profile. Set stores a literal value; Link resolves another variable by name at cell-start time and is never returned resolved by the API.">Environment</div>
              ${envRows || `<div style="color:var(--muted);font-size:12px">No environment rows yet.</div>`}
              <button class="btn" style="padding:2px 8px;font-size:11px;margin-top:6px" onclick="addAgentProfileEnvRow()" data-tip="Add a new environment-variable row. Applies on Save Agent.">+ Add row</button>
            </div>
            ${err}
            <div class="row" style="gap:8px;margin-top:12px">
              <button class="btn primary" onclick="saveAgentProfile()" data-tip="Save this agent profile to the daemon.${form.editingName ? " Existing squads/reviews already referencing it pick up the change on their next run." : ""}">Save Agent</button>
              <button class="btn" onclick="cancelAgentProfileForm()" data-tip="Discard this form without saving.">Cancel</button>
            </div>
          </div>`;
      }

      /**
       * Saves the open create/edit form via POST (create) or PATCH (update).
       * @returns {Promise<void>}
       */
      async function saveAgentProfile() {
        const form = agentProfileForm;
        if (!form) return;
        const name = form.name.trim();
        if (!name) {
          agentProfileFormError = "Name is required.";
          renderAgentProfiles();
          return;
        }
        const env = form.env
          .map((e) => ({ key: e.key.trim(), kind: e.kind, value: e.value }))
          .filter((e) => e.key !== "");
        const body = {
          backend: form.backend,
          executable: form.backend === AGENT_RAW_BACKEND && form.executable.trim() ? form.executable.trim() : null,
          model: form.model.trim() || null,
          env,
          thinking_capable: form.thinkingCapable,
        };
        try {
          const r = form.editingName
            ? await fetch(`/api/agent-profiles/${encodeURIComponent(form.editingName)}`, {
                method: "PATCH",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify(body),
              })
            : await fetch("/api/agent-profiles", {
                method: "POST",
                headers: { "Content-Type": "application/json" },
                body: JSON.stringify({ ...body, name }),
              });
          if (r.ok) {
            agentProfileForm = null;
            agentProfileFormError = "";
            await pollAgentProfiles();
            return;
          }
          agentProfileFormError = await responseError(r, "save failed");
        } catch (e) {
          agentProfileFormError = "daemon unreachable";
        }
        renderAgentProfiles();
      }

      /**
       * Deletes an agent profile; on a 409 "in_use" response, stages the
       * reported references for a forced-delete confirmation instead of
       * failing silently (`responseError` alone can't surface `body.references`).
       * @param {string} name
       * @returns {Promise<void>}
       */
      async function deleteAgentProfile(name) {
        if (!confirm(`Delete agent profile "${name}"?`)) return;
        agentProfileDeleteBlocked = null;
        agentProfileDeletePendingName = null;
        agentProfilesError = "";
        try {
          const r = await fetch(`/api/agent-profiles/${encodeURIComponent(name)}`, { method: "DELETE" });
          if (r.ok) {
            await pollAgentProfiles();
            return;
          }
          const body = await r.json().catch(() => ({}));
          if (r.status === 409 && body.error && body.error.code === "in_use") {
            agentProfileDeleteBlocked = body.references || { squad_ids: [], guardian_ids: [] };
            agentProfileDeletePendingName = name;
            renderAgentProfiles();
            return;
          }
          agentProfilesError = (body.error && body.error.message) || `delete failed (HTTP ${r.status})`;
        } catch (e) {
          agentProfilesError = "daemon unreachable";
        }
        renderAgentProfiles();
      }

      /**
       * Confirms a previously-blocked delete, retrying with `?force=true`.
       * @param {string} name
       * @returns {Promise<void>}
       */
      async function forceDeleteAgentProfile(name) {
        if (!confirm(`Force-delete agent profile "${name}" even though it is still referenced?\nThis cannot be undone.`)) return;
        try {
          const r = await fetch(`/api/agent-profiles/${encodeURIComponent(name)}?force=true`, { method: "DELETE" });
          agentProfilesError = r.ok ? "" : await responseError(r, "delete failed");
        } catch (e) {
          agentProfilesError = "daemon unreachable";
        }
        agentProfileDeleteBlocked = null;
        agentProfileDeletePendingName = null;
        await pollAgentProfiles();
      }

      /**
       * Cancels a blocked delete's confirmation without deleting.
       * @returns {void}
       */
      function cancelForceDeleteAgentProfile() {
        agentProfileDeleteBlocked = null;
        agentProfileDeletePendingName = null;
        renderAgentProfiles();
      }

      /**
       * Renders the Agents tab's profile list, delete-blocked banner, and
       * create/edit form into `#agents`.
       * @returns {void}
       */
      function renderAgentProfiles() {
        renderAgentsSummary();
        const el = byId("agents");
        const err = agentProfilesError
          ? `<div class="empty" style="color:var(--failed);margin-bottom:8px">${esc(agentProfilesError)}</div>`
          : "";
        const blocked = agentProfileDeleteBlockedHtml();
        const table = agentProfiles.length
          ? `<table class="proj-table"><thead><tr>
              <th data-tip="Unique profile name, referenced from a task/cell's agent field.">Name</th>
              <th data-tip="Backend this profile invokes.">Backend</th>
              <th data-tip="Default model, if any.">Model</th>
              <th data-tip="Number of Set/Link rows in this profile's environment table.">Env</th>
              <th data-tip="When this profile was last saved.">Updated</th>
              <th></th>
            </tr></thead><tbody>${agentProfiles.map((p) => agentProfileRowHtml(p)).join("")}</tbody></table>`
          : `<div class="empty">No agent profiles yet.</div>`;
        const addButton = agentProfileForm
          ? ""
          : `<button class="btn primary" style="margin-bottom:8px" onclick="openNewAgentProfileForm()" data-tip="Create a new agent profile.\nNothing is sent to the daemon until you click Save Agent.">+ New Agent</button>`;
        el.innerHTML = `${addButton}${err}${blocked}${table}${renderAgentProfileForm()}`;
      }
