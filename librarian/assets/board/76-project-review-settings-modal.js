      // ---------- Project "Review Settings" modal (RAL-408) ----------
      // A project's database-backed DEFAULT review settings -- resolver
      // agent/model, machine, budget, proof scope, and the project-level
      // equivalents of every `ralphus review settings` opt-out flag.
      // Applied to FUTURE reviews only (an Arbiter-created review with no
      // `[[review]]` block, or any review whose own block leaves a field
      // unset); existing reviews are unaffected.
      //
      // Reuses `66-review-edit-modal.js`'s shared field renderers
      // (`renderResolverFieldsHtml`/`renderProofScopeFieldsHtml`/
      // `renderPrSettingsFieldsHtml`/`renderAutoFixFieldsHtml`) for every
      // setting this modal has in common with the per-review Edit Details
      // modal, instead of duplicating that markup -- only the fields with no
      // per-review counterpart (machine, budget, skip-base-updates,
      // auto-build) get their own markup here.

      /**
       * The Review Settings modal's single draft object, built once from
       * `GET .../review-settings` when the modal opens. `original*` fields
       * are the value the draft was seeded with (the project's raw
       * database override, falling back to the resolved effective value for
       * booleans so an untouched checkbox still displays something
       * sensible) -- Save only sends fields that actually changed, so an
       * untouched field stays "inherit from file config" rather than being
       * re-asserted as an explicit override on every save.
       * @typedef {object} ProjectReviewSettingsDraft
       * @property {string} project
       * @property {string} cwd - This project's registered path, for the resolver-agent dropdown's profile lookup.
       * @property {Partial<EffectiveReviewDefaults>} effective - The fully resolved effective values (file config + database), for hint text.
       * @property {string} resolverAgent
       * @property {string} originalResolverAgent
       * @property {string} resolverModel
       * @property {string} originalResolverModel
       * @property {string} machine
       * @property {string} originalMachine
       * @property {number|null} maximumBudgetUsd
       * @property {number|null} originalMaximumBudgetUsd
       * @property {string} proofScope - "" (inherit) | "each_branch" | "final_branch" | "nothing"
       * @property {string} originalProofScope
       * @property {boolean} proofSkipAutoClean
       * @property {boolean} originalProofSkipAutoClean
       * @property {boolean} skipWorktrees
       * @property {boolean} originalSkipWorktrees
       * @property {boolean} skipBaseUpdates
       * @property {boolean} originalSkipBaseUpdates
       * @property {boolean} matchPrBranchName
       * @property {boolean} originalMatchPrBranchName
       * @property {boolean} separatePrBranch
       * @property {boolean} originalSeparatePrBranch
       * @property {boolean} dualRootPr
       * @property {boolean} originalDualRootPr
       * @property {string} autoBuild
       * @property {string} originalAutoBuild
       * @property {boolean} autoSubmitPrStack
       * @property {boolean} originalAutoSubmitPrStack
       * @property {boolean} autoFixPrErrors
       * @property {boolean} originalAutoFixPrErrors
       * @property {string} autoFixPromptTemplate
       * @property {string} originalAutoFixPromptTemplate
       */

      /** @type {ProjectReviewSettingsDraft|null} */
      let projectReviewSettingsDraft = null;
      let projectReviewSettingsError = "";

      /**
       * Builds the draft from `GET /api/projects/{name}/review-settings`'s
       * response -- string fields seed from the raw override (blank means
       * unset), boolean fields seed from the raw override falling back to
       * the resolved effective value (there is no blank checkbox state).
       * @param {string} project
       * @param {Partial<ProjectReviewSettingsResponse>} payload
       * @returns {ProjectReviewSettingsDraft}
       */
      function buildProjectReviewSettingsDraft(project, payload) {
        const s = /** @type {Partial<ProjectReviewSettingsRaw>} */ (payload.settings || {});
        const effective = /** @type {Partial<EffectiveReviewDefaults>} */ (payload.effective || {});
        /**
         * @param {string|number|boolean|null|undefined} v
         * @returns {string}
         */
        const str = (v) => (v === null || v === undefined ? "" : String(v));
        /**
         * @param {boolean|null|undefined} raw
         * @param {boolean|undefined} fallback
         * @returns {boolean}
         */
        const boolOr = (raw, fallback) => (raw === null || raw === undefined ? !!fallback : !!raw);
        const proj = projects.find((p) => p.name === project);
        const resolverAgent = str(s.default_resolver_agent);
        const resolverModel = str(s.default_resolver_model);
        const machine = str(s.default_machine);
        const maximumBudgetUsd = s.default_maximum_budget_usd === null || s.default_maximum_budget_usd === undefined
          ? null
          : s.default_maximum_budget_usd;
        const proofScope = str(s.default_proof_scope);
        const proofSkipAutoClean = boolOr(s.verify_skip_auto_clean, effective.skip_auto_clean);
        const skipWorktrees = boolOr(s.skip_worktrees, effective.skip_worktrees);
        const skipBaseUpdates = boolOr(s.skip_base_updates, effective.skip_base_updates);
        const matchPrBranchName = boolOr(s.match_pr_branch_name, effective.match_pr_branch_name);
        const separatePrBranch = boolOr(s.separate_pr_branch, effective.separate_pr_branch);
        const dualRootPr = boolOr(s.dual_root_pr, effective.dual_root_pr);
        const autoBuild = str(s.auto_build);
        const autoSubmitPrStack = boolOr(s.auto_submit_pr_stack, effective.auto_submit_pr_stack);
        const autoFixPrErrors = boolOr(s.auto_fix_pr_errors, effective.auto_fix_pr_errors);
        const autoFixPromptTemplate = str(s.auto_fix_prompt_template);
        return {
          project,
          cwd: proj ? proj.path : "",
          effective,
          resolverAgent, originalResolverAgent: resolverAgent,
          resolverModel, originalResolverModel: resolverModel,
          machine, originalMachine: machine,
          maximumBudgetUsd, originalMaximumBudgetUsd: maximumBudgetUsd,
          proofScope, originalProofScope: proofScope,
          proofSkipAutoClean, originalProofSkipAutoClean: proofSkipAutoClean,
          skipWorktrees, originalSkipWorktrees: skipWorktrees,
          skipBaseUpdates, originalSkipBaseUpdates: skipBaseUpdates,
          matchPrBranchName, originalMatchPrBranchName: matchPrBranchName,
          separatePrBranch, originalSeparatePrBranch: separatePrBranch,
          dualRootPr, originalDualRootPr: dualRootPr,
          autoBuild, originalAutoBuild: autoBuild,
          autoSubmitPrStack, originalAutoSubmitPrStack: autoSubmitPrStack,
          autoFixPrErrors, originalAutoFixPrErrors: autoFixPrErrors,
          autoFixPromptTemplate, originalAutoFixPromptTemplate: autoFixPromptTemplate,
        };
      }

      /**
       * Opens the Review Settings modal for one project, fetching its
       * current database overrides + effective values first.
       * @param {string} project
       * @returns {Promise<void>}
       */
      async function openProjectReviewSettings(project) {
        projectReviewSettingsError = "";
        try {
          const r = await fetch(`/api/projects/${encodeURIComponent(project)}/review-settings`);
          if (!r.ok) {
            projectReviewSettingsError = await responseError(r, "load failed");
            projectReviewSettingsDraft = buildProjectReviewSettingsDraft(project, {});
          } else {
            projectReviewSettingsDraft = buildProjectReviewSettingsDraft(project, await r.json());
          }
        } catch (err) {
          projectReviewSettingsError = "daemon unreachable";
          projectReviewSettingsDraft = buildProjectReviewSettingsDraft(project, {});
        }
        renderProjectReviewSettingsModal();
        const draft = projectReviewSettingsDraft;
        preloadAgentSelect(draft.cwd, () => projectReviewSettingsDraft === draft, renderProjectReviewSettingsModal);
      }

      /**
       * Closes the Review Settings modal, discarding the draft.
       * @returns {void}
       */
      function closeProjectReviewSettingsModal() {
        projectReviewSettingsDraft = null;
        closeModal();
      }

      /**
       * Stages a new resolver-agent choice and re-renders (a `<select>`
       * change never loses in-progress typing the way a text input would).
       * @param {string} value
       * @returns {void}
       */
      function onProjectEditResolverAgent(value) {
        if (!projectReviewSettingsDraft) return;
        projectReviewSettingsDraft.resolverAgent = value;
        renderProjectReviewSettingsModal();
      }
      /**
       * Stages a new resolver-model value, leaving focus in the text field.
       * @param {string} value
       * @returns {void}
       */
      function onProjectEditResolverModel(value) { if (projectReviewSettingsDraft) projectReviewSettingsDraft.resolverModel = value; }
      /**
       * Stages a new machine default.
       * @param {string} value
       * @returns {void}
       */
      function onProjectEditMachine(value) { if (projectReviewSettingsDraft) projectReviewSettingsDraft.machine = value; }
      /**
       * Stages a new maximum-budget-usd value; a blank field means "clear to
       * inherit" (`null`), not zero.
       * @param {string} value
       * @returns {void}
       */
      function onProjectEditMaximumBudgetUsd(value) {
        if (!projectReviewSettingsDraft) return;
        const trimmed = value.trim();
        projectReviewSettingsDraft.maximumBudgetUsd = trimmed === "" ? null : Number(trimmed);
      }
      /**
       * Stages a new Proof-scope choice and re-renders, since the
       * skip-auto-clean sub-row's visibility depends on it.
       * @param {string} value
       * @returns {void}
       */
      function onProjectEditProofScope(value) {
        if (!projectReviewSettingsDraft) return;
        projectReviewSettingsDraft.proofScope = value;
        renderProjectReviewSettingsModal();
      }
      /**
       * Stages the "skip auto-clean branches" sub-option.
       * @param {boolean} checked
       * @returns {void}
       */
      function onProjectEditProofSkipAutoClean(checked) { if (projectReviewSettingsDraft) projectReviewSettingsDraft.proofSkipAutoClean = checked; }
      /**
       * Stages the skip-per-branch-worktrees default.
       * @param {boolean} checked
       * @returns {void}
       */
      function onProjectEditSkipWorktrees(checked) { if (projectReviewSettingsDraft) projectReviewSettingsDraft.skipWorktrees = checked; }
      /**
       * Stages the skip-base-branch-auto-updates default.
       * @param {boolean} checked
       * @returns {void}
       */
      function onProjectEditSkipBaseUpdates(checked) { if (projectReviewSettingsDraft) projectReviewSettingsDraft.skipBaseUpdates = checked; }
      /**
       * Stages the separate-PR-branch default and re-renders, since it gates
       * whether "match worktree branch name" is enabled.
       * @param {boolean} checked
       * @returns {void}
       */
      function onProjectEditSeparatePrBranch(checked) {
        if (!projectReviewSettingsDraft) return;
        projectReviewSettingsDraft.separatePrBranch = checked;
        renderProjectReviewSettingsModal();
      }
      /**
       * Stages the dual-root-PR default.
       * @param {boolean} checked
       * @returns {void}
       */
      function onProjectEditDualRootPr(checked) { if (projectReviewSettingsDraft) projectReviewSettingsDraft.dualRootPr = checked; }
      /**
       * Stages the match-worktree-branch-name default.
       * @param {boolean} checked
       * @returns {void}
       */
      function onProjectEditMatchPrBranchName(checked) { if (projectReviewSettingsDraft) projectReviewSettingsDraft.matchPrBranchName = checked; }
      /**
       * Stages a new auto-build command default.
       * @param {string} value
       * @returns {void}
       */
      function onProjectEditAutoBuild(value) { if (projectReviewSettingsDraft) projectReviewSettingsDraft.autoBuild = value; }
      /**
       * Stages the auto-submit-PR-stack default.
       * @param {boolean} checked
       * @returns {void}
       */
      function onProjectEditAutoSubmitPrStack(checked) { if (projectReviewSettingsDraft) projectReviewSettingsDraft.autoSubmitPrStack = checked; }
      /**
       * Stages the auto-fix-PR-errors default.
       * @param {boolean} checked
       * @returns {void}
       */
      function onProjectEditAutoFixPrErrors(checked) { if (projectReviewSettingsDraft) projectReviewSettingsDraft.autoFixPrErrors = checked; }
      /**
       * Stages the auto-fix prompt template default. Empty resets to the
       * built-in default (or a file-config value, if the project's
       * `.ralphus.toml` sets one).
       * @param {string} value
       * @returns {void}
       */
      function onProjectEditAutoFixPromptTemplate(value) { if (projectReviewSettingsDraft) projectReviewSettingsDraft.autoFixPromptTemplate = value; }

      const PROJECT_REVIEW_SETTINGS_BUDGET_TIP = "USD spend cap applied to a future review's own resolver/prover cost when neither its [[review]] block nor the Arbiter sets one. Blank means unbounded (inherits the file-config/global value shown below).";
      const PROJECT_REVIEW_SETTINGS_MACHINE_TIP = "The machine (scheme:uri, or \"local\") a future review's worktrees and merge run on when nothing more specific sets one. Blank inherits the file-config/global value shown below.";
      const PROJECT_REVIEW_SETTINGS_AUTO_BUILD_TIP = "The build/test command run at finalize time in place of check gates, for a future review that declares no explicit [[review.auto_build]] steps and no [[review]] skip_auto_build. Blank inherits the file-config value shown below.";

      /**
       * Renders the Review Settings modal from `projectReviewSettingsDraft`.
       * @returns {void}
       */
      function renderProjectReviewSettingsModal() {
        const draft = projectReviewSettingsDraft;
        if (!draft) return;
        const resolverSection = renderResolverFieldsHtml(draft.cwd, draft.resolverAgent, draft.resolverModel, false, "onProjectEditResolverAgent", "onProjectEditResolverModel");
        const proofScopeSection = renderProofScopeFieldsHtml(draft.proofScope, draft.proofSkipAutoClean, "onProjectEditProofScope", "onProjectEditProofSkipAutoClean", true);
        const prSettingsSection = renderPrSettingsFieldsHtml(draft.separatePrBranch, draft.matchPrBranchName, draft.autoSubmitPrStack, draft.dualRootPr, "onProjectEditSeparatePrBranch", "onProjectEditMatchPrBranchName", "onProjectEditAutoSubmitPrStack", "onProjectEditDualRootPr");
        const autoFixSection = renderAutoFixFieldsHtml(draft.autoFixPrErrors, draft.autoFixPromptTemplate, "onProjectEditAutoFixPrErrors", "onProjectEditAutoFixPromptTemplate");
        const err = projectReviewSettingsError ? `<div id="project-review-settings-err" class="verr">${esc(projectReviewSettingsError)}</div>` : `<div id="project-review-settings-err" class="verr"></div>`;
        byId("modal-root").innerHTML = `<div class="modal-bg" onclick="if(event.target===this)closeProjectReviewSettingsModal()"><div class="modal review-edit-modal">
            <h2>Review Settings — ${esc(draft.project)}</h2>
            <div class="k" style="margin-bottom:8px" data-tip="These are DEFAULTS, stored in the database and editable any time -- they apply to future reviews only (an Arbiter-created review with no [[review]] block, or any review whose own block leaves a field unset). Existing reviews are unchanged.">Applies to future reviews only. Existing reviews are unaffected.</div>
            ${resolverSection}
            <div class="kv-row"><span class="k">machine</span><input type="text" class="mono" style="${REVIEW_EDIT_INPUT_STYLE};width:200px" value="${esc(draft.machine)}" placeholder="inherits: ${esc(draft.effective.machine || "local")}" oninput="onProjectEditMachine(this.value)" data-tip="${PROJECT_REVIEW_SETTINGS_MACHINE_TIP}"></div>
            <div class="kv-row"><span class="k">maximum budget usd</span><input type="number" step="0.01" min="0" class="mono" style="${REVIEW_EDIT_INPUT_STYLE};width:120px" value="${draft.maximumBudgetUsd === null ? "" : esc(String(draft.maximumBudgetUsd))}" placeholder="${draft.effective.maximum_budget_usd === undefined ? "unbounded" : esc(String(draft.effective.maximum_budget_usd))}" oninput="onProjectEditMaximumBudgetUsd(this.value)" data-tip="${PROJECT_REVIEW_SETTINGS_BUDGET_TIP}"></div>
            ${proofScopeSection}
            <h3 class="section">check gates</h3>
            <label style="display:flex;align-items:center;gap:6px;font-size:12px;color:var(--muted);margin-top:6px" data-tip="Build the entire branch stack in one shared worktree instead of isolated per-branch worktrees, for a future review that declares no explicit skip_worktrees of its own.">
              <input type="checkbox" ${draft.skipWorktrees ? "checked" : ""} onchange="onProjectEditSkipWorktrees(this.checked)">skip per-branch worktrees</label>
            <label style="display:flex;align-items:center;gap:6px;font-size:12px;color:var(--muted);margin-top:4px" data-tip="Skip the automatic base-branch auto-update rebuild, for a future review that declares no explicit skip_base_updates of its own.">
              <input type="checkbox" ${draft.skipBaseUpdates ? "checked" : ""} onchange="onProjectEditSkipBaseUpdates(this.checked)">skip base-branch auto-updates</label>
            <div style="margin-top:8px">
              <label for="project-auto-build-input" style="font-size:12px;color:var(--muted);display:block;margin-bottom:4px" data-tip="${PROJECT_REVIEW_SETTINGS_AUTO_BUILD_TIP}">auto-build command</label>
              <input id="project-auto-build-input" type="text" class="mono" style="${REVIEW_EDIT_INPUT_STYLE};width:100%;box-sizing:border-box" value="${esc(draft.autoBuild)}" placeholder="inherits: ${esc(draft.effective.auto_build || "(none)")}" oninput="onProjectEditAutoBuild(this.value)" data-tip="${PROJECT_REVIEW_SETTINGS_AUTO_BUILD_TIP}">
            </div>
            <h3 class="section">pull requests</h3>
            ${prSettingsSection}
            ${autoFixSection}
            ${err}
            <div class="btn-row" style="margin-top:12px"><button class="btn" onclick="closeProjectReviewSettingsModal()">Cancel</button><button class="btn primary" onclick="saveProjectReviewSettings()" data-tip="Apply every change made in this modal in a single request. Only fields you actually touched are sent -- an untouched field keeps inheriting from the file-config/global default.">Save</button></div>
          </div></div>`;
      }

      /**
       * Saves the Review Settings draft: builds one payload from only the
       * fields that actually changed since the modal opened, then fires a
       * single `POST .../review-settings` request. On success, closes the
       * modal; on failure, leaves it open with the draft intact and shows
       * the error so nothing is lost.
       * @returns {Promise<void>}
       */
      async function saveProjectReviewSettings() {
        const draft = projectReviewSettingsDraft;
        if (!draft) return;
        /** @type {{[key: string]: *}} */
        const body = {};
        if (draft.resolverAgent !== draft.originalResolverAgent) body.default_resolver_agent = draft.resolverAgent;
        if (draft.resolverModel !== draft.originalResolverModel) body.default_resolver_model = draft.resolverModel;
        if (draft.machine !== draft.originalMachine) body.default_machine = draft.machine;
        if (draft.maximumBudgetUsd !== draft.originalMaximumBudgetUsd) {
          if (draft.maximumBudgetUsd === null) {
            body.clear_maximum_budget_usd = true;
          } else {
            body.default_maximum_budget_usd = draft.maximumBudgetUsd;
          }
        }
        if (draft.proofScope !== draft.originalProofScope) body.default_proof_scope = draft.proofScope;
        if (draft.proofSkipAutoClean !== draft.originalProofSkipAutoClean) body.verify_skip_auto_clean = draft.proofSkipAutoClean;
        if (draft.skipWorktrees !== draft.originalSkipWorktrees) body.skip_worktrees = draft.skipWorktrees;
        if (draft.skipBaseUpdates !== draft.originalSkipBaseUpdates) body.skip_base_updates = draft.skipBaseUpdates;
        if (draft.matchPrBranchName !== draft.originalMatchPrBranchName) body.match_pr_branch_name = draft.matchPrBranchName;
        if (draft.separatePrBranch !== draft.originalSeparatePrBranch) body.separate_pr_branch = draft.separatePrBranch;
        if (draft.dualRootPr !== draft.originalDualRootPr) body.dual_root_pr = draft.dualRootPr;
        if (draft.autoBuild !== draft.originalAutoBuild) body.auto_build = draft.autoBuild;
        if (draft.autoSubmitPrStack !== draft.originalAutoSubmitPrStack) body.auto_submit_pr_stack = draft.autoSubmitPrStack;
        if (draft.autoFixPrErrors !== draft.originalAutoFixPrErrors) body.auto_fix_pr_errors = draft.autoFixPrErrors;
        if (draft.autoFixPromptTemplate !== draft.originalAutoFixPromptTemplate) body.auto_fix_prompt_template = draft.autoFixPromptTemplate;
        if (Object.keys(body).length === 0) { closeProjectReviewSettingsModal(); return; }
        try {
          const r = await fetch(`/api/projects/${encodeURIComponent(draft.project)}/review-settings`, {
            method: "POST",
            headers: { "Content-Type": "application/json" },
            body: JSON.stringify(body),
          });
          if (!r.ok) {
            projectReviewSettingsError = await responseError(r, "save failed");
            renderProjectReviewSettingsModal();
            return;
          }
        } catch (err) {
          projectReviewSettingsError = "daemon unreachable";
          renderProjectReviewSettingsModal();
          return;
        }
        closeProjectReviewSettingsModal();
      }

      void [onProjectEditResolverAgent, onProjectEditResolverModel, onProjectEditMachine, onProjectEditMaximumBudgetUsd, onProjectEditProofScope, onProjectEditProofSkipAutoClean, onProjectEditSkipWorktrees, onProjectEditSkipBaseUpdates, onProjectEditSeparatePrBranch, onProjectEditDualRootPr, onProjectEditMatchPrBranchName, onProjectEditAutoBuild, onProjectEditAutoSubmitPrStack, onProjectEditAutoFixPrErrors, onProjectEditAutoFixPromptTemplate];
