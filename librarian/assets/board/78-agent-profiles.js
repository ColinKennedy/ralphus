      // ---- Agent Profiles tab (RAL-460) ----
      // The daemon-managed registry a task's `agent = "..."` resolves
      // against, replacing the old `[agent.profiles.*]` TOML config
      // entirely. "claude-code" and "codex" are permanent, locked rows
      // (their name/backend never change and they can never be removed) --
      // every other row is a fully custom profile an admin manages here.
      //
      // Editing a stored profile never sees a literal env value's real
      // content (`GET /api/agent-profiles` redacts it) -- this placeholder
      // must match the daemon's own `crate::redact::REDACTED` constant
      // exactly, since re-submitting it unchanged on a literal row tells the
      // daemon "leave this one alone" (`server.rs::register_agent_profile`).
      const AGENT_PROFILE_REDACTED_PLACEHOLDER = "<redacted>";

      /**
       * Polls `/api/agent-profiles` and re-renders the Agent Profiles tab.
       * @returns {Promise<void>}
       */
      async function pollAgentProfiles() {
        try {
          const d = await (await fetch("/api/agent-profiles")).json();
          byId("conn").className = "dot on";
          markUpdated();
          agentProfiles = (d.profiles || []).slice().sort(
            (/** @type {AgentProfileView} */ a, /** @type {AgentProfileView} */ b) =>
              Number(b.locked) - Number(a.locked) || a.name.localeCompare(b.name));
          agentProfileBackends = d.available_backends || [];
          renderAgentProfiles();
        } catch (e) { markUnreachable(); }
      }

      /**
       * Whether `backend` is a "native" backend ralphus talks to directly --
       * no external CLI to fork, so `executable` is never meaningful for it.
       * Mirrors `crate::agent_profiles::is_native_backend`.
       * @param {string} backend
       * @returns {boolean}
       */
      function agentProfileBackendIsNative(backend) {
        return backend === "claude" || backend === "anthropic" || backend === "ollama";
      }

      /**
       * Renders one stored profile's read-only table row.
       * @param {AgentProfileView} p
       * @returns {string}
       */
      function agentProfileRowHtml(p) {
        const model = p.default_model
          ? `<span class="mono">${esc(p.default_model)}</span>`
          : `<span style="color:var(--muted)">—</span>`;
        const executable = p.executable
          ? `<span class="mono">${esc(p.executable)}</span>`
          : `<span style="color:var(--muted)">—</span>`;
        const envCount = p.env.length
          ? `${p.env.length} var${p.env.length === 1 ? "" : "s"}`
          : `<span style="color:var(--muted)">none</span>`;
        const lockedBadge = p.locked
          ? ` <span style="color:var(--muted)" data-tip="This is a permanent built-in-backend row.\nIts name/backend can never change and it can never be removed -- only its executable command can be changed.">🔒 built-in</span>`
          : "";
        const actions = p.locked
          ? `<button class="btn" data-click="startSetLockedExecutable" data-name="${esc(p.name)}" data-tip="Change the command this built-in backend runs.\nWho/when: the default (&quot;claude&quot;/&quot;codex&quot;) isn't on PATH under that name, or you want every cell using this backend to run a different binary/wrapper.\nEverything else about this row is fixed.">✎ Change executable</button>`
          : `<button class="btn" data-click="startEditAgentProfile" data-name="${esc(p.name)}" data-tip="Edit this custom agent profile's backend, default model, and environment variables.">✎ Edit</button>` +
            `<button class="btn" data-click="removeAgentProfile" data-name="${esc(p.name)}" data-tip="Remove this custom agent profile.\nAny task submitted afterwards naming this profile is rejected at submit; already-submitted squads are unaffected, since they resolved their agent when they were submitted.\nThis cannot be undone — you would have to register it again.">✕ Remove</button>`;
        return `<tr>
            <td><span class="proj-name">${esc(p.name)}</span>${lockedBadge}</td>
            <td class="mono">${esc(p.backend)}</td>
            <td>${model}</td>
            <td>${executable}</td>
            <td>${envCount}</td>
            <td>${actions}</td>
          </tr>`;
      }

      /**
       * Renders the Agent Profiles tab: the table, the locked-row executable
       * inline edit (when open), the custom-profile add/edit form (when
       * open), and the "+ Add profile" affordance.
       * @returns {void}
       */
      function renderAgentProfiles() {
        byId("agent-profile-summary").innerHTML =
          `${agentProfiles.length} profile${agentProfiles.length === 1 ? "" : "s"} `
          + `<button class="btn" style="margin-left:8px;padding:2px 8px;font-size:12px" onclick="pollAgentProfiles()" data-tip="Re-query the daemon for the registered agent profile list.">⟳ Refresh</button>`;
        const el = byId("agent-profiles");
        const err = agentProfileError
          ? `<div class="empty" style="color:var(--failed);margin-bottom:8px">${esc(agentProfileError)}</div>`
          : "";
        const rows = agentProfiles.map((p) =>
          p.name === agentProfileExecutableEditName
            ? agentProfileExecutableEditRowHtml(p)
            : p.name === agentProfileEditName
              ? agentProfileEditRowHtml()
              : agentProfileRowHtml(p));
        const table = `<table class="proj-table"><thead><tr>`
          + `<th data-tip="The profile's name. Matched exactly against a task's agent = &quot;...&quot; field.\n&quot;claude-code&quot; and &quot;codex&quot; are permanent built-in rows.">Name</th>`
          + `<th data-tip="Which backend this profile runs under. Fixed for a built-in row; chosen from the available backend list for a custom profile.">Backend</th>`
          + `<th data-tip="Default model applied when a task's cell/task/review doesn't set its own model. An authored model always wins over this.">Default model</th>`
          + `<th data-tip="The program actually invoked for a claude-code/codex/pi/raw backend. Not meaningful for a native backend (claude/anthropic/ollama).">Executable</th>`
          + `<th data-tip="How many environment variables this profile sets, each either a literal value or a link to another environment variable resolved at cell-run time.">Env</th>`
          + `<th></th>`
          + `</tr></thead><tbody>${rows.join("")}</tbody></table>`;
        const addRow = agentProfileEditName === "__add__" ? agentProfileEditRowHtml() : "";
        const addButton = agentProfileEditName === null
          ? `<div style="margin-top:12px"><button class="btn primary" onclick="startAddAgentProfile()" data-tip="Register a new custom agent profile a task's agent = &quot;...&quot; field can reference.\nWho/when: you want a task to run under a non-default model/executable/environment for a backend, without editing every task file by hand.">+ Add profile</button></div>`
          : "";
        el.innerHTML = err + table + addRow + addButton;
      }

      /**
       * The locked-row "change executable" inline edit row.
       * @param {AgentProfileView} p
       * @returns {string}
       */
      function agentProfileExecutableEditRowHtml(p) {
        return `<tr>
            <td><span class="proj-name">${esc(p.name)}</span> <span style="color:var(--muted)">🔒 built-in</span></td>
            <td class="mono">${esc(p.backend)}</td>
            <td colspan="3">
              <input id="agent-profile-executable-input" type="text" value="${esc(agentProfileExecutableDraft)}"
                oninput="agentProfileExecutableDraft = this.value" style="width:100%"
                data-tip="The command this backend runs, e.g. a bare name resolved on PATH or an absolute path." />
            </td>
            <td>
              <button class="btn primary" onclick="saveAgentProfileExecutable()" data-tip="Save this executable change.">Save</button>
              <button class="btn" onclick="cancelAgentProfileExecutableEdit()" data-tip="Discard this change.">Cancel</button>
            </td>
          </tr>`;
      }

      /**
       * The add/edit form for a custom agent profile, rendered as one table
       * row (name/backend/model/executable) followed by the env table editor
       * and Save/Cancel actions -- shared between "+ Add profile" and
       * "✎ Edit" on an existing custom row.
       * @returns {string}
       */
      function agentProfileEditRowHtml() {
        const isAdd = agentProfileEditName === "__add__";
        const backendOptions = agentProfileBackends
          .map((b) => `<option value="${esc(b)}" ${agentProfileFieldsDraft.backend === b ? "selected" : ""}>${esc(b)}</option>`)
          .join("");
        const showExecutable = agentProfileFieldsDraft.backend
          ? !agentProfileBackendIsNative(agentProfileFieldsDraft.backend)
          : true;
        const nameField = isAdd
          ? `<input type="text" placeholder="name" value="${esc(agentProfileFieldsDraft.name)}" oninput="agentProfileFieldsDraft.name = this.value" data-tip="Unique profile name. Matched exactly against a task's agent = &quot;...&quot; field.\nCannot collide with a reserved built-in backend name." />`
          : `<span class="proj-name">${esc(agentProfileFieldsDraft.name)}</span>`;
        const envRows = agentProfileEnvDraft.map((row, idx) => agentProfileEnvDraftRowHtml(row, idx)).join("");
        const envTable = `<div class="kv-row" style="margin-top:8px"><b style="font-size:12px">environment variables</b></div>
          ${envRows || `<div class="kv-row"><span class="v" style="color:var(--muted)">none</span></div>`}
          <div class="kv-row"><button class="btn" onclick="addAgentProfileEnvRow()" data-tip="Add another environment variable row to this profile.">+ Add env var</button></div>`;
        return `<tr>
            <td>${nameField}</td>
            <td><select onchange="agentProfileFieldsDraft.backend = this.value; renderAgentProfiles()" data-tip="The backend this profile runs under.">
                <option value="">(choose)</option>${backendOptions}
              </select></td>
            <td><input type="text" placeholder="default model (optional)" value="${esc(agentProfileFieldsDraft.default_model)}" oninput="agentProfileFieldsDraft.default_model = this.value" data-tip="Model used when a task's cell/task/review doesn't set its own. Leave blank for none." /></td>
            <td>${showExecutable
              ? `<input type="text" placeholder="executable" value="${esc(agentProfileFieldsDraft.executable)}" oninput="agentProfileFieldsDraft.executable = this.value" data-tip="The program to invoke. Required for backend &quot;raw&quot;; not meaningful for a native backend (claude/anthropic/ollama)." />`
              : `<span style="color:var(--muted)">n/a for this backend</span>`}
            </td>
            <td colspan="2">
              ${envTable}
              <div class="kv-row" style="margin-top:8px">
                <button class="btn primary" onclick="saveAgentProfile()" data-tip="${isAdd ? "Register this profile." : "Save changes to this profile."}">Save</button>
                <button class="btn" onclick="cancelAgentProfileEdit()" data-tip="Discard these changes.">Cancel</button>
              </div>
            </td>
          </tr>`;
      }

      /**
       * One env-variable draft row inside the profile add/edit form: a key
       * input, an equals/link kind selector, a value input (placeholder
       * changes with `kind`), and a remove button.
       * @param {AgentProfileEnvDraftRow} row
       * @param {number} idx
       * @returns {string}
       */
      function agentProfileEnvDraftRowHtml(row, idx) {
        const valuePlaceholder = row.kind === "link"
          ? "linked environment variable name"
          : "leave blank to keep the existing value";
        const valueTip = row.kind === "link"
          ? "The name of another environment variable to resolve this value from at cell-run time (re-resolved every run, so a rotated credential doesn't need this profile re-saved)."
          : "The literal value to set. Blank means &quot;don't change what's already stored&quot; when editing an existing literal row -- the daemon never sends the real value back to this form.";
        return `<div class="kv-row" style="gap:6px">
            <input type="text" placeholder="KEY" value="${esc(row.key)}" style="flex:1" oninput="agentProfileEnvDraft[${idx}].key = this.value" data-tip="Environment variable name." />
            <select style="width:90px" onchange="setAgentProfileEnvKind(${idx}, this.value)" data-tip="equals: a literal value.\nlink: resolve from another environment variable name at cell-run time.">
              <option value="literal" ${row.kind === "literal" ? "selected" : ""}>equals</option>
              <option value="link" ${row.kind === "link" ? "selected" : ""}>link</option>
            </select>
            <input type="text" placeholder="${esc(valuePlaceholder)}" value="${esc(row.value)}" style="flex:1" oninput="agentProfileEnvDraft[${idx}].value = this.value" data-tip="${valueTip}" />
            <button class="btn" onclick="removeAgentProfileEnvRow(${idx})" data-tip="Remove this environment variable row.">✕</button>
          </div>`;
      }

      /**
       * Changes one env draft row's kind (equals/link) and re-renders, since
       * the value input's placeholder/tooltip depend on it.
       * @param {number} idx
       * @param {string} kind
       * @returns {void}
       */
      function setAgentProfileEnvKind(idx, kind) {
        agentProfileEnvDraft[idx].kind = kind === "link" ? "link" : "literal";
        renderAgentProfiles();
      }

      /**
       * Appends a blank env draft row and re-renders.
       * @returns {void}
       */
      function addAgentProfileEnvRow() {
        agentProfileEnvDraft.push({ key: "", kind: "literal", value: "" });
        renderAgentProfiles();
      }

      /**
       * Removes one env draft row and re-renders.
       * @param {number} idx
       * @returns {void}
       */
      function removeAgentProfileEnvRow(idx) {
        agentProfileEnvDraft.splice(idx, 1);
        renderAgentProfiles();
      }

      /**
       * Opens the "+ Add profile" form with empty drafts.
       * @returns {void}
       */
      function startAddAgentProfile() {
        agentProfileEditName = "__add__";
        agentProfileFieldsDraft = { name: "", backend: "", executable: "", default_model: "" };
        agentProfileEnvDraft = [];
        agentProfileError = "";
        renderAgentProfiles();
      }

      /**
       * Opens the edit form for an existing custom profile, pre-filled.
       * Literal env values start blank (the API never sends the real value)
       * with a placeholder explaining that blank means "unchanged"; link
       * rows show the real target variable name, since that's not a secret.
       * @param {string} name
       * @returns {void}
       */
      function startEditAgentProfile(name) {
        const p = agentProfiles.find((x) => x.name === name);
        if (!p) return;
        agentProfileEditName = name;
        agentProfileFieldsDraft = {
          name: p.name,
          backend: p.backend,
          executable: p.executable || "",
          default_model: p.default_model || "",
        };
        agentProfileEnvDraft = p.env.map((v) => ({
          key: v.key,
          kind: v.kind,
          value: v.kind === "link" ? v.value : "",
        }));
        agentProfileError = "";
        renderAgentProfiles();
      }

      /**
       * Closes whichever custom-profile add/edit form is open, discarding
       * its drafts.
       * @returns {void}
       */
      function cancelAgentProfileEdit() {
        agentProfileEditName = null;
        renderAgentProfiles();
      }

      /**
       * Submits the open add/edit form. A literal env row left blank while
       * editing an existing profile round-trips the redaction placeholder,
       * which the daemon resolves back to whatever is already stored under
       * that key (see this file's module doc comment) -- never a bare blank
       * string, which would instead overwrite the secret with an empty value.
       * @returns {Promise<void>}
       */
      async function saveAgentProfile() {
        const isAdd = agentProfileEditName === "__add__";
        const name = agentProfileFieldsDraft.name.trim();
        const backend = agentProfileFieldsDraft.backend.trim();
        if (!name || !backend) {
          agentProfileError = "Name and backend are both required.";
          renderAgentProfiles();
          return;
        }
        const wasLiteralByKey = new Map(
          (isAdd ? [] : (agentProfiles.find((p) => p.name === agentProfileEditName)?.env || []))
            .filter((v) => v.kind === "literal")
            .map((v) => [v.key, true]));
        const env = agentProfileEnvDraft
          .filter((row) => row.key.trim())
          .map((row) => {
            const value = row.kind === "literal" && row.value === "" && wasLiteralByKey.has(row.key)
              ? AGENT_PROFILE_REDACTED_PLACEHOLDER
              : row.value;
            return { key: row.key.trim(), kind: row.kind, value };
          });
        const body = {
          name,
          backend,
          default_model: agentProfileFieldsDraft.default_model.trim() || null,
          executable: agentProfileFieldsDraft.executable.trim() || null,
          env,
        };
        try {
          const r = await fetch("/api/agent-profiles", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify(body),
          });
          agentProfileError = r.ok ? "" : await responseError(r, "save failed");
        } catch (e) { agentProfileError = "daemon unreachable"; }
        if (!agentProfileError) agentProfileEditName = null;
        await pollAgentProfiles();
      }

      /**
       * Removes a custom agent profile.
       * @param {string} name
       * @returns {Promise<void>}
       */
      async function removeAgentProfile(name) {
        if (!confirm(`Remove agent profile "${name}"?\n\nTasks submitted afterwards that name this profile will be rejected.`)) return;
        try {
          const r = await fetch(`/api/agent-profiles/${encodeURIComponent(name)}`, { method: "DELETE" });
          agentProfileError = r.ok ? "" : await responseError(r, "remove failed");
        } catch (e) { agentProfileError = "daemon unreachable"; }
        await pollAgentProfiles();
      }

      /**
       * Opens the locked-row "change executable" inline edit.
       * @param {string} name
       * @returns {void}
       */
      function startSetLockedExecutable(name) {
        const p = agentProfiles.find((x) => x.name === name);
        agentProfileExecutableEditName = name;
        agentProfileExecutableDraft = p?.executable || "";
        agentProfileError = "";
        renderAgentProfiles();
      }

      /**
       * Closes the locked-row executable inline edit without saving.
       * @returns {void}
       */
      function cancelAgentProfileExecutableEdit() {
        agentProfileExecutableEditName = null;
        renderAgentProfiles();
      }

      /**
       * Saves the locked-row executable inline edit.
       * @returns {Promise<void>}
       */
      async function saveAgentProfileExecutable() {
        const name = agentProfileExecutableEditName;
        if (!name) return;
        const executable = agentProfileExecutableDraft.trim();
        if (!executable) {
          agentProfileError = "Executable must not be empty.";
          renderAgentProfiles();
          return;
        }
        try {
          const r = await fetch(`/api/agent-profiles/${encodeURIComponent(name)}/executable`, {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ executable }),
          });
          agentProfileError = r.ok ? "" : await responseError(r, "save failed");
        } catch (e) { agentProfileError = "daemon unreachable"; }
        if (!agentProfileError) agentProfileExecutableEditName = null;
        await pollAgentProfiles();
      }
