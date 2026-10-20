      // ---- Agents tab: MCP server + skill capabilities (RAL-695) ----
      /**
       * One Set/Link row of an MCP server's env or headers as the API returns it. `Set` values are masked.
       * @typedef {object} CapabilityEnvRow
       * @property {string} key
       * @property {string} kind - "set" | "link"
       * @property {string} value
       */
      /**
       * One entry of `GET /api/capabilities`'s `items`.
       * @typedef {object} CapabilityItemView
       * @property {string} kind - "mcp" | "skill"
       * @property {string} name
       * @property {string} source - "manual", "import:<origin>" or "builtin".
       * @property {{command: string|null, args: string[], env: CapabilityEnvRow[], url: string|null, headers: CapabilityEnvRow[]}|null} mcp
       * @property {{path: string}|null} skill
       * @property {string} status - "untested" | "ok" | "failing" | "found" | "missing".
       * @property {string|null} status_detail
       * @property {number|null} tool_count
       * @property {string[]|null} [tools] - tool names from the last successful check (MCP servers).
       * @property {number|null} checked_at_ms
       */
      /**
       * One profile's `GET /api/agent-profiles/{name}/capabilities` answer.
       * @typedef {object} ProfileCapabilityView
       * @property {string} profile
       * @property {{kind: string, name: string}[]} enabled
       * @property {{mode: string, args: string[]}} delivery
       * @property {{backend: string, supported: boolean, delivery_configurable: boolean, tool_filter: boolean}} support
       * @property {Object<string, string[]>} denied_tools - denied tool names per MCP server.
       */
      /**
       * The open add/edit form.
       * @typedef {object} CapabilityForm
       * @property {boolean} editing
       * @property {string} kind
       * @property {string} name
       * @property {string} command
       * @property {string} args - one argument per line.
       * @property {string} url
       * @property {string} path
       * @property {CapabilityEnvRow[]} env
       * @property {CapabilityEnvRow[]} headers
       */

      const CAPABILITY_BUILTIN_PROFILES = ["claude-code", "codex", "pi", "claude", "anthropic", "ollama"];
      /** @type {CapabilityItemView[]} */
      let capabilityItems = [];
      let capabilityAllowRalphusMcp = false;
      /** @type {ProfileCapabilityView[]} */
      let capabilityProfiles = [];
      let capabilityError = "";
      /** @type {CapabilityForm|null} */
      let capabilityForm = null;
      let capabilityFormError = "";
      /** @type {Set<string>} */
      let capabilityChecking = new Set();
      /** The delivery args draft per raw profile, kept across re-renders. */
      /** @type {Map<string, string>} */
      let capabilityDeliveryArgsDraft = new Map();
      /** Profile/server pairs whose tool list is expanded, kept across re-renders. */
      /** @type {Set<string>} */
      let capabilityToolsOpen = new Set();

      /**
       * The per-tool allow/deny list of one enabled MCP server for one profile.
       * Tools are allowed unless denied, so a tool a server adds later is allowed;
       * a denied tool the server no longer lists stays visible so it can be cleared.
       * @param {ProfileCapabilityView} pc
       * @param {CapabilityItemView} it
       * @returns {string}
       */
      function capabilityToolsHtml(pc, it) {
        const denied = new Set((pc.denied_tools || {})[it.name] || []);
        const listed = it.tools || [];
        const stale = [...denied].filter((t) => !listed.includes(t));
        const key = `${pc.profile}/${it.name}`;
        if (!pc.support.tool_filter) {
          return `<div style="margin-left:18px;color:var(--ignored)" data-tip="This backend cannot filter individual MCP tools: every tool of an enabled server is available to the agent.">${esc(it.name)}: per-tool filtering unsupported for ${esc(pc.support.backend)}</div>`;
        }
        if (listed.length === 0 && stale.length === 0) {
          return `<div style="margin-left:18px;color:var(--muted)" data-tip="Run Check on this server in the catalog to list its tools.">${esc(it.name)}: check the server to list its tools</div>`;
        }
        const rows = [...listed.map((t) => [t, false]), ...stale.map((t) => [t, true])]
          .map(
            ([t, isStale]) =>
              `<label style="display:block;white-space:nowrap" data-tip="${isStale ? "The server no longer lists this tool; the deny is kept until you clear it." : "Uncheck to stop this profile's agent calling the tool. New tools a server adds are allowed."}">
                <input type="checkbox" ${denied.has(t) ? "" : "checked"} data-profile="${esc(pc.profile)}" data-server="${esc(it.name)}" data-tool="${esc(t)}" onchange="setProfileToolAllowed(this.dataset.profile, this.dataset.server, this.dataset.tool, this.checked)" /> <span class="mono">${esc(t)}</span>${isStale ? ` <span style="color:var(--ignored)">(not listed)</span>` : ""}</label>`,
          )
          .join("");
        return `<details style="margin-left:18px" ${capabilityToolsOpen.has(key) ? "open" : ""} data-key="${esc(key)}" ontoggle="if (this.open) capabilityToolsOpen.add(this.dataset.key || ''); else capabilityToolsOpen.delete(this.dataset.key || '')">
            <summary data-tip="Choose which tools of ${esc(it.name)} this profile's agent may call.">${esc(it.name)} tools (${listed.length - [...denied].filter((t) => listed.includes(t)).length}/${listed.length} allowed)</summary>${rows}</details>`;
      }

      /**
       * The names of every profile a toggle row is shown for: the built-ins, then the stored profiles.
       * @returns {string[]}
       */
      function capabilityProfileNames() {
        const stored = agentProfiles.map((p) => p.name).filter((n) => !CAPABILITY_BUILTIN_PROFILES.includes(n));
        return [...CAPABILITY_BUILTIN_PROFILES, ...stored];
      }

      /**
       * Loads the catalog and every profile's toggles, then re-renders `#agent-capabilities`.
       * @returns {Promise<void>}
       */
      async function pollAgentCapabilities() {
        try {
          const d = await (await fetch("/api/capabilities")).json();
          capabilityItems = d.items || [];
          capabilityAllowRalphusMcp = !!d.allow_ralphus_mcp;
          const names = capabilityProfileNames();
          const rows = await Promise.all(
            names.map(async (n) => {
              const r = await fetch(`/api/agent-profiles/${encodeURIComponent(n)}/capabilities`);
              return r.ok ? /** @type {ProfileCapabilityView} */ (await r.json()) : null;
            }),
          );
          capabilityProfiles = /** @type {ProfileCapabilityView[]} */ (rows.filter(Boolean));
          capabilityError = "";
        } catch (e) {
          capabilityError = "daemon unreachable";
        }
        renderAgentCapabilities();
      }

      /**
       * Renders one status cell for a catalog row.
       * @param {CapabilityItemView} it
       * @returns {string}
       */
      function capabilityStatusHtml(it) {
        const color = it.status === "ok" || it.status === "found" ? "var(--done)" : it.status === "failing" || it.status === "missing" ? "var(--failed)" : "var(--muted)";
        const label = it.status === "ok" && it.tool_count !== null ? `ok · ${it.tool_count} tool${it.tool_count === 1 ? "" : "s"}` : it.status;
        const detail = it.status_detail ? `\n${it.status_detail}` : "";
        return `<span style="color:${color}" data-tip="${esc(it.kind === "mcp" ? "MCP server probe result: ok (with the tool count), failing, or untested. Probes run only when you click Check." : "Skill folder status: found when the folder holds a SKILL.md, missing otherwise.")}${esc(detail)}">${esc(label)}</span>`;
      }

      /**
       * Renders one catalog table row.
       * @param {CapabilityItemView} it
       * @returns {string}
       */
      function capabilityRowHtml(it) {
        const key = `${it.kind}/${it.name}`;
        const builtin = it.source === "builtin";
        const what = it.kind === "mcp" && it.mcp ? esc(it.mcp.command ? [it.mcp.command, ...it.mcp.args].join(" ") : it.mcp.url || "") : esc(it.skill ? it.skill.path : "");
        const checking = capabilityChecking.has(key);
        const edit = builtin
          ? ""
          : `<button class="btn" data-click="editCapability" data-kind="${esc(it.kind)}" data-name="${esc(it.name)}" data-tip="Edit this catalog entry.\nSet env values stay masked; leave a masked value untouched to keep the stored one.">Edit</button>
             <button class="btn" data-click="removeCapability" data-kind="${esc(it.kind)}" data-name="${esc(it.name)}" data-tip="Remove this entry from the catalog and from every profile that enabled it.\nThis cannot be undone.">Remove</button>`;
        return `<tr>
            <td class="mono">${esc(it.kind)}</td>
            <td><span class="proj-name">${esc(it.name)}</span></td>
            <td>${esc(it.source)}</td>
            <td class="mono">${what}</td>
            <td>${capabilityStatusHtml(it)}</td>
            <td>
              <button class="btn" ${checking ? "disabled" : ""} data-click="checkCapability" data-kind="${esc(it.kind)}" data-name="${esc(it.name)}" data-tip="${it.kind === "mcp" ? "Start this MCP server once and count its tools.\nRuns a real process on the daemon host; nothing is probed until you click." : "Re-check that the skill folder exists on the daemon host."}">${checking ? "Checking…" : "Check"}</button>
              ${edit}
            </td>
          </tr>`;
      }

      /**
       * Renders one profile's toggle row.
       * @param {ProfileCapabilityView} pc
       * @returns {string}
       */
      function capabilityProfileHtml(pc) {
        const on = new Set(pc.enabled.map((e) => `${e.kind}/${e.name}`));
        const unsupported = !pc.support.supported && !pc.support.delivery_configurable;
        const note = unsupported
          ? `<span style="color:var(--ignored)" data-tip="This backend has no MCP/skill delivery: the native agent loop runs its own tool set. Toggles are stored but never reach the agent.">unsupported</span>`
          : "";
        const boxes = capabilityItems
          .map((it) => {
            const key = `${it.kind}/${it.name}`;
            return `<label style="margin-right:12px;white-space:nowrap" data-tip="Give this profile's agent the ${esc(it.kind)} \"${esc(it.name)}\".\nDefault is off; capabilities are the only source of MCP servers and skills for a spawned agent.">
                <input type="checkbox" ${on.has(key) ? "checked" : ""} ${unsupported ? "disabled" : ""} data-kind="${esc(it.kind)}" data-name="${esc(it.name)}" data-profile="${esc(pc.profile)}" onchange="toggleProfileCapability(this.dataset.profile, this.dataset.kind, this.dataset.name, this.checked)" /> ${esc(it.kind === "mcp" ? "" : "skill: ")}${esc(it.name)}</label>`;
          })
          .join("");
        const toolLists = capabilityItems
          .filter((it) => it.kind === "mcp" && on.has(`mcp/${it.name}`) && !unsupported)
          .map((it) => capabilityToolsHtml(pc, it))
          .join("");
        let delivery = "";
        if (pc.support.delivery_configurable) {
          const draft = capabilityDeliveryArgsDraft.get(pc.profile) ?? pc.delivery.args.join(" ");
          delivery = `<div style="margin-top:4px">
              <select data-profile="${esc(pc.profile)}" onchange="setProfileCapabilityDelivery(this.dataset.profile, this.value)" data-tip="How this custom command receives capabilities.\nbackend_default: not delivered (a raw command has none).\nextra_args: the arguments below are appended to the command.\nnone: nothing is passed.">
                ${["backend_default", "extra_args", "none"].map((m) => `<option value="${m}" ${pc.delivery.mode === m ? "selected" : ""}>${m}</option>`).join("")}
              </select>
              <input type="text" class="mono" style="width:320px" placeholder="--mcp-config {mcp_config}" value="${esc(draft)}" data-profile="${esc(pc.profile)}" oninput="capabilityDeliveryArgsDraft.set(this.dataset.profile || '', this.value)" data-tip="Extra arguments (space separated) appended for the extra_args mode. Applies when you change the mode or click Save args." />
              <button class="btn" data-click="saveCapabilityDeliveryArgs" data-profile="${esc(pc.profile)}" data-tip="Save these extra arguments with extra_args as the delivery mode.">Save args</button>
            </div>`;
        }
        return `<tr><td><span class="proj-name">${esc(pc.profile)}</span> ${note}</td><td class="mono">${esc(pc.support.backend)}</td><td>${boxes || `<span style="color:var(--muted)">catalog is empty</span>`}${toolLists}${delivery}</td></tr>`;
      }

      /**
       * Renders the capability catalog, toggles and add/edit form into `#agent-capabilities`.
       * @returns {void}
       */
      function renderAgentCapabilities() {
        const el = document.getElementById("agent-capabilities");
        if (!el) return;
        const err = capabilityError ? `<div class="empty" style="color:var(--failed);margin-bottom:8px">${esc(capabilityError)}</div>` : "";
        const ralphusNote = `<div style="color:var(--muted);font-size:11px;margin-bottom:8px" data-tip="The ralphus MCP server entry appears only when [agents] allow_ralphus_mcp = true in the daemon config; it is off by default so agents cannot drive ralphus itself.">ralphus MCP server: ${capabilityAllowRalphusMcp ? "available (allow_ralphus_mcp = true)" : "hidden (set [agents] allow_ralphus_mcp = true to offer it)"}</div>`;
        const table = capabilityItems.length
          ? `<table class="proj-table"><thead><tr>
              <th data-tip="mcp (a tool server) or skill (a folder with SKILL.md).">Kind</th>
              <th data-tip="Catalog name, unique within its kind.">Name</th>
              <th data-tip="manual, import:<origin> (from the Import action) or builtin.">Source</th>
              <th data-tip="The server command line or URL, or the skill folder path.">Definition</th>
              <th data-tip="Last probe result.">Status</th>
              <th></th>
            </tr></thead><tbody>${capabilityItems.map((it) => capabilityRowHtml(it)).join("")}</tbody></table>`
          : `<div class="empty">The capability catalog is empty. Add an entry or import from your existing Claude Code / Codex / Pi configuration.</div>`;
        const buttons = capabilityForm
          ? ""
          : `<button class="btn primary" data-click="openCapabilityForm" data-kind="mcp" data-tip="Add an MCP server to the catalog.">+ MCP server</button>
             <button class="btn primary" data-click="openCapabilityForm" data-kind="skill" data-tip="Add a skill folder to the catalog.">+ Skill</button>
             <button class="btn" data-click="importCapabilities" data-tip="Copy MCP servers and skills found in the daemon host's home configuration (Claude Code, Codex, Pi) into this catalog.\nExisting names are kept; nothing is enabled for any profile.">Import</button>`;
        const toggles = capabilityProfiles.length
          ? `<table class="proj-table" style="margin-top:12px"><thead><tr>
              <th data-tip="Agent profile (built-in backends and stored profiles).">Profile</th>
              <th data-tip="Backend the profile runs on.">Backend</th>
              <th data-tip="Items this profile's agent receives. Nothing is enabled by default.">Enabled capabilities</th>
            </tr></thead><tbody>${capabilityProfiles.map((pc) => capabilityProfileHtml(pc)).join("")}</tbody></table>`
          : "";
        el.innerHTML = `<div style="margin:20px 0 8px" data-tip="MCP servers and skills a spawned agent may use (RAL-695).\nThis catalog is ralphus-owned; a profile's toggles are the only source of MCP servers and skills for its agents. Not sent to native agents (claude, anthropic, ollama) or an unconfigured raw command.">Capabilities</div>${err}${ralphusNote}<div style="margin-bottom:8px">${buttons}</div>${table}${renderCapabilityForm()}${toggles}`;
      }

      /**
       * Renders Set/Link rows for the open form's env or headers.
       * @param {"env"|"headers"} field
       * @param {CapabilityEnvRow[]} rows
       * @returns {string}
       */
      function capabilityRowsHtml(field, rows) {
        const body = rows
          .map(
            (row, i) => `<div class="row" style="gap:6px;margin-top:6px;align-items:center">
            <input type="text" class="mono" style="width:200px" placeholder="KEY" value="${esc(row.key)}" oninput="capabilityForm && (capabilityForm.${field}[${i}].key = this.value)" data-tip="Variable or header name." />
            <select onchange="capabilityForm && (capabilityForm.${field}[${i}].kind = this.value === 'link' ? 'link' : 'set')" data-tip="Set -- the literal value (shown masked after saving).\nLink -- the name of a daemon environment variable resolved at spawn time; the resolved value is never shown.">
              <option value="set" ${row.kind === "set" ? "selected" : ""}>Set</option>
              <option value="link" ${row.kind === "link" ? "selected" : ""}>Link</option>
            </select>
            <input type="text" class="mono" style="flex:1" placeholder="value" value="${esc(row.value)}" oninput="capabilityForm && (capabilityForm.${field}[${i}].value = this.value)" data-tip="The literal value, or the linked variable's name." />
            <button class="btn" style="padding:2px 8px;font-size:11px" data-click="removeCapabilityRow" data-field="${field}" data-i="${i}" data-tip="Remove this row. Applies on Save.">✕</button>
          </div>`,
          )
          .join("");
        return `${body}<button class="btn" style="padding:2px 8px;font-size:11px;margin-top:6px" data-click="addCapabilityRow" data-field="${field}" data-tip="Add a ${field === "env" ? "environment variable" : "header"} row.">+ Add row</button>`;
      }

      /**
       * Renders the add/edit form, or an empty string when closed.
       * @returns {string}
       */
      function renderCapabilityForm() {
        const f = capabilityForm;
        if (!f) return "";
        const err = capabilityFormError ? `<div class="verr" style="margin-top:8px">${esc(capabilityFormError)}</div>` : "";
        const fields =
          f.kind === "skill"
            ? `<div class="row" style="gap:8px;margin-top:8px"><label style="width:110px" data-tip="Folder on the daemon host holding SKILL.md. It is copied to remote machines when a cell runs there.">Folder</label>
                <input type="text" class="mono" style="flex:1" value="${esc(f.path)}" oninput="capabilityForm && (capabilityForm.path = this.value)" placeholder="/home/me/.claude/skills/my-skill" data-tip="Absolute path of the skill folder." /></div>`
            : `<div class="row" style="gap:8px;margin-top:8px"><label style="width:110px" data-tip="Executable for a stdio MCP server. Leave blank when using a URL.">Command</label>
                <input type="text" class="mono" style="flex:1" value="${esc(f.command)}" oninput="capabilityForm && (capabilityForm.command = this.value)" placeholder="npx" data-tip="Executable started for a stdio MCP server." /></div>
              <div class="row" style="gap:8px;margin-top:8px"><label style="width:110px" data-tip="One argument per line.">Arguments</label>
                <textarea class="mono" rows="3" style="flex:1" oninput="capabilityForm && (capabilityForm.args = this.value)" data-tip="One command-line argument per line.">${esc(f.args)}</textarea></div>
              <div class="row" style="gap:8px;margin-top:8px"><label style="width:110px" data-tip="Endpoint of a remote (HTTP) MCP server. Leave blank when using a command.">URL</label>
                <input type="text" class="mono" style="flex:1" value="${esc(f.url)}" oninput="capabilityForm && (capabilityForm.url = this.value)" placeholder="https://example.com/mcp" data-tip="Remote MCP server URL." /></div>
              <div style="margin-top:12px"><div style="font-size:12px;color:var(--muted)" data-tip="Environment for the server process. Set values are masked everywhere after saving; Link rows name a daemon environment variable.">Environment</div>${capabilityRowsHtml("env", f.env)}</div>
              <div style="margin-top:12px"><div style="font-size:12px;color:var(--muted)" data-tip="HTTP headers for a URL server. Same Set/Link handling as the environment.">Headers</div>${capabilityRowsHtml("headers", f.headers)}</div>`;
        return `<div style="margin-top:16px;padding-top:12px;border-top:1px solid var(--border)">
            <div style="margin-bottom:8px" data-tip="Nothing is sent to the daemon until Save.">${f.editing ? "Edit" : "New"} ${esc(f.kind === "skill" ? "skill" : "MCP server")}</div>
            <div class="row" style="gap:8px"><label style="width:110px" data-tip="Unique name within its kind.">Name</label>
              <input type="text" class="mono" style="flex:1" value="${esc(f.name)}" ${f.editing ? "disabled" : ""} oninput="capabilityForm && (capabilityForm.name = this.value)" placeholder="my-server" data-tip="Catalog name.${f.editing ? " Cannot be changed once created." : ""}" /></div>
            ${fields}${err}
            <div class="row" style="gap:8px;margin-top:12px">
              <button class="btn primary" data-click="saveCapability" data-tip="Save this catalog entry to the daemon.">Save</button>
              <button class="btn" data-click="cancelCapabilityForm" data-tip="Discard this form without saving.">Cancel</button>
            </div>
          </div>`;
      }

      /**
       * Opens the add form for a kind.
       * @param {string} kind
       * @returns {void}
       */
      function openCapabilityForm(kind) {
        capabilityForm = { editing: false, kind, name: "", command: "", args: "", url: "", path: "", env: [], headers: [] };
        capabilityFormError = "";
        renderAgentCapabilities();
      }

      /**
       * Opens the edit form for an existing, non-builtin entry.
       * @param {string} kind
       * @param {string} name
       * @returns {void}
       */
      function editCapability(kind, name) {
        const it = capabilityItems.find((x) => x.kind === kind && x.name === name);
        if (!it) return;
        const m = it.mcp;
        capabilityForm = {
          editing: true,
          kind,
          name,
          command: (m && m.command) || "",
          args: m ? m.args.join("\n") : "",
          url: (m && m.url) || "",
          path: it.skill ? it.skill.path : "",
          env: m ? m.env.map((r) => ({ ...r })) : [],
          headers: m ? m.headers.map((r) => ({ ...r })) : [],
        };
        capabilityFormError = "";
        renderAgentCapabilities();
      }

      /**
       * Closes the form without saving.
       * @returns {void}
       */
      function cancelCapabilityForm() {
        capabilityForm = null;
        capabilityFormError = "";
        renderAgentCapabilities();
      }

      /**
       * Adds or removes a Set/Link row in the open form.
       * @param {string} field - "env" | "headers"
       * @param {number} index - row to remove, or -1 to append a blank row.
       * @returns {void}
       */
      function editCapabilityRows(field, index) {
        const f = capabilityForm;
        if (!f) return;
        const rows = field === "headers" ? f.headers : f.env;
        if (index < 0) rows.push({ key: "", kind: "set", value: "" });
        else rows.splice(index, 1);
        renderAgentCapabilities();
      }

      /**
       * Saves the open form via POST (create) or PATCH (edit).
       * @returns {Promise<void>}
       */
      async function saveCapability() {
        const f = capabilityForm;
        if (!f) return;
        const name = f.name.trim();
        if (!name) {
          capabilityFormError = "Name is required.";
          renderAgentCapabilities();
          return;
        }
        const rows = (/** @type {CapabilityEnvRow[]} */ r) => r.filter((x) => x.key.trim() !== "").map((x) => ({ key: x.key.trim(), kind: x.kind, value: x.value }));
        const body =
          f.kind === "skill"
            ? { kind: "skill", name, skill: { path: f.path.trim() } }
            : {
                kind: "mcp",
                name,
                mcp: {
                  command: f.command.trim() || null,
                  args: f.args.split("\n").filter((a) => a !== ""),
                  url: f.url.trim() || null,
                  env: rows(f.env),
                  headers: rows(f.headers),
                },
              };
        try {
          const r = f.editing
            ? await fetch(`/api/capabilities/${encodeURIComponent(f.kind)}/${encodeURIComponent(name)}`, { method: "PATCH", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) })
            : await fetch("/api/capabilities", { method: "POST", headers: { "Content-Type": "application/json" }, body: JSON.stringify(body) });
          if (r.ok) {
            capabilityForm = null;
            capabilityFormError = "";
            await pollAgentCapabilities();
            return;
          }
          capabilityFormError = await responseError(r, "save failed");
        } catch (e) {
          capabilityFormError = "daemon unreachable";
        }
        renderAgentCapabilities();
      }

      /**
       * Removes a catalog entry.
       * @param {string} kind
       * @param {string} name
       * @returns {Promise<void>}
       */
      async function removeCapability(kind, name) {
        if (!confirm(`Remove ${kind} "${name}" from the catalog and every profile?`)) return;
        try {
          const r = await fetch(`/api/capabilities/${encodeURIComponent(kind)}/${encodeURIComponent(name)}`, { method: "DELETE" });
          capabilityError = r.ok ? "" : await responseError(r, "remove failed");
        } catch (e) {
          capabilityError = "daemon unreachable";
        }
        await pollAgentCapabilities();
      }

      /**
       * Probes one entry on demand.
       * @param {string} kind
       * @param {string} name
       * @returns {Promise<void>}
       */
      async function checkCapability(kind, name) {
        const key = `${kind}/${name}`;
        await guardedAction(`capability-check:${key}`, async () => {
          capabilityChecking.add(key);
          renderAgentCapabilities();
          try {
            const r = await fetch(`/api/capabilities/${encodeURIComponent(kind)}/${encodeURIComponent(name)}/check`, { method: "POST" });
            capabilityError = r.ok ? "" : await responseError(r, "check failed");
          } catch (e) {
            capabilityError = "daemon unreachable";
          }
          capabilityChecking.delete(key);
          await pollAgentCapabilities();
        });
      }

      /**
       * Imports entries from the daemon host's home configuration.
       * @returns {Promise<void>}
       */
      async function importCapabilities() {
        await guardedAction("capability-import", async () => {
          try {
            const r = await fetch("/api/capabilities/import", { method: "POST" });
            capabilityError = r.ok ? "" : await responseError(r, "import failed");
          } catch (e) {
            capabilityError = "daemon unreachable";
          }
          await pollAgentCapabilities();
        });
      }

      /**
       * Enables or disables one catalog item for a profile.
       * @param {string} profile
       * @param {string} kind
       * @param {string} name
       * @param {boolean} enabled
       * @returns {Promise<void>}
       */
      async function toggleProfileCapability(profile, kind, name, enabled) {
        try {
          const r = await fetch(`/api/agent-profiles/${encodeURIComponent(profile)}/capabilities`, {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ kind, name, enabled }),
          });
          capabilityError = r.ok ? "" : await responseError(r, "toggle failed");
        } catch (e) {
          capabilityError = "daemon unreachable";
        }
        await pollAgentCapabilities();
      }

      /**
       * Allows or denies one tool of an MCP server for a profile.
       * @param {string} profile
       * @param {string} server
       * @param {string} tool
       * @param {boolean} allowed
       * @returns {Promise<void>}
       */
      async function setProfileToolAllowed(profile, server, tool, allowed) {
        try {
          const r = await fetch(`/api/agent-profiles/${encodeURIComponent(profile)}/capabilities/tools`, {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ server, tool, allowed }),
          });
          capabilityError = r.ok ? "" : await responseError(r, "tool change failed");
        } catch (e) {
          capabilityError = "daemon unreachable";
        }
        await pollAgentCapabilities();
      }

      /**
       * Sets a raw profile's delivery mode (and, for extra_args, its argument draft).
       * @param {string} profile
       * @param {string} mode
       * @returns {Promise<void>}
       */
      async function setProfileCapabilityDelivery(profile, mode) {
        const draft = capabilityDeliveryArgsDraft.get(profile) || "";
        const args = mode === "extra_args" ? draft.split(/\s+/).filter((a) => a !== "") : [];
        try {
          const r = await fetch(`/api/agent-profiles/${encodeURIComponent(profile)}/capabilities/delivery`, {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({ mode, args }),
          });
          capabilityError = r.ok ? "" : await responseError(r, "delivery change failed");
          if (r.ok) capabilityDeliveryArgsDraft.delete(profile);
        } catch (e) {
          capabilityError = "daemon unreachable";
        }
        await pollAgentCapabilities();
      }

      CLICK_HANDLERS.openCapabilityForm = (e, ds) => openCapabilityForm(ds.kind || "mcp");
      CLICK_HANDLERS.editCapability = (e, ds) => editCapability(ds.kind || "", ds.name || "");
      CLICK_HANDLERS.removeCapability = (e, ds) => removeCapability(ds.kind || "", ds.name || "");
      CLICK_HANDLERS.checkCapability = (e, ds) => checkCapability(ds.kind || "", ds.name || "");
      CLICK_HANDLERS.importCapabilities = () => importCapabilities();
      CLICK_HANDLERS.saveCapability = () => saveCapability();
      CLICK_HANDLERS.cancelCapabilityForm = () => cancelCapabilityForm();
      CLICK_HANDLERS.addCapabilityRow = (e, ds) => editCapabilityRows(ds.field || "env", -1);
      CLICK_HANDLERS.removeCapabilityRow = (e, ds) => editCapabilityRows(ds.field || "env", Number(ds.i));
      CLICK_HANDLERS.saveCapabilityDeliveryArgs = (e, ds) => setProfileCapabilityDelivery(ds.profile || "", "extra_args");
