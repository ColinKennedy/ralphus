      // ---- Presets tab (RAL-…) ----
      /**
       * The preset name the editor panel was last loaded from via Edit, or ""
       * while it holds a new, unsaved preset.
       * @type {string}
       */
      let presetEditing = "";
      /**
       * Polls `GET /api/presets` and re-renders the Presets tab.
       * @returns {Promise<void>}
       */
      async function pollPresets() {
        try {
          const d = await (await fetch("/api/presets")).json();
          byId("conn").className = "dot on";
          markUpdated();
          presets = (d.presets || []).slice().sort(
            (/** @type {PresetView} */ a, /** @type {PresetView} */ b) => a.name.localeCompare(b.name));
          renderPresets();
        } catch (e) { markUnreachable(); }
      }
      /**
       * Renders the Presets tab. The editor panel is built once and then left
       * alone, so a poll never wipes a half-typed preset; only the error banner,
       * the summary count, and the preset list re-render.
       * @returns {void}
       */
      function renderPresets() {
        const el = byId("presets");
        if (!el) return;
        if (!byId("presets-list")) {
          el.innerHTML = `<div id="presets-error"></div>
            <div class="presets-layout">
              <div id="presets-list" class="presets-list"></div>
              ${presetEditorHtml()}
            </div>`;
        }
        byId("presets-error").innerHTML = presetsError
          ? `<div class="presets-error" data-tip="The daemon's response to the last register/remove request.">⚠ ${esc(presetsError)}</div>`
          : "";
        const summary = byId("presets-summary");
        if (summary) {
          const disk = presets.filter((p) => p.source === "disk").length;
          summary.textContent = presets.length
            ? `${presets.length} preset${presets.length === 1 ? "" : "s"}${disk ? ` · ${disk} from disk` : ""}`
            : "";
        }
        const list = byId("presets-list");
        preserveUserState(list, () => { list.innerHTML = presetListHtml(); });
      }
      /**
       * Renders the preset list, grouped by namespace (the part of a name
       * before its last `/`), or an empty-state explainer.
       * @returns {string}
       */
      function presetListHtml() {
        if (!presets.length) {
          return `<div class="presets-empty">
              <div class="presets-empty-icon">🧩</div>
              <div class="presets-empty-title">No presets registered yet</div>
              <div class="presets-empty-body">A preset is a named bundle of field defaults. Register one with the form, then reference it from a task file:</div>
              <pre class="presets-snippet">[[task.cell]]
extends = [<span class="preset-sentinel">"&lt;&lt;ralphus:presets/roles/reviewer&gt;&gt;"</span>]</pre>
              <div class="presets-empty-body">Any field the cell leaves unset is stamped from the preset at submit time.</div>
            </div>`;
        }
        /** @type {Map<string, PresetView[]>} */
        const groups = new Map();
        for (const p of presets) {
          const cut = p.name.lastIndexOf("/");
          const ns = cut < 0 ? "" : p.name.slice(0, cut);
          const group = groups.get(ns);
          if (group) group.push(p);
          else groups.set(ns, [p]);
        }
        return [...groups.entries()].map(([ns, items]) => `<section class="presets-group">
            <div class="presets-group-head" data-tip="${ns ? `Presets in the &quot;${esc(ns)}&quot; namespace, referenced as &lt;&lt;ralphus:presets/${esc(ns)}/&lt;name&gt;&gt;&gt;.` : "Presets with no namespace, referenced as &lt;&lt;ralphus:presets/&lt;name&gt;&gt;&gt;."}">
              <span class="mono">${ns ? `${esc(ns)}/` : "(no namespace)"}</span>
              <span class="presets-group-count">${items.length}</span>
            </div>
            <div class="presets-cards">${items.map((p) => presetCardHtml(p)).join("")}</div>
          </section>`).join("");
      }
      /**
       * Escapes preset template text and highlights its `<<ralphus:…>>` sentinels.
       * @param {string} v
       * @returns {string}
       */
      function presetTemplateHtml(v) {
        return esc(v).replace(/&lt;&lt;ralphus:(?:(?!&gt;&gt;).)*&gt;&gt;/g, (m) => `<span class="preset-sentinel">${m}</span>`);
      }
      /**
       * Renders one registered preset as a card.
       * @param {PresetView} p
       * @returns {string}
       */
      function presetCardHtml(p) {
        const cut = p.name.lastIndexOf("/");
        const leaf = cut < 0 ? p.name : p.name.slice(cut + 1);
        const readOnly = p.source === "disk";
        const sourceBadge = readOnly
          ? `<span class="badge preset-src-disk" data-tip="Defined in ${esc(p.path || "a file")}, named by preset_paths in the daemon's config.toml, so read-only here. Edit that file to change it.">disk · read-only</span>`
          : `<span class="badge" data-tip="Stored in the daemon's database: editable here.">db</span>`;
        const template = (/** @type {string} */ field, /** @type {string|null} */ v, /** @type {string} */ tip) => v
          ? `<div class="preset-field">
              <div class="preset-field-label mono" data-tip="${tip}">${field}</div>
              <pre class="preset-template${v.split("\n").length > 5 || v.length > 220 ? " clamped" : ""}" data-tip="${esc(v)}">${presetTemplateHtml(v)}</pre>
            </div>`
          : "";
        const stat = (/** @type {string} */ field, /** @type {string|number|null} */ v, /** @type {string} */ tip) => v === null || v === undefined || v === ""
          ? ""
          : `<span class="preset-stat" data-tip="${tip}"><span class="mono">${field}</span><b>${esc(typeof v === "number" ? v.toLocaleString() : v)}</b></span>`;
        const stats = stat("system_prompt_position", p.system_prompt_position, "Stamped alongside system_prompt. The only accepted value is &quot;append&quot;.")
          + stat("maximum_context", p.maximum_context, "Stamped into an extending task's or cell's own maximum_context field, only if left unset. Not applicable to a proof step.")
          + stat("auto_compact_threshold", p.auto_compact_threshold, "Stamped into an extending task's or cell's own auto_compact_threshold field, only if left unset. Not applicable to a proof step.")
          + stat("maximum_tool_output_tokens", p.maximum_tool_output_tokens, "Stamped into an extending task's, cell's, or proof step's own maximum_tool_output_tokens field, only if left unset.");
        const body = template("system_prompt", p.system_prompt, "A system-prompt template stamped into an extending cell's own system_prompt field, only if that cell left it unset. Linked fields expand as in prompt.")
          + template("prompt", p.prompt, "A prompt template stamped into an extending cell's or prompt proof step's own prompt field. &lt;&lt;ralphus:linked-field/./prompt&gt;&gt; inside it expands to that entity's own prompt (&lt;&lt;ralphus:linked-field/../prompt&gt;&gt; to its parent's).\nWithout such a reference it only fills a prompt the entity left unset.")
          + (stats ? `<div class="preset-stats">${stats}</div>` : "");
        const actions = readOnly
          ? ""
          : `<button class="btn" data-click="editPreset" data-name="${esc(p.name)}" data-tip="Load this preset into the editor so you can change and re-register it.">Edit</button>`
            + `<button class="btn danger" data-click="removePreset" data-name="${esc(p.name)}" data-tip="Deregister this preset.\nA squad already submitted before removal keeps whatever values were already stamped into it -- only a future submission's &quot;extends&quot; referencing this name is affected. This cannot be undone.">Remove</button>`;
        return `<div class="preset-card${p.name === presetEditing ? " editing" : ""}">
            <div class="preset-card-head">
              <span class="preset-name mono" data-tip="${esc(p.name)}">${esc(leaf)}</span>
              ${sourceBadge}
              <span class="preset-card-actions">
                <button class="copy-btn" data-tip="Copy this preset's extends entry to the clipboard." data-copy="${esc(`"<<ralphus:presets/${p.name}>>"`)}" onclick="copyText(event)">⧉</button>
                ${actions}
              </span>
            </div>
            ${body || `<div class="preset-field-empty">Sets no fields.</div>`}
          </div>`;
      }
      /**
       * Renders the preset editor panel: a labeled field per preset value.
       * @returns {string}
       */
      function presetEditorHtml() {
        return `<aside class="presets-editor">
            <div class="presets-editor-title" id="preset-editor-title" data-tip="Register a preset so &quot;extends = [\\&quot;&lt;&lt;ralphus:presets/&lt;name&gt;&gt;&gt;\\&quot;]&quot; on a task, cell, or proof step can stamp these field values into any of that entity's own fields still unset at submit time.\nA field left blank here is simply not stamped by this preset. A field this preset sets that doesn't exist on the entity it's applied to (e.g. system_prompt on a task) is silently skipped.">New preset</div>
            <label class="preset-input">
              <span>Name</span>
              <input id="preset-name" type="text" class="mono" placeholder="roles/reviewer" autocomplete="off" data-tip="Preset name -- what &lt;&lt;ralphus:presets/&lt;name&gt;&gt;&gt; names. Use / for a namespace, e.g. roles/reviewer." />
            </label>
            <label class="preset-input">
              <span>system_prompt <em>cell only</em></span>
              <textarea id="preset-system-prompt" rows="4" placeholder="You are a meticulous code reviewer…" data-tip="Appended-system-prompt text stamped into an extending cell's own system_prompt field, only if that cell left it unset. Cell-level only; silently skipped on a task or proof step.\nMay embed &lt;&lt;ralphus:linked-field/./prompt&gt;&gt; to splice in the cell's own prompt."></textarea>
            </label>
            <label class="preset-input">
              <span>prompt</span>
              <textarea id="preset-prompt" rows="4" placeholder="Review the following change: &lt;&lt;ralphus:linked-field/./prompt&gt;&gt;" data-tip="Prompt template stamped into an extending cell's or prompt proof step's own prompt field.\n&lt;&lt;ralphus:linked-field/./prompt&gt;&gt; expands to the entity's own prompt (the preset then frames it); &lt;&lt;ralphus:linked-field/../prompt&gt;&gt; to its parent's, e.g. a proof step's cell. An unresolvable reference becomes &quot;&lt;field prompt was not found&gt;&quot;."></textarea>
            </label>
            <div class="preset-input-grid">
              <label class="preset-input">
                <span>system_prompt_position</span>
                <select id="preset-system-prompt-position" data-tip="Stamped alongside system_prompt. The only accepted value is &quot;append&quot;.">
                  <option value="">— unset —</option>
                  <option value="append">append</option>
                </select>
              </label>
              <label class="preset-input">
                <span>maximum_context</span>
                <input id="preset-maximum-context" type="number" min="1" step="1" placeholder="tokens" data-tip="Context-window token limit stamped into an extending task's or cell's own maximum_context field, only if left unset. Not applicable to a proof step." />
              </label>
              <label class="preset-input">
                <span>auto_compact_threshold</span>
                <input id="preset-auto-compact-threshold" type="number" min="1" step="1" placeholder="tokens" data-tip="Auto-compact trigger threshold stamped into an extending task's or cell's own auto_compact_threshold field, only if left unset. Not applicable to a proof step." />
              </label>
              <label class="preset-input">
                <span>maximum_tool_output_tokens</span>
                <input id="preset-maximum-tool-output-tokens" type="number" min="1" step="1" placeholder="tokens" data-tip="Per-tool-call output token cap stamped into an extending task's, cell's, or proof step's own maximum_tool_output_tokens field, only if left unset." />
              </label>
            </div>
            <div class="presets-editor-hint">Blank fields aren't stamped. A field the extending entity already sets always wins.</div>
            <div class="btn-row">
              <button class="btn primary" onclick="registerPreset()" data-tip="Register this preset with the daemon.\nRe-registering an existing name updates its field values in place rather than creating a duplicate.">Save preset</button>
              <button class="btn" onclick="clearPresetEditor()" data-tip="Empty every field so you can start a new preset.">Clear</button>
            </div>
          </aside>`;
      }
      /**
       * Points the editor title (and the highlighted card) at `name`, or back
       * to "New preset" when `name` is "".
       * @param {string} name
       * @returns {void}
       */
      function setPresetEditing(name) {
        presetEditing = name;
        const title = byId("preset-editor-title");
        if (title) title.innerHTML = name ? `Editing <span class="mono">${esc(name)}</span>` : "New preset";
        document.querySelectorAll(".preset-card").forEach((c) => {
          const btn = /** @type {HTMLElement|null} */ (c.querySelector("[data-click=editPreset]"));
          c.classList.toggle("editing", !!name && !!btn && btn.dataset.name === name);
        });
      }
      /**
       * Loads a registered preset into the editor panel.
       * @param {string} name
       * @returns {void}
       */
      function editPreset(name) {
        const p = presets.find((x) => x.name === name);
        if (!p) return;
        const set = (/** @type {string} */ id, /** @type {string|number|null} */ v) => {
          /** @type {HTMLInputElement} */ (byId(id)).value = v === null || v === undefined ? "" : String(v);
        };
        set("preset-name", p.name);
        set("preset-prompt", p.prompt);
        set("preset-system-prompt", p.system_prompt);
        set("preset-system-prompt-position", p.system_prompt_position);
        set("preset-maximum-context", p.maximum_context);
        set("preset-auto-compact-threshold", p.auto_compact_threshold);
        set("preset-maximum-tool-output-tokens", p.maximum_tool_output_tokens);
        setPresetEditing(p.name);
        byId("preset-name").scrollIntoView({ block: "nearest" });
        byId("preset-name").focus({ preventScroll: true });
      }
      /**
       * Empties the editor panel back to a blank new preset.
       * @returns {void}
       */
      function clearPresetEditor() {
        for (const id of ["preset-name", "preset-prompt", "preset-system-prompt", "preset-system-prompt-position",
          "preset-maximum-context", "preset-auto-compact-threshold", "preset-maximum-tool-output-tokens"]) {
          /** @type {HTMLInputElement} */ (byId(id)).value = "";
        }
        setPresetEditing("");
      }
      /**
       * Registers (or updates) a preset from the editor panel's fields.
       * @returns {Promise<void>}
       */
      async function registerPreset() {
        const name = /** @type {HTMLInputElement} */ (byId("preset-name")).value.trim();
        const prompt = /** @type {HTMLInputElement} */ (byId("preset-prompt")).value.trim();
        const systemPrompt = /** @type {HTMLInputElement} */ (byId("preset-system-prompt")).value.trim();
        const systemPromptPosition = /** @type {HTMLInputElement} */ (byId("preset-system-prompt-position")).value.trim();
        const maximumContext = /** @type {HTMLInputElement} */ (byId("preset-maximum-context")).value.trim();
        const autoCompactThreshold = /** @type {HTMLInputElement} */ (byId("preset-auto-compact-threshold")).value.trim();
        const maximumToolOutputTokens = /** @type {HTMLInputElement} */ (byId("preset-maximum-tool-output-tokens")).value.trim();
        if (!name) { presetsError = "Preset name is required."; renderPresets(); return; }
        try {
          const r = await fetch("/api/presets", {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify({
              name,
              prompt: prompt || null,
              system_prompt: systemPrompt || null,
              system_prompt_position: systemPromptPosition || null,
              maximum_context: maximumContext === "" ? null : Number(maximumContext),
              auto_compact_threshold: autoCompactThreshold === "" ? null : Number(autoCompactThreshold),
              maximum_tool_output_tokens: maximumToolOutputTokens === "" ? null : Number(maximumToolOutputTokens),
            }),
          });
          presetsError = r.ok ? "" : await responseError(r, "register failed");
          if (r.ok) presetEditing = name;
        } catch (e) { presetsError = "daemon unreachable"; }
        await pollPresets();
        if (!presetsError) setPresetEditing(name);
      }
      /**
       * Deregisters a preset.
       * @param {string} name
       * @returns {Promise<void>}
       */
      async function removePreset(name) {
        if (!confirm(`Deregister preset "${name}"?\n\nA squad already submitted before removal keeps whatever values were already stamped into it -- only a future submission's "extends" referencing this name is affected.`)) return;
        try {
          const r = await fetch(`/api/presets/${encodeURIComponent(name)}`, { method: "DELETE" });
          presetsError = r.ok ? "" : await responseError(r, "remove failed");
          if (r.ok && presetEditing === name) setPresetEditing("");
        } catch (e) { presetsError = "daemon unreachable"; }
        await pollPresets();
      }
      // Chunk 80 routes the initial hash before this chunk has loaded, so it
      // leaves a `#/presets` deep link for here, once `pollPresets` exists.
      if (typeof pendingHash !== "undefined" && pendingHash && pendingHash.tab === "presets") {
        pendingHash = null;
        showTab("presets");
      }
