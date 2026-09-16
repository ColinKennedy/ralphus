      // ---- Presets tab (RAL-…) ----
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
       * Renders the Presets tab: the registered preset registry table plus
       * its register form.
       * @returns {void}
       */
      function renderPresets() {
        const err = presetsError
          ? `<div class="empty" style="color:var(--failed);margin-bottom:8px">${esc(presetsError)}</div>`
          : "";
        const el = byId("presets");
        preserveUserState(el, () => {
          el.innerHTML = err + renderPresetsSection();
        });
      }
      /**
       * Renders the preset registry table plus its register form.
       * @returns {string}
       */
      function renderPresetsSection() {
        const rows = presets.length
          ? presets.map((p) => presetRowHtml(p)).join("")
          : `<tr><td colspan="7" class="empty">No presets registered yet.</td></tr>`;
        const table = `<table class="proj-table"><thead><tr>`
          + `<th data-tip="The name an &quot;extends&quot; sentinel names: &lt;&lt;ralphus:presets/&lt;name&gt;&gt;&gt;.">Name</th>`
          + `<th data-tip="Stamped into an extending cell's own system_prompt field, only if that cell left it unset.">system_prompt</th>`
          + `<th data-tip="Stamped alongside system_prompt. The only accepted value is &quot;append&quot;.">system_prompt_position</th>`
          + `<th data-tip="Stamped into an extending task's or cell's own maximum_context field, only if left unset. Not applicable to a proof step.">maximum_context</th>`
          + `<th data-tip="Stamped into an extending task's or cell's own auto_compact_threshold field, only if left unset. Not applicable to a proof step.">auto_compact_threshold</th>`
          + `<th data-tip="Stamped into an extending task's, cell's, or proof step's own maximum_tool_output_tokens field, only if left unset.">maximum_tool_output_tokens</th>`
          + `<th></th>`
          + `</tr></thead><tbody>${rows}</tbody></table>`;
        const form = `<div style="margin-top:16px;padding-top:12px;border-top:1px solid var(--border)">
            <div style="margin-bottom:8px" data-tip="Register a preset so &quot;extends = [\\&quot;&lt;&lt;ralphus:presets/&lt;name&gt;&gt;&gt;\\&quot;]&quot; on a task, cell, or proof step can stamp these field values into any of that entity's own fields still unset at submit time.\nA field left blank here is simply not stamped by this preset. A field this preset sets that doesn't exist on the entity it's applied to (e.g. system_prompt on a task) is silently skipped.">Register a preset</div>
            <div class="row" style="gap:8px;flex-wrap:wrap;align-items:flex-start">
              <input id="preset-name" type="text" placeholder="name (e.g. commit_and_push)" style="width:200px" data-tip="Preset name -- what &lt;&lt;ralphus:presets/&lt;name&gt;&gt;&gt; names." />
              <input id="preset-system-prompt" type="text" placeholder="system_prompt" style="flex:1;min-width:260px" data-tip="Appended-system-prompt text stamped into an extending cell's own system_prompt field, only if that cell left it unset. Cell-level only; silently skipped on a task or proof step." />
              <input id="preset-system-prompt-position" type="text" placeholder="system_prompt_position (append)" style="width:180px" data-tip="Stamped alongside system_prompt. The only accepted value is &quot;append&quot;." />
            </div>
            <div class="row" style="gap:8px;flex-wrap:wrap;align-items:flex-start;margin-top:8px">
              <input id="preset-maximum-context" type="number" min="1" step="1" placeholder="maximum_context" style="width:160px" data-tip="Context-window token limit stamped into an extending task's or cell's own maximum_context field, only if left unset. Not applicable to a proof step." />
              <input id="preset-auto-compact-threshold" type="number" min="1" step="1" placeholder="auto_compact_threshold" style="width:180px" data-tip="Auto-compact trigger threshold stamped into an extending task's or cell's own auto_compact_threshold field, only if left unset. Not applicable to a proof step." />
              <input id="preset-maximum-tool-output-tokens" type="number" min="1" step="1" placeholder="maximum_tool_output_tokens" style="width:200px" data-tip="Per-tool-call output token cap stamped into an extending task's, cell's, or proof step's own maximum_tool_output_tokens field, only if left unset." />
              <button class="primary" onclick="registerPreset()" data-tip="Register this preset with the daemon.\nRe-registering an existing name updates its field values in place rather than creating a duplicate.">Register</button>
            </div>
          </div>`;
        return `<div>
            <div style="font-weight:600;margin-bottom:8px" data-tip="Every preset an &quot;extends&quot; sentinel can stamp field defaults from. A field an entity already set explicitly always wins over anything a preset would stamp; when more than one preset in the same extends list defines a field, the last one wins.">Presets</div>
            ${table}${form}
          </div>`;
      }
      /**
       * Renders one registered preset's table row.
       * @param {PresetView} p
       * @returns {string}
       */
      function presetRowHtml(p) {
        const cell = (/** @type {string|number|null} */ v) => v === null || v === undefined || v === ""
          ? `<span style="color:var(--muted)">—</span>`
          : esc(String(v));
        return `<tr>
            <td><span class="proj-name">${esc(p.name)}</span></td>
            <td>${cell(p.system_prompt)}</td>
            <td>${cell(p.system_prompt_position)}</td>
            <td>${cell(p.maximum_context)}</td>
            <td>${cell(p.auto_compact_threshold)}</td>
            <td>${cell(p.maximum_tool_output_tokens)}</td>
            <td><button class="btn" data-click="removePreset" data-name="${esc(p.name)}" data-tip="Deregister this preset.\nA squad already submitted before removal keeps whatever values were already stamped into it -- only a future submission's &quot;extends&quot; referencing this name is affected. This cannot be undone.">Remove</button></td>
          </tr>`;
      }
      /**
       * Registers (or updates) a preset from the register form's fields.
       * @returns {Promise<void>}
       */
      async function registerPreset() {
        const name = /** @type {HTMLInputElement} */ (byId("preset-name")).value.trim();
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
              system_prompt: systemPrompt || null,
              system_prompt_position: systemPromptPosition || null,
              maximum_context: maximumContext === "" ? null : Number(maximumContext),
              auto_compact_threshold: autoCompactThreshold === "" ? null : Number(autoCompactThreshold),
              maximum_tool_output_tokens: maximumToolOutputTokens === "" ? null : Number(maximumToolOutputTokens),
            }),
          });
          presetsError = r.ok ? "" : await responseError(r, "register failed");
        } catch (e) { presetsError = "daemon unreachable"; }
        await pollPresets();
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
        } catch (e) { presetsError = "daemon unreachable"; }
        await pollPresets();
      }
