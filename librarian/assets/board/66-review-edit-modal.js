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
       * @property {boolean} resolverFrozen - True once the review is merged/deployed -- base/resolver are rejected server-side past that point.
       * @property {string} resolverAgent
       * @property {string} originalResolverAgent
       * @property {string} resolverModel
       * @property {string} originalResolverModel
       * @property {string} proofScope - "each_branch" | "final_branch" | "nothing"
       * @property {string} originalProofScope
       * @property {boolean} proofSkipAutoClean
       * @property {boolean} originalProofSkipAutoClean
       * @property {boolean} skipWorktrees
       * @property {boolean} originalSkipWorktrees
       * @property {boolean} skipBaseUpdates - RAL-514: this review's own override for whether the automatic base-branch auto-update rebuild is skipped.
       * @property {boolean} originalSkipBaseUpdates
       * @property {boolean} separatePrBranch
       * @property {boolean} originalSeparatePrBranch
       * @property {boolean} matchPrBranchName
       * @property {boolean} originalMatchPrBranchName
       * @property {boolean} autoSubmitPrStack
       * @property {boolean} originalAutoSubmitPrStack
       * @property {boolean} dualRootPr
       * @property {boolean} originalDualRootPr
       * @property {boolean} autoFixPrErrors
       * @property {boolean} originalAutoFixPrErrors
       * @property {string} autoFixPromptTemplate
       * @property {string} originalAutoFixPromptTemplate
       * @property {boolean} discourageTests
       * @property {boolean} originalDiscourageTests
       * @property {RebuildOnDraft} rebuildOn - When this review's prepared build is rebuilt; `inherit` means the project default applies.
       * @property {RebuildOnDraft} originalRebuildOn
       * @property {string[]} rebuildOnEffective - The resolved triggers shown while inheriting.
       * @property {{[project: string]: boolean}} squash
       * @property {string[]} originalSquashOn
       * @property {string[]} projects
       * @property {string} focus - Which setup-strip chip opened this modal ("onto", "resolver", "proof", "squash", "rebuild", "worktrees"), or "" when opened from the editor button. The matching group is highlighted and scrolled to.
       */

      /**
       * One environment-override scope opened on its own, from the ⋯ of the
       * section it governs rather than from the review's Setup modal.
       *
       * Piling all four scopes into Setup meant reading a stack of tables and
       * working out which one applied to the commands you were looking at. A
       * scope reached from its own section needs no such disambiguation, so
       * this editor shows exactly one — and Setup no longer shows any.
       * @typedef {object} EnvEditDraft
       * @property {string} gid
       * @property {string} scope - "build" | "manual_checks" | "branch"
       * @property {string} branchId - Empty unless `scope` is "branch".
       * @property {string} title - Human-readable scope label, e.g. "test actions".
       * @property {{[key: string]: string}} inherited
       * @property {EnvScopeDraft} sd
       */

      /** @type {ReviewEditDraft|null} */
      let reviewEditDraft = null;
      /** @type {EnvEditDraft|null} */
      let envEditDraft = null;

      const REVIEW_EDIT_INPUT_STYLE = "background:var(--bg);border:1px solid var(--border);color:var(--text);border-radius:4px;padding:2px 5px;font-size:12px";
      const REVIEW_EDIT_TEXTAREA_STYLE = "width:100%;box-sizing:border-box;resize:vertical;font-family:inherit;font-size:12px;padding:6px;background:var(--bg);color:var(--text);border:1px solid var(--border);border-radius:4px";
      const AUTO_FIX_PROMPT_TEMPLATE_TIP = "Prompt handed to the resolver agent when 'auto-fix PR errors' fires, with the literal <<prompt>> placeholder replaced by the failing branch's own Cell prompts. Must contain <<prompt>> or Save is rejected. Leave blank to inherit the project default. Applies on Save.";
      const DISCOURAGE_TESTS_TIP = "When on, the resolver agent dispatched to fix this review's pull request -- whether from 'auto-fix PR errors' or a manual PR-fix request -- is told to prefer automatic formatters, linters, and static-analysis tools and to avoid running a broad or expensive test suite. Comprehensive validation still happens separately, through the pull request's own checks. Applies on Save.";

      // RALPHUS-REBUILD-ON:BEGIN
      // When a review's prepared build is torn down and rebuilt. The draft
      // logic is pure; the two render helpers need only `esc` and `document`.
      // node --test slices this region and exercises it (test/board-rebuild-on.test.mjs).
      // The review Setup modal and the project Review Settings modal share it.

      /**
       * One modal's draft of the `rebuild_on` setting.
       * @typedef {object} RebuildOnDraft
       * @property {boolean} inherit - True = no explicit value; the project/global default applies.
       * @property {string[]} triggers - The explicit triggers (canonical order). Ignored while `inherit` is true.
       */

      /** Every event that can rebuild a review's prepared build, in display order. */
      const REBUILD_TRIGGERS = ["rebase", "feedback", "auto_fix"];
      /** Reader-facing names for each trigger. */
      const REBUILD_TRIGGER_LABELS = { rebase: "rebase", feedback: "reviewer feedback", auto_fix: "auto PR fix" };

      /**
       * Reduces any list to the canonical form the daemon stores: only known
       * triggers, deduplicated, in display order.
       * @param {string[]|null|undefined} list
       * @returns {string[]}
       */
      function normalizeRebuildOn(list) {
        const wanted = new Set(Array.isArray(list) ? list : []);
        return REBUILD_TRIGGERS.filter((t) => wanted.has(t));
      }

      /**
       * Seeds a draft from a review/project's explicit value and its resolved
       * default. A null/absent explicit value means "inherit".
       * @param {string[]|null|undefined} explicit
       * @param {string[]|null|undefined} effective
       * @returns {RebuildOnDraft}
       */
      function buildRebuildOnDraft(explicit, effective) {
        const inherit = explicit === null || explicit === undefined;
        const seed = inherit ? (effective === null || effective === undefined ? REBUILD_TRIGGERS : effective) : explicit;
        return { inherit, triggers: normalizeRebuildOn(seed) };
      }

      /**
       * The triggers a draft currently displays: the resolved default while
       * inheriting, its own explicit set otherwise.
       * @param {RebuildOnDraft} draft
       * @param {string[]|null|undefined} effective
       * @returns {string[]}
       */
      function rebuildOnShown(draft, effective) {
        if (draft.inherit) return normalizeRebuildOn(effective === null || effective === undefined ? REBUILD_TRIGGERS : effective);
        return normalizeRebuildOn(draft.triggers);
      }

      /**
       * Switches a draft between inheriting and explicit. Leaving inherit
       * seeds the explicit set from what was being inherited, so unticking the
       * box pins today's behavior instead of silently changing it.
       * @param {RebuildOnDraft} draft
       * @param {boolean} inherit
       * @param {string[]|null|undefined} effective
       * @returns {void}
       */
      function setRebuildInherit(draft, inherit, effective) {
        if (!inherit && draft.inherit) draft.triggers = rebuildOnShown(draft, effective);
        draft.inherit = inherit;
      }

      /**
       * Turns one trigger on or off in an explicit draft. Unknown triggers are ignored.
       * @param {RebuildOnDraft} draft
       * @param {string} trigger
       * @param {boolean} on
       * @returns {void}
       */
      function setRebuildTrigger(draft, trigger, on) {
        if (!REBUILD_TRIGGERS.includes(trigger)) return;
        const set = new Set(draft.triggers);
        if (on) set.add(trigger); else set.delete(trigger);
        draft.triggers = normalizeRebuildOn(Array.from(set));
      }

      /**
       * One-line, human-readable policy for a draft.
       * @param {RebuildOnDraft} draft
       * @param {string[]|null|undefined} effective
       * @returns {string}
       */
      function rebuildOnSummary(draft, effective) {
        const shown = rebuildOnShown(draft, effective);
        const prefix = draft.inherit ? "inherited: " : "";
        if (shown.length === 0) return `${prefix}never — rebuild manually`;
        if (shown.length === REBUILD_TRIGGERS.length) return `${prefix}every rebase, feedback, and auto PR fix`;
        return `${prefix}${shown.map((t) => REBUILD_TRIGGER_LABELS[/** @type {keyof typeof REBUILD_TRIGGER_LABELS} */ (t)]).join(" + ")}`;
      }

      /**
       * Short value for the setup-strip chip.
       * @param {string[]|null|undefined} effective - The review's resolved triggers.
       * @returns {string}
       */
      function rebuildOnChipText(effective) {
        const shown = normalizeRebuildOn(effective === null || effective === undefined ? REBUILD_TRIGGERS : effective);
        if (shown.length === 0) return "manual only";
        if (shown.length === REBUILD_TRIGGERS.length) return "on every change";
        return shown.map((t) => t.replace("_", " ")).join(" + ");
      }

      /**
       * Whether a draft differs from the state it was seeded with. Two
       * inheriting drafts are equal whatever their (ignored) triggers hold.
       * @param {RebuildOnDraft} draft
       * @param {RebuildOnDraft} original
       * @returns {boolean}
       */
      function rebuildOnChanged(draft, original) {
        if (draft.inherit || original.inherit) return draft.inherit !== original.inherit;
        return JSON.stringify(normalizeRebuildOn(draft.triggers)) !== JSON.stringify(normalizeRebuildOn(original.triggers));
      }

      /**
       * The request-body value for a draft: `null` clears back to inherit, a
       * list (possibly empty = manual only) sets an explicit policy.
       * @param {RebuildOnDraft} draft
       * @returns {string[]|null}
       */
      function rebuildOnBodyValue(draft) {
        return draft.inherit ? null : normalizeRebuildOn(draft.triggers);
      }

      const REBUILD_ON_TIP = "When this review's prepared build is torn down and built again. A rebuild first runs each action's [review.action.lifecycle] before_reset_command teardown hooks (if declared), resets its build root, then re-runs its preparation. Tick the events that should trigger one; untick all of them to build once and only rebuild when you press 'Rebuild now'. Applies on Save.";
      /** Tooltip for each trigger checkbox. */
      const REBUILD_TRIGGER_TIPS = {
        rebase: "Rebuild after every merge / rebase of this review's stack (including automatic base-branch rebases). Untick if the build is slow and the rebased code rarely changes what you test.",
        feedback: "Rebuild after reviewer feedback has been applied to a branch and the stack restacked. Untick to keep testing the previous build while feedback lands.",
        auto_fix: "Rebuild after an automatic PR-error fix has been applied and the stack restacked. Untick so unattended fix loops never trigger a rebuild.",
      };

      /**
       * Renders the "rebuild preparation when" control shared by the review
       * Setup modal and the project Review Settings modal: a use-the-default
       * checkbox, one checkbox per trigger, and a one-line summary.
       * @param {string} prefix - Element-id prefix, unique per modal ("review" | "project").
       * @param {RebuildOnDraft} draft
       * @param {string[]|null|undefined} effective - The default shown while inheriting.
       * @param {string} inheritLabel - Label for the use-the-default checkbox.
       * @param {string} inheritTip - Tooltip for it.
       * @param {string} onInherit - Global handler name called with the checked state.
       * @param {string} onTrigger - Global handler name called with (trigger, checked).
       * @returns {string}
       */
      function renderRebuildOnFieldsHtml(prefix, draft, effective, inheritLabel, inheritTip, onInherit, onTrigger) {
        const shown = rebuildOnShown(draft, effective);
        const boxes = REBUILD_TRIGGERS.map((t) => `<label style="display:flex;align-items:center;gap:6px;font-size:12px;color:var(--muted);margin-top:4px;margin-left:18px" data-tip="${esc(REBUILD_TRIGGER_TIPS[/** @type {keyof typeof REBUILD_TRIGGER_TIPS} */ (t)])}">
            <input type="checkbox" id="${prefix}-rebuild-${t}" ${shown.includes(t) ? "checked" : ""} ${draft.inherit ? "disabled" : ""} onchange="${onTrigger}('${t}',this.checked)">${esc(REBUILD_TRIGGER_LABELS[/** @type {keyof typeof REBUILD_TRIGGER_LABELS} */ (t)])}</label>`).join("");
        return `<div id="${prefix}-rebuild-on" data-tip="${esc(REBUILD_ON_TIP)}">
            <label style="display:flex;align-items:center;gap:6px;font-size:12px;color:var(--muted);margin-top:4px" data-tip="${esc(inheritTip)}">
              <input type="checkbox" id="${prefix}-rebuild-inherit" ${draft.inherit ? "checked" : ""} onchange="${onInherit}(this.checked)">${esc(inheritLabel)}</label>
            ${boxes}
            <div class="hint" id="${prefix}-rebuild-summary" data-tip="${esc(REBUILD_ON_TIP)}">${esc(rebuildOnSummary(draft, effective))}</div>
          </div>`;
      }

      /**
       * Updates an already-rendered rebuild-on control in place after a
       * checkbox change, so the modal does not re-render (a full re-render
       * would reset its scroll position).
       * @param {string} prefix - The same prefix passed to `renderRebuildOnFieldsHtml`.
       * @param {RebuildOnDraft} draft
       * @param {string[]|null|undefined} effective
       * @returns {void}
       */
      function refreshRebuildOnFields(prefix, draft, effective) {
        const shown = rebuildOnShown(draft, effective);
        for (const t of REBUILD_TRIGGERS) {
          const box = /** @type {HTMLInputElement|null} */ (document.getElementById(`${prefix}-rebuild-${t}`));
          if (!box) continue;
          box.checked = shown.includes(t);
          box.disabled = draft.inherit;
        }
        const summary = document.getElementById(`${prefix}-rebuild-summary`);
        if (summary) summary.textContent = rebuildOnSummary(draft, effective);
      }
      // RALPHUS-REBUILD-ON:END

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
        const squashOn = g.squash_projects || [];
        const projects = (g.projects && g.projects.length) ? g.projects : [g.git_root || ""];
        /** @type {{[project: string]: boolean}} */
        const squash = {};
        for (const p of projects) squash[p] = squashOn.includes(p);
        const proofScope = g.effective_proof_scope || "each_branch";
        const proofSkipAutoClean = !!g.effective_proof_skip_auto_clean;
        const skipWorktrees = !!g.skip_worktrees;
        const skipBaseUpdates = !!g.effective_skip_base_updates;
        const separatePrBranch = !!g.effective_separate_pr_branch;
        const matchPrBranchName = !!g.effective_match_pr_branch_name;
        const autoSubmitPrStack = !!g.effective_auto_submit_pr_stack;
        const dualRootPr = !!g.effective_dual_root_pr;
        const autoFixPrErrors = !!g.auto_fix_pr_errors;
        const autoFixPromptTemplate = g.auto_fix_prompt_template || "";
        const discourageTests = !!g.discourage_tests_during_auto_pull_request_fixes;
        const rebuildOnEffective = normalizeRebuildOn(g.effective_rebuild_on === undefined ? REBUILD_TRIGGERS : g.effective_rebuild_on);
        return {
          gid: g.id,
          name: g.name,
          originalName: g.name,
          baseBranch: g.base_branch || "",
          originalBaseBranch: g.base_branch || "",
          resolverFrozen: ["merged", "approved", "deployed"].includes(g.status),
          resolverAgent: g.resolver_agent || "",
          originalResolverAgent: g.resolver_agent || "",
          resolverModel: g.resolver_model || "",
          originalResolverModel: g.resolver_model || "",
          proofScope, originalProofScope: proofScope,
          proofSkipAutoClean, originalProofSkipAutoClean: proofSkipAutoClean,
          skipWorktrees, originalSkipWorktrees: skipWorktrees,
          skipBaseUpdates, originalSkipBaseUpdates: skipBaseUpdates,
          separatePrBranch, originalSeparatePrBranch: separatePrBranch,
          matchPrBranchName, originalMatchPrBranchName: matchPrBranchName,
          autoSubmitPrStack, originalAutoSubmitPrStack: autoSubmitPrStack,
          dualRootPr, originalDualRootPr: dualRootPr,
          autoFixPrErrors, originalAutoFixPrErrors: autoFixPrErrors,
          autoFixPromptTemplate, originalAutoFixPromptTemplate: autoFixPromptTemplate,
          discourageTests, originalDiscourageTests: discourageTests,
          rebuildOn: buildRebuildOnDraft(g.rebuild_on, rebuildOnEffective),
          originalRebuildOn: buildRebuildOnDraft(g.rebuild_on, rebuildOnEffective),
          rebuildOnEffective,
          squash,
          originalSquashOn: squashOn.slice(),
          projects,
          focus: "",
        };
      }

      /**
       * Opens the Edit Details modal for a review, seeding the draft from its
       * current server state. Nothing this modal does reaches the server
       * until Save. This can be reached from a sidebar row's own "..." menu
       * for a review that has never been the selected/open one -- unlike
       * `renderReviewDetail`, this can't just wait for the next poll to fill
       * in full detail, so it fetches it itself via `fetchGuardianDetail`
       * (see `70-sse.js`) when missing.
       * @param {string} gid
       * @param {string} [focus] - A setup-strip chip key ("onto", "resolver",
       *   "proof", "squash", "worktrees"). The chip is both the
       *   display of a setting and the way in to editing it, so arriving from
       *   one lands on the field it showed rather than at the top of a modal
       *   you then have to search.
       * @returns {Promise<void>}
       */
      async function openEditReviewDetails(gid, focus) {
        let g = guardians.find((x) => x.id === gid);
        if (!g) return;
        if (!g.branches) {
          await fetchGuardianDetail(gid);
          g = guardians.find((x) => x.id === gid);
          if (!g || !g.branches) { notify("error", "Failed to load review details."); return; }
        }
        const draft = buildReviewEditDraft(g);
        draft.focus = focus || "";
        reviewEditDraft = draft;
        renderReviewEditModal();
        const cwd = g.git_root || (g.projects && g.projects[0]) || "";
        preloadAgentSelect(cwd, () => reviewEditDraft === draft, renderReviewEditModal);
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
       * Resolves which `EnvScopeDraft` a scope/branch pair addresses. Only the
       * standalone per-scope editor edits environment overrides now, so this
       * resolves against whichever scope that editor currently has open.
       * @param {string} scope - "build" | "manual_checks" | "branch"
       * @param {string} branchId - Ignored unless `scope` is "branch".
       * @returns {EnvScopeDraft|null}
       */
      function reviewEditScopeDraft(scope, branchId) {
        const d = envEditDraft;
        if (!d || d.scope !== scope) return null;
        if (scope === "branch" && d.branchId !== branchId) return null;
        return d.sd;
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
       * Stages the skip-per-branch-worktrees flag.
       * @param {boolean} checked
       * @returns {void}
       */
      function onEditSkipWorktrees(checked) { if (reviewEditDraft) reviewEditDraft.skipWorktrees = checked; }
      /**
       * Stages the skip-automatic-base-rebasing flag (RAL-514).
       * @param {boolean} checked
       * @returns {void}
       */
      function onEditSkipBaseUpdates(checked) { if (reviewEditDraft) reviewEditDraft.skipBaseUpdates = checked; }
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
       * Stages the dual-root-PR flag.
       * @param {boolean} checked
       * @returns {void}
       */
      function onEditDualRootPr(checked) { if (reviewEditDraft) reviewEditDraft.dualRootPr = checked; }
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
       * Stages the discourage-tests-during-auto-PR-fix flag.
       * @param {boolean} checked
       * @returns {void}
       */
      function onEditDiscourageTests(checked) { if (reviewEditDraft) reviewEditDraft.discourageTests = checked; }
      /**
       * Stages "use the project default" for when this review rebuilds its
       * prepared build.
       * @param {boolean} checked
       * @returns {void}
       */
      function onEditRebuildOnInherit(checked) {
        const d = reviewEditDraft;
        if (!d) return;
        setRebuildInherit(d.rebuildOn, checked, d.rebuildOnEffective);
        refreshRebuildOnFields("review", d.rebuildOn, d.rebuildOnEffective);
      }
      /**
       * Stages one rebuild trigger on or off for this review.
       * @param {string} trigger - "rebase" | "feedback" | "auto_fix"
       * @param {boolean} checked
       * @returns {void}
       */
      function onEditRebuildOnTrigger(trigger, checked) {
        const d = reviewEditDraft;
        if (!d) return;
        setRebuildTrigger(d.rebuildOn, trigger, checked);
        refreshRebuildOnFields("review", d.rebuildOn, d.rebuildOnEffective);
      }
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
        renderEnvOverridesEditor();
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
        renderEnvOverridesEditor();
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
        renderEnvOverridesEditor();
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
        renderEnvOverridesEditor();
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
       * Opens the standalone environment-override editor for one scope, reached
       * from the ⋯ of the section that scope governs.
       *
       * The review's own detail already carries every scope's saved overrides
       * and its inherited layer, so this opens from state the board has rather
       * than fetching anything — except when reached for a review that has
       * never been the open one, where there is no detail to read yet.
       * @param {string} gid
       * @param {string} scope - "build" | "manual_checks" | "branch"
       * @param {string} branchId - Empty unless `scope` is "branch".
       * @returns {Promise<void>}
       */
      async function openEnvOverridesEditor(gid, scope, branchId) {
        let g = guardians.find((x) => x.id === gid);
        if (!g) return;
        if (!g.branches) {
          await fetchGuardianDetail(gid);
          g = guardians.find((x) => x.id === gid);
          if (!g || !g.branches) { notify("error", "Failed to load review details."); return; }
        }
        const review = g;
        if (scope === "branch") {
          const b = review.branches.find((x) => x.id === branchId);
          if (!b) { notify("error", "That branch is no longer part of this review."); return; }
          envEditDraft = {
            gid, scope, branchId,
            title: `branch ${b.branch}`,
            inherited: b.inherited_env || {},
            sd: envScopeDraftFromOwn(b.env_overrides || {}),
          };
        } else if (scope === "manual_checks") {
          envEditDraft = {
            gid, scope, branchId: "",
            title: "manual checks",
            inherited: review.combined_env || {},
            sd: envScopeDraftFromOwn(review.manual_checks_env_overrides || {}),
          };
        } else {
          envEditDraft = {
            gid, scope: "build", branchId: "",
            title: "manual-check preparation and test actions",
            inherited: review.combined_env || {},
            sd: envScopeDraftFromOwn(review.build_env_overrides || {}),
          };
        }
        renderEnvOverridesEditor();
      }
      /**
       * Closes the per-scope environment-override editor, discarding its draft.
       * @returns {void}
       */
      function closeEnvOverridesEditor() {
        envEditDraft = null;
        closeModal();
      }
      /**
       * Renders the per-scope environment-override editor from `envEditDraft`.
       * @returns {void}
       */
      function renderEnvOverridesEditor() {
        const d = envEditDraft;
        if (!d) return;
        byId("modal-root").innerHTML = `<div class="modal-bg" onclick="if(event.target===this)closeEnvOverridesEditor()"><div class="modal" style="width:620px;max-width:94vw">
            <h2 data-tip="Environment variables this one surface overrides.\nEverything not listed here is inherited — the inherited layer is listed below the overrides.\nNothing is sent until Save.">Environment — ${esc(d.title)}</h2>
            ${renderReviewEditEnvScope(d.scope, d.branchId, d.title, d.inherited, d.sd)}
            <div class="btn-row" style="margin-top:12px;justify-content:flex-end"><button class="btn" onclick="closeEnvOverridesEditor()" data-tip="Close without applying anything typed here.">Cancel</button><button class="btn primary" onclick="saveEnvOverridesEditor()" data-tip="Apply this scope's overrides in one request. Only this scope is touched — no other environment, and no other review setting.">Save</button></div>
          </div></div>`;
      }
      /**
       * Saves the per-scope environment-override editor through the same
       * `POST .../details` route the Setup modal uses, sending only this one
       * scope's patch.
       * @returns {Promise<void>}
       */
      async function saveEnvOverridesEditor() {
        const d = envEditDraft;
        if (!d) return;
        const patch = envPatchFromScopeDraft(d.sd);
        if (envPatchIsEmpty(patch)) { closeEnvOverridesEditor(); return; }
        /** @type {{[key: string]: *}} */
        const body = {};
        if (d.scope === "branch") body.branch_env = { [d.branchId]: patch };
        else if (d.scope === "manual_checks") body.manual_checks_env = patch;
        else body.build_env = patch;
        const resp = await guardianAction(`/api/guardians/${d.gid}/details`, body);
        if (resp && resp.ok) {
          notify("success", `Environment saved for ${d.title}.`);
          closeEnvOverridesEditor();
          tick();
        }
      }

      /**
       * RAL-408: resolver agent/model fields, shared between the per-review
       * Edit Details modal and the per-project Review Settings modal (each
       * supplies its own onchange handler NAME since the two modals mutate
       * different draft globals -- inline `onchange`/`oninput` attributes
       * need a literal function name, not a closure). `frozen` (review-only
       * -- a project default is never frozen) renders the resolved values
       * read-only, same as a merged/deployed review's own resolver
       * fields.
       * @param {string} cwd
       * @param {string} resolverAgent
       * @param {string} resolverModel
       * @param {boolean} frozen
       * @param {string} onAgentChange
       * @param {string} onModelChange
       * @returns {string}
       */
      function renderResolverFieldsHtml(cwd, resolverAgent, resolverModel, frozen, onAgentChange, onModelChange) {
        return frozen
          ? `<div class="kv-row"><span class="k">resolver agent</span><span class="v mono">${esc(resolverAgent || "agent default")}</span></div>
             <div class="kv-row"><span class="k">resolver model</span><span class="v mono">${esc(resolverModel || "agent default")}</span></div>`
          : `<div class="kv-row"><span class="k">resolver agent</span>${renderAgentSelectHtml("", cwd, resolverAgent, onAgentChange, REVIEW_EDIT_INPUT_STYLE, "Conflict-resolver backend used when the AI agent resolves merge conflicts. Applies on Save.")}</div>
             <div class="kv-row"><span class="k">resolver model</span><input type="text" class="mono" style="${REVIEW_EDIT_INPUT_STYLE};width:200px" value="${esc(resolverModel)}" placeholder="agent default" oninput="${onModelChange}(this.value)" data-tip="Exact model passed to the selected resolver agent. Clear to use the agent's default. Applies on Save."></div>`;
      }
      /**
       * RAL-408: proof-scope + skip-auto-clean fields, shared as
       * [`renderResolverFieldsHtml`] above. `allowInherit` (project-settings
       * scope only -- a review's effective proof scope is always a concrete
       * resolved value, never blank) adds a leading "(inherit)" option for
       * `proofScope === ""`, so a project default can be told apart from an
       * explicit `each_branch` override.
       * @param {string} proofScope
       * @param {boolean} proofSkipAutoClean
       * @param {string} onScopeChange
       * @param {string} onSkipAutoCleanChange
       * @param {boolean} [allowInherit]
       * @returns {string}
       */
      function renderProofScopeFieldsHtml(proofScope, proofSkipAutoClean, onScopeChange, onSkipAutoCleanChange, allowInherit) {
        return `<div class="kv-row"><span class="k">proof scope</span><div style="display:flex;flex-direction:column;gap:4px">
            <select style="${REVIEW_EDIT_INPUT_STYLE}" onchange="${onScopeChange}(this.value)" data-tip="How often the dedicated final-proof agent call runs after a branch's rebase. Applies on Save.">
              ${allowInherit ? `<option value="" ${proofScope === "" ? "selected" : ""}>(inherit)</option>` : ""}
              <option value="each_branch" ${proofScope === "each_branch" ? "selected" : ""}>each branch</option>
              <option value="final_branch" ${proofScope === "final_branch" ? "selected" : ""}>the final branch</option>
              <option value="nothing" ${proofScope === "nothing" ? "selected" : ""}>nothing</option>
            </select>
            ${proofScope === "each_branch" ? `<label style="display:flex;align-items:center;gap:6px;font-size:12px;color:var(--muted)" data-tip="Within \"each branch\" scope, additionally skip verification on branches whose rebase applied cleanly with no conflict at all.">
              <input type="checkbox" ${proofSkipAutoClean ? "checked" : ""} onchange="${onSkipAutoCleanChange}(this.checked)">skip auto-clean branches</label>` : ""}
          </div></div>`;
      }
      /**
       * RAL-408: pull-request settings fields (separate PR branch / match
       * worktree branch name / auto-submit PR stack / dual root PR), shared
       * as above.
       * @param {boolean} separatePrBranch
       * @param {boolean} matchPrBranchName
       * @param {boolean} autoSubmitPrStack
       * @param {boolean} dualRootPr
       * @param {string} onSeparateChange
       * @param {string} onMatchChange
       * @param {string} onAutoSubmitChange
       * @param {string} onDualRootPrChange
       * @returns {string}
       */
      function renderPrSettingsFieldsHtml(separatePrBranch, matchPrBranchName, autoSubmitPrStack, dualRootPr, onSeparateChange, onMatchChange, onAutoSubmitChange, onDualRootPrChange) {
        return `<label style="display:flex;align-items:center;gap:6px;font-size:12px;color:var(--muted);margin-top:6px" data-tip="Push this review's PRs to a branch of their own, derived from the task branch, instead of opening them straight from the review branch. Applies on Save.">
            <input type="checkbox" ${separatePrBranch ? "checked" : ""} onchange="${onSeparateChange}(this.checked)">separate PR branch</label>
          <label style="display:flex;align-items:center;gap:6px;font-size:12px;color:var(--muted);margin-top:4px${separatePrBranch ? "" : ";opacity:0.5"}" data-tip="Only applies when 'separate PR branch' is on. Use the exact worktree/feature branch name as the PR branch. Applies on Save.">
            <input type="checkbox" ${matchPrBranchName ? "checked" : ""} ${separatePrBranch ? "" : "disabled"} onchange="${onMatchChange}(this.checked)">match worktree branch name</label>
          <label style="display:flex;align-items:center;gap:6px;font-size:12px;color:var(--muted);margin-top:4px" data-tip="Automatically submit/grow this review's PR stack as each branch finishes rebasing. Applies on Save.">
            <input type="checkbox" ${autoSubmitPrStack ? "checked" : ""} onchange="${onAutoSubmitChange}(this.checked)">auto-submit PR stack</label>
          <label style="display:flex;align-items:center;gap:6px;font-size:12px;color:var(--muted);margin-top:4px" data-tip="Fork-routed reviews only. When on, the stack's root branch (and whichever branch later gets promoted to root) gets a second, same-repo PR into a mirror of the parent's base branch, so it visually joins the rest of the PR stack -- alongside the existing PR that actually gets merged. Off by default. Applies on Save.">
            <input type="checkbox" ${dualRootPr ? "checked" : ""} onchange="${onDualRootPrChange}(this.checked)">dual root PR</label>`;
      }
      /**
       * RAL-408: auto-fix-PR-errors checkbox + prompt-template textarea, plus
       * the RAL-505 discourage-tests-during-auto-PR-fix checkbox, shared as
       * above.
       * @param {boolean} autoFixPrErrors
       * @param {string} autoFixPromptTemplate
       * @param {boolean} discourageTests
       * @param {string} onErrorsChange
       * @param {string} onTemplateChange
       * @param {string} onDiscourageTestsChange
       * @returns {string}
       */
      function renderAutoFixFieldsHtml(autoFixPrErrors, autoFixPromptTemplate, discourageTests, onErrorsChange, onTemplateChange, onDiscourageTestsChange) {
        return `<label style="display:flex;align-items:center;gap:6px;font-size:12px;color:var(--muted);margin-top:4px" data-tip="Automatically dispatch the resolver agent to fix this review's PR when its CI checks go red. Applies on Save.">
            <input type="checkbox" ${autoFixPrErrors ? "checked" : ""} onchange="${onErrorsChange}(this.checked)">auto-fix PR errors</label>
          <div style="margin-top:8px">
            <label for="auto-fix-prompt-template-input" style="font-size:12px;color:var(--muted);display:block;margin-bottom:4px" data-tip="${AUTO_FIX_PROMPT_TEMPLATE_TIP}">auto-fix prompt template</label>
            <textarea id="auto-fix-prompt-template-input" rows="4" style="${REVIEW_EDIT_TEXTAREA_STYLE}" placeholder="inherits project default" oninput="${onTemplateChange}(this.value)" data-tip="${AUTO_FIX_PROMPT_TEMPLATE_TIP}">${esc(autoFixPromptTemplate)}</textarea>
          </div>
          <label style="display:flex;align-items:center;gap:6px;font-size:12px;color:var(--muted);margin-top:8px" data-tip="${DISCOURAGE_TESTS_TIP}">
            <input type="checkbox" ${discourageTests ? "checked" : ""} onchange="${onDiscourageTestsChange}(this.checked)">discourage tests during auto PR fixes</label>`;
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
        const resolverSection = renderResolverFieldsHtml(cwd, draft.resolverAgent, draft.resolverModel, draft.resolverFrozen, "onEditResolverAgent", "onEditResolverModel");
        const proofScopeSection = renderProofScopeFieldsHtml(draft.proofScope, draft.proofSkipAutoClean, "onEditProofScope", "onEditProofSkipAutoClean");
        const squashSection = draft.projects.map((p) => {
          const label = (p || "").split(/[\\/]/).filter(Boolean).pop() || p;
          return `<label style="display:flex;align-items:center;gap:6px;font-size:12px;color:var(--muted);margin-top:4px" data-tip="Collapse this git project's task branches to a single squashed commit each in the review worktree. Applies on the next Merge / rebase.">
            <input type="checkbox" ${draft.squash[p] ? "checked" : ""} onchange="onEditSquashToggle(${JSON.stringify(p)},this.checked)">squash${draft.projects.length > 1 ? ` — <span class="mono" style="font-size:11px">${esc(label)}</span>` : " each task branch to a single commit"}</label>`;
        }).join("");
        const grp = (/** @type {string} */ key) => `class="setup-group${draft.focus === key ? " focused" : ""}" data-setup-group="${key}"`;
        byId("modal-root").innerHTML = `<div class="modal-bg" onclick="if(event.target===this)closeEditReviewDetails()"><div class="modal review-edit-modal">
            <h2>Review setup</h2>
            <div class="edit-form">
              <label data-tip="Display name for this review — shown in the sidebar list. Applies on Save.">name<input type="text" value="${esc(draft.name)}" oninput="onEditName(this.value)"></label>
            </div>
            <div ${grp("onto")}><h3 class="section">upstream</h3>
              <div class="edit-form">
                <label data-tip="The branch every submitted branch is rebased onto. Type or focus to load remote branch suggestions. Triggers a rebase on Save.">upstream branch<input type="text" class="mono" list="base-datalist" value="${esc(draft.baseBranch)}" onfocus="loadBaseBranches(${JSON.stringify(draft.gid)})" oninput="onEditBaseBranch(this.value)"><datalist id="base-datalist">${(baseBranchCache[draft.gid] || []).map((b) => `<option value="${esc(b)}"></option>`).join("")}</datalist></label>
              </div></div>
            <div ${grp("resolver")}><h3 class="section">resolver</h3>${resolverSection}</div>
            <div ${grp("proof")}><h3 class="section">proof</h3>${proofScopeSection}</div>
            <div><h3 class="section">rebase</h3>
              <label style="display:flex;align-items:center;gap:6px;font-size:12px;color:var(--muted);margin-top:4px" data-tip="Overrides the project default for this review only: when its upstream branch moves, don't automatically rebuild/rebase this review's stack onto the new tip. Use for a review whose auto-rebase keeps getting in the way (e.g. one under heavy manual conflict resolution). You can still start a merge/rebase manually at any time, individually or via the review list's bulk Merge/Rebase action, regardless of this setting. Applies on Save.">
                <input type="checkbox" ${draft.skipBaseUpdates ? "checked" : ""} onchange="onEditSkipBaseUpdates(this.checked)">skip automatic base-branch rebasing</label>
              <div class="hint">Manual-check preparation is declared in task TOML and runs automatically before its controls unlock.</div></div>
            <div ${grp("rebuild")}><h3 class="section" data-tip="${esc(REBUILD_ON_TIP)}">rebuild preparation when</h3>
              ${renderRebuildOnFieldsHtml("review", draft.rebuildOn, draft.rebuildOnEffective, "use the project default", "Follow the project's default for when preparation is rebuilt (set in the project's Review Settings; every trigger when nothing sets one). Untick to give this review its own policy. Applies on Save.", "onEditRebuildOnInherit", "onEditRebuildOnTrigger")}</div>
            <div ${grp("worktrees")}><h3 class="section">worktrees</h3>
              <label style="display:flex;align-items:center;gap:6px;font-size:12px;color:var(--muted);margin-top:6px" data-tip="Build the entire branch stack in one shared worktree instead of isolated per-branch worktrees. Applies on Save.">
                <input type="checkbox" ${draft.skipWorktrees ? "checked" : ""} onchange="onEditSkipWorktrees(this.checked)">skip per-branch worktrees</label></div>
            <div ${grp("squash")}><h3 class="section">squash</h3>${squashSection}</div>
            <h3 class="section">pull requests</h3>
            ${renderPrSettingsFieldsHtml(draft.separatePrBranch, draft.matchPrBranchName, draft.autoSubmitPrStack, draft.dualRootPr, "onEditSeparatePrBranch", "onEditMatchPrBranchName", "onEditAutoSubmitPrStack", "onEditDualRootPr")}
            ${renderAutoFixFieldsHtml(draft.autoFixPrErrors, draft.autoFixPromptTemplate, draft.discourageTests, "onEditAutoFixPrErrors", "onEditAutoFixPromptTemplate", "onEditDiscourageTests")}
            <div id="review-edit-err" class="verr"></div>
            <div class="btn-row" style="margin-top:12px"><button class="btn" onclick="closeEditReviewDetails()">Cancel</button><button class="btn primary" onclick="saveReviewEditDetails()" data-tip="Apply every change made in this modal in a single request. At most one rebase is triggered, only if something rebase-relevant changed.">Save</button></div>
          </div></div>`;
        if (draft.focus) {
          const el = document.querySelector(`[data-setup-group="${draft.focus}"]`);
          if (el) el.scrollIntoView({ block: "center" });
        }
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
        if (draft.skipWorktrees !== draft.originalSkipWorktrees) body.skip_worktrees = draft.skipWorktrees;
        if (draft.skipBaseUpdates !== draft.originalSkipBaseUpdates) body.skip_base_updates = draft.skipBaseUpdates;
        if (draft.separatePrBranch !== draft.originalSeparatePrBranch) body.separate_pr_branch = draft.separatePrBranch;
        if (draft.matchPrBranchName !== draft.originalMatchPrBranchName) body.match_pr_branch_name = draft.matchPrBranchName;
        if (draft.autoSubmitPrStack !== draft.originalAutoSubmitPrStack) body.auto_submit_pr_stack = draft.autoSubmitPrStack;
        if (draft.dualRootPr !== draft.originalDualRootPr) body.dual_root_pr = draft.dualRootPr;
        if (draft.autoFixPrErrors !== draft.originalAutoFixPrErrors) body.auto_fix_pr_errors = draft.autoFixPrErrors;
        if (draft.autoFixPromptTemplate !== draft.originalAutoFixPromptTemplate) body.auto_fix_prompt_template = draft.autoFixPromptTemplate;
        if (draft.discourageTests !== draft.originalDiscourageTests) body.discourage_tests_during_auto_pull_request_fixes = draft.discourageTests;
        if (rebuildOnChanged(draft.rebuildOn, draft.originalRebuildOn)) body.rebuild_on = rebuildOnBodyValue(draft.rebuildOn);
        const squashOn = draft.projects.filter((p) => draft.squash[p]).sort();
        if (JSON.stringify(squashOn) !== JSON.stringify(draft.originalSquashOn.slice().sort())) {
          body.squash_projects = squashOn;
        }
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
            notify(
              "warn",
              change.message || "A rebase is already in progress. We'll trigger a new rebase once this one completes.",
              { action },
            );
          } else if (change && change.message) {
            notify("info", change.message);
          } else {
            notify("success", "Review details saved.");
          }
          closeEditReviewDetails();
          tick();
        }
      }

      void [onEditResolverAgent, onEditResolverModel, onEditProofScope, onEditProofSkipAutoClean, onEditSeparatePrBranch, onEditMatchPrBranchName, onEditAutoSubmitPrStack, onEditDualRootPr, onEditAutoFixPrErrors, onEditAutoFixPromptTemplate, onEditDiscourageTests, onEditRebuildOnInherit, onEditRebuildOnTrigger];
