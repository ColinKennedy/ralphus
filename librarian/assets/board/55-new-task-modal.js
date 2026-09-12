      // ---------- new task modal ----------
      // RAL-97: "Files" (drag-and-drop / browse, accepts multiple .toml
      // files, each queued as its own squad) and "Paste" (single TOML
      // pasted/edited inline). RAL-297 adds "Simple": a deterministic,
      // template-driven form that assembles its own TOML client-side (no
      // LLM call) and submits it through the same `POST /api/squads` the
      // other two tabs use.
      let ntTab = "simple";
      /**
       * Whether the New Task modal is the thing currently mounted into
       * `#modal-root` -- tracked so a Simple-tab generation call that's
       * still in flight when the user cancels doesn't pop the modal back
       * open (or land on the confirm step) once it resolves; see
       * `submitTaskSimple`'s guarded `renderNewTaskModal()` call.
       */
      let ntModalOpen = false;
      /**
       * @typedef {object} NtFile
       * @property {string} name
       * @property {number} size
       * @property {string} content
       * @property {string|null} err
       */
      /** @type {NtFile[]} files tab: loaded .toml files, each queued as its own squad */
      let ntFiles = [];
      let ntPasteToml = "";   // paste tab: preserved across tab switches
      let ntLabel = "";       // paste tab: optional squad label
      /**
       * Opens the New Task modal. The Simple tab's form is reset to its
       * defaults on every open, carrying forward only the fields worth
       * remembering across submissions -- see `ntSimpleResetKeepingProjectFields`.
       * @returns {void}
       */
      function openNewTask() {
        ntTab = ntConfigDefaultTab; ntFiles = []; ntPasteToml = ""; ntLabel = "";
        ntSimpleResetKeepingProjectFields();
        ntModalOpen = true;
        renderNewTaskModal();
        loadNtSimpleConfig();
        if (!projects.length) {
          pollProjects().then(() => { if (ntTab === "simple") renderNewTaskModal(); });
        }
      }
      /**
       * Renders the New Task modal (Simple, Files, or Paste tab).
       * @returns {void}
       */
      function renderNewTaskModal() {
        const body = ntTab === "simple" ? ntSimpleTabHtml() : ntTab === "files" ? ntFilesTabHtml() : ntPasteTabHtml();
        const tip = ntTab === "simple"
          ? "Fill in the form and queue a single deterministic squad — no TOML to write by hand."
          : ntTab === "files"
            ? "Validate every loaded file and queue each as its own squad."
            : "Validate the TOML and queue the squad.";
        byId("modal-root").innerHTML = `
          <div class="modal-bg" onclick="if(event.target===this)closeModal()"><div class="modal">
            <h2>New Squad</h2>
            <div class="modal-tabs">
              <button class="modal-tab ${ntTab === "simple" ? "active" : ""}" onclick="ntSwitchTab('simple')" data-tip="A guided form for the common case: one prompt, one agent/model, one project, an optional review.\nDeterministic — no LLM call is made just to build the submission (only the opt-in Generate buttons call an agent).">🧭 Simple</button>
              <button class="modal-tab ${ntTab === "files" ? "active" : ""}" onclick="ntSwitchTab('files')" data-tip="Load one or more .toml task files by dragging them in or browsing.\nUse this when the task is already saved as a file — each file is queued as its own separate squad.">📁 Files</button>
              <button class="modal-tab ${ntTab === "paste" ? "active" : ""}" onclick="ntSwitchTab('paste')" data-tip="Paste or type a single TOML task definition directly, instead of loading a file.\nUse this for a quick one-off task you haven't saved anywhere.">✒ Paste</button>
            </div>
            <div id="nt-body">${body}</div>
            <div id="nt-err" class="verr"></div>
            <div class="btn-row"><button class="btn" onclick="closeModal()" data-tip="Close without submitting.">Cancel</button><button class="btn primary" onclick="submitTask()" data-tip="${tip}\nThe daemon will start each as soon as a scheduler slot is available.">Validate &amp; Queue</button></div>
          </div></div>`;
      }
      /**
       * Switches the New Task modal's active tab, preserving the Paste tab's draft.
       * @param {string} tab
       * @returns {void}
       */
      function ntSwitchTab(tab) {
        const ta = /** @type {HTMLTextAreaElement|null} */ (document.getElementById("nt-toml"));
        if (ta) ntPasteToml = ta.value;
        const lbl = /** @type {HTMLInputElement|null} */ (document.getElementById("nt-label"));
        if (lbl) ntLabel = lbl.value;
        ntTab = tab;
        renderNewTaskModal();
      }
      /**
       * Closes whichever modal is currently open. If it was the New Task
       * modal with a Simple-tab generation call still in flight, that call's
       * agent subprocess is killed too (`ntCancelActiveGenerations`) --
       * Cancel means cancel, not just "stop showing me this."
       * @returns {void}
       */
      function closeModal() {
        if (ntModalOpen && ntTab === "simple" && ntSimple.activeGenerationIds.length) ntCancelActiveGenerations();
        byId("modal-root").innerHTML = "";
        ntModalOpen = false;
      }
      /**
       * Fires `POST /api/generate/{id}/cancel` for every generation job
       * `ntRunGenerationStep` currently has in flight, killing each one's
       * agent subprocess, and clears the tracking list. Fire-and-forget --
       * the modal is already closing, so there's nothing left to update on
       * the response.
       * @returns {void}
       */
      function ntCancelActiveGenerations() {
        const ids = ntSimple.activeGenerationIds;
        ntSimple.activeGenerationIds = [];
        ids.forEach((id) => { post(`/api/generate/${id}/cancel`).catch(() => {}); });
      }

      // ---------- new task modal: Simple tab (RAL-297) ----------
      // A deterministic, template-driven form: the five fixed base fields
      // (prompt/agent/model/project/proofs) plus whatever supplementary
      // fields the selected [[templates]] entry declares. Building the
      // submitted TOML is pure string substitution — no LLM call — except
      // the explicit, opt-in "Generate proof steps"/"Generate manual
      // checks"/"Generate auto-build steps" buttons, which call
      // POST /api/generate.
      const NT_INPUT_STYLE = "width:100%;background:var(--bg);border:1px solid var(--border);color:var(--text);border-radius:6px;padding:6px;margin-top:2px";
      // Suggests the ROSE shape (Reproduction, Observations, Solutions
      // considered, Expected result) without forcing it -- a free-text
      // hint for what makes a prompt easy for an agent to act on.
      const NT_PROMPT_PLACEHOLDER = "e.g. Reproduction: what leads to this&#10;Observations: what you've seen&#10;Solutions considered: what's been tried&#10;Expected result: what should happen instead";
      /**
       * @typedef {object} NtTemplateField
       * @property {string} name
       * @property {string} [label]
       * @property {string} [type]
       * @property {boolean} required
       */
      /**
       * @typedef {object} NtTemplate
       * @property {string} name
       * @property {string} [label]
       * @property {string} [description]
       * @property {NtTemplateField[]} fields
       * @property {string} [prompt_template]
       */
      /**
       * @typedef {object} NtCatalogAgent
       * @property {string} id
       * @property {string} kind
       * @property {string} backend
       * @property {string[]} models
       */
      /** @type {NtCatalogAgent[]} */
      const NT_FALLBACK_AGENTS = [
        { id: "claude", kind: "builtin", backend: "claude", models: [] },
        { id: "claude-code", kind: "builtin", backend: "claude-code", models: ["sonnet", "opus", "haiku", "fable"] },
        { id: "ollama", kind: "builtin", backend: "ollama", models: [] },
      ];
      /** @type {NtTemplate[]} loaded from GET /api/config/templates */
      let ntTemplates = [];
      /** whether `ntTemplates` is just the built-in fallback (zero valid [[templates]] configured) */
      let ntTemplatesFallback = true;
      /** the `.ralphus.toml [ui] new_task_default_tab` value, applied the next time the modal opens */
      let ntConfigDefaultTab = "simple";
      /** @type {NtCatalogAgent[]} loaded from GET /api/agents/catalog */
      let ntAgentCatalog = [];
      let ntAgentCatalogDefault = "claude";
      /**
       * @typedef {object} NtListItem
       * @property {string} label
       * @property {string} value
       */
      /**
       * The Simple tab's review combo box: `"explicit"` shows and submits a
       * hand-authored `[[review]]` block (manual checks + auto-build steps
       * editable up front); `"auto"` opts the work cell into Triage
       * (`triage = true`, no `triage_type` -- the Arbiter classifies it) so
       * the daemon pools and auto-creates a review later; `"none"` requests
       * no review at all.
       * @typedef {"explicit"|"auto"|"none"} NtReviewMode
       */
      /**
       * @typedef {object} NtSimpleState
       * @property {string} templateName
       * @property {string} label Optional, user-typed squad label/task-name
       *   basis (RAL-398). Left blank, the submit flow names the squad/task
       *   after a ticket id found in the prompt, or (failing that) an
       *   AI-suggested name applied once the background `suggest-name` job
       *   resolves -- see `ntResolveNaming`.
       * @property {string} prompt
       * @property {{[fieldName: string]: string}} fieldValues
       * @property {string} agent
       * @property {string} model
       * @property {string} project
       * @property {string} upstreamBranch
       * @property {boolean} proofs
       * @property {NtReviewMode} reviewMode
       * @property {boolean} generateManualChecks
       * @property {boolean} skipAutoBuild
       * @property {boolean} generateAutoBuild
       * @property {NtListItem[]} proofItems
       * @property {NtListItem[]} checkItems
       * @property {NtListItem[]} buildItems
       * @property {boolean} generating
       * @property {boolean} confirmStep
       * @property {NtFieldError[]} fieldErrors Set by the last failed submit
       *   attempt so the form can show each error inline next to its field;
       *   empty until then, and reset on every open/successful submit like
       *   every other non-carried-forward field.
       * @property {string[]} activeGenerationIds `POST /api/generate` job
       *   ids currently in flight (populated by `ntRunGenerationStep`,
       *   drained as each resolves) -- if the New Task modal is cancelled
       *   while any remain, `closeModal` fires `POST /api/generate/{id}/cancel`
       *   for each so the underlying agent subprocess is actually killed,
       *   not just abandoned.
       */
      /** @type {NtSimpleState} */
      let ntSimple;
      /**
       * Builds a fresh Simple-tab form state.
       * @returns {NtSimpleState}
       */
      function ntFreshSimpleState() {
        return {
          templateName: ntTemplates.length ? ntTemplates[0].name : NT_FALLBACK_TEMPLATE.name,
          label: "",
          prompt: "", fieldValues: {}, agent: ntAgentCatalogDefault, model: "",
          project: "", upstreamBranch: "", proofs: true, reviewMode: "auto", generateManualChecks: true,
          skipAutoBuild: true, generateAutoBuild: true,
          proofItems: [], checkItems: [], buildItems: [], generating: false, confirmStep: false,
          fieldErrors: [], activeGenerationIds: [],
        };
      }
      /**
       * Fully resets the Simple tab's form state to `ntFreshSimpleState()`.
       * @returns {void}
       */
      function ntSimpleReset() { ntSimple = ntFreshSimpleState(); }
      /**
       * Resets the Simple tab to its defaults, but carries forward
       * `templateName`/`agent`/`model`/`project`/`upstreamBranch` from the
       * current state first -- the only fields worth remembering across
       * submissions. Everything else (prompt, review mode, the proofs/
       * manual-checks/auto-build generation choices and their item lists,
       * the generating/confirm-step view state, ...) resets to a known
       * default every time: called both when the modal (re)opens
       * (`openNewTask`) and right after a successful submit
       * (`submitTaskSimple`), so neither a cancelled submission's leftover
       * generated items nor a completed one's leak into the next,
       * unrelated task.
       * @returns {void}
       */
      function ntSimpleResetKeepingProjectFields() {
        const prev = ntSimple;
        ntSimpleReset();
        if (prev) {
          ntSimple.templateName = prev.templateName;
          ntSimple.agent = prev.agent;
          ntSimple.model = prev.model;
          ntSimple.project = prev.project;
          ntSimple.upstreamBranch = prev.upstreamBranch;
        }
      }
      /**
       * Loads the Simple tab's template list and agent catalog, then
       * re-renders if the modal is still open on the Simple tab. Never
       * throws — a network failure just leaves the hardcoded fallbacks in
       * place, the same graceful-degradation pattern `fetchAgentOptions`
       * uses for the review-resolver dropdown.
       * @returns {Promise<void>}
       */
      async function loadNtSimpleConfig() {
        try {
          const [tplResp, catResp] = await Promise.all([fetch("/api/config/templates"), fetch("/api/agents/catalog")]);
          if (tplResp.ok) {
            const tpl = await tplResp.json();
            ntTemplates = tpl.templates || [];
            ntTemplatesFallback = !!tpl.using_fallback;
            ntConfigDefaultTab = ntResolveDefaultTab(tpl);
            if (!ntSimple.prompt && !Object.keys(ntSimple.fieldValues).length && ntTemplates.length) {
              ntSimple.templateName = ntTemplates[0].name;
            }
          }
          if (catResp.ok) {
            const cat = await catResp.json();
            ntAgentCatalog = cat.agents || [];
            ntAgentCatalogDefault = cat.default_agent || "claude";
            if (!ntSimple.agent) ntSimple.agent = ntAgentCatalogDefault;
          }
        } catch (e) { /* keep the hardcoded fallbacks; the form stays usable */ }
        if (ntTab === "simple") renderNewTaskModal();
      }
      // RALPHUS-SIMPLE-TAB:BEGIN
      // Pure decision logic for the Simple tab (RAL-297): template
      // resolution/fallback, field validation, the generic editable-list-
      // widget row operations shared by proof steps and manual checks, which
      // generation jobs a submit click should launch, the generate-then-
      // confirm vs. build-and-submit branch, and the config-driven default
      // tab. Deliberately free of DOM/fetch/module-level state so
      // test/board-simple-tab.mjs can slice this region out and exercise it
      // under `node --test` -- anything reaching for `document`/`fetch`
      // stays in ntSimpleTabHtml/ntRunGenerationStep/submitTaskSimple/
      // loadNtSimpleConfig, which call into these.

      /** @type {NtTemplate} */
      const NT_FALLBACK_TEMPLATE = {
        name: "hello-world", label: "(Built-in)",
        description: "Minimal one-shot task: run a prompt as-is, no extra context.",
        fields: [], prompt_template: "{prompt}",
      };
      /**
       * Resolves the selected template, falling back to the built-in one
       * when `templates` is empty or `templateName` isn't found in it.
       * @param {NtTemplate[]} templates
       * @param {string} templateName
       * @returns {NtTemplate}
       */
      function ntPickTemplate(templates, templateName) {
        const list = templates.length ? templates : [NT_FALLBACK_TEMPLATE];
        return list.find((t) => t.name === templateName) || list[0];
      }
      /**
       * Substitutes `{prompt}` and each declared field's `{name}` into a
       * template's `prompt_template`.
       * @param {NtTemplate} template
       * @param {string} prompt
       * @param {{[fieldName: string]: string}} fieldValues
       * @returns {string}
       */
      function ntBuildEffectivePrompt(template, prompt, fieldValues) {
        let text = template.prompt_template || "{prompt}";
        text = text.split("{prompt}").join(prompt);
        (template.fields || []).forEach((f) => { text = text.split(`{${f.name}}`).join(fieldValues[f.name] || ""); });
        return text;
      }
      /**
       * One field-level validation failure: `field` is a stable key the form
       * uses to show the message inline next to the offending field --
       * `"prompt"`, `"agent"`, `"project"`, or `"field:<templateFieldName>"`
       * for a template's own supplementary field.
       * @typedef {object} NtFieldError
       * @property {string} field
       * @property {string} message
       */
      /**
       * Client-side field validation (type/required) against a resolved
       * template -- separate from the daemon's own TOML validator, which
       * only ever sees the already-assembled TOML. Structured (rather than
       * plain message strings) so the form can show each error inline next
       * to its field, not just in the shared error panel.
       * @param {NtSimpleState} state
       * @param {NtTemplate} template
       * @returns {NtFieldError[]} empty when valid
       */
      function ntValidateSimpleFields(state, template) {
        /** @type {NtFieldError[]} */
        const errors = [];
        if (!state.prompt.trim()) errors.push({ field: "prompt", message: "Prompt is required." });
        (template.fields || []).forEach((f) => {
          const v = (state.fieldValues[f.name] || "").trim();
          const label = f.label || f.name;
          const field = `field:${f.name}`;
          if (f.required && !v) errors.push({ field, message: `"${label}" is required.` });
          if (v && f.type === "number" && Number.isNaN(Number(v))) errors.push({ field, message: `"${label}" must be a number.` });
        });
        if (!state.agent) errors.push({ field: "agent", message: "Agent is required." });
        if (!state.project) errors.push({ field: "project", message: "Project is required." });
        return errors;
      }
      /**
       * Appends a blank row.
       * @param {NtListItem[]} items
       * @returns {NtListItem[]}
       */
      function ntListInsertRow(items) { return items.concat([{ label: "", value: "" }]); }
      /**
       * Removes row `i`.
       * @param {NtListItem[]} items
       * @param {number} i
       * @returns {NtListItem[]}
       */
      function ntListRemoveRow(items, i) { return items.slice(0, i).concat(items.slice(i + 1)); }
      /**
       * Swaps row `i` with its neighbor `i + delta`; a no-op (same array) if
       * the neighbor is out of range.
       * @param {NtListItem[]} items
       * @param {number} i
       * @param {number} delta
       * @returns {NtListItem[]}
       */
      function ntListSwapRows(items, i, delta) {
        const j = i + delta;
        if (j < 0 || j >= items.length) return items;
        const copy = items.slice();
        const tmp = copy[i]; copy[i] = copy[j]; copy[j] = tmp;
        return copy;
      }
      /**
       * Replaces one field of row `i`.
       * @param {NtListItem[]} items
       * @param {number} i
       * @param {"label"|"value"} field
       * @param {string} value
       * @returns {NtListItem[]}
       */
      function ntListEditRow(items, i, field, value) {
        return items.map((it, idx) => {
          if (idx !== i) return it;
          return field === "label" ? { label: value, value: it.value } : { label: it.label, value: value };
        });
      }
      /**
       * Which `/api/generate` kinds a submit click should launch, per the
       * proofs/generate-manual-checks/generate-auto-build-steps checkboxes.
       * Manual checks and auto-build steps only ever plan when
       * `reviewMode` is `"explicit"` -- Auto Review and No Review never
       * show (or submit) a hand-authored `[[review]]` block at all.
       * @param {NtSimpleState} state
       * @returns {("proof_steps"|"manual_checks"|"auto_build_steps")[]}
       */
      function ntPlannedGenerationKinds(state) {
        /** @type {("proof_steps"|"manual_checks"|"auto_build_steps")[]} */
        const kinds = [];
        if (state.proofs) kinds.push("proof_steps");
        if (state.reviewMode === "explicit" && state.generateManualChecks) kinds.push("manual_checks");
        if (state.reviewMode === "explicit" && !state.skipAutoBuild && state.generateAutoBuild) kinds.push("auto_build_steps");
        return kinds;
      }
      /**
       * Whether a submit click should run generation-then-confirm first, or
       * go straight to build-and-submit: generation runs once per opted-in
       * kind, then a second click (now `confirmStep`) submits.
       * @param {NtSimpleState} state
       * @returns {"generate"|"submit"}
       */
      function ntSimpleSubmitAction(state) {
        return ntPlannedGenerationKinds(state).length > 0 && !state.confirmStep ? "generate" : "submit";
      }
      /**
       * Resolves the New Task modal's default tab from the parsed
       * `GET /api/config/templates` response, defaulting to "simple" when
       * the field is absent (including when the fetch itself failed).
       * @param {{default_new_task_tab?: string}|undefined} payload
       * @returns {string}
       */
      function ntResolveDefaultTab(payload) { return (payload && payload.default_new_task_tab) || "simple"; }
      /**
       * Sanitizes a Simple-tab squad label for `POST /api/squads`: truncates
       * to 60 chars, strips the one character the daemon actually rejects a
       * label for (a comma -- squad labels are split on `,` for multi-name
       * filtering, see `reject_label_with_comma` in `daemon/src/server.rs`),
       * then trims. Returns `null` when nothing meaningful survives, so the
       * daemon falls back to showing the squad id instead of an empty label.
       * @param {string} prompt
       * @returns {string|null}
       */
      function ntSanitizeSquadLabel(prompt) {
        const cleaned = prompt.slice(0, 60).replace(/,/g, "").trim();
        return cleaned || null;
      }
      // RAL-398: a ticket-id-shaped token found in the prompt (JIRA-style --
      // a short alpha prefix, a separator, a number, optionally more
      // separator-joined words) is a strong, free, zero-latency naming
      // signal -- deliberately permissive (case-insensitive, `-`/`_`
      // interchangeable) since a submitter isn't going to reformat their
      // prompt to make the ticket easier to find.
      const TICKET_ID_RE = /\b[a-z]{2,10}[-_]\d{2,8}(?:[-_][a-z0-9]+)*\b/i;
      /**
       * The first ticket-id-shaped token in `text` (e.g. `"PIPE-5163"`,
       * `"dev_1234"`, `"FOO-980713-some_description"`), or `null` if none is
       * found.
       * @param {string} text
       * @returns {string|null}
       */
      function extractTicketId(text) {
        const m = TICKET_ID_RE.exec(text);
        return m ? m[0] : null;
      }
      /**
       * Sanitizes free text into a name safe to store as a task's `name`
       * (and use as part of a git branch): lowercased, runs of anything
       * other than `[a-z0-9]` collapsed to a single `-`, leading/trailing
       * `-` trimmed, capped at 60 chars. Mirrors `slugify_task_name` in
       * `daemon/src/generation.rs` (the same rules, applied to an
       * AI-suggested name instead of user/ticket text) -- kept as a
       * separate implementation since the two run in different runtimes,
       * not as a shared module.
       * @param {string} s
       * @returns {string}
       */
      function slugifyTaskName(s) {
        const slug = s.toLowerCase().replace(/[^a-z0-9]+/g, "-").replace(/^-+|-+$/g, "");
        return slug.slice(0, 60);
      }
      /**
       * @typedef {object} NtNaming
       * @property {string|null} taskName A slug ready to use as the task's
       *   `name`, or `null` when neither a typed label nor a ticket id was
       *   available and an AI suggestion must be requested after submit.
       * @property {string|null} squadLabel The squad label to submit
       *   (verbatim typed text or the raw ticket id), or `null` when it must
       *   wait on the same AI suggestion as `taskName`.
       * @property {boolean} needsGeneration Whether the Simple tab must fire
       *   `POST .../suggest-name` after submit.
       */
      /**
       * Resolves the Simple tab's task name / squad label (RAL-398): a
       * typed label wins outright; failing that, a ticket-id-shaped token in
       * the prompt (found instantly, no LLM call); failing that, both are
       * left unresolved so the caller falls back to a placeholder name and
       * asks the daemon to suggest one in the background.
       * @param {string} prompt
       * @param {string} label
       * @returns {NtNaming}
       */
      function ntResolveNaming(prompt, label) {
        const typedLabel = label.trim();
        if (typedLabel) {
          return { taskName: slugifyTaskName(typedLabel), squadLabel: typedLabel, needsGeneration: false };
        }
        const ticket = extractTicketId(prompt);
        if (ticket) {
          return { taskName: slugifyTaskName(ticket), squadLabel: ticket, needsGeneration: false };
        }
        return { taskName: null, squadLabel: null, needsGeneration: true };
      }
      // RALPHUS-SIMPLE-TAB:END
      // `ntSimple`'s initial value is assigned here, after the marked region
      // above, rather than at its `let` declaration near `ntFreshSimpleState`.
      // That declaration runs as top-level script execution reaches it,
      // which -- if it called `ntFreshSimpleState()` immediately, as it used
      // to -- would read `NT_FALLBACK_TEMPLATE.name` before this same
      // top-level script's `const NT_FALLBACK_TEMPLATE = ...` (declared
      // further down, inside the marked region) had executed: a temporal-
      // dead-zone `ReferenceError` that aborted every subsequent top-level
      // statement in this script, including the initial poll/render calls
      // the Reviews and Projects tabs depend on -- not just the new-task
      // modal. Deferring the call to here, after the region that defines
      // `NT_FALLBACK_TEMPLATE` has run, fixes that without moving
      // `NT_FALLBACK_TEMPLATE` itself out of the marked region (it's part of
      // `exported` in test/board-simple-tab.mjs).
      ntSimple = ntFreshSimpleState();
      /**
       * The currently-selected template (falls back to the built-in one).
       * @returns {NtTemplate}
       */
      function ntSelectedTemplate() { return ntPickTemplate(ntTemplates, ntSimple.templateName); }
      /**
       * Substitutes `{prompt}` and each declared field's `{name}` into the
       * selected template's `prompt_template`.
       * @returns {string}
       */
      function ntSimpleEffectivePrompt() {
        return ntBuildEffectivePrompt(ntSelectedTemplate(), ntSimple.prompt, ntSimple.fieldValues);
      }
      /**
       * Client-side field validation (type/required), run before generation
       * and before final submission -- separate from the daemon's own TOML
       * validator, which only ever sees the already-assembled TOML.
       * @returns {NtFieldError[]} empty when valid
       */
      function ntSimpleValidateFields() { return ntValidateSimpleFields(ntSimple, ntSelectedTemplate()); }
      /**
       * Submit-time-only check that a typed upstream branch actually exists
       * for the selected git project (RAL-297 interview decision: checked
       * at submit, not live-as-you-type). Never blocks on a lookup failure
       * -- an unreachable daemon shouldn't prevent submission any harder
       * than the eventual worktree-materialization failure already would.
       * @returns {Promise<string|null>} an error message, or null if OK
       */
      async function ntSimpleValidateUpstream() {
        const branch = ntSimple.upstreamBranch.trim();
        if (!branch) return null;
        const project = projects.find((p) => p.name === ntSimple.project);
        if (!project || project.vcs !== "git") return null;
        try {
          const resp = await fetch(`/api/projects/${encodeURIComponent(ntSimple.project)}/branches`);
          if (!resp.ok) return null;
          const data = await resp.json();
          /** @type {string[]} */
          const branches = data.branches || [];
          if (!branches.includes(branch) && !branches.includes(`origin/${branch}`)) {
            return `Upstream branch "${branch}" was not found in project "${ntSimple.project}".`;
          }
        } catch (e) { return null; }
        return null;
      }
      /**
       * Quotes and escapes a string as a single-line TOML basic string,
       * including embedded newlines/tabs -- always valid regardless of the
       * user's own text, unlike a `"""`-delimited literal (which would need
       * to worry about an embedded `"""` sequence).
       * @param {string} s
       * @returns {string}
       */
      function tomlStr(s) {
        const escaped = String(s)
          .replace(/\\/g, "\\\\")
          .replace(/"/g, '\\"')
          .replace(/\r/g, "")
          .replace(/\n/g, "\\n")
          .replace(/\t/g, "\\t");
        return `"${escaped}"`;
      }
      // The system prompts below are reused verbatim from `ralphus task
      // show-tutor`'s recommended work+finalize layout (cli/src/tutor.rs)
      // so a Simple submission's generated cells match what that tutorial
      // teaches a user to write by hand.
      const NT_NO_COMMIT_SYSTEM_PROMPT = "Do NOT commit and do NOT push under any circumstances.";
      const NT_FINALIZE_TEXT = "ONLY git stage the relevant source files, commit them, and push the commit if a remote exists.";
      // Mirrors `core::schema::agent_supports_system_prompt` -- the daemon's
      // TOML validator hard-rejects `system_prompt`/`system_prompt_position`
      // for any other *known* builtin agent name (e.g. "claude", "ollama"),
      // both of which the agent picker can offer. An unrecognized value
      // (a custom `[agent.profiles.*]` name) is treated the same as
      // "unsupported" here too -- conservative but always submittable,
      // since core itself only defers (rather than allows) that case once
      // the daemon resolves the profile's real backend.
      const NT_SYSTEM_PROMPT_CAPABLE_AGENTS = ["claude-code", "claude-cli", "codex", "codex-cli", "pi"];
      /**
       * Appends `system_prompt`/`system_prompt_position` lines when `agent`
       * supports appended system prompts; otherwise prepends the same text
       * onto `prompt` so the instruction still reaches an agent with no
       * such delivery mechanism.
       * @param {string[]} lines
       * @param {string} agent
       * @param {string} systemPrompt
       * @param {string} prompt
       * @returns {string}
       */
      function ntPushSystemPromptOrInline(lines, agent, systemPrompt, prompt) {
        if (NT_SYSTEM_PROMPT_CAPABLE_AGENTS.includes(agent)) {
          lines.push(`system_prompt = ${tomlStr(systemPrompt)}`);
          lines.push('system_prompt_position = "append"');
          return prompt;
        }
        return `${systemPrompt}\n\n${prompt}`;
      }
      /**
       * Assembles the Simple tab's task TOML: a project-scoped placeholder
       * worktree `cwd`, a `work` cell (template-substituted prompt, agent,
       * model, optional proof steps) and a `finalize` cell -- mirroring
       * `ralphus task show-tutor`'s recommended per-branch layout. Per
       * `reviewMode`: `"explicit"` sets `review = "<<ralphus:new-review/
       * simple>>"` on the work cell and emits a `[[review]]` block (with
       * `skip_auto_build`/`[[review.auto_build]]` and manual-check
       * `[[review.action]]` entries); `"auto"` sets `triage = true` on the
       * work cell instead, with no `[[review]]` block (the daemon's Arbiter
       * pools and auto-creates the review later); `"none"` sets neither.
       * @returns {{toml: string, naming: NtNaming, taskName: string, fallbackName: string}}
       *   the assembled TOML plus the naming decision `submitTaskSimple`
       *   needs to pick the squad label and decide whether to fire
       *   `POST .../suggest-name` after submit (RAL-398). `taskName` is
       *   what's actually in the TOML (a placeholder when generation is
       *   still needed); `fallbackName` is the safe, non-placeholder name
       *   to pass as `suggest-name`'s `fallback_name`.
       */
      function ntSimpleBuildToml() {
        const branch = `simple-${Date.now().toString(36)}${Math.random().toString(36).slice(2, 6)}`;
        // `parse_worktree_placeholder_upstream` (core/src/schema.rs) does raw
        // string splitting on this query suffix, not URL-decoding -- percent-
        // encoding `upstream` here would corrupt the `<<default>>` sentinel
        // into a literal (and invalid) branch name, so it's embedded as-is.
        const upstream = ntSimple.upstreamBranch.trim() || "<<default>>";
        const cwd = `<<ralphus:new-worktree/${branch}?upstream=${upstream}>>`;
        const prompt = ntSimpleEffectivePrompt();
        // RAL-398: the task's display `name` is resolved independently of
        // `branch` (which stays the fast, always-unique, purely internal
        // worktree slug) -- a ticket id or typed label names it instantly;
        // otherwise `pending-name-<branch>` is a recognizable-as-temporary
        // placeholder (see `ntTaskDisplayName`) until the post-submit
        // `suggest-name` background job renames it for real. `fallbackName`
        // (plain `branch`, no `pending-name-` prefix) is what the daemon
        // renames to instead if that background call fails -- it must NOT
        // reuse the placeholder text, or a failed rename would still match
        // `ntTaskDisplayName`'s check and show "generating…" forever even
        // though nothing is generating anymore.
        const naming = ntResolveNaming(ntSimple.prompt, ntSimple.label);
        const taskName = naming.taskName || `pending-name-${branch}`;
        const fallbackName = naming.taskName || branch;
        /** @type {string[]} */
        const lines = [];
        lines.push("[[task]]");
        lines.push(`name = ${tomlStr(taskName)}`);
        lines.push(`project = ${tomlStr(ntSimple.project)}`);
        lines.push("");
        lines.push("[[task.cell]]");
        lines.push('id = "work"');
        lines.push(`agent = ${tomlStr(ntSimple.agent)}`);
        if (ntSimple.model.trim()) lines.push(`model = ${tomlStr(ntSimple.model.trim())}`);
        lines.push(`cwd = ${tomlStr(cwd)}`);
        if (ntSimple.reviewMode === "explicit") lines.push('review = "<<ralphus:new-review/simple>>"');
        if (ntSimple.reviewMode === "auto") lines.push("triage = true");
        const workPrompt = ntPushSystemPromptOrInline(lines, ntSimple.agent, NT_NO_COMMIT_SYSTEM_PROMPT, prompt);
        lines.push(`prompt = ${tomlStr(workPrompt)}`);
        ntSimple.proofItems.filter((it) => it.value.trim()).forEach((it) => {
          lines.push("");
          lines.push("[[task.cell.proof]]");
          if (it.label.trim()) lines.push(`id = ${tomlStr(it.label.trim())}`);
          lines.push(`command = ${tomlStr(it.value.trim())}`);
        });
        lines.push("");
        lines.push("[[task.cell]]");
        lines.push('id = "finalize"');
        lines.push(`agent = ${tomlStr(ntSimple.agent)}`);
        if (ntSimple.model.trim()) lines.push(`model = ${tomlStr(ntSimple.model.trim())}`);
        lines.push(`cwd = ${tomlStr(cwd)}`);
        lines.push('depends_on = ["work"]');
        if (NT_SYSTEM_PROMPT_CAPABLE_AGENTS.includes(ntSimple.agent)) {
          lines.push(`system_prompt = ${tomlStr(NT_FINALIZE_TEXT)}`);
          lines.push('system_prompt_position = "append"');
        }
        lines.push(`prompt = ${tomlStr(NT_FINALIZE_TEXT)}`);
        if (ntSimple.reviewMode === "explicit") {
          lines.push("");
          lines.push("[[review]]");
          lines.push('id = "ralphus:new-review/simple"');
          const buildSteps = ntSimple.buildItems.filter((it) => it.value.trim());
          if (ntSimple.skipAutoBuild) {
            lines.push("skip_auto_build = true");
          } else {
            buildSteps.forEach((it) => {
              lines.push("");
              lines.push("[[review.auto_build]]");
              lines.push(`command = ${tomlStr(it.value.trim())}`);
            });
          }
          ntSimple.checkItems.filter((it) => it.value.trim()).forEach((it) => {
            lines.push("");
            lines.push("[[review.action]]");
            lines.push(`label = ${tomlStr(it.label.trim() || "Manual check")}`);
            lines.push(`prompt = ${tomlStr(it.value.trim())}`);
          });
        }
        return { toml: `${lines.join("\n")}\n`, naming, taskName, fallbackName };
      }
      // ---------- generic editable-list-widget primitive (RAL-297) ----------
      // Reused as-is for proof steps, manual checks, and auto-build steps --
      // the only differences between the three call sites are the
      // placeholder/tooltip copy and the `singleField` flag passed in
      // `opts`, never the widget's own markup or behavior.
      /**
       * The state key backing a given list kind.
       * @param {"proofs"|"checks"|"builds"} kind
       * @returns {"proofItems"|"checkItems"|"buildItems"}
       */
      function ntListKey(kind) { return kind === "proofs" ? "proofItems" : kind === "checks" ? "checkItems" : "buildItems"; }
      /**
       * @param {"proofs"|"checks"|"builds"} kind
       * @returns {NtListItem[]}
       */
      function ntListRef(kind) { return ntSimple[ntListKey(kind)]; }
      /**
       * Adds an empty row to the given list and re-renders.
       * @param {"proofs"|"checks"|"builds"} kind
       * @returns {void}
       */
      function ntListAdd(kind) { ntSimple[ntListKey(kind)] = ntListInsertRow(ntListRef(kind)); renderNewTaskModal(); }
      /**
       * Removes row `i` from the given list and re-renders.
       * @param {"proofs"|"checks"|"builds"} kind
       * @param {number} i
       * @returns {void}
       */
      function ntListRemove(kind, i) { ntSimple[ntListKey(kind)] = ntListRemoveRow(ntListRef(kind), i); renderNewTaskModal(); }
      /**
       * Swaps row `i` with its neighbor `i + delta` and re-renders.
       * @param {"proofs"|"checks"|"builds"} kind
       * @param {number} i
       * @param {number} delta
       * @returns {void}
       */
      function ntListMove(kind, i, delta) { ntSimple[ntListKey(kind)] = ntListSwapRows(ntListRef(kind), i, delta); renderNewTaskModal(); }
      /**
       * Edits one field of row `i` in the given list (no re-render needed --
       * plain text input, same pattern as the Files tab's inline textarea).
       * @param {"proofs"|"checks"|"builds"} kind
       * @param {number} i
       * @param {"label"|"value"} field
       * @param {string} value
       * @returns {void}
       */
      function ntListEdit(kind, i, field, value) { ntSimple[ntListKey(kind)] = ntListEditRow(ntListRef(kind), i, field, value); }
      /**
       * @typedef {object} NtListWidgetOpts
       * @property {string} [labelPlaceholder] Unused when `singleField` is true.
       * @property {string} valuePlaceholder
       * @property {string} addLabel
       * @property {string} [labelTip] Unused when `singleField` is true.
       * @property {string} valueTip
       * @property {boolean} [singleField] When true, renders only the value
       *   column (full width) -- used for auto-build steps, which have no
       *   `label`-equivalent field in `AutoBuildDef` (unlike a proof step's
       *   `id` or a manual check's button label).
       */
      /**
       * Renders a generic editable list of `{label, value}` rows: add,
       * remove, reorder (via up/down buttons rather than drag-and-drop, to
       * keep the widget's state trivially serializable), and edit-in-place.
       * @param {NtListItem[]} items
       * @param {"proofs"|"checks"|"builds"} kind
       * @param {NtListWidgetOpts} opts
       * @returns {string}
       */
      function ntListWidgetHtml(items, kind, opts) {
        const rows = items.map((it, i) => `
          <div class="row" style="gap:4px;margin-bottom:4px">
            ${opts.singleField ? "" : `<input style="flex:1" value="${esc(it.label)}" placeholder="${esc(opts.labelPlaceholder)}" oninput="ntListEdit('${kind}', ${i}, 'label', this.value)" data-tip="${esc(opts.labelTip)}">`}
            <input style="flex:2" value="${esc(it.value)}" placeholder="${esc(opts.valuePlaceholder)}" oninput="ntListEdit('${kind}', ${i}, 'value', this.value)" data-tip="${esc(opts.valueTip)}">
            <button class="btn" style="padding:1px 6px" ${i === 0 ? "disabled" : ""} onclick="ntListMove('${kind}', ${i}, -1)" data-tip="Move this row up.">▲</button>
            <button class="btn" style="padding:1px 6px" ${i === items.length - 1 ? "disabled" : ""} onclick="ntListMove('${kind}', ${i}, 1)" data-tip="Move this row down.">▼</button>
            <button class="btn" style="padding:1px 6px" onclick="ntListRemove('${kind}', ${i})" data-tip="Remove this row from the list.\nThis cannot be undone, but you can always add a new row.">✕</button>
          </div>`).join("");
        return `${rows}<button class="btn" onclick="ntListAdd('${kind}')" data-tip="${esc(opts.addLabel)}">+ Add</button>`;
      }
      // ---------- generic "generation step" primitive (RAL-297) ----------
      /**
       * Kicks off one opt-in generation step (`POST /api/generate`) and
       * polls `GET /api/generate/{id}` until it resolves. Reused as-is for
       * "Generate proof steps", "Generate manual checks", and "Generate
       * auto-build steps" -- only `kind` differs between the call sites.
       *
       * Tracks the job id in `ntSimple.activeGenerationIds` while it's in
       * flight (removed in `finally`, regardless of outcome) so `closeModal`
       * can kill its agent subprocess via `POST /api/generate/{id}/cancel`
       * if the New Task modal is dismissed before this resolves -- "Cancel"
       * must actually stop the agent, not just abandon the poll.
       * @param {"proof_steps"|"manual_checks"|"auto_build_steps"} kind
       * @returns {Promise<NtListItem[]|null>} the proposed items, or null on any failure (never throws)
       */
      async function ntRunGenerationStep(kind) {
        const project = projects.find((p) => p.name === ntSimple.project);
        const cwd = project ? project.path : ".";
        /** @type {string|null} */
        let id = null;
        try {
          const startResp = await fetch("/api/generate", {
            method: "POST",
            body: JSON.stringify({
              kind, cwd, agent: ntSimple.agent,
              model: ntSimple.model.trim() || undefined,
              prompt_context: ntSimpleEffectivePrompt(),
            }),
          });
          if (!startResp.ok) return null;
          ({ id } = await startResp.json());
          if (!id) return null;
          ntSimple.activeGenerationIds.push(id);
          for (let i = 0; i < 80; i++) {
            await new Promise((resolve) => setTimeout(resolve, 1500));
            const poll = await fetch(`/api/generate/${id}`);
            if (!poll.ok) return null;
            const job = await poll.json();
            if (job.status === "done") return job.items || [];
            if (job.status === "error") return null;
          }
          return null;
        } catch (e) { return null; } finally {
          if (id) ntSimple.activeGenerationIds = ntSimple.activeGenerationIds.filter((activeId) => activeId !== id);
        }
      }
      /**
       * The message from `ntSimple.fieldErrors` for `field`, or `""` if that
       * field has no outstanding error (including before the first submit
       * attempt, when the list is always empty).
       * @param {string} field
       * @returns {string}
       */
      function ntFieldErrorText(field) {
        const e = ntSimple.fieldErrors.find((fe) => fe.field === field);
        return e ? e.message : "";
      }
      /**
       * Renders `field`'s inline error message (if any) as a small red line,
       * meant to sit directly under that field's input. Tagged with
       * `data-field-error` so `ntClearFieldErrorInline` can remove it
       * without a full re-render.
       * @param {string} field
       * @returns {string}
       */
      function ntFieldErrorHtml(field) {
        const msg = ntFieldErrorText(field);
        return msg ? `<div class="verr" data-field-error="${esc(field)}" style="margin:2px 0 0">${esc(msg)}</div>` : "";
      }
      /**
       * Drops `field`'s entry from `ntSimple.fieldErrors`, if any -- called
       * whenever the user changes a field that previously failed validation,
       * so a fixed field doesn't keep showing a stale error until the next
       * submit attempt.
       * @param {string} field
       * @returns {void}
       */
      function ntClearFieldError(field) {
        ntSimple.fieldErrors = ntSimple.fieldErrors.filter((fe) => fe.field !== field);
      }
      /**
       * Same as `ntClearFieldError`, but for a field whose input doesn't
       * re-render on every keystroke (the prompt textarea, a template's own
       * text field) -- typing there must still make a shown error disappear
       * immediately, so this also edits the DOM directly: drops the `.err`
       * border class from `el` and removes its sibling error message (found
       * by `data-field-error`, written by `ntFieldErrorHtml`) rather than
       * waiting for the next full render.
       * @param {string} field
       * @param {HTMLElement} el
       * @returns {void}
       */
      function ntClearFieldErrorInline(field, el) {
        ntClearFieldError(field);
        el.classList.remove("err");
        // The error message renders as a sibling *after* the field's own
        // <label> (see ntFieldErrorHtml's call sites), not nested inside
        // it, so this looks it up from the document rather than el's own
        // parent.
        const errDiv = document.querySelector(`[data-field-error="${field}"]`);
        if (errDiv) errDiv.remove();
      }
      /**
       * Renders the Simple tab's form -- the template picker, prompt,
       * agent/model, project (+ upstream branch when git), proofs/review
       * checkboxes and their editable lists -- or, mid-generation/-confirm,
       * the generation-progress notice or the confirm-and-edit step.
       * @returns {string}
       */
      function ntSimpleTabHtml() {
        if (ntSimple.generating) {
          const parts = [];
          if (ntSimple.proofs) parts.push("proof steps");
          if (ntSimple.reviewMode === "explicit" && !ntSimple.skipAutoBuild && ntSimple.generateAutoBuild) parts.push("auto-build steps");
          if (ntSimple.reviewMode === "explicit" && ntSimple.generateManualChecks) parts.push("manual checks");
          return `<p style="color:var(--muted);font-size:13px">Generating ${parts.join(" and ")}… this calls the selected agent, so it may take a little while.</p>`;
        }
        if (ntSimple.confirmStep) {
          const proofsSection = ntSimple.proofs
            ? `<h4 style="margin:10px 0 4px">Proof steps</h4>${ntListWidgetHtml(ntSimple.proofItems, "proofs", {
                labelPlaceholder: "id", valuePlaceholder: "shell command",
                addLabel: "Add a proof step to run after the work cell — a command that must pass.",
                labelTip: "A short id for this proof step.", valueTip: "The shell command this proof step runs.",
              })}`
            : "";
          const checksSection = ntSimple.reviewMode === "explicit" && ntSimple.generateManualChecks
            ? `<h4 style="margin:10px 0 4px">Manual checks</h4>${ntListWidgetHtml(ntSimple.checkItems, "checks", {
                labelPlaceholder: "label", valuePlaceholder: "what to check",
                addLabel: "Add a manual check button reviewers will see on this review.",
                labelTip: "The manual check's button label.", valueTip: "What a reviewer should check or try.",
              })}`
            : "";
          const buildsSection = ntSimple.reviewMode === "explicit" && !ntSimple.skipAutoBuild && ntSimple.generateAutoBuild
            ? `<h4 style="margin:10px 0 4px">Auto-build steps</h4>${ntListWidgetHtml(ntSimple.buildItems, "builds", {
                singleField: true, valuePlaceholder: "shell command",
                addLabel: "Add a build command run when this review's branches merge.",
                valueTip: "The shell command this build step runs.",
              })}`
            : "";
          return `<p style="color:var(--muted);font-size:13px">Review the generated items below — edit or remove any you don't want — then submit.</p>${proofsSection}${buildsSection}${checksSection}`;
        }
        const templates = ntTemplates.length ? ntTemplates : [NT_FALLBACK_TEMPLATE];
        const t = ntSelectedTemplate();
        const templateOptions = templates.map((tpl) => `<option value="${esc(tpl.name)}" ${tpl.name === t.name ? "selected" : ""}>${esc(tpl.label || tpl.name)}</option>`).join("");
        const fieldRows = (t.fields || []).map((f) => `
          <label style="display:block;font-size:12px;color:var(--muted);margin-top:8px" data-tip="${esc(f.label || f.name)}${f.required ? " (required)" : " (optional)"} — a supplementary field for the &quot;${esc(t.label || t.name)}&quot; template, substituted into the generated prompt.">
            ${esc(f.label || f.name)}${f.required ? " *" : ""}
            <input class="${ntFieldErrorText(`field:${f.name}`) ? "err" : ""}" style="${NT_INPUT_STYLE}" value="${esc(ntSimple.fieldValues[f.name] || "")}" oninput="ntSimple.fieldValues[${JSON.stringify(f.name)}]=this.value;ntClearFieldErrorInline(${JSON.stringify(`field:${f.name}`)}, this)">
          </label>${ntFieldErrorHtml(`field:${f.name}`)}`).join("");
        const catalog = ntAgentCatalog.length ? ntAgentCatalog : NT_FALLBACK_AGENTS;
        const sortedCatalog = [...catalog].sort((a, b) => a.id.localeCompare(b.id));
        const agentOptions = sortedCatalog.map((a) => `<option value="${esc(a.id)}" ${a.id === ntSimple.agent ? "selected" : ""}>${esc(a.id)}${a.id === ntAgentCatalogDefault ? " (default)" : ""}</option>`).join("");
        const selectedAgent = catalog.find((a) => a.id === ntSimple.agent);
        const modelDatalist = ((selectedAgent && selectedAgent.models) || []).map((m) => `<option value="${esc(m)}">`).join("");
        const projectOptions = projects.map((p) => `<option value="${esc(p.name)}" ${p.name === ntSimple.project ? "selected" : ""}>${esc(p.name)}</option>`).join("");
        const selectedProject = projects.find((p) => p.name === ntSimple.project);
        const showUpstream = !!(selectedProject && selectedProject.vcs === "git");
        return `
          <label style="display:block;font-size:12px;color:var(--muted)" data-tip="Optional display name for this squad and its task, shown in the sidebar and board view.\nLeave blank to name it automatically: a ticket id (e.g. ABC-1234) found in your prompt is used if there is one; otherwise the agent is asked to suggest a short name once you submit.">
            Squad label (optional)
            <input style="${NT_INPUT_STYLE}" value="${esc(ntSimple.label)}" oninput="ntSimple.label=this.value" placeholder="e.g. PIPE-5163, or leave blank to auto-name">
          </label>
          <label style="display:block;font-size:12px;color:var(--muted);margin-top:8px" data-tip="${ntTemplatesFallback ? "No [[templates]] are configured in .ralphus.toml — using the built-in default template. See docs/simple-task-templates.md to define your own." : "Choose a template — see docs/simple-task-templates.md for the schema. Templates are defined under [[templates]] in .ralphus.toml."}">
            Template
            <select style="${NT_INPUT_STYLE}" ${ntTemplatesFallback ? "disabled" : ""} onchange="ntSimple.templateName=this.value;ntSimple.fieldValues={};renderNewTaskModal()">${templateOptions}</select>
          </label>
          ${t.description ? `<p style="font-size:11px;color:var(--muted);margin:2px 0 0">${esc(t.description)}</p>` : ""}
          ${fieldRows}
          <label style="display:block;font-size:12px;color:var(--muted);margin-top:8px" data-tip="The instruction the work cell's agent runs. Combined with the selected template's supplementary fields, if any.">
            Prompt
            <textarea class="${ntFieldErrorText("prompt") ? "err" : ""}" style="${NT_INPUT_STYLE};height:90px" placeholder="${NT_PROMPT_PLACEHOLDER}" oninput="ntSimple.prompt=this.value;ntClearFieldErrorInline('prompt', this)">${esc(ntSimple.prompt)}</textarea>
          </label>
          ${ntFieldErrorHtml("prompt")}
          <div class="row" style="gap:8px;margin-top:8px;align-items:flex-start">
            <label style="flex:1;font-size:12px;color:var(--muted)" data-tip="Which agent backend runs the work cell. Independent of the chosen project — not scoped to any particular worktree.">
              Agent
              <select class="${ntFieldErrorText("agent") ? "err" : ""}" style="${NT_INPUT_STYLE}" onchange="ntSimple.agent=this.value;ntSimple.model='';ntClearFieldError('agent');renderNewTaskModal()">${agentOptions}</select>
              ${ntFieldErrorHtml("agent")}
            </label>
            <label style="flex:1;font-size:12px;color:var(--muted)" data-tip="The model to use. Choices are scoped to the selected agent when known, but you can type any model name — an unsupported value fails at submit time, not as you type.">
              Model
              <input list="nt-model-list" style="${NT_INPUT_STYLE}" value="${esc(ntSimple.model)}" oninput="ntSimple.model=this.value" placeholder="(agent default)">
              <datalist id="nt-model-list">${modelDatalist}</datalist>
            </label>
          </div>
          <div class="row" style="gap:8px;margin-top:8px;align-items:flex-start">
            <label style="flex:1;font-size:12px;color:var(--muted)" data-tip="Which registered project the work runs against. The task always runs in a fresh worktree branch for this project — never the project's raw checkout directly.">
              Project
              <select class="${ntFieldErrorText("project") ? "err" : ""}" style="${NT_INPUT_STYLE}" onchange="ntSimple.project=this.value;ntClearFieldError('project');renderNewTaskModal()"><option value="">(select a project)</option>${projectOptions}</select>
              ${ntFieldErrorHtml("project")}
            </label>
            ${showUpstream ? `
            <label style="flex:1;font-size:12px;color:var(--muted)" data-tip="The branch the fresh worktree starts from. Leave blank to use the project's default branch. Checked against the project's real branches when you submit.">
              Upstream branch (optional)
              <input style="${NT_INPUT_STYLE}" value="${esc(ntSimple.upstreamBranch)}" oninput="ntSimple.upstreamBranch=this.value" placeholder="(project default)">
            </label>` : ""}
          </div>
          <label style="display:block;font-size:12px;color:var(--muted);margin-top:10px" data-tip="When checked, the selected agent/model is asked to propose proof step(s) — commands that must pass — against this project's codebase before you submit, shown to you for edit/removal first. You can also add proof steps by hand regardless of this checkbox.">
            <input type="checkbox" ${ntSimple.proofs ? "checked" : ""} onchange="ntSimple.proofs=this.checked;renderNewTaskModal()"> Generate proof steps
          </label>
          ${ntSimple.proofItems.length
            ? `<div style="margin-top:6px">${ntListWidgetHtml(ntSimple.proofItems, "proofs", { labelPlaceholder: "id", valuePlaceholder: "shell command", addLabel: "Add a proof step by hand.", labelTip: "A short id for this proof step.", valueTip: "The shell command this proof step runs." })}</div>`
            : `<button class="btn" style="margin-top:6px" onclick="ntListAdd('proofs')" data-tip="Add a proof step by hand, without generating one.">+ Add a proof step by hand</button>`}
          <label style="display:block;font-size:12px;color:var(--muted);margin-top:10px" data-tip="Whether/how this task gets reviewed.\n&quot;Auto Review&quot; (the default) pools the work cell into Triage — the daemon's Arbiter classifies it and a review is created automatically once its pool threshold or schedule fires, no triage type needed from you.\n&quot;Add a Review&quot; creates an explicit review up front, letting you configure manual checks and auto-build steps now.\n&quot;No Review&quot; skips review entirely.">
            Review
            <select style="${NT_INPUT_STYLE}" onchange="ntSimple.reviewMode=this.value;renderNewTaskModal()">
              <option value="auto" ${ntSimple.reviewMode === "auto" ? "selected" : ""}>Auto Review</option>
              <option value="explicit" ${ntSimple.reviewMode === "explicit" ? "selected" : ""}>Add a Review</option>
              <option value="none" ${ntSimple.reviewMode === "none" ? "selected" : ""}>No Review</option>
            </select>
          </label>
          ${ntSimple.reviewMode === "explicit" ? `
          <label style="display:block;font-size:12px;color:var(--muted);margin-top:10px" data-tip="Every review must say how (or whether) it builds. Checked (the default) means this review deliberately has no build step (skip_auto_build = true). Uncheck to generate or hand-author build command(s) run when this review's branches merge instead.">
            <input type="checkbox" ${ntSimple.skipAutoBuild ? "checked" : ""} onchange="ntSimple.skipAutoBuild=this.checked;renderNewTaskModal()"> Skip auto-build
          </label>
          ${!ntSimple.skipAutoBuild ? `
          <label style="display:block;font-size:12px;color:var(--muted);margin-top:6px" data-tip="When checked, the selected agent/model is asked to propose build/compile command(s) to run automatically when this review's branches merge, shown to you for edit/removal first. You can also add auto-build steps by hand regardless of this checkbox.">
            <input type="checkbox" ${ntSimple.generateAutoBuild ? "checked" : ""} onchange="ntSimple.generateAutoBuild=this.checked;renderNewTaskModal()"> Generate auto-build steps
          </label>
          ${ntSimple.buildItems.length
            ? `<div style="margin-top:6px">${ntListWidgetHtml(ntSimple.buildItems, "builds", { singleField: true, valuePlaceholder: "shell command", addLabel: "Add an auto-build step by hand.", valueTip: "The shell command this build step runs when the review's branches merge." })}</div>`
            : `<button class="btn" style="margin-top:6px" onclick="ntListAdd('builds')" data-tip="Add an auto-build step by hand, without generating one.">+ Add an auto-build step by hand</button>`}
          ` : ""}
          <label style="display:block;font-size:12px;color:var(--muted);margin-top:10px" data-tip="When checked, the selected agent/model is asked to propose manual check button(s) reviewers can run against this project's codebase before you submit, shown to you for edit/removal first. You can also add manual checks by hand regardless of this checkbox.">
            <input type="checkbox" ${ntSimple.generateManualChecks ? "checked" : ""} onchange="ntSimple.generateManualChecks=this.checked;renderNewTaskModal()"> Generate manual checks
          </label>
          ${ntSimple.checkItems.length
            ? `<div style="margin-top:6px">${ntListWidgetHtml(ntSimple.checkItems, "checks", { labelPlaceholder: "label", valuePlaceholder: "what to check", addLabel: "Add a manual check by hand.", labelTip: "The manual check's button label.", valueTip: "What a reviewer should check or try." })}</div>`
            : `<button class="btn" style="margin-top:6px" onclick="ntListAdd('checks')" data-tip="Add a manual check by hand, without generating one.">+ Add a manual check by hand</button>`}
          ` : ""}`;
      }
      /**
       * Validates and submits the Simple tab. If any opted-in generation
       * step would produce a UI update, the first click runs the step(s)
       * (concurrently when more than one is enabled) and shows the confirm
       * step instead of submitting; a second click (now `confirmStep`)
       * assembles the TOML and submits.
       *
       * Field errors (prompt/agent/project/template fields) render two ways:
       * inline, next to each offending field (via `ntSimple.fieldErrors` +
       * `ntFieldErrorHtml`, read by `ntSimpleTabHtml` on the `renderNewTaskModal()`
       * call below), and as a single generic pointer in the shared `#nt-err`
       * panel -- re-fetched fresh afterward since that render just replaced
       * the DOM node the earlier `byId("nt-err")` reference pointed at.
       * @returns {Promise<void>}
       */
      async function submitTaskSimple() {
        byId("nt-err").innerHTML = "";
        const fieldErrors = ntSimpleValidateFields();
        ntSimple.fieldErrors = fieldErrors;
        if (fieldErrors.length) {
          renderNewTaskModal();
          byId("nt-err").textContent = "Errors prevented submission — please fix before continuing.";
          return;
        }
        const errEl = byId("nt-err");
        const upstreamError = await ntSimpleValidateUpstream();
        if (upstreamError) { errEl.textContent = upstreamError; return; }

        if (ntSimpleSubmitAction(ntSimple) === "generate") {
          ntSimple.generating = true;
          renderNewTaskModal();
          const kinds = ntPlannedGenerationKinds(ntSimple);
          /** @type {Promise<void>[]} */
          const jobs = [];
          if (kinds.includes("proof_steps")) {
            jobs.push(ntRunGenerationStep("proof_steps").then((items) => { if (items) ntSimple.proofItems = ntSimple.proofItems.concat(items); }));
          }
          if (kinds.includes("manual_checks")) {
            jobs.push(ntRunGenerationStep("manual_checks").then((items) => { if (items) ntSimple.checkItems = ntSimple.checkItems.concat(items); }));
          }
          if (kinds.includes("auto_build_steps")) {
            jobs.push(ntRunGenerationStep("auto_build_steps").then((items) => { if (items) ntSimple.buildItems = ntSimple.buildItems.concat(items); }));
          }
          await Promise.all(jobs);
          ntSimple.generating = false;
          ntSimple.confirmStep = true;
          // The modal may have been cancelled while these jobs were still
          // running (they poll for up to 120s) -- only pop it back open on
          // the confirm step if it's still the thing on screen.
          if (ntModalOpen && ntTab === "simple") renderNewTaskModal();
          return;
        }

        const { toml, naming, fallbackName } = ntSimpleBuildToml();
        const v = await ntValidateOne(toml);
        if (!v.valid) {
          errEl.innerHTML = (v.errors || []).map((e) => `line ${e.line ?? "?"}: ${esc(e.message)}`).join("<br>") || "validation failed";
          return;
        }
        /** @type {(label: string|null) => Promise<Response>} */
        const postSquad = (label) => fetch("/api/squads", { method: "POST", headers: traceHeaders(), body: JSON.stringify({ toml, label }) });
        const squadLabel = naming.squadLabel === null ? null : ntSanitizeSquadLabel(naming.squadLabel);
        let resp = await postSquad(squadLabel);
        if (!resp.ok) {
          const b = await resp.json().catch(() => ({}));
          // The label is sanitized above, but if the daemon still rejects
          // it for some reason not anticipated here, fall back to no label
          // at all (the squad id shows instead) rather than blocking submission.
          if (b.error && b.error.code === "invalid_label") {
            resp = await postSquad(null);
          } else {
            errEl.textContent = (b.error && b.error.message) || "submit failed";
            return;
          }
        }
        if (!resp.ok) { const b = await resp.json().catch(() => ({})); errEl.textContent = (b.error && b.error.message) || "submit failed"; return; }
        if (naming.needsGeneration) {
          const created = await resp.json().catch(() => null);
          if (created && created.squad_id) ntRequestSuggestedName(created.squad_id, fallbackName);
        }
        ntSimpleResetKeepingProjectFields();
        closeModal(); tick();
      }
      /**
       * Fires `POST /api/squads/{squadId}/tasks/0/suggest-name` (RAL-398)
       * after a Simple-tab submit whose prompt had neither a typed label nor
       * a ticket-id-shaped token to name the task after. Fire-and-forget --
       * the daemon runs the naming call and applies the result (or falls
       * back to `fallbackName`) entirely on its own background thread, so
       * there is nothing here to poll: the renamed task/squad shows up on
       * the board's next regular poll, whether or not this modal (or even
       * this browser tab) is still open by then.
       * @param {string} squadId
       * @param {string} fallbackName
       * @returns {void}
       */
      function ntRequestSuggestedName(squadId, fallbackName) {
        const project = projects.find((p) => p.name === ntSimple.project);
        const cwd = project ? project.path : ".";
        fetch(`/api/squads/${encodeURIComponent(squadId)}/tasks/0/suggest-name`, {
          method: "POST",
          body: JSON.stringify({
            cwd, agent: ntSimple.agent, model: ntSimple.model.trim() || undefined,
            prompt_context: ntSimpleEffectivePrompt(), fallback_name: fallbackName,
          }),
        }).catch(() => {});
      }

      // ---------- new task modal: Files tab ----------
      /**
       * Renders the New Task modal's Files tab (dropzone + loaded file list).
       * @returns {string}
       */
      function ntFilesTabHtml() {
        const rows = ntFiles.map((f, i) => `
          <div style="border:1px solid var(--border);border-radius:6px;padding:8px;margin-bottom:6px">
            <div class="row" style="justify-content:space-between;align-items:center">
              <strong>${esc(f.name)}</strong>
              <span>
                <span style="color:var(--muted);font-size:11px;margin-right:8px">${(f.size / 1024).toFixed(1)} KB</span>
                <button class="btn" style="padding:1px 6px;font-size:11px" data-click="ntRemoveFile" data-i="${i}" data-tip="Remove this file from the batch before queuing.\nYou can drop or browse it again to re-add it.">✕</button>
              </span>
            </div>
            <textarea style="width:100%;height:90px;margin-top:6px" data-i="${i}" oninput="ntFiles[Number(this.dataset.i)].content=this.value" data-tip="Edit this file's TOML before queuing, if needed.">${esc(f.content)}</textarea>
            ${f.err ? `<div class="verr">${f.err}</div>` : ""}
          </div>`).join("");
        return `
          <div class="drop-zone" id="nt-dropzone" ondragover="ntDragOver(event)" ondragleave="ntDragLeave(event)" ondrop="ntDrop(event)" data-tip="Drop one or more .toml task files here, or click to browse for them on your machine.\nEach file lands below, still editable, and is queued as its own squad named after the file.">
            <input type="file" id="nt-file" accept=".toml" multiple onchange="ntFileChange(this)">
            <div id="nt-dz-label">📁 Drop <code>.toml</code> file(s) here, or <u>click to browse</u></div>
            <div class="dz-hint" id="nt-dz-hint">${ntFiles.length ? `${ntFiles.length} file(s) loaded` : "No files selected"}</div>
          </div>
          ${rows || `<div class="empty">No files loaded yet.</div>`}`;
      }
      /**
       * Dropzone dragover handler: shows the drag-active state.
       * @param {DragEvent} e
       * @returns {void}
       */
      function ntDragOver(e) { e.preventDefault(); e.stopPropagation(); byId("nt-dropzone").classList.add("drag-over"); }
      /**
       * Dropzone dragleave handler: clears the drag-active state once the pointer truly exits.
       * @param {DragEvent} e
       * @returns {void}
       */
      function ntDragLeave(e) { const dz = byId("nt-dropzone"); if (!dz.contains(/** @type {Node} */ (e.relatedTarget))) dz.classList.remove("drag-over"); }
      /**
       * Dropzone drop handler: loads every dropped `.toml` file.
       * @param {DragEvent} e
       * @returns {void}
       */
      function ntDrop(e) {
        e.preventDefault(); e.stopPropagation();
        byId("nt-dropzone").classList.remove("drag-over");
        Array.from(e.dataTransfer?.files || []).filter((f) => f.name.toLowerCase().endsWith(".toml")).forEach(ntLoadFile);
      }
      /**
       * File-input change handler: loads every selected file and resets the input.
       * @param {HTMLInputElement} input
       * @returns {void}
       */
      function ntFileChange(input) {
        Array.from(input.files || []).forEach(ntLoadFile);
        input.value = "";
      }
      /**
       * Reads a dropped/selected file as text and adds it to the Files tab list.
       * @param {File} f
       * @returns {void}
       */
      function ntLoadFile(f) {
        const fr = new FileReader();
        fr.onload = (ev) => {
          ntFiles.push({ name: f.name, size: f.size, content: /** @type {string} */ (ev.target?.result), err: null });
          if (ntTab === "files") renderNewTaskModal();
        };
        fr.readAsText(f);
      }
      /**
       * Removes one file from the Files tab list by index.
       * @param {number} i
       * @returns {void}
       */
      function ntRemoveFile(i) { ntFiles.splice(i, 1); renderNewTaskModal(); }

      // ---------- new task modal: Paste tab ----------
      /**
       * Renders the New Task modal's Paste tab (TOML textarea + label field).
       * @returns {string}
       */
      function ntPasteTabHtml() {
        return `
          <textarea id="nt-toml" class="paste-area" placeholder="Paste TOML here…" oninput="ntPasteToml=this.value;ntClearErr()" onblur="ntValidate()" data-tip="Paste or edit a TOML task definition here.\nSee docs/daemon-api.md for the full schema — at minimum you need [[task]] with a name and at least one [[task.cell]].\nRe-validated automatically when you click away from this box, and always re-checked before queuing.">${esc(ntPasteToml)}</textarea>
          <label style="font-size:12px;color:var(--muted)" data-tip="Optional display label for this squad — shown in the sidebar and board view. Defaults to the squad ID if left blank.">label (optional) <input id="nt-label" style="width:100%;background:var(--bg);border:1px solid var(--border);color:var(--text);border-radius:6px;padding:6px" value="${esc(ntLabel)}" oninput="ntLabel=this.value"></label>`;
      }

      // ---------- new task modal: validation ----------
      // Clears the error panel while the user is actively typing, so stale
      // errors don't linger over content that's already changed; the real
      // check re-runs on blur (ntValidate) and always again before submit.
      /**
       * @typedef {object} ValidateResponse
       * @property {boolean} valid
       * @property {{line?: number, kind?: string, message: string}[]} [errors]
       * @property {{line?: number, kind?: string, message: string}[]} [warnings]
       */
      /**
       * Clears the Paste tab's validation error/warning display.
       * @returns {void}
       */
      function ntClearErr() {
        const taEl = document.getElementById("nt-toml");
        if (taEl) taEl.classList.remove("err");
        const errEl = byId("nt-err");
        if (errEl) errEl.innerHTML = "";
      }
      // Validate a TOML string against the daemon without submitting.
      /**
       * Validates a TOML string against the daemon without persisting it.
       * @param {string} toml
       * @returns {Promise<ValidateResponse>}
       */
      async function ntValidateOne(toml) {
        try {
          return await (await fetch("/api/squads/validate", { method: "POST", body: JSON.stringify({ toml }) })).json();
        } catch (e) {
          return { valid: false, errors: [{ message: "network error while validating" }] };
        }
      }
      // Validate the Paste tab's textarea; renders both hard errors and
      // non-blocking warnings so problems are caught as early as possible —
      // right after a file drop, or on leaving the textarea.
      /**
       * Validates the Paste tab's current textarea contents and renders the result.
       * @returns {Promise<ValidateResponse|null>}
       */
      async function ntValidate() {
        const toml = /** @type {HTMLTextAreaElement} */ (document.getElementById("nt-toml")).value;
        const errEl = byId("nt-err");
        const taEl = byId("nt-toml");
        if (!toml.trim()) { errEl.innerHTML = ""; taEl.classList.remove("err"); return null; }
        const v = await ntValidateOne(toml);
        renderNtValidation(v);
        return v;
      }
      /**
       * Renders a TOML validation response's errors/warnings into the Paste tab's error panel.
       * @param {ValidateResponse} v
       * @returns {void}
       */
      function renderNtValidation(v) {
        const errEl = byId("nt-err");
        const taEl = byId("nt-toml");
        /** @type {string[]} */
        const lines = [];
        (v.errors || []).forEach((e) => lines.push(`line ${e.line ?? "?"}: ${esc(e.message)}`));
        (v.warnings || []).forEach((w) => lines.push(`<span class="vwarn">⚠ line ${w.line ?? "?"}: ${esc(w.message)}</span>`));
        errEl.innerHTML = lines.join("<br>");
        taEl.classList.toggle("err", !v.valid);
      }

