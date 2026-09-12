      // ---------- Review "Edit Details" modal (RAL-410) ----------
      // Batches every editable Guardian/Review setting -- upstream, resolver
      // agent/model, proof scope, build/worktree flags, per-project squash,
      // PR settings, and the three environment-override scopes -- into one
      // popup with a single local draft. Nothing here reaches the server
      // until Save, which fires exactly one `POST .../details` request (see
      // `guardian_details` in daemon/src/server.rs) instead of the ~11
      // independent, often-immediate save paths this modal replaces.
      //
      // The modal mounts into #modal-root, a sibling of #review-detail in
      // board.html -- so unlike the widgets it replaces, it is structurally
      // immune to `pollReviews()`'s background `renderReviewDetail()`
      // innerHTML replacement. That's what actually fixes the "paste into
      // upstream gets silently wiped" bug: there is nothing to race.

      /**
       * @typedef {object} EnvOverrideDraftRow
       * @property {string} key
       * @property {string} value - Ignored while `tombstone` is true.
       * @property {boolean} tombstone - True = "unset": remove this key from the effective environment even though it would otherwise be inherited.
       * @property {boolean} readOnlyKey - True once the row's key is fixed (derived from an existing override or a clicked inherited key) -- only a freshly-added blank row allows editing the key, so a rename can never orphan the original key server-side.
       */
      /**
       * One environment-override scope's draft state (build step,
       * manual-checks step, or one review branch).
       * @typedef {object} EnvScopeDraft
       * @property {EnvOverrideDraftRow[]} rows
       * @property {Set<string>} originalOwnKeys - Keys this scope owned when the draft was built; a removed row only becomes a `clear` if its key is in here.
       * @property {Set<string>} removedOwnKeys - Keys removed since the draft was built -- sent as `clear` on Save.
       */
      /**
       * The Edit Details modal's single draft object, built once from the
       * server's `GuardianView` when the modal opens and mutated in place by
       * every field handler. `original*` fields are the value the draft was
       * seeded with, so Save can send only what actually changed instead of
       * re-asserting every field on every save.
       * @typedef {object} ReviewEditDraft
       * @property {string} gid
       * @property {string} name
       * @property {string} originalName
       * @property {string} baseBranch
       * @property {string} originalBaseBranch
       * @property {boolean} resolverFrozen - True once the review is approved/deployed -- base/resolver are rejected server-side past that point.
       * @property {string} resolverAgent
       * @property {string} originalResolverAgent
       * @property {string} resolverModel
       * @property {string} originalResolverModel
       * @property {string} proofScope - "each_branch" | "final_branch" | "nothing"
       * @property {string} originalProofScope
       * @property {boolean} proofSkipAutoClean
       * @property {boolean} originalProofSkipAutoClean
       * @property {boolean} skipAutoBuild
       * @property {boolean} originalSkipAutoBuild
       * @property {boolean} skipWorktrees
       * @property {boolean} originalSkipWorktrees
       * @property {boolean} separatePrBranch
       * @property {boolean} originalSeparatePrBranch
       * @property {boolean} matchPrBranchName
       * @property {boolean} originalMatchPrBranchName
       * @property {boolean} autoSubmitPrStack
       * @property {boolean} originalAutoSubmitPrStack
       * @property {boolean} autoFixPrErrors
       * @property {boolean} originalAutoFixPrErrors
       * @property {string} autoFixPromptTemplate
       * @property {string} originalAutoFixPromptTemplate
       * @property {{[project: string]: boolean}} squash
       * @property {string[]} originalSquashOn
       * @property {string[]} projects
       * @property {EnvScopeDraft} buildEnv
       * @property {{[key: string]: string}} buildEnvInherited
       * @property {EnvScopeDraft} manualChecksEnv
       * @property {{[key: string]: string}} manualChecksEnvInherited
       * @property {{[branchId: string]: EnvScopeDraft}} branchEnv
       * @property {{[branchId: string]: {[key: string]: string}}} branchEnvInherited
       * @property {{id: string, branch: string}[]} branches
       */

      /** @type {ReviewEditDraft|null} */
      let reviewEditDraft = null;

      const REVIEW_EDIT_INPUT_STYLE = "background:var(--bg);border:1px solid var(--border);color:var(--text);border-radius:4px;padding:2px 5px;font-size:12px";
      const REVIEW_EDIT_TEXTAREA_STYLE = "width:100%;box-sizing:border-box;resize:vertical;font-family:inherit;font-size:12px;padding:6px;background:var(--bg);color:var(--text);border:1px solid var(--border);border-radius:4px";
      const AUTO_FIX_PROMPT_TEMPLATE_TIP = "Prompt handed to the resolver agent when 'auto-fix PR errors' fires, with the literal <<prompt>> placeholder replaced by the failing branch's own Cell prompts. Must contain <<prompt>> or Save is rejected. Leave blank to inherit the project default. Applies on Save.";

      /**
       * Builds an env-override scope's draft rows from its currently-saved
       * `own` overrides object (a string value overrides the inherited one;
       * `null` is a tombstone).
       * @param {{[key: string]: (string|null)}} own
       * @returns {EnvScopeDraft}
       */
      function envScopeDraftFromOwn(own) {
        const keys = Object.keys(own || {});
        const rows = keys.sort().map((key) => {
          const v = own[key];
          return { key, value: v === null ? "" : v, tombstone: v === null, readOnlyKey: true };
        });
        return { rows, originalOwnKeys: new Set(keys), removedOwnKeys: new Set() };
      }

      /**
       * Builds the Edit Details draft from a review's current server state.
       * @param {GuardianView} g
       * @returns {ReviewEditDraft}
       */
      function buildReviewEditDraft(g) {
        /** @type {{[branchId: string]: EnvScopeDraft}} */
        const branchEnv = {};
        /** @type {{[branchId: string]: {[key: string]: string}}} */
        const branchEnvInherited = {};
        for (const b of g.branches) {
          branchEnv[b.id] = envScopeDraftFromOwn(b.env_overrides || {});
          branchEnvInherited[b.id] = b.inherited_env || {};
        }
        const squashOn = g.squash_projects || [];
        const projects = (g.projects && g.projects.length) ? g.projects : [g.git_root || ""];
        /** @type {{[project: string]: boolean}} */
        const squash = {};
        for (const p of projects) squash[p] = squashOn.includes(p);
        const proofScope = g.effective_proof_scope || "each_branch";
        const proofSkipAutoClean = !!g.effective_proof_skip_auto_clean;
        const skipAutoBuild = !!g.skip_auto_build;
        const skipWorktrees = !!g.skip_worktrees;
        const separatePrBranch = !!g.effective_separate_pr_branch;
        const matchPrBranchName = !!g.effective_match_pr_branch_name;
        const autoSubmitPrStack = !!g.effective_auto_submit_pr_stack;
        const autoFixPrErrors = !!g.auto_fix_pr_errors;
        const autoFixPromptTemplate = g.auto_fix_prompt_template || "";
        return {
          gid: g.id,
          name: g.name,
          originalName: g.name,
          baseBranch: g.base_branch || "",
          originalBaseBranch: g.base_branch || "",
          resolverFrozen: ["approved", "deployed"].includes(g.status),
          resolverAgent: g.resolver_agent || "",
          originalResolverAgent: g.resolver_agent || "",
          resolverModel: g.resolver_model || "",
          originalResolverModel: g.resolver_model || "",
          proofScope, originalProofScope: proofScope,
          proofSkipAutoClean, originalProofSkipAutoClean: proofSkipAutoClean,
          skipAutoBuild, originalSkipAutoBuild: skipAutoBuild,
          skipWorktrees, originalSkipWorktrees: skipWorktrees,
          separatePrBranch, originalSeparatePrBranch: separatePrBranch,
          matchPrBranchName, originalMatchPrBranchName: matchPrBranchName,
          autoSubmitPrStack, originalAutoSubmitPrStack: autoSubmitPrStack,
          autoFixPrErrors, originalAutoFixPrErrors: autoFixPrErrors,
          autoFixPromptTemplate, originalAutoFixPromptTemplate: autoFixPromptTemplate,
          squash,
          originalSquashOn: squashOn.slice(),
          projects,
          buildEnv: envScopeDraftFromOwn(g.build_env_overrides || {}),
          buildEnvInherited: g.combined_env || {},
          manualChecksEnv: envScopeDraftFromOwn(g.manual_checks_env_overrides || {}),
          manualChecksEnvInherited: g.combined_env || {},
          branchEnv,
          branchEnvInherited,
          branches: g.branches.map((b) => ({ id: b.id, branch: b.branch })),
        };
      }

      /**
       * Opens the Edit Details modal for a review, seeding the draft from its
       * current server state. Nothing this modal does reaches the server
       * until Save.
       * @param {string} gid
       * @returns {void}
       */
      function openEditReviewDetails(gid) {
        const g = guardians.find((x) => x.id === gid);
        if (!g) return;
        reviewEditDraft = buildReviewEditDraft(g);
        renderReviewEditModal();
      }

      /**
       * Closes the Edit Details modal, discarding the draft.
       * @returns {void}
       */
      function closeEditReviewDetails() {
        reviewEditDraft = null;
        closeModal();
      }

      /**
       * Resolves which `EnvScopeDraft` a scope/branch pair addresses.
       * @param {string} scope - "build" | "manual_checks" | "branch"
       * @param {string} branchId - Ignored unless `scope` is "branch".
       * @returns {EnvScopeDraft|null}
       */
      function reviewEditScopeDraft(scope, branchId) {
        if (!reviewEditDraft) return null;
        if (scope === "build") return reviewEditDraft.buildEnv;
        if (scope === "manual_checks") return reviewEditDraft.manualChecksEnv;
        if (scope === "branch" && branchId) return reviewEditDraft.branchEnv[branchId] || null;
        return null;
      }

      /**
       * Stages a new display name for the review.
       * @param {string} value
       * @returns {void}
       */
      function onEditName(value) { if (reviewEditDraft) reviewEditDraft.name = value; }
      /**
       * Stages a new upstream/base branch.
       * @param {string} value
       * @returns {void}
       */
      function onEditBaseBranch(value) { if (reviewEditDraft) reviewEditDraft.baseBranch = value; }
      /**
       * Stages a new resolver-agent choice and re-renders (a `<select>`
       * change never loses in-progress typing the way a text input would).
       * @param {string} value
       * @returns {void}
       */
      function onEditResolverAgent(value) {
        if (!reviewEditDraft) return;
        reviewEditDraft.resolverAgent = value;
        renderReviewEditModal();
      }
      /**
       * Stages a new resolver-model value, leaving focus in the text field.
       * @param {string} value
       * @returns {void}
       */
      function onEditResolverModel(value) { if (reviewEditDraft) reviewEditDraft.resolverModel = value; }
      /**
       * Stages a new Proof-scope choice and re-renders, since the
       * skip-auto-clean sub-row's visibility depends on it.
       * @param {string} value
       * @returns {void}
       */
      function onEditProofScope(value) {
        if (!reviewEditDraft) return;
        reviewEditDraft.proofScope = value;
        renderReviewEditModal();
      }
      /**
       * Stages the "skip auto-clean branches" sub-option.
       * @param {boolean} checked
       * @returns {void}
       */
      function onEditProofSkipAutoClean(checked) { if (reviewEditDraft) reviewEditDraft.proofSkipAutoClean = checked; }
      /**
       * Stages the skip-auto-build flag.
       * @param {boolean} checked
       * @returns {void}
       */
      function onEditSkipAutoBuild(checked) { if (reviewEditDraft) reviewEditDraft.skipAutoBuild = checked; }
      /**
       * Stages the skip-per-branch-worktrees flag.
       * @param {boolean} checked
       * @returns {void}
       */
      function onEditSkipWorktrees(checked) { if (reviewEditDraft) reviewEditDraft.skipWorktrees = checked; }
      /**
       * Stages the separate-PR-branch flag and re-renders, since it gates
       * whether "match worktree branch name" is enabled.
       * @param {boolean} checked
       * @returns {void}
       */
      function onEditSeparatePrBranch(checked) {
        if (!reviewEditDraft) return;
        reviewEditDraft.separatePrBranch = checked;
        renderReviewEditModal();
      }
      /**
       * Stages the match-worktree-branch-name flag.
       * @param {boolean} checked
       * @returns {void}
       */
      function onEditMatchPrBranchName(checked) { if (reviewEditDraft) reviewEditDraft.matchPrBranchName = checked; }
      /**
       * Stages the auto-submit-PR-stack flag.
       * @param {boolean} checked
       * @returns {void}
       */
      function onEditAutoSubmitPrStack(checked) { if (reviewEditDraft) reviewEditDraft.autoSubmitPrStack = checked; }
      /**
       * Stages the auto-fix-PR-errors flag.
       * @param {boolean} checked
       * @returns {void}
       */
      function onEditAutoFixPrErrors(checked) { if (reviewEditDraft) reviewEditDraft.autoFixPrErrors = checked; }
      /**
       * Stages the auto-fix prompt template. Empty resets to the project default.
       * @param {string} value
       * @returns {void}
       */
      function onEditAutoFixPromptTemplate(value) { if (reviewEditDraft) reviewEditDraft.autoFixPromptTemplate = value; }
      /**
       * Stages one git project's squash toggle.
       * @param {string} project
       * @param {boolean} checked
       * @returns {void}
       */
      function onEditSquashToggle(project, checked) { if (reviewEditDraft) reviewEditDraft.squash[project] = checked; }

      /**
       * Stages a draft env-override row's key (only reachable for a
       * not-yet-read-only row, i.e. a freshly added one).
       * @param {string} scope
       * @param {string} branchId
       * @param {number} index
       * @param {string} value
       * @returns {void}
       */
      function onEnvRowKeyInput(scope, branchId, index, value) {
        const sd = reviewEditScopeDraft(scope, branchId);
        if (!sd || !sd.rows[index]) return;
        sd.rows[index].key = value;
      }
      /**
       * Stages a draft env-override row's value.
       * @param {string} scope
       * @param {string} branchId
       * @param {number} index
       * @param {string} value
       * @returns {void}
       */
      function onEnvRowValueInput(scope, branchId, index, value) {
        const sd = reviewEditScopeDraft(scope, branchId);
        if (!sd || !sd.rows[index]) return;
        sd.rows[index].value = value;
      }
      /**
       * Toggles a draft env-override row's tombstone state and re-renders
       * (the value field's disabled state depends on it).
       * @param {string} scope
       * @param {string} branchId
       * @param {number} index
       * @param {boolean} checked
       * @returns {void}
       */
      function onEnvRowTombstoneToggle(scope, branchId, index, checked) {
        const sd = reviewEditScopeDraft(scope, branchId);
        if (!sd || !sd.rows[index]) return;
        sd.rows[index].tombstone = checked;
        renderReviewEditModal();
      }
      /**
       * Adds a blank, freshly-editable env-override row to a scope.
       * @param {string} scope
       * @param {string} branchId
       * @returns {void}
       */
      function addEnvOverrideRow(scope, branchId) {
        const sd = reviewEditScopeDraft(scope, branchId);
        if (!sd) return;
        sd.rows.push({ key: "", value: "", tombstone: false, readOnlyKey: false });
        renderReviewEditModal();
      }
      /**
       * Adds a draft row pre-filled from a currently-inherited key, ready to
       * be edited into an override for this scope only.
       * @param {string} scope
       * @param {string} branchId
       * @param {string} key
       * @param {string} value
       * @returns {void}
       */
      function overrideInheritedKey(scope, branchId, key, value) {
        const sd = reviewEditScopeDraft(scope, branchId);
        if (!sd) return;
        sd.rows.push({ key, value, tombstone: false, readOnlyKey: true });
        renderReviewEditModal();
      }
      /**
       * Removes a draft env-override row. If the row's key was one of this
       * scope's saved overrides, its removal is staged as a `clear` on Save;
       * a freshly-added row is simply dropped.
       * @param {string} scope
       * @param {string} branchId
       * @param {number} index
       * @returns {void}
       */
      function removeEnvOverrideRow(scope, branchId, index) {
        const sd = reviewEditScopeDraft(scope, branchId);
        if (!sd || !sd.rows[index]) return;
        const row = sd.rows[index];
        if (sd.originalOwnKeys.has(row.key)) sd.removedOwnKeys.add(row.key);
        sd.rows.splice(index, 1);
        renderReviewEditModal();
      }

      /**
       * Renders one environment-override scope's editable rows plus its
       * not-yet-overridden inherited keys.
       * @param {string} scope - "build" | "manual_checks" | "branch"
       * @param {string} branchId - Empty string unless `scope` is "branch".
       * @param {string} title - Human-readable scope label for tooltips.
       * @param {{[key: string]: string}} inherited
       * @param {EnvScopeDraft} sd
       * @returns {string}
       */
      function renderReviewEditEnvScope(scope, branchId, title, inherited, sd) {
        const scopeArg = JSON.stringify(scope);
        const branchArg = JSON.stringify(branchId || "");
        const ownKeys = new Set(sd.rows.map((r) => r.key));
        const inheritedOnly = Object.keys(inherited || {}).filter((k) => !ownKeys.has(k)).sort();
        const rowsHtml = sd.rows.map((r, i) => `<div class="review-edit-env-row">
            <input type="text" class="mono review-edit-env-key" style="${REVIEW_EDIT_INPUT_STYLE}" placeholder="KEY" value="${esc(r.key)}" ${r.readOnlyKey ? "disabled" : ""} oninput="onEnvRowKeyInput(${scopeArg},${branchArg},${i},this.value)" data-tip="Environment variable name.">
            <input type="text" class="mono review-edit-env-value" style="${REVIEW_EDIT_INPUT_STYLE}" placeholder="value" value="${esc(r.value)}" ${r.tombstone ? "disabled" : ""} oninput="onEnvRowValueInput(${scopeArg},${branchArg},${i},this.value)" data-tip="Value for ${esc(title)}. Disabled while marked unset.">
            <label style="display:flex;align-items:center;gap:3px;font-size:11px;color:var(--muted)" data-tip="Unset -- remove this key from ${esc(title)}'s environment entirely, even though it would otherwise be inherited.">
              <input type="checkbox" ${r.tombstone ? "checked" : ""} onchange="onEnvRowTombstoneToggle(${scopeArg},${branchArg},${i},this.checked)">unset
            </label>
            <button class="btn" style="padding:1px 6px;font-size:11px" data-click="removeEnvOverrideRow" data-scope="${esc(scope)}" data-branch-id="${esc(branchId || "")}" data-i="${i}" data-tip="Remove this row for ${esc(title)}. Applies on Save.">✕</button>
          </div>`).join("");
        const inheritedHtml = inheritedOnly.length
          ? `<div class="row" style="flex-wrap:wrap;gap:4px;margin-top:4px">${inheritedOnly.map((k) => `<span class="badge mono" style="font-size:10px;cursor:pointer" data-click="overrideInheritedKey" data-scope="${esc(scope)}" data-branch-id="${esc(branchId || "")}" data-key="${esc(k)}" data-value="${esc(inherited[k])}" data-tip="Inherited: ${esc(inherited[k])}\nClick to override for ${esc(title)} only.">${esc(k)} ▸</span>`).join("")}</div>`
          : "";
        return `<div style="margin-top:8px">
          <div style="font-size:12px;color:var(--muted);margin-bottom:4px">${esc(title)}</div>
          ${rowsHtml || `<div class="kv-row" style="margin:0"><span class="v" style="font-size:11px;color:var(--muted)">No overrides</span></div>`}
          ${inheritedHtml}
          <button class="btn" style="padding:2px 8px;font-size:11px;margin-top:4px" data-click="addEnvOverrideRow" data-scope="${esc(scope)}" data-branch-id="${esc(branchId || "")}" data-tip="Add a new environment-variable override for ${esc(title)}. Applies on Save.">+ Add override</button>
        </div>`;
      }

      /**
       * Renders the Edit Details modal from `reviewEditDraft`. A one-shot
       * `innerHTML` write into `#modal-root`, matching every other modal in
       * this codebase -- not itself subject to `renderReviewDetail()`'s
       * background re-renders.
       * @returns {void}
       */
      function renderReviewEditModal() {
        const draft = reviewEditDraft;
        if (!draft) return;
        const g = guardians.find((x) => x.id === draft.gid);
        const cwd = (g && (g.git_root || (g.projects && g.projects[0]))) || "";
        const resolverSection = draft.resolverFrozen
          ? `<div class="kv-row"><span class="k">resolver agent</span><span class="v mono">${esc(draft.resolverAgent || "agent default")}</span></div>
             <div class="kv-row"><span class="k">resolver model</span><span class="v mono">${esc(draft.resolverModel || "agent default")}</span></div>`
          : `<div class="kv-row"><span class="k">resolver agent</span><select style="${REVIEW_EDIT_INPUT_STYLE}" onchange="onEditResolverAgent(this.value)" data-tip="Conflict-resolver backend used when the AI agent resolves merge conflicts. Applies on Save.">${resolverOptionHtml(cwd, draft.resolverAgent)}</select></div>
             <div class="kv-row"><span class="k">resolver model</span><input type="text" class="mono" style="${REVIEW_EDIT_INPUT_STYLE};width:200px" value="${esc(draft.resolverModel)}" placeholder="agent default" oninput="onEditResolverModel(this.value)" data-tip="Exact model passed to the selected resolver agent. Clear to use the agent's default. Applies on Save."></div>`;
        const proofScopeSection = `<div class="kv-row"><span class="k">proof scope</span><div style="display:flex;flex-direction:column;gap:4px">
            <select style="${REVIEW_EDIT_INPUT_STYLE}" onchange="onEditProofScope(this.value)" data-tip="How often the dedicated final-proof agent call runs after a branch's rebase. Applies on Save.">
              <option value="each_branch" ${draft.proofScope === "each_branch" ? "selected" : ""}>each branch</option>
              <option value="final_branch" ${draft.proofScope === "final_branch" ? "selected" : ""}>the final branch</option>
              <option value="nothing" ${draft.proofScope === "nothing" ? "selected" : ""}>nothing</option>
            </select>
            ${draft.proofScope === "each_branch" ? `<label style="display:flex;align-items:center;gap:6px;font-size:12px;color:var(--muted)" data-tip="Within \"each branch\" scope, additionally skip verification on branches whose rebase applied cleanly with no conflict at all.">
              <input type="checkbox" ${draft.proofSkipAutoClean ? "checked" : ""} onchange="onEditProofSkipAutoClean(this.checked)">skip auto-clean branches</label>` : ""}
          </div></div>`;
        const squashSection = draft.projects.map((p) => {
          const label = (p || "").split(/[\\/]/).filter(Boolean).pop() || p;
          return `<label style="display:flex;align-items:center;gap:6px;font-size:12px;color:var(--muted);margin-top:4px" data-tip="Collapse this git project's task branches to a single squashed commit each in the review worktree. Applies on the next Merge / rebase.">
            <input type="checkbox" ${draft.squash[p] ? "checked" : ""} onchange="onEditSquashToggle(${JSON.stringify(p)},this.checked)">squash${draft.projects.length > 1 ? ` — <span class="mono" style="font-size:11px">${esc(label)}</span>` : " each task branch to a single commit"}</label>`;
        }).join("");
        const buildEnvSection = renderReviewEditEnvScope("build", "", "the build step", draft.buildEnvInherited, draft.buildEnv);
        const manualChecksEnvSection = renderReviewEditEnvScope("manual_checks", "", "the manual-checks step", draft.manualChecksEnvInherited, draft.manualChecksEnv);
        const branchEnvSections = draft.branches.map((b) => renderReviewEditEnvScope("branch", b.id, `branch ${b.branch}`, draft.branchEnvInherited[b.id] || {}, draft.branchEnv[b.id])).join("");
        byId("modal-root").innerHTML = `<div class="modal-bg" onclick="if(event.target===this)closeEditReviewDetails()"><div class="modal review-edit-modal">
            <h2>Edit Details</h2>
            <div class="edit-form">
              <label data-tip="Display name for this review — shown in the sidebar list. Applies on Save.">name<input type="text" value="${esc(draft.name)}" oninput="onEditName(this.value)"></label>
              <label data-tip="The branch every submitted branch is rebased onto. Type or focus to load remote branch suggestions. Triggers a rebase on Save.">upstream branch<input type="text" class="mono" list="base-datalist" value="${esc(draft.baseBranch)}" onfocus="loadBaseBranches(${JSON.stringify(draft.gid)})" oninput="onEditBaseBranch(this.value)"><datalist id="base-datalist">${(baseBranchCache[draft.gid] || []).map((b) => `<option value="${esc(b)}"></option>`).join("")}</datalist></label>
            </div>
            ${resolverSection}
            ${proofScopeSection}
            <h3 class="section">check gates</h3>
            <label style="display:flex;align-items:center;gap:6px;font-size:12px;color:var(--muted);margin-top:6px" data-tip="Skip the finalize-time build/check step entirely. Applies on Save.">
              <input type="checkbox" ${draft.skipAutoBuild ? "checked" : ""} onchange="onEditSkipAutoBuild(this.checked)">skip auto-build</label>
            <label style="display:flex;align-items:center;gap:6px;font-size:12px;color:var(--muted);margin-top:4px" data-tip="Build the entire branch stack in one shared worktree instead of isolated per-branch worktrees. Applies on Save.">
              <input type="checkbox" ${draft.skipWorktrees ? "checked" : ""} onchange="onEditSkipWorktrees(this.checked)">skip per-branch worktrees</label>
            <h3 class="section">squash</h3>${squashSection}
            <h3 class="section">pull requests</h3>
            <label style="display:flex;align-items:center;gap:6px;font-size:12px;color:var(--muted);margin-top:6px" data-tip="Push this review's PRs to a branch of their own, derived from the task branch, instead of opening them straight from the review branch. Applies on Save.">
              <input type="checkbox" ${draft.separatePrBranch ? "checked" : ""} onchange="onEditSeparatePrBranch(this.checked)">separate PR branch</label>
            <label style="display:flex;align-items:center;gap:6px;font-size:12px;color:var(--muted);margin-top:4px${draft.separatePrBranch ? "" : ";opacity:0.5"}" data-tip="Only applies when 'separate PR branch' is on. Use the exact worktree/feature branch name as the PR branch. Applies on Save.">
              <input type="checkbox" ${draft.matchPrBranchName ? "checked" : ""} ${draft.separatePrBranch ? "" : "disabled"} onchange="onEditMatchPrBranchName(this.checked)">match worktree branch name</label>
            <label style="display:flex;align-items:center;gap:6px;font-size:12px;color:var(--muted);margin-top:4px" data-tip="Automatically submit/grow this review's PR stack as each branch finishes rebasing. Applies on Save.">
              <input type="checkbox" ${draft.autoSubmitPrStack ? "checked" : ""} onchange="onEditAutoSubmitPrStack(this.checked)">auto-submit PR stack</label>
            <label style="display:flex;align-items:center;gap:6px;font-size:12px;color:var(--muted);margin-top:4px" data-tip="Automatically dispatch the resolver agent to fix this review's PR when its CI checks go red. Applies on Save.">
              <input type="checkbox" ${draft.autoFixPrErrors ? "checked" : ""} onchange="onEditAutoFixPrErrors(this.checked)">auto-fix PR errors</label>
            <div style="margin-top:8px">
              <label for="auto-fix-prompt-template-input" style="font-size:12px;color:var(--muted);display:block;margin-bottom:4px" data-tip="${AUTO_FIX_PROMPT_TEMPLATE_TIP}">auto-fix prompt template</label>
              <textarea id="auto-fix-prompt-template-input" rows="4" style="${REVIEW_EDIT_TEXTAREA_STYLE}" placeholder="inherits project default" oninput="onEditAutoFixPromptTemplate(this.value)" data-tip="${AUTO_FIX_PROMPT_TEMPLATE_TIP}">${esc(draft.autoFixPromptTemplate)}</textarea>
            </div>
            <h3 class="section">environment overrides</h3>
            ${buildEnvSection}
            ${manualChecksEnvSection}
            ${branchEnvSections}
            <div id="review-edit-err" class="verr"></div>
            <div class="btn-row" style="margin-top:12px"><button class="btn" onclick="closeEditReviewDetails()">Cancel</button><button class="btn primary" onclick="saveReviewEditDetails()" data-tip="Apply every change made in this modal in a single request. At most one rebase is triggered, only if something rebase-relevant changed.">Save</button></div>
          </div></div>`;
      }

      /**
       * Builds one env-override scope's `{set, unset, clear}` patch from its
       * draft rows and removed-own-key set, matching the shape
       * `POST .../details`'s `build_env`/`manual_checks_env`/`branch_env`
       * fields expect.
       * @param {EnvScopeDraft} sd
       * @returns {{set: {[key: string]: string}, unset: string[], clear: string[]}}
       */
      function envPatchFromScopeDraft(sd) {
        /** @type {{[key: string]: string}} */
        const set = {};
        const unset = [];
        for (const row of sd.rows) {
          const key = row.key.trim();
          if (!key) continue;
          if (row.tombstone) unset.push(key); else set[key] = row.value;
        }
        return { set, unset, clear: Array.from(sd.removedOwnKeys) };
      }
      /**
       * Whether an env patch has nothing to send.
       * @param {{set: {[key: string]: string}, unset: string[], clear: string[]}} patch
       * @returns {boolean}
       */
      function envPatchIsEmpty(patch) {
        return Object.keys(patch.set).length === 0 && patch.unset.length === 0 && patch.clear.length === 0;
      }

      /**
       * Saves the Edit Details draft: builds one payload from only the
       * fields that actually changed since the modal opened, then fires a
       * single `POST .../details` request. On success, closes the modal and
       * refreshes the board; on failure (surfaced by `guardianAction`'s own
       * error toast), leaves the modal open with the draft intact so nothing
       * is lost.
       * @returns {Promise<void>}
       */
      async function saveReviewEditDetails() {
        const draft = reviewEditDraft;
        if (!draft) return;
        /** @type {{[key: string]: *}} */
        const body = {};
        const name = draft.name.trim();
        if (name && name !== draft.originalName) body.name = name;
        const base = draft.baseBranch.trim();
        if (base && base !== draft.originalBaseBranch) body.base_branch = base;
        if (!draft.resolverFrozen) {
          const model = draft.resolverModel.trim();
          if (draft.resolverAgent !== draft.originalResolverAgent || model !== draft.originalResolverModel) {
            body.resolver_agent = draft.resolverAgent;
            body.resolver_model = model;
          }
        }
        if (draft.proofScope !== draft.originalProofScope || draft.proofSkipAutoClean !== draft.originalProofSkipAutoClean) {
          body.proof_scope = draft.proofScope;
          body.proof_skip_auto_clean = draft.proofSkipAutoClean;
        }
        if (draft.skipAutoBuild !== draft.originalSkipAutoBuild) body.skip_auto_build = draft.skipAutoBuild;
        if (draft.skipWorktrees !== draft.originalSkipWorktrees) body.skip_worktrees = draft.skipWorktrees;
        if (draft.separatePrBranch !== draft.originalSeparatePrBranch) body.separate_pr_branch = draft.separatePrBranch;
        if (draft.matchPrBranchName !== draft.originalMatchPrBranchName) body.match_pr_branch_name = draft.matchPrBranchName;
        if (draft.autoSubmitPrStack !== draft.originalAutoSubmitPrStack) body.auto_submit_pr_stack = draft.autoSubmitPrStack;
        if (draft.autoFixPrErrors !== draft.originalAutoFixPrErrors) body.auto_fix_pr_errors = draft.autoFixPrErrors;
        if (draft.autoFixPromptTemplate !== draft.originalAutoFixPromptTemplate) body.auto_fix_prompt_template = draft.autoFixPromptTemplate;
        const squashOn = draft.projects.filter((p) => draft.squash[p]).sort();
        if (JSON.stringify(squashOn) !== JSON.stringify(draft.originalSquashOn.slice().sort())) {
          body.squash_projects = squashOn;
        }
        const buildPatch = envPatchFromScopeDraft(draft.buildEnv);
        if (!envPatchIsEmpty(buildPatch)) body.build_env = buildPatch;
        const manualPatch = envPatchFromScopeDraft(draft.manualChecksEnv);
        if (!envPatchIsEmpty(manualPatch)) body.manual_checks_env = manualPatch;
        /** @type {{[branchId: string]: *}} */
        const branchEnvBody = {};
        for (const b of draft.branches) {
          const sd = draft.branchEnv[b.id];
          if (!sd) continue;
          const patch = envPatchFromScopeDraft(sd);
          if (!envPatchIsEmpty(patch)) branchEnvBody[b.id] = patch;
        }
        if (Object.keys(branchEnvBody).length) body.branch_env = branchEnvBody;

        if (Object.keys(body).length === 0) { closeEditReviewDetails(); return; }
        const resp = await guardianAction(`/api/guardians/${draft.gid}/details`, body);
        if (resp && resp.ok) {
          const data = await resp.json().catch(() => null);
          const change = data && data.base_change;
          if (change && change.status === "rebase_in_progress") {
            const action = change.action && change.action.url
              ? {
                  label: change.action.label || "Stop and restart now",
                  run: async () => {
                    await guardianAction(change.action.url);
                    tick();
                  },
                }
              : undefined;
            showWarningToast(
              change.message || "A rebase is already in progress. We'll trigger a new rebase once this one completes.",
              action,
            );
          } else if (change && change.message) {
            showInfoToast(change.message);
          }
          closeEditReviewDetails();
          tick();
        }
      }
