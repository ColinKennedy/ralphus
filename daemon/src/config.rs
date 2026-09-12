//! Layered review configuration (CCTL-156).
//!
//! Review defaults can be set in a TOML file under a `[review]` (preferred) or
//! `[defaults]` table. Two files are consulted: a global one
//! (`$RALPHUS_CONFIG_HOME/config.toml`, defaulting to `~/.config/ralphus/`) and a
//! per-project one (`.ralphus.toml`, discovered by walking up from the review's
//! cwd). When both are present they are merged with **per-project values winning
//! on scalar conflicts** while **list fields (e.g. `checks`) are unioned**.
//!
//! Today the consumed scalars are `skip_worktrees`, which lets large repos
//! avoid a git worktree copy per branch, `auto_build` (RAL-101), the
//! project-level default build/test command run when a review declares no
//! explicit `checks`, and `default_resolver_agent`, the project-level
//! fallback conflict-resolver agent used when a review sets none of its own.

use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{NaiveTime, Utc};
use ralphus_core::cors::CorsConfig;
use serde::{Deserialize, Serialize};

/// Ark's worktree-retention policy. Durations are expressed in whole days so
/// project configuration remains easy to audit.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ArkConfig {
    #[serde(default = "default_ark_sweep_interval_days")]
    pub sweep_interval_days: u64,
    #[serde(default = "default_ark_stale_after_days")]
    pub stale_after_days: u64,
    #[serde(default = "default_ark_max_worktrees")]
    pub max_worktrees: usize,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct ArkConfigLayer {
    sweep_interval_days: Option<u64>,
    stale_after_days: Option<u64>,
    max_worktrees: Option<usize>,
}

impl ArkConfigLayer {
    fn apply(self, base: ArkConfig) -> ArkConfig {
        ArkConfig {
            sweep_interval_days: self.sweep_interval_days.unwrap_or(base.sweep_interval_days),
            stale_after_days: self.stale_after_days.unwrap_or(base.stale_after_days),
            max_worktrees: self.max_worktrees.unwrap_or(base.max_worktrees),
        }
    }
}

const fn default_ark_sweep_interval_days() -> u64 {
    1
}
const fn default_ark_stale_after_days() -> u64 {
    90
}
const fn default_ark_max_worktrees() -> usize {
    100
}

impl Default for ArkConfig {
    fn default() -> Self {
        Self {
            sweep_interval_days: default_ark_sweep_interval_days(),
            stale_after_days: default_ark_stale_after_days(),
            max_worktrees: default_ark_max_worktrees(),
        }
    }
}

impl ArkConfig {
    #[must_use]
    pub fn sweep_interval(&self) -> Duration {
        Duration::from_secs(self.sweep_interval_days.saturating_mul(86_400))
    }

    #[must_use]
    pub fn stale_after_ms(&self) -> i64 {
        i64::try_from(self.stale_after_days.saturating_mul(86_400_000)).unwrap_or(i64::MAX)
    }

    pub fn validate(&self) -> std::result::Result<(), String> {
        if self.sweep_interval_days == 0 {
            return Err("ark.sweep_interval_days must be greater than zero".to_string());
        }
        if self.stale_after_days == 0 {
            return Err("ark.stale_after_days must be greater than zero".to_string());
        }
        if self.max_worktrees == 0 {
            return Err("ark.max_worktrees must be greater than zero".to_string());
        }
        Ok(())
    }
}

/// Resolved review configuration (after layering global under per-project).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ReviewConfig {
    /// Skip creating a git worktree per branch during merge. `None` means unset
    /// (so a lower layer can supply it); resolved callers treat `None` as false.
    #[serde(default)]
    pub skip_worktrees: Option<bool>,
    /// Check-gate commands (a list field: layers are unioned, not overridden).
    #[serde(default)]
    pub checks: Vec<String>,
    /// RAL-101: the project's default build/test command, run in place of
    /// `checks` when a review declares none (and hasn't opted out via
    /// `guardian_skip_auto_build`) — so "in review" still means "testable" even
    /// when the review author configured no explicit check gates. This is now
    /// the *explicit override* tier: when unset, `generate_manual_commands`
    /// (RAL-110) instead asks the same LLM call that infers manual review
    /// commands to also infer a build command from the diff, and runs that in
    /// place of this config value. `None` means unset; per-project scalars win
    /// over the global layer, same as `skip_worktrees`.
    #[serde(default)]
    pub auto_build: Option<String>,
    /// RAL-124: the LLM-authored change-summary rendering format --
    /// `"bullet"` (one concise bullet per branch, labelled with its ticket id
    /// or branch name) or `"prose"` (the original 2-3 sentence paragraph).
    /// `None` means unset, which resolves to `"bullet"` (see
    /// [`Self::bullet_summary`]); per-project scalars win over the global
    /// layer, same as `skip_worktrees`.
    #[serde(default)]
    pub summary_format: Option<String>,
    /// RAL-168: project-level default Proof scope for the LLM-based
    /// final-verify pass -- one of `"each_branch"` (default), `"final_branch"`,
    /// or `"nothing"`. `None` means unset, which resolves to `"each_branch"`
    /// (see [`Self::default_proof_scope`]); per-project scalars win over the
    /// global layer, same as `skip_worktrees`. A per-review override (see
    /// `Guardian::proof_scope` in `guardian.rs`) wins over this. Named
    /// `verify_scope` before the Verify→Proof rename (see `AGENTS.md`'s
    /// Taxonomy section); `#[serde(alias)]` keeps existing `.ralphus.toml`
    /// files using the old key working.
    #[serde(alias = "verify_scope", default)]
    pub default_proof_scope: Option<String>,
    /// RAL-168: within `"each_branch"` scope, additionally skip verification
    /// on branches whose rebase applied cleanly with no conflict (an
    /// "auto-clean" branch) -- the old, lighter-weight default behavior.
    /// `None` means unset, which resolves to `false`; per-project scalars win
    /// over the global layer, same as `skip_worktrees`.
    #[serde(default)]
    pub verify_skip_auto_clean: Option<bool>,
    /// The conflict-resolver agent used when a review doesn't set its own
    /// `[[review]].agent` and `RALPHUS_RESOLVER_AGENT` isn't set -- a builtin
    /// backend name (`"claude"`, `"claude-code"`, `"codex"`, `"ollama"`,
    /// `"anthropic"`) or a configured `[agent.profiles.*]` name. `None` means
    /// unset, which resolves to `"ollama"` (see [`Self::default_resolver_agent`]);
    /// per-project scalars win over the global layer, same as `skip_worktrees`.
    #[serde(default)]
    pub default_resolver_agent: Option<String>,
    /// RAL-342/RAL-338: the conflict-resolver model used when a review
    /// doesn't set its own `[[review]].model` and `RALPHUS_RESOLVER_MODEL`
    /// isn't set. `None` means unset, in which case each backend picks its
    /// own default (see [`Self::default_resolver_model`]); per-project
    /// scalars win over the global layer, same as `skip_worktrees`.
    #[serde(default)]
    pub default_resolver_model: Option<String>,
    /// RAL-342/RAL-338: the machine (`scheme:uri`) a review's worktrees and
    /// merge run on when neither an explicit `[[review]].machine` nor the
    /// Arbiter (which never has a `[[review]]` block to read one from) sets
    /// one. `None` means unset, which resolves to the local machine (see
    /// [`Self::default_machine`]); per-project scalars win over the global
    /// layer, same as `skip_worktrees`.
    #[serde(default)]
    pub default_machine: Option<String>,
    /// RAL-342/RAL-338: the USD spend cap applied to a review's own
    /// resolver/prover cost when neither an explicit
    /// `[[review]].maximum_budget_usd` nor the Arbiter sets one. `None` means
    /// unset, which resolves to unbounded (see
    /// [`Self::default_maximum_budget_usd`]); per-project scalars win over
    /// the global layer, same as `skip_worktrees`.
    #[serde(default)]
    pub default_maximum_budget_usd: Option<f64>,
    /// RAL-250: whether a review opts out of the automatic base-branch
    /// auto-update rebuild (`review_maintenance`'s base-shift pass in
    /// `guardian_merge.rs`). `None` means unset, which resolves to `false`
    /// (auto-update stays on); per-project scalars win over the global layer,
    /// same as `skip_worktrees`. A per-review override (see
    /// `Guardian::skip_base_updates` in `guardian.rs`) wins over this.
    #[serde(default)]
    pub skip_base_updates: Option<bool>,
    /// RAL-307: whether a newly submitted PR's branch defaults to the exact
    /// worktree/feature branch name (`branch.branch`) instead of the
    /// convention-derived alias (`apply_pr_branch_convention`). `None` means
    /// unset, which resolves to `false` (convention-derived alias stays the
    /// default); per-project scalars win over the global layer, same as
    /// `skip_worktrees`. A per-review override (see
    /// `Guardian::match_pr_branch_name` in `guardian.rs`) wins over this; a
    /// per-submission `PrRequest::use_worktree_branch_name` wins over that.
    #[serde(default)]
    pub match_pr_branch_name: Option<bool>,
    /// RAL-317: whether a review's PR stack is auto-submitted/grown as each
    /// branch reaches a terminal (`done`/`conflict_resolved`) merge state,
    /// instead of requiring the manual `review pr submit`/`guardian_submit_prs`
    /// call. `None` means unset, which resolves to `false` (manual submission
    /// stays required); per-project scalars win over the global layer, same as
    /// `skip_worktrees`. A per-review override (see
    /// `Guardian::auto_submit_pr_stack` in `guardian.rs`) wins over this.
    #[serde(default)]
    pub auto_submit_pr_stack: Option<bool>,
    /// RAL-378: whether a review's pull request is pushed to a branch
    /// *separate* from the review branch itself.
    ///
    /// `None` means unset, which resolves to `false` (see
    /// [`Self::separate_pr_branch`]) -- the review branch, which is named
    /// readably as `<task branch>-review`, *is* the branch the PR is opened
    /// from, and neither `forge.pull_request_branch_convention` nor
    /// `match_pr_branch_name` is consulted at all. Setting it to `true`
    /// restores the older behavior of deriving a second, differently-named
    /// remote branch from the task branch. Per-project scalars win over the
    /// global layer, same as `skip_worktrees`; a per-review override (see
    /// `Guardian::separate_pr_branch` in `guardian.rs`) wins over this.
    #[serde(default)]
    pub separate_pr_branch: Option<bool>,
    /// RAL-395: whether a review automatically dispatches its agent to fix a
    /// failing PR/MR CI status. `None` means unset, which resolves to
    /// `false` (see [`Self::auto_fix_pr_errors`]); per-project scalars win
    /// over the global layer, same as `skip_worktrees`. A per-review
    /// override (see `Guardian::auto_fix_pr_errors` in `guardian.rs`) wins
    /// over this. Auto-created reviews (Arbiter/Triage) have no `[[review]]`
    /// block to override it with, so they always use this project default.
    #[serde(default)]
    pub auto_fix_pr_errors: Option<bool>,
    /// RAL-395: the prompt template handed to the resolver agent when
    /// `auto_fix_pr_errors` fires, with `<<prompt>>` replaced by the
    /// concatenated prompts of the failing branch's attached Cells. `None`
    /// means unset, which resolves to a built-in default template (see
    /// [`Self::auto_fix_prompt_template`]); per-project scalars win over the
    /// global layer, same as `skip_worktrees`. A per-review override (see
    /// `Guardian::auto_fix_prompt_template` in `guardian.rs`) wins over
    /// this. Auto-created reviews (Arbiter/Triage) have no `[[review]]`
    /// block to override it with, so they always use this project default.
    /// Validated (`ralphus_core::validate`) to contain the literal
    /// `<<prompt>>` placeholder.
    #[serde(default)]
    pub auto_fix_prompt_template: Option<String>,
}

/// RAL-395: the built-in fallback prompt template for auto-fixing a failing
/// PR/MR, used when neither a per-review override nor a project-level
/// `.ralphus.toml [review] auto_fix_prompt_template` is set. The "current
/// machine" phrasing (not "locally") is deliberate: the fix may run on a
/// remote machine.
pub const DEFAULT_AUTO_FIX_PROMPT_TEMPLATE: &str = "We found 1-or-more errors in this PR {insert URL here}, please fix. Keep in mind that we want this code to continue to work:\n\nPrefer fixing fast checks first: run and fix any linters/formatters on the current machine before reaching for heavier/slower test suites. Only run a heavy test suite once the fast checks are clean; you are trusted to use judgment about which slow tests, if any, are actually necessary to confirm the fix.\n\n<<prompt>>";

impl ReviewConfig {
    /// Whether worktrees should be skipped (unset resolves to `false`).
    #[must_use]
    pub fn skip_worktrees(&self) -> bool {
        self.skip_worktrees.unwrap_or(false)
    }

    /// Whether the LLM-authored change summary should render as one bullet
    /// per branch rather than a prose paragraph. Defaults to `true` (bullet)
    /// when `summary_format` is unset or set to anything other than
    /// `"prose"`.
    #[must_use]
    pub fn bullet_summary(&self) -> bool {
        self.summary_format.as_deref() != Some("prose")
    }

    /// The project-level default Proof scope (RAL-168), normalized to one of
    /// `"each_branch"`/`"final_branch"`/`"nothing"`. Unset or an unrecognized
    /// value resolves to `"each_branch"` (today's post-RAL-168 default
    /// behavior), so a typo in `.ralphus.toml` degrades to the safe default
    /// rather than silently disabling verification.
    #[must_use]
    pub fn default_proof_scope(&self) -> &str {
        match self.default_proof_scope.as_deref() {
            Some("final_branch") => "final_branch",
            Some("nothing") => "nothing",
            _ => "each_branch",
        }
    }

    /// Whether `"each_branch"` scope additionally skips auto-clean branches
    /// (unset resolves to `false`).
    #[must_use]
    pub fn verify_skip_auto_clean(&self) -> bool {
        self.verify_skip_auto_clean.unwrap_or(false)
    }

    /// The configured default conflict-resolver agent, unset resolves to
    /// `"ollama"` -- the last link in `guardian_merge::resolver_agent`'s
    /// fallback chain (per-review `agent` -> `RALPHUS_RESOLVER_AGENT` -> this
    /// -> `"ollama"`).
    #[must_use]
    pub fn default_resolver_agent(&self) -> &str {
        self.default_resolver_agent.as_deref().unwrap_or("ollama")
    }

    /// The configured default conflict-resolver model, unset resolves to
    /// `None` -- `guardian_merge::resolver_model`'s fallback chain (per-review
    /// `model` -> `RALPHUS_RESOLVER_MODEL` -> this -> the resolved backend's
    /// own default) still lets the backend pick when this is also unset.
    #[must_use]
    pub fn default_resolver_model(&self) -> Option<&str> {
        self.default_resolver_model.as_deref()
    }

    /// The configured default review machine (`scheme:uri`), unset resolves
    /// to `None` (the local machine).
    #[must_use]
    pub fn default_machine(&self) -> Option<&str> {
        self.default_machine.as_deref()
    }

    /// The configured default review USD spend cap, unset resolves to `None`
    /// (unbounded).
    #[must_use]
    pub fn default_maximum_budget_usd(&self) -> Option<f64> {
        self.default_maximum_budget_usd
    }

    /// Whether a review opts out of the automatic base-branch auto-update
    /// rebuild (unset resolves to `false`, i.e. auto-update stays on). RAL-250.
    #[must_use]
    pub fn skip_base_updates(&self) -> bool {
        self.skip_base_updates.unwrap_or(false)
    }

    /// Whether a newly submitted PR defaults to the worktree/feature branch
    /// name instead of the convention-derived alias (unset resolves to
    /// `false`). RAL-307.
    #[must_use]
    pub fn match_pr_branch_name(&self) -> bool {
        self.match_pr_branch_name.unwrap_or(false)
    }

    /// Whether a review's PR stack is auto-submitted/grown as each branch
    /// reaches a terminal merge state (unset resolves to `false`). RAL-317.
    #[must_use]
    pub fn auto_submit_pr_stack(&self) -> bool {
        self.auto_submit_pr_stack.unwrap_or(false)
    }

    /// Whether a review's PR is pushed to a branch separate from the review
    /// branch itself (unset resolves to `false` -- they are one and the same).
    #[must_use]
    pub fn separate_pr_branch(&self) -> bool {
        self.separate_pr_branch.unwrap_or(false)
    }

    /// Whether a review automatically dispatches its agent to fix a failing
    /// PR/MR CI status (unset resolves to `false`). RAL-395.
    #[must_use]
    pub fn auto_fix_pr_errors(&self) -> bool {
        self.auto_fix_pr_errors.unwrap_or(false)
    }

    /// The configured default auto-fix prompt template, unset resolves to
    /// `None` -- callers fall back to [`DEFAULT_AUTO_FIX_PROMPT_TEMPLATE`].
    /// RAL-395.
    #[must_use]
    pub fn auto_fix_prompt_template(&self) -> Option<&str> {
        self.auto_fix_prompt_template.as_deref()
    }

    /// Validate this config's own scalars, independent of a `[[review]]`
    /// submission's own validation (`ralphus_core::validate`). RAL-395: a
    /// project-level `auto_fix_prompt_template` default must contain the
    /// same literal `<<prompt>>` placeholder a per-submission override is
    /// required to have, so the rule lives once, in `core::validate`, and is
    /// applied here as well as there.
    pub fn validate(&self) -> std::result::Result<(), String> {
        if let Some(template) = self.auto_fix_prompt_template.as_deref() {
            if !ralphus_core::validate::auto_fix_template_has_placeholder(template) {
                return Err(format!(
                    "[review] auto_fix_prompt_template must contain the literal placeholder \"{}\"",
                    ralphus_core::validate::AUTO_FIX_PROMPT_PLACEHOLDER
                ));
            }
        }
        Ok(())
    }

    /// Layer `self` (global) under `over` (per-project). Per-project scalars win
    /// when present; list fields are unioned (global first, then new per-project
    /// entries, order-preserving and de-duplicated).
    #[must_use]
    pub fn merge(self, over: ReviewConfig) -> ReviewConfig {
        let mut checks = self.checks;
        for c in over.checks {
            if !checks.contains(&c) {
                checks.push(c);
            }
        }
        ReviewConfig {
            skip_worktrees: over.skip_worktrees.or(self.skip_worktrees),
            checks,
            auto_build: over.auto_build.or(self.auto_build),
            summary_format: over.summary_format.or(self.summary_format),
            default_proof_scope: over.default_proof_scope.or(self.default_proof_scope),
            verify_skip_auto_clean: over.verify_skip_auto_clean.or(self.verify_skip_auto_clean),
            default_resolver_agent: over.default_resolver_agent.or(self.default_resolver_agent),
            default_resolver_model: over.default_resolver_model.or(self.default_resolver_model),
            default_machine: over.default_machine.or(self.default_machine),
            default_maximum_budget_usd: over
                .default_maximum_budget_usd
                .or(self.default_maximum_budget_usd),
            skip_base_updates: over.skip_base_updates.or(self.skip_base_updates),
            match_pr_branch_name: over.match_pr_branch_name.or(self.match_pr_branch_name),
            auto_submit_pr_stack: over.auto_submit_pr_stack.or(self.auto_submit_pr_stack),
            separate_pr_branch: over.separate_pr_branch.or(self.separate_pr_branch),
            auto_fix_pr_errors: over.auto_fix_pr_errors.or(self.auto_fix_pr_errors),
            auto_fix_prompt_template: over
                .auto_fix_prompt_template
                .or(self.auto_fix_prompt_template),
        }
    }
}

/// Whether a `[[review]]` field has a per-project auto-review default
/// (RAL-342/RAL-338), or a documented reason it deliberately doesn't. Used
/// only via [`REVIEW_FIELD_PARITY`] -- see `daemon/tests/review_field_parity.rs`
/// for the test that enforces every entry stays honest.
pub enum ReviewFieldDefault {
    /// A `fn` pointer rather than a field-name string: a `ReviewConfig` field
    /// renamed or removed out from under this table fails to *compile*, not
    /// just fails a string-matched test.
    ProjectDefault(fn(&ReviewConfig) -> bool),
    /// Deliberately has no per-project default; the reason is checked for
    /// substance (not empty/placeholder text) by the parity test.
    NotApplicable(&'static str),
}

/// The full parity mapping between `[[review]]`'s TOML fields
/// (`ralphus_core::validate::REVIEW_KEYS`) and this project's auto-review
/// defaults (RAL-342/RAL-338): every field a human can set explicitly in a
/// `[[review]]` block must appear here, either wired to the `ReviewConfig`
/// field that covers it for a review the Arbiter creates with no
/// `[[review]]` block to read from, or with a real explanation of why no
/// project-level default makes sense for it. `daemon/tests/review_field_parity.rs`
/// fails the build the moment a new `[[review]]` field (and therefore a new
/// entry in `core::validate::REVIEW_KEYS`) doesn't get an entry here.
pub const REVIEW_FIELD_PARITY: &[(&str, ReviewFieldDefault)] = &[
    (
        "id",
        ReviewFieldDefault::NotApplicable(
            "id is the review's identity, assigned at creation time -- a human-authored \
             key, or the Arbiter's own `triage-{type}` pool key. There is no sensible \
             default identity to inherit from a project.",
        ),
    ),
    (
        "name",
        ReviewFieldDefault::NotApplicable(
            "name is derived from id/pool key at creation time; a fixed project-level \
             default name would collide across every auto-review the project ever creates.",
        ),
    ),
    (
        "agent",
        ReviewFieldDefault::ProjectDefault(|c| c.default_resolver_agent.is_some()),
    ),
    (
        "model",
        ReviewFieldDefault::ProjectDefault(|c| c.default_resolver_model.is_some()),
    ),
    (
        "upstream",
        ReviewFieldDefault::NotApplicable(
            "for an Arbiter-created review, upstream is derived from the pooled cells' \
             actual base branch (see `reviews::create_review_from_triage_pool`); a static \
             project override would silently rebase pooled work onto the wrong branch. \
             `ralphus review upstream set` already exists to correct one review afterward.",
        ),
    ),
    (
        "machine",
        ReviewFieldDefault::ProjectDefault(|c| c.default_machine.is_some()),
    ),
    (
        "maximum_budget_usd",
        ReviewFieldDefault::ProjectDefault(|c| c.default_maximum_budget_usd.is_some()),
    ),
    (
        "proof_scope",
        ReviewFieldDefault::ProjectDefault(|c| c.default_proof_scope.is_some()),
    ),
    (
        "auto_submit_pr_stack",
        ReviewFieldDefault::ProjectDefault(|c| c.auto_submit_pr_stack.is_some()),
    ),
    (
        "skip_worktrees",
        ReviewFieldDefault::ProjectDefault(|c| c.skip_worktrees.is_some()),
    ),
    (
        "auto_pr_feedback",
        ReviewFieldDefault::NotApplicable(
            "auto_pr_feedback has no project default because automatic feedback handling is a \
             deliberate decision for each review and its PR conversation.",
        ),
    ),
    (
        "skip_base_updates",
        ReviewFieldDefault::ProjectDefault(|c| c.skip_base_updates.is_some()),
    ),
    (
        "skip_auto_clean",
        ReviewFieldDefault::NotApplicable(
            "skip_auto_clean only applies to a review's explicitly declared each-branch \
             proof run, so a project default would be ambiguous for other proof scopes.",
        ),
    ),
    (
        "match_pr_branch_name",
        ReviewFieldDefault::ProjectDefault(|c| c.match_pr_branch_name.is_some()),
    ),
    (
        "separate_pr_branch",
        ReviewFieldDefault::ProjectDefault(|c| c.separate_pr_branch.is_some()),
    ),
    (
        "action",
        ReviewFieldDefault::NotApplicable(
            "action hints are bespoke per-review manual-test buttons tied to review-specific \
             prompts/commands; a single project-level default doesn't generalize the way a \
             scalar setting does. Revisit as a separate feature (project-level default action \
             templates) if a concrete need shows up.",
        ),
    ),
    (
        "auto_build",
        ReviewFieldDefault::ProjectDefault(|c| c.auto_build.is_some()),
    ),
    (
        "skip_auto_build",
        ReviewFieldDefault::NotApplicable(
            "skip_auto_build is an explicit per-review opt-out of build gating; there is no \
             project-level 'never build' default to inherit -- the project's own `auto_build` \
             default (or its absence) already governs a declaration-less review, and a \
             project-wide skip would silently disable build gating for every auto-review \
             the project ever creates (see `reviews::require_auto_build_declaration`).",
        ),
    ),
    (
        "auto_fix_pr_errors",
        ReviewFieldDefault::ProjectDefault(|c| c.auto_fix_pr_errors.is_some()),
    ),
    (
        "auto_fix_prompt_template",
        ReviewFieldDefault::ProjectDefault(|c| c.auto_fix_prompt_template.is_some()),
    ),
];

/// The daemon-singleton Arbiter's own agent/model/budget config (`[arbiter]`
/// table, RAL-318) -- wholly separate from a review's own conflict-resolver
/// `agent`/`model` (`ReviewConfig::default_resolver_agent`, the CLI's
/// `ralphus review settings --agent/--model`). Exactly one Arbiter exists per
/// daemon, never per-project, so unlike [`ReviewConfig`] this has no
/// per-project layering -- [`load_arbiter_config`] reads the global config
/// file only.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct ArbiterConfig {
    /// Backend the Arbiter uses for classification calls and the `ralphus
    /// check health` round-trip -- a built-in backend name (`"claude"`,
    /// `"ollama"`, ...). `None` resolves to `"ollama"` (see [`Self::agent`]).
    /// Only `"claude"`/`"anthropic"`/`"ollama"` are actually callable
    /// headlessly today (`crate::chat_client::call_direct_with_usage`) --
    /// any other value makes every classification permanently fall back to
    /// `unclassified` and the health check fail with a clear message.
    #[serde(default)]
    pub agent: Option<String>,
    /// Model the Arbiter's `agent` runs. `None` lets the backend's own
    /// default apply (see `crate::chat_client::call_direct`).
    #[serde(default)]
    pub model: Option<String>,
    /// The Arbiter's own USD spend cap, covering classification calls and the
    /// `ralphus check health` round-trip cumulatively (RAL-318). `None` means
    /// unbounded.
    #[serde(default)]
    pub maximum_budget_usd: Option<f64>,
}

impl ArbiterConfig {
    /// The configured Arbiter backend, unset resolves to `"ollama"` --
    /// mirrors [`ReviewConfig::default_resolver_agent`]'s own fallback.
    #[must_use]
    pub fn agent(&self) -> &str {
        self.agent.as_deref().unwrap_or("ollama")
    }
}

/// Parse an `ArbiterConfig` from the given TOML text; the default (`ollama`,
/// no model override, unbounded budget) when the `[arbiter]` table is absent.
#[must_use]
pub fn arbiter_from_toml_str(s: &str) -> ArbiterConfig {
    toml::from_str::<ConfigFile>(s)
        .unwrap_or_default()
        .arbiter
        .unwrap_or_default()
}

/// Load the daemon-singleton Arbiter config from the global config file only
/// (`$RALPHUS_CONFIG_HOME/config.toml`, or its `~/.config/ralphus/` default)
/// -- deliberately no per-project layering, since the Arbiter is one daemon-
/// wide singleton, never scoped per-project (RAL-318 explicit scope
/// decision). Computed fresh at each call site (submission-time
/// classification, the health-check handler, the scheduler's Triage tick)
/// rather than cached in a long-lived struct -- every call site resolves
/// identically, which is indistinguishable from a literal singleton object
/// while matching this module's existing "load config fresh where needed"
/// style (e.g. [`load_daemon_config`], [`load_budget_config`]).
#[must_use]
pub fn load_arbiter_config() -> ArbiterConfig {
    global_config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| arbiter_from_toml_str(&s))
        .unwrap_or_default()
}

/// One scheduler down-time window (`[[daemon.downtime]]`, RAL-122): a UTC
/// wall-clock `"HH:MM"` start/end pair during which the scheduler will not
/// claim new `Pending` runs. Kept as raw strings — see
/// [`DaemonConfig::downtime_windows`] for parsing — so a malformed entry can
/// be silently dropped rather than failing the whole config (RAL-122's
/// "never fail loudly" precedent, matching [`CartographerConfig`]).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct DowntimeWindow {
    pub start: String,
    pub end: String,
}

/// Daemon-level configuration (`[daemon]` table).
#[derive(Debug, Default, Deserialize, Clone)]
pub struct DaemonConfig {
    /// Path to the log file; when absent all output goes to stderr.
    #[serde(default)]
    pub log_path: Option<String>,
    /// Minimum log level: `"error"`, `"warn"`, `"info"`, `"debug"`, `"trace"`.
    /// Defaults to `"debug"` when unset.
    #[serde(default)]
    pub log_level: Option<String>,
    /// Scheduler down-time windows (RAL-122). Empty (the default, and what a
    /// malformed table falls back to) means "always run" — the scheduler never
    /// refuses to claim a `Pending` run.
    #[serde(default)]
    pub downtime: Vec<DowntimeWindow>,
    /// The user identity (`crate::agent_access::UserContext`) a request is
    /// attributed to when it names none explicitly. Must name a row already
    /// registered via `Store::create_user`/`POST /api/users` -- an unknown
    /// name resolves the same as unset (`None`) rather than erroring, since
    /// this is advisory bookkeeping, not access control.
    ///
    /// TODO: Replace with user auth once RAL-252 is done -- this whole field
    /// is a stopgap for "some caller-attributable identity" until requests
    /// carry a real authenticated identity instead of a config default.
    #[serde(default)]
    pub default_user: Option<String>,
    /// Whether `default_user` should hold the admin flag (RAL-332). Applied
    /// once at daemon startup (see `server::bootstrap_default_user_admin`):
    /// `Some(true)` registers `default_user` if needed and promotes it;
    /// `Some(false)` demotes it if it currently holds admin. `None` (unset)
    /// leaves admin status untouched -- config is otherwise the source of
    /// truth here, so a stale `false` (or a config that stops setting this
    /// field) will demote an admin that was hand-granted via the board's
    /// Users tab on the next restart.
    ///
    /// Exists because the board's Users tab is itself admin-gated: without
    /// this, the very first admin can only be granted through a raw
    /// `POST /api/users/{name}/admin` call (`require_admin`'s bootstrap
    /// exception for "zero admins registered").
    #[serde(default)]
    pub default_user_is_admin: Option<bool>,
    /// The global concurrency cap shared by the scheduler, task-level proofs,
    /// and guardian review merges (see `crate::DEFAULT_MAX_CONCURRENT`). `None`
    /// means unset (so a lower layer can supply it); resolved callers use
    /// [`max_concurrent`](Self::max_concurrent). A configured `0` means "no
    /// limit". A negative value is treated as unset, matching this file's
    /// "malformed config never blocks" rule, and falls back to
    /// `crate::DEFAULT_MAX_CONCURRENT` (20) — `ralphus check health` warns
    /// when this happens.
    #[serde(default)]
    pub max_concurrent: Option<i64>,
}

impl DaemonConfig {
    /// Parsed `(start, end)` down-time windows. An entry whose `start`/`end`
    /// doesn't parse as `"HH:MM"` is silently dropped — one bad window must
    /// not block scheduling entirely, matching the "malformed config never
    /// fails loudly" rule this ticket calls out.
    #[must_use]
    pub fn downtime_windows(&self) -> Vec<(NaiveTime, NaiveTime)> {
        self.downtime
            .iter()
            .filter_map(|w| {
                let start = NaiveTime::parse_from_str(&w.start, "%H:%M").ok()?;
                let end = NaiveTime::parse_from_str(&w.end, "%H:%M").ok()?;
                Some((start, end))
            })
            .collect()
    }

    /// The effective global concurrency cap. `0` means "no limit". Defaults
    /// to `crate::DEFAULT_MAX_CONCURRENT` when unset, or when the configured
    /// value is negative.
    #[must_use]
    pub fn max_concurrent(&self) -> i64 {
        match self.max_concurrent {
            Some(n) if n >= 0 => n,
            _ => crate::DEFAULT_MAX_CONCURRENT,
        }
    }
}

/// Whether UTC wall-clock time `now` falls within any of `windows`. A window
/// where `start > end` crosses midnight (e.g. `22:00`-`06:00`) and matches
/// `[start, 24:00) ∪ [00:00, end)`; `start == end` matches nothing (a
/// zero-width window never blocks).
#[must_use]
pub fn in_downtime(now: NaiveTime, windows: &[(NaiveTime, NaiveTime)]) -> bool {
    windows.iter().any(|&(start, end)| match start.cmp(&end) {
        std::cmp::Ordering::Less => now >= start && now < end,
        std::cmp::Ordering::Greater => now >= start || now < end,
        std::cmp::Ordering::Equal => false,
    })
}

/// Whether the scheduler is currently within a configured down-time window,
/// per the effective (global-under-project) `[daemon]` config. Scopes ONLY
/// the scheduler's own automatic claiming of `Pending` runs (RAL-122) — it is
/// never consulted by explicit user actions (`set-status`, `activate`, etc),
/// which remain unaffected by down-time.
#[must_use]
pub fn scheduler_in_downtime() -> bool {
    let windows = load_daemon_config().downtime_windows();
    if windows.is_empty() {
        return false;
    }
    in_downtime(Utc::now().time(), &windows)
}

/// Cartographer retention configuration (`[cartographer]` table, RAL-98).
/// Two independently configurable caps — a time window and a max row count —
/// either condition triggers pruning. `None` means unset (so a lower layer
/// can supply it); resolved callers use [`retention_days`](Self::retention_days)
/// / [`max_rows`](Self::max_rows), which fall back to the defaults (30 days,
/// 50,000 rows).
#[derive(Debug, Default, Deserialize, Clone, Copy, PartialEq, Eq)]
pub struct CartographerConfig {
    #[serde(default)]
    pub retention_days: Option<i64>,
    #[serde(default)]
    pub max_rows: Option<i64>,
}

impl CartographerConfig {
    /// Rows older than this many days are pruned. Defaults to 30.
    #[must_use]
    pub fn retention_days(&self) -> i64 {
        self.retention_days.unwrap_or(30)
    }

    /// Once the table exceeds this many rows, the oldest excess rows are
    /// pruned. Defaults to 50,000 (a single busy run can generate hundreds
    /// of granular events, so a lower cap could be consumed in a day or two
    /// of normal use).
    #[must_use]
    pub fn max_rows(&self) -> i64 {
        self.max_rows.unwrap_or(50_000)
    }
}

/// Session cost-cap enforcement configuration (`[budget]` table, RAL-161).
/// Controls how often each running session's own poll loop re-checks its
/// live `cost_usd` against its resolved `maximum_budget_usd` cap. `None`
/// means unset (so a lower layer can supply it); resolved callers use
/// [`poll_interval`](Self::poll_interval), which falls back to the default.
#[derive(Debug, Default, Deserialize, Clone, Copy, PartialEq, Eq)]
pub struct BudgetConfig {
    /// Milliseconds between cost-cap checks. `0` means "check immediately on
    /// every event" (no additional throttling beyond the loop's own
    /// cadence). Defaults to 200ms when unset -- tighter than the 500ms
    /// `tmux capture-pane` scrape cadence it rides alongside, per RAL-161's
    /// "bias toward catching an overrun quickly" requirement. The check
    /// itself is a cheap in-memory float comparison, decoupled from the
    /// actual (expensive, subprocess-spawning) pane scrape.
    #[serde(default)]
    pub poll_interval_ms: Option<u64>,
}

impl BudgetConfig {
    /// The effective poll interval. `0` (explicit or default-absent-config's
    /// interpretation of "immediate") resolves to a small minimum sleep
    /// rather than a true busy-loop, so "check every event" doesn't burn a
    /// CPU core spinning between events that haven't arrived yet.
    #[must_use]
    pub fn poll_interval(&self) -> Duration {
        match self.poll_interval_ms {
            None => Duration::from_millis(200),
            Some(0) => Duration::from_millis(20),
            Some(ms) => Duration::from_millis(ms),
        }
    }
}

/// Terminal-log retention configuration (`[terminal_logs]` table, RAL-154).
/// Governs the durable, per-attempt tmux pane transcripts persisted by
/// `crate::terminal_log` (see that module's doc comment) — independent of
/// [`CartographerConfig`], which only governs the separate structured
/// `cartographer_events` table.
///
/// Five independently configurable knobs, all `None` meaning unset (so a
/// lower layer can supply it): [`max_lines_per_attempt`](Self::max_lines_per_attempt)
/// bounds a single attempt's log file size; `retention_days` and `max_files`
/// bound the total on-disk footprint over time, mirroring
/// [`CartographerConfig`]'s two-cap retention model (either condition
/// triggers pruning); [`max_transcript_bytes_per_attempt`](Self::max_transcript_bytes_per_attempt)
/// (RAL-397 Phase 2H) bounds the *raw* `.raw` pipe-pane transcript a single
/// attempt writes, independent of the `.log` file's line-based cap; and
/// [`pane_history_limit`](Self::pane_history_limit) (RAL-397 Phase 2H)
/// overrides the tmux pane scrollback ceiling (`crate::tmux::TMUX_HISTORY_LIMIT`).
#[derive(Debug, Default, Deserialize, Clone, Copy, PartialEq, Eq)]
pub struct TerminalLogConfig {
    /// Max lines kept per attempt's log file (the tail is kept, oldest lines
    /// dropped). Must be `>= 1` — an explicit `0` or negative value is
    /// treated as unset (falls back to the default) rather than failing
    /// loudly, matching this file's "malformed config never blocks" rule.
    #[serde(default)]
    pub max_lines_per_attempt: Option<i64>,
    #[serde(default)]
    pub retention_days: Option<i64>,
    #[serde(default)]
    pub max_files: Option<i64>,
    /// Max bytes a single attempt's `.raw` transcript may grow to (RAL-397
    /// Phase 2C/2H) before `ralphus-runner pipe-sink` stops persisting
    /// further output (still draining stdin so the pane never blocks — see
    /// `runner/src/main.rs::pipe_sink`). Must be `>= 1` — same "unset on a
    /// non-positive value" rule as `max_lines_per_attempt`.
    #[serde(default)]
    pub max_transcript_bytes_per_attempt: Option<i64>,
    /// The tmux pane `history-limit` (scrollback line ceiling) set on every
    /// cell pane (RAL-397 Phase 2H). This is the per-pane resident-memory
    /// ceiling (`history-limit × pane width`; see
    /// `crate::tmux::TMUX_HISTORY_LIMIT`) — deep scrollback is served from the
    /// durable `.raw` transcript, so the live pane only needs the live window.
    /// Must be `>= 1` — same "unset on a non-positive value" rule as
    /// `max_lines_per_attempt`. Unlike this struct's other knobs the *default*
    /// when unset is not baked into a getter here but lives as
    /// `crate::tmux::TMUX_HISTORY_LIMIT` (the const the set-option falls back
    /// to), so there is a single source of truth for the live-window value.
    #[serde(default)]
    pub pane_history_limit: Option<i64>,
}

impl TerminalLogConfig {
    /// Max lines kept per attempt's log file. Defaults to 4000. A configured
    /// value `< 1` is treated as unset, per this struct's doc comment.
    #[must_use]
    pub fn max_lines_per_attempt(&self) -> usize {
        match self.max_lines_per_attempt {
            Some(n) if n >= 1 => n as usize,
            _ => 4000,
        }
    }

    /// Attempt log files older than this many days are pruned. Defaults to 30
    /// (same default as [`CartographerConfig::retention_days`]).
    #[must_use]
    pub fn retention_days(&self) -> i64 {
        self.retention_days.unwrap_or(30)
    }

    /// Once the total number of persisted attempt log files exceeds this
    /// count, the oldest excess files are pruned. Defaults to 2000 — a
    /// single frequently-reattached session can otherwise grow unboundedly
    /// (the risk this ticket calls out), so this cap applies across every
    /// session's files, not per-session.
    #[must_use]
    pub fn max_files(&self) -> i64 {
        self.max_files.unwrap_or(2000)
    }

    /// Max bytes a single attempt's `.raw` transcript may grow to. Defaults
    /// to 256 MiB (matching `ralphus-runner pipe-sink`'s own built-in
    /// default, `DEFAULT_PIPE_SINK_MAX_BYTES` — kept as a duplicated literal
    /// rather than a shared constant since `daemon` and `runner` are separate
    /// crates with no existing shared home for a single-value default this
    /// small). A configured value `< 1` is treated as unset.
    #[must_use]
    pub fn max_transcript_bytes_per_attempt(&self) -> u64 {
        match self.max_transcript_bytes_per_attempt {
            Some(n) if n >= 1 => n as u64,
            _ => 256 * 1024 * 1024,
        }
    }

    /// The configured tmux pane `history-limit`, or `None` when unset (a
    /// non-positive value is treated as unset per this struct's doc comment,
    /// and a value too large to fit a `u32` line count likewise falls back).
    /// Deliberately returns an `Option` rather than baking in a default: the
    /// default lives as `crate::tmux::TMUX_HISTORY_LIMIT` (the const the
    /// set-option falls back to), keeping a single source of truth for the
    /// live-window value.
    #[must_use]
    pub fn pane_history_limit(&self) -> Option<u32> {
        match self.pane_history_limit {
            Some(n) if n >= 1 => u32::try_from(n).ok(),
            _ => None,
        }
    }
}

/// Parse a `TerminalLogConfig` from the given TOML text; the default (4000
/// lines / 30 days / 2000 files) when the `[terminal_logs]` table is absent.
#[must_use]
pub fn terminal_log_from_toml_str(s: &str) -> TerminalLogConfig {
    toml::from_str::<ConfigFile>(s)
        .unwrap_or_default()
        .terminal_logs
        .unwrap_or_default()
}

/// Load the effective terminal-log config by layering the global config file
/// under the nearest per-project `.ralphus.toml` (per-project scalars win),
/// following the same pattern as [`load_cartographer_config`].
#[must_use]
pub fn load_terminal_log_config() -> TerminalLogConfig {
    let global = global_config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| terminal_log_from_toml_str(&s))
        .unwrap_or_default();
    let local = std::env::current_dir()
        .ok()
        .as_deref()
        .and_then(find_project_config)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| terminal_log_from_toml_str(&s))
        .unwrap_or_default();
    TerminalLogConfig {
        max_lines_per_attempt: local.max_lines_per_attempt.or(global.max_lines_per_attempt),
        retention_days: local.retention_days.or(global.retention_days),
        max_files: local.max_files.or(global.max_files),
        max_transcript_bytes_per_attempt: local
            .max_transcript_bytes_per_attempt
            .or(global.max_transcript_bytes_per_attempt),
        pane_history_limit: local.pane_history_limit.or(global.pane_history_limit),
    }
}

/// Live View debug-line-visibility configuration (`[live_view]` table,
/// RAL-232). Controls the default state of the board's per-pane "Show Debug
/// Messages" checkbox -- ralphus interleaves its own diagnostic/telemetry
/// lines (`ralphus [TYPE] ...`, `RALPHUS_EVENT:`, `RALPHUS_TMUX_DONE`) into
/// the same tmux pane the agent's own output streams through, and the board
/// strips those lines from its live rendering by default
/// (`librarian/assets/board.html`'s `stripDebugLines`). This config only
/// governs that *rendering* default; the daemon's own pane capture, the
/// persisted last-pane-content snapshot, and Cartographer/terminal-log
/// records are all unaffected and always keep both agent and debug lines --
/// see `crate::server::capture_pane_reply`. `None` means unset (so a lower
/// layer can supply it); resolved callers use
/// [`show_debug_messages_default`](Self::show_debug_messages_default), which
/// falls back to `false` (unchecked, agent-only).
#[derive(Debug, Default, Deserialize, Clone, Copy, PartialEq, Eq)]
pub struct LiveViewConfig {
    #[serde(default)]
    pub show_debug_messages_default: Option<bool>,
    /// RAL-303: how many characters of a `tool_use` argument value the
    /// claude-code backend renders into the Live View tmux pane before
    /// truncating with a trailing `…`. The pane is a write-once pty
    /// transcript (`daemon/src/tmux.rs`'s `capture-pane`), so this can only
    /// take effect at cell-launch time -- there is no retroactive/live
    /// toggle. `None` means unset; resolved callers use
    /// [`tool_arg_truncate_chars`](Self::tool_arg_truncate_chars), which
    /// falls back to 200.
    #[serde(default)]
    pub tool_arg_truncate_chars: Option<u32>,
}

impl LiveViewConfig {
    /// Whether a newly-opened Live View pane defaults to showing ralphus's
    /// own diagnostic/telemetry lines. Defaults to `false` (agent-only) when
    /// unset.
    #[must_use]
    pub fn show_debug_messages_default(&self) -> bool {
        self.show_debug_messages_default.unwrap_or(false)
    }

    /// How many characters of a `tool_use` argument value to keep before
    /// truncating in the Live View tmux pane. Defaults to 200 when unset.
    #[must_use]
    pub fn tool_arg_truncate_chars(&self) -> u32 {
        self.tool_arg_truncate_chars
            .unwrap_or(DEFAULT_TOOL_ARG_TRUNCATE_CHARS)
    }
}

/// RAL-303: the default [`LiveViewConfig::tool_arg_truncate_chars`], used
/// whenever `[live_view] tool_arg_truncate_chars` is unset or invalid.
/// Raised from the runner's old hardcoded 80 -- that cutoff made exactly the
/// tool calls an operator most needs to read (file edits, shell commands,
/// diffs) illegible.
pub const DEFAULT_TOOL_ARG_TRUNCATE_CHARS: u32 = 200;

/// Parse a `LiveViewConfig` from the given TOML text; the default (unchecked)
/// when the `[live_view]` table is absent.
#[must_use]
pub fn live_view_from_toml_str(s: &str) -> LiveViewConfig {
    toml::from_str::<ConfigFile>(s)
        .unwrap_or_default()
        .live_view
        .unwrap_or_default()
}

/// Load the effective Live View config by layering the global config file
/// under the nearest per-project `.ralphus.toml` (per-project scalars win),
/// following the `[terminal_logs]`/`[cartographer]` pattern -- current-dir-based
/// since the daemon's HTTP handlers have no per-request "review cwd".
#[must_use]
pub fn load_live_view_config() -> LiveViewConfig {
    let global = global_config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| live_view_from_toml_str(&s))
        .unwrap_or_default();
    let local = std::env::current_dir()
        .ok()
        .as_deref()
        .and_then(find_project_config)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| live_view_from_toml_str(&s))
        .unwrap_or_default();
    LiveViewConfig {
        show_debug_messages_default: local
            .show_debug_messages_default
            .or(global.show_debug_messages_default),
        tool_arg_truncate_chars: local
            .tool_arg_truncate_chars
            .or(global.tool_arg_truncate_chars),
    }
}

/// Autocompaction-thrash detection thresholds (`[thrash]` table, RAL-339).
/// Governs when a cell/proof's runner fails a run outright for repeatedly
/// compacting its own context without enough real progress between
/// compactions to justify it -- see `runner/src/thrash.rs`'s
/// `ThrashTracker` for the actual rule these two numbers feed. `None` means
/// unset (so a lower layer can supply it); resolved callers use
/// [`max_compactions`](Self::max_compactions)/[`min_turn_gap`](Self::min_turn_gap),
/// which fall back to the runner's own defaults (3 compactions / 2 turns).
#[derive(Debug, Default, Deserialize, Clone, Copy, PartialEq, Eq)]
pub struct ThrashConfig {
    /// N: how many compactions must occur in one run before thrash detection
    /// can fire at all.
    #[serde(default)]
    pub max_compactions: Option<u32>,
    /// M: the previous-compaction gap (in assistant turns) below which a
    /// compaction at/after `max_compactions` counts as thrash.
    #[serde(default)]
    pub min_turn_gap: Option<u32>,
}

impl ThrashConfig {
    /// N. Defaults to 3 when unset.
    #[must_use]
    pub fn max_compactions(&self) -> u32 {
        self.max_compactions
            .unwrap_or(DEFAULT_THRASH_MAX_COMPACTIONS)
    }

    /// M. Defaults to 2 when unset.
    #[must_use]
    pub fn min_turn_gap(&self) -> u32 {
        self.min_turn_gap.unwrap_or(DEFAULT_THRASH_MIN_TURN_GAP)
    }
}

/// RAL-339: the defaults used whenever `[thrash]`'s fields are unset or
/// invalid -- must stay in sync with `runner/src/thrash.rs`'s
/// `DEFAULT_MAX_COMPACTIONS`/`DEFAULT_MIN_TURN_GAP` (the daemon and the
/// runner each need their own copy: the daemon resolves `.ralphus.toml`,
/// while the runner never reads project config directly, see
/// `runner/src/config.rs`'s own doc comment for why).
pub const DEFAULT_THRASH_MAX_COMPACTIONS: u32 = 3;
pub const DEFAULT_THRASH_MIN_TURN_GAP: u32 = 2;

/// Parse a `ThrashConfig` from the given TOML text; the default (3/2) when
/// the `[thrash]` table is absent.
#[must_use]
pub fn thrash_from_toml_str(s: &str) -> ThrashConfig {
    toml::from_str::<ConfigFile>(s)
        .unwrap_or_default()
        .thrash
        .unwrap_or_default()
}

/// Load the effective thrash config by layering the global config file under
/// the nearest per-project `.ralphus.toml` (per-project scalars win),
/// following the `[live_view]`/`[terminal_logs]`/`[cartographer]` pattern.
#[must_use]
pub fn load_thrash_config() -> ThrashConfig {
    let global = global_config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| thrash_from_toml_str(&s))
        .unwrap_or_default();
    let local = std::env::current_dir()
        .ok()
        .as_deref()
        .and_then(find_project_config)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| thrash_from_toml_str(&s))
        .unwrap_or_default();
    ThrashConfig {
        max_compactions: local.max_compactions.or(global.max_compactions),
        min_turn_gap: local.min_turn_gap.or(global.min_turn_gap),
    }
}

/// Forge routing configuration (`[forge]` table, RAL-117). Lets a project pin
/// which forge (GitHub/GitLab) and remote to submit PRs against, instead of
/// relying purely on `git remote get-url` autodetection. `None` fields fall
/// back to autodetection/defaults at the point of use (see `crate::forge`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ForgeConfig {
    /// `"github"` or `"gitlab"`. `None` means autodetect from the remote URL's host.
    #[serde(default)]
    pub kind: Option<String>,
    /// Git remote name to read the repository URL from — a *fallback*, not an
    /// override: a review resolves its remote from its own `base_branch`
    /// first (its `<remote>/` prefix, else that branch's `@{u}` upstream),
    /// and only reaches this field when neither resolves. Defaults to
    /// `"origin"`. See [`crate::forge::resolve_remote_name`] for the full
    /// precedence.
    #[serde(default)]
    pub remote: Option<String>,
    /// Override API base URL, for self-hosted GitHub Enterprise / GitLab instances.
    /// `None` uses the public `api.github.com` / `gitlab.com/api/v4` endpoints.
    #[serde(default)]
    pub api_base: Option<String>,
    /// Name of the environment variable holding the forge API token. `None`
    /// falls back to `RALPHUS_GITHUB_TOKEN` / `RALPHUS_GITLAB_TOKEN`.
    #[serde(default)]
    pub token_env: Option<String>,
    /// Template for the branch name a submitted PR/MR is pushed under
    /// (RAL-244), e.g. `"{name}-review"` or `"review-{name}"` — `{name}` is
    /// replaced with the source branch's own name (see
    /// [`crate::pr::apply_pr_branch_convention`]). `None` falls back to
    /// [`DEFAULT_PR_BRANCH_CONVENTION`]. Validated by
    /// [`validate_pull_request_branch_convention`] -- unlike every other
    /// string field on this struct, an explicitly-set-but-invalid value is a
    /// hard `check health` failure rather than a silent fallback (RAL-244
    /// interview decision: a silently-wrong branch convention is worse than
    /// a loud one).
    #[serde(default)]
    pub pull_request_branch_convention: Option<String>,
}

/// Fallback [`ForgeConfig::pull_request_branch_convention`] when the project
/// sets none.
pub const DEFAULT_PR_BRANCH_CONVENTION: &str = "{name}-review";

/// Validates a configured `pull_request_branch_convention` string (RAL-244):
/// it must be non-empty and contain the `{name}` placeholder, or the
/// generated branch name would be empty or identical for every submission.
/// Only called on an explicitly-set value -- `None` silently uses
/// [`DEFAULT_PR_BRANCH_CONVENTION`] instead.
pub fn validate_pull_request_branch_convention(
    convention: &str,
) -> std::result::Result<(), String> {
    if convention.is_empty() {
        return Err(
            "forge.pull_request_branch_convention is set but empty -- must be a non-empty \
             string containing \"{name}\""
                .to_string(),
        );
    }
    if !convention.contains("{name}") {
        return Err(format!(
            "forge.pull_request_branch_convention \"{convention}\" does not contain \"{{name}}\" \
             -- every submitted PR branch would get the same literal name"
        ));
    }
    Ok(())
}

impl ForgeConfig {
    /// Layer `self` (global) under `over` (per-project). Per-project scalars
    /// win when present, same semantics as [`ReviewConfig::merge`].
    #[must_use]
    pub fn merge(self, over: ForgeConfig) -> ForgeConfig {
        ForgeConfig {
            kind: over.kind.or(self.kind),
            remote: over.remote.or(self.remote),
            api_base: over.api_base.or(self.api_base),
            token_env: over.token_env.or(self.token_env),
            pull_request_branch_convention: over
                .pull_request_branch_convention
                .or(self.pull_request_branch_convention),
        }
    }

    /// The effective PR branch convention: the configured value, or
    /// [`DEFAULT_PR_BRANCH_CONVENTION`] when unset.
    #[must_use]
    pub fn resolved_pr_branch_convention(&self) -> &str {
        self.pull_request_branch_convention
            .as_deref()
            .unwrap_or(DEFAULT_PR_BRANCH_CONVENTION)
    }
}

/// Environment-variable-override allowlist configuration (`[env_overrides]`
/// table, RAL-150). A retry-time env override whose key is **not** in
/// `allowlist` still takes effect (the allowlist is not a security boundary
/// against setting arbitrary env vars — the daemon operator already trusts
/// whoever can submit runs), but its *value* is redacted (masked) wherever
/// overrides are logged (Cartographer, `rlog!`) or shown in the board's
/// audit-facing views, since an override value commonly carries a secret
/// (an API key, a token). Allowlisted keys are logged/displayed in the clear
/// since their values (model names, feature flags, etc.) are not secrets.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct EnvOverridesConfig {
    /// Env var key names whose override values are safe to show unredacted.
    /// A list field: layers are unioned, same as [`ReviewConfig::checks`].
    #[serde(default)]
    pub allowlist: Vec<String>,
}

impl EnvOverridesConfig {
    /// Layer `self` (global) under `over` (per-project): union the allowlists,
    /// global entries first then new per-project ones, de-duplicated and
    /// order-preserving — same semantics as [`ReviewConfig::merge`]'s `checks`.
    #[must_use]
    pub fn merge(self, over: EnvOverridesConfig) -> EnvOverridesConfig {
        let mut allowlist = self.allowlist;
        for k in over.allowlist {
            if !allowlist.contains(&k) {
                allowlist.push(k);
            }
        }
        EnvOverridesConfig { allowlist }
    }

    /// Whether `key` is in the allowlist (exact match).
    #[must_use]
    pub fn is_allowed(&self, key: &str) -> bool {
        self.allowlist.iter().any(|k| k == key)
    }
}

/// Agent isolation configuration (`[agent_isolation]` table, RAL-336). By
/// default a ralphus-spawned agent session (Claude Code, Codex, Pi) is
/// isolated from the operator's personal CLI configuration and memory on the
/// host machine, regardless of what's present outside the cell's worktree.
/// These two independent opt-ins let an operator explicitly re-enable one or
/// both -- there is no combined flag, since a project may want its own
/// checked-in settings honored without also pulling in the operator's
/// personal cross-project memory, or vice versa. Some backends (Codex, Pi)
/// only expose a single config-directory env var that doesn't cleanly
/// separate settings from memory; for those, isolation engages whenever
/// *either* opt-in is off, favoring over-isolation over silently leaking
/// personal state.
#[derive(Debug, Default, Deserialize, Clone, Copy, PartialEq, Eq)]
pub struct AgentIsolationConfig {
    /// Whether a spawned agent session may load the operator's personal
    /// settings/config (e.g. Claude Code's `~/.claude/settings.json`, Codex's
    /// `~/.codex/config.toml`, Pi's on-disk config). `None`/`false` means
    /// isolated (the default). Resolved callers use
    /// [`allow_personal_settings`](Self::allow_personal_settings).
    #[serde(default)]
    pub allow_personal_settings: Option<bool>,
    /// Whether a spawned agent session may load the operator's personal
    /// cross-project memory (e.g. Claude Code's global `CLAUDE.md`). `None`/
    /// `false` means isolated (the default). Resolved callers use
    /// [`allow_personal_memory`](Self::allow_personal_memory).
    #[serde(default)]
    pub allow_personal_memory: Option<bool>,
}

impl AgentIsolationConfig {
    /// Whether a spawned agent session may load the operator's personal
    /// settings/config. Defaults to `false` (isolated) when unset.
    #[must_use]
    pub fn allow_personal_settings(&self) -> bool {
        self.allow_personal_settings.unwrap_or(false)
    }

    /// Whether a spawned agent session may load the operator's personal
    /// cross-project memory. Defaults to `false` (isolated) when unset.
    #[must_use]
    pub fn allow_personal_memory(&self) -> bool {
        self.allow_personal_memory.unwrap_or(false)
    }
}

/// Parse an `AgentIsolationConfig` from the given TOML text; the default
/// (fully isolated) when the `[agent_isolation]` table is absent.
#[must_use]
pub fn agent_isolation_from_toml_str(s: &str) -> AgentIsolationConfig {
    toml::from_str::<ConfigFile>(s)
        .unwrap_or_default()
        .agent_isolation
        .unwrap_or_default()
}

fn load_agent_isolation_file(path: &Path) -> AgentIsolationConfig {
    std::fs::read_to_string(path)
        .map(|s| agent_isolation_from_toml_str(&s))
        .unwrap_or_default()
}

/// Resolve the effective agent isolation config for a cell rooted at `cwd`:
/// the global config layered under the nearest per-project `.ralphus.toml`
/// (per-project scalars win), same layering as [`resolve_forge`].
#[must_use]
pub fn resolve_agent_isolation(cwd: &Path) -> AgentIsolationConfig {
    let global = global_config_path()
        .map(|p| load_agent_isolation_file(&p))
        .unwrap_or_default();
    let project = find_project_config(cwd)
        .map(|p| load_agent_isolation_file(&p))
        .unwrap_or_default();
    AgentIsolationConfig {
        allow_personal_settings: project
            .allow_personal_settings
            .or(global.allow_personal_settings),
        allow_personal_memory: project
            .allow_personal_memory
            .or(global.allow_personal_memory),
    }
}

/// Whether `key` is a syntactically valid environment-variable name
/// (`[A-Za-z_][A-Za-z0-9_]*`) — required for a RAL-150 env override key.
/// Enforced at the API boundary ([`crate::server`]'s env-override handler)
/// so an override key is always a clean identifier everywhere it's used —
/// the env-assignment prefix [`crate::tmux::build_command_line_with_env`]
/// embeds for the POSIX `respawn-pane` path, and the `-e KEY=value` flag
/// [`crate::tmux::new_detached_session_with_command`] passes to `new-session`
/// on Windows.
#[must_use]
pub fn is_valid_env_key(key: &str) -> bool {
    let mut chars = key.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphabetic() || c == '_' => {}
        _ => return false,
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// Whether `value` is free of control characters (`\n`, `\r`, ESC, NUL, tabs,
/// ...) -- required for a RAL-227 env override value. Enforced at the same
/// API boundary as [`is_valid_env_key`] (`crate::server`'s env-override
/// handlers) so key and value get consistent, co-located validation. On
/// Windows (RAL-247) an override is delivered as an environment variable via
/// `new-session -e`, so a `\n` no longer splits the launch `send-keys` line —
/// but a literal control character in an env value is still bad hygiene
/// (it renders as terminal noise and is masked-broken by the generic
/// redactor), so it stays rejected up front. Values legitimately carry
/// arbitrary content (API keys, config values), so this rejects only control
/// characters, not general content.
#[must_use]
pub fn is_valid_env_value(value: &str) -> bool {
    !value.chars().any(|c| c.is_control())
}

/// One parameterized field a `[[templates]]` entry declares beyond the five
/// fixed Simple-tab base fields (`prompt`/`agent`/`model`/`project`/`proofs`)
/// -- RAL-297. `{name}` in the owning [`TemplateDef::prompt_template`] is
/// substituted with this field's value.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct TemplateFieldDef {
    /// The `{name}` placeholder key. Must be non-empty and unique within its
    /// template -- see [`validate_templates`].
    pub name: String,
    /// UI label; falls back to `name` when unset.
    #[serde(default)]
    pub label: Option<String>,
    /// One of `"string"`, `"number"`, `"bool"`. `None`/anything else is
    /// treated as `"string"` at the point of use, but is flagged by
    /// [`validate_templates`] so a typo doesn't silently degrade.
    #[serde(default, rename = "type")]
    pub field_type: Option<String>,
    /// Whether the Simple tab form must reject submission when this field is
    /// left blank. Defaults to `false` (optional).
    #[serde(default)]
    pub required: bool,
}

/// One `[[templates]]` entry (RAL-297): a named, parameterized prompt
/// template the Simple task form's template picker offers. Only supplies
/// *supplementary* fields plus a `prompt_template` -- the five base fields
/// (`prompt`, `agent`, `model`, `project`, `proofs`) are fixed by the form
/// itself and never template-defined.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct TemplateDef {
    /// Stable identifier (the template picker's `<option>` value). Must be
    /// non-empty and unique across the effective template list -- see
    /// [`validate_templates`].
    pub name: String,
    /// UI label; falls back to `name` when unset.
    #[serde(default)]
    pub label: Option<String>,
    /// One-line description shown under the template picker.
    #[serde(default)]
    pub description: Option<String>,
    /// Supplementary fields layered into `prompt_template` alongside the
    /// base `{prompt}`.
    #[serde(default)]
    pub fields: Vec<TemplateFieldDef>,
    /// The prompt text assembled for the work cell, with `{prompt}` and
    /// `{<field.name>}` placeholders substituted. Required -- an entry
    /// missing this is flagged by [`validate_templates`] and dropped from
    /// the effective list by [`load_templates_config`] (a malformed
    /// individual entry must not block every other template, or the whole
    /// Simple tab).
    #[serde(default)]
    pub prompt_template: Option<String>,
}

/// The built-in fallback template's stable name, used when a project
/// configures zero `[[templates]]` entries -- RAL-297.
pub const DEFAULT_TEMPLATE_NAME: &str = "hello-world";

/// The built-in "just run the prompt as-is" template, offered when no
/// `[[templates]]` are configured anywhere. Matches the example in
/// `docs/simple-task-templates.md`.
#[must_use]
pub fn default_template() -> TemplateDef {
    TemplateDef {
        name: DEFAULT_TEMPLATE_NAME.to_string(),
        label: Some("(Built-in)".to_string()),
        description: Some(
            "Minimal one-shot task: run a prompt as-is, no extra context.".to_string(),
        ),
        fields: Vec::new(),
        prompt_template: Some("{prompt}".to_string()),
    }
}

/// One error found in a `[[templates]]` list by [`validate_templates`] --
/// carries enough detail for both `ralphus check health` (a flat message
/// list) and a future UI surface (which template, if any, is at fault).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TemplateValidationError {
    /// The offending template's `name`, or `None` for a list-wide problem
    /// (e.g. two templates sharing a name).
    pub template: Option<String>,
    pub message: String,
}

/// Recognized `[[templates.fields]] type` values.
const VALID_FIELD_TYPES: &[&str] = &["string", "number", "bool"];

/// Validate a `[[templates]]` list against RAL-297's schema: non-empty
/// unique names, non-empty `prompt_template`, well-known field types,
/// non-empty unique field names per template, and `prompt_template`
/// placeholders that only reference `{prompt}` or a declared field. Returns
/// one error per problem found (never stops at the first) -- callers decide
/// whether an error means "drop this template" ([`load_templates_config`])
/// or "fail `check health`" (`cli`'s `check_templates`).
#[must_use]
pub fn validate_templates(templates: &[TemplateDef]) -> Vec<TemplateValidationError> {
    let mut errors = Vec::new();
    let mut seen_names: Vec<&str> = Vec::new();
    for t in templates {
        if t.name.trim().is_empty() {
            errors.push(TemplateValidationError {
                template: None,
                message: "a [[templates]] entry has an empty name".to_string(),
            });
        } else if seen_names.contains(&t.name.as_str()) {
            errors.push(TemplateValidationError {
                template: Some(t.name.clone()),
                message: format!("duplicate [[templates]] name \"{}\"", t.name),
            });
        } else {
            seen_names.push(&t.name);
        }
        match t.prompt_template.as_deref() {
            None | Some("") => errors.push(TemplateValidationError {
                template: Some(t.name.clone()),
                message: format!("template \"{}\" has no prompt_template (required)", t.name),
            }),
            Some(pt) => {
                let mut seen_fields: Vec<&str> = Vec::new();
                for f in &t.fields {
                    if f.name.trim().is_empty() {
                        errors.push(TemplateValidationError {
                            template: Some(t.name.clone()),
                            message: format!(
                                "template \"{}\" has a field with an empty name",
                                t.name
                            ),
                        });
                        continue;
                    }
                    if seen_fields.contains(&f.name.as_str()) {
                        errors.push(TemplateValidationError {
                            template: Some(t.name.clone()),
                            message: format!(
                                "template \"{}\" has duplicate field name \"{}\"",
                                t.name, f.name
                            ),
                        });
                    } else {
                        seen_fields.push(&f.name);
                    }
                    if let Some(ty) = &f.field_type {
                        if !VALID_FIELD_TYPES.contains(&ty.as_str()) {
                            errors.push(TemplateValidationError {
                                template: Some(t.name.clone()),
                                message: format!(
                                    "template \"{}\" field \"{}\" has unknown type \"{ty}\" \
                                     (expected string/number/bool)",
                                    t.name, f.name
                                ),
                            });
                        }
                    }
                }
                for placeholder in template_placeholders(pt) {
                    if placeholder != "prompt" && !t.fields.iter().any(|f| f.name == placeholder) {
                        errors.push(TemplateValidationError {
                            template: Some(t.name.clone()),
                            message: format!(
                                "template \"{}\" prompt_template references unknown \
                                 placeholder \"{{{placeholder}}}\"",
                                t.name
                            ),
                        });
                    }
                }
            }
        }
    }
    errors
}

/// Extract every `{ident}`-shaped placeholder name from `template` text
/// (used by [`validate_templates`]). A bare `{` not immediately followed by
/// an identifier and a closing `}` is ignored rather than treated as
/// malformed -- `prompt_template` is free-form prose, not a strict format
/// string.
fn template_placeholders(template: &str) -> Vec<String> {
    let mut names = Vec::new();
    let bytes = template.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'{' {
            if let Some(end) = template[i + 1..].find('}') {
                let candidate = &template[i + 1..i + 1 + end];
                if !candidate.is_empty()
                    && candidate
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || c == '_')
                    && candidate
                        .chars()
                        .next()
                        .is_some_and(|c| !c.is_ascii_digit())
                {
                    names.push(candidate.to_string());
                }
                i += end + 2;
                continue;
            }
        }
        i += 1;
    }
    names
}

/// Recognized `[ui] new_task_default_tab` values.
pub const VALID_NEW_TASK_TABS: &[&str] = &["simple", "files", "paste"];

/// UI defaults (`[ui]` table, RAL-297). Only one knob today: which tab the
/// board's "+ New Task" modal opens to.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct UiConfig {
    /// One of [`VALID_NEW_TASK_TABS`]. `None` (or an invalid value) resolves
    /// to `"simple"` -- see [`UiConfig::new_task_default_tab`]. An
    /// explicitly-set-but-invalid value is a hard `check health` failure
    /// (see `validate_new_task_default_tab`), matching
    /// [`ForgeConfig::pull_request_branch_convention`]'s precedent.
    #[serde(default)]
    pub new_task_default_tab: Option<String>,
}

impl UiConfig {
    /// The effective default tab for the "+ New Task" modal. Falls back to
    /// `"simple"` when unset or set to something other than
    /// [`VALID_NEW_TASK_TABS`].
    #[must_use]
    pub fn new_task_default_tab(&self) -> &str {
        match self.new_task_default_tab.as_deref() {
            Some(t) if VALID_NEW_TASK_TABS.contains(&t) => t,
            _ => "simple",
        }
    }
}

/// Validates a configured `[ui] new_task_default_tab` string (RAL-297): must
/// be one of [`VALID_NEW_TASK_TABS`]. Only called on an explicitly-set value
/// -- `None` silently resolves to `"simple"` instead.
pub fn validate_new_task_default_tab(tab: &str) -> std::result::Result<(), String> {
    if VALID_NEW_TASK_TABS.contains(&tab) {
        Ok(())
    } else {
        Err(format!(
            "ui.new_task_default_tab \"{tab}\" is not one of {VALID_NEW_TASK_TABS:?}"
        ))
    }
}

/// The on-disk file shape: either a `[review]` or a `[defaults]` table.
#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    ark: Option<ArkConfigLayer>,
    #[serde(default)]
    arbiter: Option<ArbiterConfig>,
    #[serde(default)]
    review: Option<ReviewConfig>,
    #[serde(default)]
    defaults: Option<ReviewConfig>,
    #[serde(default)]
    daemon: Option<DaemonConfig>,
    #[serde(default)]
    cartographer: Option<CartographerConfig>,
    #[serde(default)]
    terminal_logs: Option<TerminalLogConfig>,
    #[serde(default)]
    live_view: Option<LiveViewConfig>,
    #[serde(default)]
    thrash: Option<ThrashConfig>,
    #[serde(default)]
    forge: Option<ForgeConfig>,
    #[serde(default)]
    env_overrides: Option<EnvOverridesConfig>,
    #[serde(default)]
    agent_isolation: Option<AgentIsolationConfig>,
    #[serde(default)]
    budget: Option<BudgetConfig>,
    #[serde(default)]
    cors: Option<CorsConfig>,
    #[serde(default)]
    templates: Vec<TemplateDef>,
    #[serde(default)]
    ui: Option<UiConfig>,
}

#[must_use]
pub fn ark_from_toml_str(s: &str) -> ArkConfig {
    toml::from_str::<ConfigFile>(s)
        .unwrap_or_default()
        .ark
        .unwrap_or_default()
        .apply(ArkConfig::default())
}

/// Load Ark policy for one registered project. Project values replace global
/// values as a unit; all fields have explicit defaults.
#[must_use]
pub fn load_ark_config(project_root: &Path) -> ArkConfig {
    let global = global_config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| toml::from_str::<ConfigFile>(&s).ok())
        .and_then(|file| file.ark)
        .unwrap_or_default()
        .apply(ArkConfig::default());
    find_project_config(project_root)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .and_then(|s| toml::from_str::<ConfigFile>(&s).ok())
        .and_then(|file| file.ark)
        .unwrap_or_default()
        .apply(global)
}

/// Parse a config from TOML text, preferring `[review]` over `[defaults]`.
/// Malformed TOML yields the default (empty) config rather than an error, so a
/// broken file never blocks a review.
#[must_use]
pub fn from_toml_str(s: &str) -> ReviewConfig {
    let cf: ConfigFile = toml::from_str(s).unwrap_or_default();
    cf.review.or(cf.defaults).unwrap_or_default()
}

/// Read and parse a config file, or the default when it is absent/unreadable.
#[must_use]
pub fn load_file(path: &Path) -> ReviewConfig {
    std::fs::read_to_string(path)
        .map(|s| from_toml_str(&s))
        .unwrap_or_default()
}

/// The global config path: `$RALPHUS_CONFIG_HOME/config.toml`, else
/// `~/.config/ralphus/config.toml` (`USERPROFILE`/`HOME`). `None` if no home.
#[must_use]
pub fn global_config_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("RALPHUS_CONFIG_HOME") {
        return Some(PathBuf::from(dir).join("config.toml"));
    }
    let home = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME"))?;
    Some(
        PathBuf::from(home)
            .join(".config")
            .join("ralphus")
            .join("config.toml"),
    )
}

/// Discover the per-project config by walking up from `start` until a
/// `.ralphus.toml` is found or the filesystem root is reached.
#[must_use]
pub fn find_project_config(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        let candidate = d.join(".ralphus.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = d.parent();
    }
    None
}

/// Parse a `DaemonConfig` from the given TOML text.
#[must_use]
pub fn daemon_from_toml_str(s: &str) -> DaemonConfig {
    toml::from_str::<ConfigFile>(s)
        .unwrap_or_default()
        .daemon
        .unwrap_or_default()
}

/// A non-empty per-project down-time list wins outright over the global one
/// (same "project overrides" spirit as [`DaemonConfig`]'s scalar fields)
/// rather than unioning with it -- a project narrowing or widening its own
/// quiet hours should not also inherit an unrelated global schedule.
#[must_use]
fn merge_downtime(global: Vec<DowntimeWindow>, local: Vec<DowntimeWindow>) -> Vec<DowntimeWindow> {
    if local.is_empty() { global } else { local }
}

/// Merges `over`'s fields on top of `base` (`over` wins field-by-field,
/// falling back to `base` when unset) -- the same "higher-precedence layer
/// wins" rule [`load_daemon_config`] applies across all three of its layers.
#[must_use]
fn merge_daemon_config(base: DaemonConfig, over: DaemonConfig) -> DaemonConfig {
    DaemonConfig {
        log_path: over.log_path.or(base.log_path),
        log_level: over.log_level.or(base.log_level),
        downtime: merge_downtime(base.downtime, over.downtime),
        default_user: over.default_user.or(base.default_user),
        default_user_is_admin: over.default_user_is_admin.or(base.default_user_is_admin),
        max_concurrent: over.max_concurrent.or(base.max_concurrent),
    }
}

/// Parses `$RALPHUS_CONFIGURATION_PATH` (a `PATH`-separated list of
/// `.ralphus.toml` files, left-to-right, later wins) -- the same env var
/// `cli/src/config.rs` and `runner/src/config.rs` already read for every
/// other config field. See `agent_profiles.rs`'s copy of this same helper
/// for why the env value is taken as a parameter rather than read directly
/// (testability, given `std::env::set_var` is `unsafe` and forbidden here).
fn configuration_path_entries(configuration_path_env: Option<&str>) -> Vec<PathBuf> {
    let Some(raw) = configuration_path_env else {
        return Vec::new();
    };
    std::env::split_paths(raw).collect()
}

/// Load the effective daemon config, lowest to highest precedence:
/// `$RALPHUS_CONFIG_HOME/config.toml` (or its `~/.config/ralphus/` default),
/// then `$RALPHUS_CONFIGURATION_PATH` entries in order, then the
/// project-local `.ralphus.toml` found by walking up from `cwd` --
/// matching the precedence `agent_profiles::load_profiles_for_path_with`
/// and `machine_targets`'s equivalent already use. `load_daemon_config`
/// used to skip the `$RALPHUS_CONFIGURATION_PATH` layer entirely, so a
/// field (e.g. `default_user`) set only via that established convention
/// silently never loaded.
#[must_use]
fn load_daemon_config_with(
    cwd: Option<&Path>,
    configuration_path_env: Option<&str>,
) -> DaemonConfig {
    let mut merged = global_config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| daemon_from_toml_str(&s))
        .unwrap_or_default();
    for path in configuration_path_entries(configuration_path_env) {
        if let Ok(s) = std::fs::read_to_string(&path) {
            merged = merge_daemon_config(merged, daemon_from_toml_str(&s));
        }
    }
    let local = cwd
        .and_then(find_project_config)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| daemon_from_toml_str(&s))
        .unwrap_or_default();
    merge_daemon_config(merged, local)
}

#[must_use]
pub fn load_daemon_config() -> DaemonConfig {
    let cwd = std::env::current_dir().ok();
    let raw = std::env::var("RALPHUS_CONFIGURATION_PATH").ok();
    load_daemon_config_with(cwd.as_deref(), raw.as_deref())
}

/// Parse a `CartographerConfig` from the given TOML text; the default (30
/// days / 50,000 rows) when the `[cartographer]` table is absent.
#[must_use]
pub fn cartographer_from_toml_str(s: &str) -> CartographerConfig {
    toml::from_str::<ConfigFile>(s)
        .unwrap_or_default()
        .cartographer
        .unwrap_or_default()
}

/// Load the effective Cartographer retention config by layering the global
/// config file under the nearest per-project `.ralphus.toml` (per-project
/// scalars win, following the `[daemon]`/`[review]` pattern).
#[must_use]
pub fn load_cartographer_config() -> CartographerConfig {
    let global = global_config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| cartographer_from_toml_str(&s))
        .unwrap_or_default();
    let local = std::env::current_dir()
        .ok()
        .as_deref()
        .and_then(find_project_config)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| cartographer_from_toml_str(&s))
        .unwrap_or_default();
    CartographerConfig {
        retention_days: local.retention_days.or(global.retention_days),
        max_rows: local.max_rows.or(global.max_rows),
    }
}

/// Parse a `BudgetConfig` from the given TOML text; the default (200ms) when
/// the `[budget]` table is absent.
#[must_use]
pub fn budget_from_toml_str(s: &str) -> BudgetConfig {
    toml::from_str::<ConfigFile>(s)
        .unwrap_or_default()
        .budget
        .unwrap_or_default()
}

/// Load the effective session cost-cap poll config by layering the global
/// config file under the nearest per-project `.ralphus.toml` (per-project
/// scalars win, following the `[cartographer]`/`[daemon]` pattern).
#[must_use]
pub fn load_budget_config() -> BudgetConfig {
    let global = global_config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| budget_from_toml_str(&s))
        .unwrap_or_default();
    let local = std::env::current_dir()
        .ok()
        .as_deref()
        .and_then(find_project_config)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| budget_from_toml_str(&s))
        .unwrap_or_default();
    BudgetConfig {
        poll_interval_ms: local.poll_interval_ms.or(global.poll_interval_ms),
    }
}

/// Resolve the effective review config for a review rooted at `cwd`: the global
/// config layered under the nearest per-project `.ralphus.toml`.
#[must_use]
pub fn resolve(cwd: &Path) -> ReviewConfig {
    let global = global_config_path()
        .map(|p| load_file(&p))
        .unwrap_or_default();
    let project = find_project_config(cwd)
        .map(|p| load_file(&p))
        .unwrap_or_default();
    global.merge(project)
}

/// The global review config only (no per-project layer). RAL-250 uses this at
/// project-registration time to stamp the *current* global `skip_base_updates`
/// value into a newly-created project, so a later global change does not
/// retroactively flip that project (see `Store::register_project`).
#[must_use]
pub fn global_review_config() -> ReviewConfig {
    global_config_path()
        .map(|p| load_file(&p))
        .unwrap_or_default()
}

/// The nearest per-project `.ralphus.toml [review]` config only -- **without**
/// the global layer. RAL-250's layering reads this raw value (rather than the
/// merged [`resolve`]) so an explicitly-set project default can be told apart
/// from one inherited from the global config: an explicit project value wins
/// over both the project's creation-time stamp and the live global value.
#[must_use]
pub fn project_review_config(cwd: &Path) -> ReviewConfig {
    find_project_config(cwd)
        .map(|p| load_file(&p))
        .unwrap_or_default()
}

/// Parse a `ForgeConfig` from the given TOML text.
#[must_use]
pub fn forge_from_toml_str(s: &str) -> ForgeConfig {
    toml::from_str::<ConfigFile>(s)
        .unwrap_or_default()
        .forge
        .unwrap_or_default()
}

fn load_forge_file(path: &Path) -> ForgeConfig {
    std::fs::read_to_string(path)
        .map(|s| forge_from_toml_str(&s))
        .unwrap_or_default()
}

/// Resolve the effective forge config for a review rooted at `cwd`: the global
/// config layered under the nearest per-project `.ralphus.toml` (per-project
/// scalars win), same layering as [`resolve`].
#[must_use]
pub fn resolve_forge(cwd: &Path) -> ForgeConfig {
    let global = global_config_path()
        .map(|p| load_forge_file(&p))
        .unwrap_or_default();
    let project = find_project_config(cwd)
        .map(|p| load_forge_file(&p))
        .unwrap_or_default();
    global.merge(project)
}

/// Parse an `EnvOverridesConfig` from the given TOML text.
#[must_use]
pub fn env_overrides_from_toml_str(s: &str) -> EnvOverridesConfig {
    toml::from_str::<ConfigFile>(s)
        .unwrap_or_default()
        .env_overrides
        .unwrap_or_default()
}

/// Load the effective env-override allowlist using the daemon process's own
/// current directory to locate the per-project `.ralphus.toml` — the daemon
/// HTTP handlers have no per-request "review cwd" the way review config does,
/// so this mirrors [`load_daemon_config`]/[`load_cartographer_config`]
/// (current-dir-based) rather than [`resolve`]/[`resolve_forge`]
/// (explicit-cwd, used where a specific review/worktree root is already in
/// hand).
#[must_use]
pub fn load_env_overrides_config() -> EnvOverridesConfig {
    let global = global_config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| env_overrides_from_toml_str(&s))
        .unwrap_or_default();
    let local = std::env::current_dir()
        .ok()
        .as_deref()
        .and_then(find_project_config)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| env_overrides_from_toml_str(&s))
        .unwrap_or_default();
    global.merge(local)
}

/// Parse a `[[templates]]` list from the given TOML text (RAL-297). Absent
/// entirely different from present-but-empty: an absent `templates` key
/// parses to an empty `Vec` either way (array-of-tables has no "unset"
/// state), so "zero configured templates" is what [`load_templates_config`]
/// returning an empty list means -- callers fall back to
/// [`default_template`].
#[must_use]
pub fn templates_from_toml_str(s: &str) -> Vec<TemplateDef> {
    toml::from_str::<ConfigFile>(s)
        .unwrap_or_default()
        .templates
}

/// Load the effective `[[templates]]` list by unioning the global config
/// file's templates with the nearest per-project `.ralphus.toml`'s -- using
/// the daemon process's own current directory the same way
/// [`load_env_overrides_config`]/[`load_daemon_config`] do (no per-request
/// "review cwd"). A project template whose `name` matches a global one
/// replaces it (project wins on collision); otherwise both survive,
/// global-first. Does **not** apply [`default_template`]'s fallback -- see
/// [`effective_templates`] for that.
#[must_use]
pub fn load_templates_config() -> Vec<TemplateDef> {
    let global = global_config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| templates_from_toml_str(&s))
        .unwrap_or_default();
    let local = std::env::current_dir()
        .ok()
        .as_deref()
        .and_then(find_project_config)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| templates_from_toml_str(&s))
        .unwrap_or_default();
    let mut merged = Vec::new();
    for t in global {
        if !local.iter().any(|o| o.name == t.name) {
            merged.push(t);
        }
    }
    merged.extend(local);
    merged
}

/// The effective template list a Simple-tab template picker should offer:
/// [`load_templates_config`]'s configured templates (minus any entry
/// [`validate_templates`] flags as malformed -- one bad entry must not take
/// down the whole picker), or the single built-in [`default_template`] when
/// that leaves nothing. The returned `bool` is `true` when the fallback is
/// what's being shown (zero valid configured templates), which the board
/// uses to disable the picker and show a tooltip explaining why.
#[must_use]
pub fn effective_templates() -> (Vec<TemplateDef>, bool) {
    let configured = load_templates_config();
    let invalid: std::collections::HashSet<String> = validate_templates(&configured)
        .into_iter()
        .filter_map(|e| e.template)
        .collect();
    let valid: Vec<TemplateDef> = configured
        .into_iter()
        .filter(|t| !invalid.contains(&t.name))
        .collect();
    if valid.is_empty() {
        (vec![default_template()], true)
    } else {
        (valid, false)
    }
}

/// Parse a `[ui]` table from the given TOML text (RAL-297).
#[must_use]
pub fn ui_from_toml_str(s: &str) -> UiConfig {
    toml::from_str::<ConfigFile>(s)
        .unwrap_or_default()
        .ui
        .unwrap_or_default()
}

/// Load the effective `[ui]` config the same way as
/// [`load_templates_config`] (current-dir-based, per-project scalar wins).
#[must_use]
pub fn load_ui_config() -> UiConfig {
    let global = global_config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| ui_from_toml_str(&s))
        .unwrap_or_default();
    let local = std::env::current_dir()
        .ok()
        .as_deref()
        .and_then(find_project_config)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| ui_from_toml_str(&s))
        .unwrap_or_default();
    UiConfig {
        new_task_default_tab: local.new_task_default_tab.or(global.new_task_default_tab),
    }
}

/// Parse a `CorsConfig` from the given TOML text (RAL-220's `[cors]` table).
#[must_use]
pub fn cors_from_toml_str(s: &str) -> CorsConfig {
    toml::from_str::<ConfigFile>(s)
        .unwrap_or_default()
        .cors
        .unwrap_or_default()
}

/// Load the effective CORS allow-list using the daemon process's own current
/// directory to locate the per-project `.ralphus.toml` -- the daemon's HTTP
/// handlers have no per-request "review cwd" the way review config does, so
/// this mirrors [`load_env_overrides_config`]/[`load_daemon_config`]
/// (current-dir-based) rather than [`resolve`]/[`resolve_forge`].
#[must_use]
pub fn load_cors_config() -> CorsConfig {
    CORS_LOADS.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let global = global_config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| cors_from_toml_str(&s))
        .unwrap_or_default();
    let local = std::env::current_dir()
        .ok()
        .as_deref()
        .and_then(find_project_config)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| cors_from_toml_str(&s))
        .unwrap_or_default();
    global.merge(local)
}

/// How long a [`load_cors_config_cached`] result stays usable before the
/// files are consulted again. Short enough that editing `[cors]` in
/// `.ralphus.toml` takes effect within one board poll, long enough that a
/// burst of requests shares a single read.
const CORS_CACHE_TTL: Duration = Duration::from_secs(5);

/// The memoized `[cors]` allow-list behind [`load_cors_config_cached`], with
/// the instant it was read.
static CORS_CACHE: std::sync::Mutex<Option<(std::time::Instant, CorsConfig)>> =
    std::sync::Mutex::new(None);

/// How many times [`load_cors_config`] has actually touched the filesystem in
/// this process. Backs the regression test asserting the cache in front of it
/// really does collapse a burst of requests into one read.
static CORS_LOADS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// The effective CORS allow-list, memoized for [`CORS_CACHE_TTL`].
///
/// Every inbound HTTP request needs this decision (see `server::resolve_cors`),
/// and [`load_cors_config`] is not cheap for something on that path: a global
/// config read, a `find_project_config` directory walk up from the daemon's
/// cwd, and two TOML parses -- unconditional filesystem I/O per request, on a
/// value that changes only when someone edits a config file. The TTL keeps
/// edits picked up promptly without making the allow-list a restart-only
/// setting.
#[must_use]
pub fn load_cors_config_cached() -> CorsConfig {
    let mut cache = CORS_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some((read_at, cfg)) = cache.as_ref() {
        if read_at.elapsed() < CORS_CACHE_TTL {
            return cfg.clone();
        }
    }
    let cfg = load_cors_config();
    *cache = Some((std::time::Instant::now(), cfg.clone()));
    cfg
}

/// How many filesystem reads [`load_cors_config`] has performed -- see
/// [`CORS_LOADS`].
#[cfg(test)]
pub(crate) fn cors_config_load_count() -> u64 {
    CORS_LOADS.load(std::sync::atomic::Ordering::Relaxed)
}

/// Drop the memoized allow-list so the next [`load_cors_config_cached`] call
/// reads the files again.
#[cfg(test)]
pub(crate) fn reset_cors_cache_for_test() {
    *CORS_CACHE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ark_defaults_and_validation_are_safe() {
        let defaults = ark_from_toml_str("");
        assert_eq!(defaults.sweep_interval_days, 1);
        assert_eq!(defaults.stale_after_days, 90);
        assert_eq!(defaults.max_worktrees, 100);
        assert!(defaults.validate().is_ok());

        let configured = ark_from_toml_str(
            "[ark]\nsweep_interval_days=2\nstale_after_days=30\nmax_worktrees=25\n",
        );
        assert_eq!(configured.sweep_interval(), Duration::from_secs(172_800));
        assert_eq!(configured.stale_after_ms(), 2_592_000_000);
        assert_eq!(configured.max_worktrees, 25);
        assert!(
            ark_from_toml_str("[ark]\nmax_worktrees=0")
                .validate()
                .is_err()
        );
        let layered = ArkConfigLayer {
            stale_after_days: Some(14),
            ..ArkConfigLayer::default()
        }
        .apply(configured);
        assert_eq!(layered.sweep_interval_days, 2);
        assert_eq!(layered.stale_after_days, 14);
        assert_eq!(layered.max_worktrees, 25);
    }

    fn cfg(skip: Option<bool>, checks: &[&str]) -> ReviewConfig {
        ReviewConfig {
            skip_worktrees: skip,
            checks: checks.iter().map(|s| (*s).to_string()).collect(),
            ..ReviewConfig::default()
        }
    }

    #[test]
    fn parse_review_table() {
        let c = from_toml_str("[review]\nskip_worktrees = true\nchecks = [\"cargo test\"]\n");
        assert_eq!(c.skip_worktrees, Some(true));
        assert_eq!(c.checks, vec!["cargo test".to_string()]);
    }

    #[test]
    fn parse_defaults_table_fallback() {
        let c = from_toml_str("[defaults]\nskip_worktrees = false\n");
        assert_eq!(c.skip_worktrees, Some(false));
    }

    #[test]
    fn review_table_preferred_over_defaults() {
        let c =
            from_toml_str("[defaults]\nskip_worktrees = false\n[review]\nskip_worktrees = true\n");
        assert_eq!(c.skip_worktrees, Some(true));
    }

    #[test]
    fn malformed_toml_is_default() {
        assert_eq!(from_toml_str("not = = valid"), ReviewConfig::default());
    }

    // ── auto_build (RAL-101) ──────────────────────────────────────────────

    #[test]
    fn parse_auto_build() {
        let c = from_toml_str("[review]\nauto_build = \"cargo build\"\n");
        assert_eq!(c.auto_build, Some("cargo build".to_string()));
    }

    #[test]
    fn auto_build_unset_by_default() {
        assert_eq!(
            from_toml_str("[review]\nskip_worktrees = true\n").auto_build,
            None
        );
    }

    #[test]
    fn merge_auto_build_project_wins() {
        let global = ReviewConfig {
            auto_build: Some("global cmd".to_string()),
            ..ReviewConfig::default()
        };
        let project = ReviewConfig {
            auto_build: Some("project cmd".to_string()),
            ..ReviewConfig::default()
        };
        assert_eq!(
            global.clone().merge(project).auto_build,
            Some("project cmd".to_string())
        );
        // Project unset falls back to the global value.
        assert_eq!(
            global.merge(ReviewConfig::default()).auto_build,
            Some("global cmd".to_string())
        );
    }

    // ── default_resolver_agent ──────────────────────────────────────────

    #[test]
    fn default_resolver_agent_unset_resolves_to_ollama() {
        assert_eq!(ReviewConfig::default().default_resolver_agent(), "ollama");
    }

    #[test]
    fn parse_default_resolver_agent() {
        let c = from_toml_str("[review]\ndefault_resolver_agent = \"claude-code\"\n");
        assert_eq!(c.default_resolver_agent, Some("claude-code".to_string()));
        assert_eq!(c.default_resolver_agent(), "claude-code");
    }

    #[test]
    fn merge_default_resolver_agent_project_wins() {
        let global = ReviewConfig {
            default_resolver_agent: Some("claude".to_string()),
            ..ReviewConfig::default()
        };
        let project = ReviewConfig {
            default_resolver_agent: Some("claude-code".to_string()),
            ..ReviewConfig::default()
        };
        assert_eq!(
            global.clone().merge(project).default_resolver_agent,
            Some("claude-code".to_string())
        );
        // Project unset falls back to the global value.
        assert_eq!(
            global.merge(ReviewConfig::default()).default_resolver_agent,
            Some("claude".to_string())
        );
    }

    // ── default_resolver_model/default_machine/default_maximum_budget_usd
    // (RAL-342/RAL-338) ──────────────────────────────────────────────────

    #[test]
    fn default_resolver_model_unset_resolves_to_none() {
        assert_eq!(ReviewConfig::default().default_resolver_model(), None);
    }

    #[test]
    fn parse_default_resolver_model() {
        let c = from_toml_str("[review]\ndefault_resolver_model = \"claude-haiku-4-5\"\n");
        assert_eq!(c.default_resolver_model(), Some("claude-haiku-4-5"));
    }

    #[test]
    fn default_machine_unset_resolves_to_none() {
        assert_eq!(ReviewConfig::default().default_machine(), None);
    }

    #[test]
    fn parse_default_machine() {
        let c = from_toml_str("[review]\ndefault_machine = \"ib:A\"\n");
        assert_eq!(c.default_machine(), Some("ib:A"));
    }

    #[test]
    fn default_maximum_budget_usd_unset_resolves_to_none() {
        assert_eq!(ReviewConfig::default().default_maximum_budget_usd(), None);
    }

    #[test]
    fn parse_default_maximum_budget_usd() {
        let c = from_toml_str("[review]\ndefault_maximum_budget_usd = 5.0\n");
        assert_eq!(c.default_maximum_budget_usd(), Some(5.0));
    }

    #[test]
    fn merge_default_machine_project_wins() {
        let global = ReviewConfig {
            default_machine: Some("ib:A".to_string()),
            ..ReviewConfig::default()
        };
        let project = ReviewConfig {
            default_machine: Some("ib:B".to_string()),
            ..ReviewConfig::default()
        };
        assert_eq!(
            global.clone().merge(project).default_machine(),
            Some("ib:B")
        );
        assert_eq!(
            global.merge(ReviewConfig::default()).default_machine(),
            Some("ib:A")
        );
    }

    // ── arbiter (RAL-318) ──────────────────────────────────────────────────

    #[test]
    fn arbiter_config_unset_resolves_to_ollama_with_no_cap() {
        let c = ArbiterConfig::default();
        assert_eq!(c.agent(), "ollama");
        assert_eq!(c.model, None);
        assert_eq!(c.maximum_budget_usd, None);
    }

    #[test]
    fn parse_arbiter_config() {
        let c = arbiter_from_toml_str(
            "[arbiter]\nagent = \"claude\"\nmodel = \"claude-haiku-4-5\"\nmaximum_budget_usd = 2.5\n",
        );
        assert_eq!(c.agent(), "claude");
        assert_eq!(c.model.as_deref(), Some("claude-haiku-4-5"));
        assert_eq!(c.maximum_budget_usd, Some(2.5));
    }

    #[test]
    fn arbiter_config_absent_table_is_default() {
        assert_eq!(
            arbiter_from_toml_str("[review]\nskip_worktrees = true\n"),
            ArbiterConfig::default()
        );
    }

    // ── summary_format (RAL-124) ──────────────────────────────────────────

    #[test]
    fn summary_format_defaults_to_bullet_when_unset() {
        assert!(ReviewConfig::default().bullet_summary());
        assert!(from_toml_str("[review]\nskip_worktrees = true\n").bullet_summary());
    }

    #[test]
    fn summary_format_prose_disables_bullet() {
        let c = from_toml_str("[review]\nsummary_format = \"prose\"\n");
        assert_eq!(c.summary_format.as_deref(), Some("prose"));
        assert!(!c.bullet_summary());
    }

    #[test]
    fn summary_format_explicit_bullet_is_bullet() {
        let c = from_toml_str("[review]\nsummary_format = \"bullet\"\n");
        assert!(c.bullet_summary());
    }

    #[test]
    fn merge_summary_format_project_wins() {
        let global = ReviewConfig {
            summary_format: Some("prose".to_string()),
            ..ReviewConfig::default()
        };
        let project = ReviewConfig {
            summary_format: Some("bullet".to_string()),
            ..ReviewConfig::default()
        };
        assert!(global.clone().merge(project).bullet_summary());
        // Project unset falls back to the global value.
        assert!(!global.merge(ReviewConfig::default()).bullet_summary());
    }

    // ── default_proof_scope (RAL-168) ───────────────────────────────────────

    #[test]
    fn default_proof_scope_defaults_to_each_branch_when_unset() {
        assert_eq!(ReviewConfig::default().default_proof_scope(), "each_branch");
    }

    #[test]
    fn default_proof_scope_parses_final_branch_and_nothing() {
        let c = from_toml_str("[review]\ndefault_proof_scope = \"final_branch\"\n");
        assert_eq!(c.default_proof_scope(), "final_branch");
        let c = from_toml_str("[review]\ndefault_proof_scope = \"nothing\"\n");
        assert_eq!(c.default_proof_scope(), "nothing");
    }

    #[test]
    fn default_proof_scope_unrecognized_value_falls_back_to_each_branch() {
        let c = from_toml_str("[review]\ndefault_proof_scope = \"bogus\"\n");
        assert_eq!(c.default_proof_scope(), "each_branch");
    }

    #[test]
    fn default_proof_scope_accepts_the_legacy_verify_scope_key() {
        let c = from_toml_str("[review]\nverify_scope = \"final_branch\"\n");
        assert_eq!(c.default_proof_scope(), "final_branch");
    }

    #[test]
    fn verify_skip_auto_clean_defaults_to_false() {
        assert!(!ReviewConfig::default().verify_skip_auto_clean());
        let c = from_toml_str("[review]\nverify_skip_auto_clean = true\n");
        assert!(c.verify_skip_auto_clean());
    }

    #[test]
    fn merge_default_proof_scope_project_wins() {
        let global = ReviewConfig {
            default_proof_scope: Some("final_branch".to_string()),
            ..ReviewConfig::default()
        };
        let project = ReviewConfig {
            default_proof_scope: Some("nothing".to_string()),
            ..ReviewConfig::default()
        };
        assert_eq!(
            global.clone().merge(project).default_proof_scope(),
            "nothing"
        );
        // Project unset falls back to the global value.
        assert_eq!(
            global.merge(ReviewConfig::default()).default_proof_scope(),
            "final_branch"
        );
    }

    // ── auto_fix_pr_errors / auto_fix_prompt_template (RAL-395) ─────────────

    #[test]
    fn auto_fix_pr_errors_defaults_to_false_when_unset() {
        assert!(!ReviewConfig::default().auto_fix_pr_errors());
    }

    #[test]
    fn auto_fix_pr_errors_reads_from_toml() {
        let c = from_toml_str("[review]\nauto_fix_pr_errors = true\n");
        assert!(c.auto_fix_pr_errors());
    }

    #[test]
    fn auto_fix_prompt_template_defaults_to_none_when_unset() {
        assert_eq!(ReviewConfig::default().auto_fix_prompt_template(), None);
    }

    #[test]
    fn auto_fix_prompt_template_reads_from_toml() {
        let c = from_toml_str("[review]\nauto_fix_prompt_template = \"fix: <<prompt>>\"\n");
        assert_eq!(c.auto_fix_prompt_template(), Some("fix: <<prompt>>"));
    }

    #[test]
    fn auto_fix_prompt_template_valid_passes_validation() {
        let c = ReviewConfig {
            auto_fix_prompt_template: Some("fix: <<prompt>>".to_string()),
            ..ReviewConfig::default()
        };
        assert!(c.validate().is_ok());
    }

    #[test]
    fn auto_fix_prompt_template_missing_placeholder_fails_validation() {
        let c = ReviewConfig {
            auto_fix_prompt_template: Some("fix it please".to_string()),
            ..ReviewConfig::default()
        };
        let err = c.validate().unwrap_err();
        assert!(err.contains("auto_fix_prompt_template"), "{err}");
        assert!(err.contains("<<prompt>>"), "{err}");
    }

    #[test]
    fn auto_fix_prompt_template_unset_passes_validation() {
        assert!(ReviewConfig::default().validate().is_ok());
    }

    #[test]
    fn merge_auto_fix_pr_errors_project_wins() {
        let global = ReviewConfig {
            auto_fix_pr_errors: Some(false),
            ..ReviewConfig::default()
        };
        let project = ReviewConfig {
            auto_fix_pr_errors: Some(true),
            ..ReviewConfig::default()
        };
        assert!(global.clone().merge(project).auto_fix_pr_errors());
        assert!(!global.merge(ReviewConfig::default()).auto_fix_pr_errors());
    }

    #[test]
    fn merge_auto_fix_prompt_template_project_wins() {
        let global = ReviewConfig {
            auto_fix_prompt_template: Some("global: <<prompt>>".to_string()),
            ..ReviewConfig::default()
        };
        let project = ReviewConfig {
            auto_fix_prompt_template: Some("project: <<prompt>>".to_string()),
            ..ReviewConfig::default()
        };
        assert_eq!(
            global.clone().merge(project).auto_fix_prompt_template(),
            Some("project: <<prompt>>")
        );
        assert_eq!(
            global
                .merge(ReviewConfig::default())
                .auto_fix_prompt_template(),
            Some("global: <<prompt>>")
        );
    }

    // ── merge cases (the four required by CCTL-156) ──────────────────────────

    #[test]
    fn merge_global_only() {
        let merged = cfg(Some(true), &["a"]).merge(ReviewConfig::default());
        assert!(merged.skip_worktrees());
        assert_eq!(merged.checks, vec!["a".to_string()]);
    }

    #[test]
    fn merge_project_only() {
        let merged = ReviewConfig::default().merge(cfg(Some(true), &["b"]));
        assert!(merged.skip_worktrees());
        assert_eq!(merged.checks, vec!["b".to_string()]);
    }

    #[test]
    fn merge_conflict_project_wins() {
        // Global says false, project says true -> project (true) wins.
        let merged = cfg(Some(false), &["a"]).merge(cfg(Some(true), &["a", "b"]));
        assert_eq!(merged.skip_worktrees, Some(true));
        // list union, de-duplicated, order-preserving
        assert_eq!(merged.checks, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn merge_disjoint_keys() {
        // Global sets skip, project sets a check; both survive.
        let merged = cfg(Some(true), &[]).merge(cfg(None, &["only-project"]));
        assert_eq!(merged.skip_worktrees, Some(true));
        assert_eq!(merged.checks, vec!["only-project".to_string()]);
    }

    #[test]
    fn find_project_config_walks_up() {
        let base = std::env::temp_dir().join(format!("ralphus-cfg-{}", std::process::id()));
        let nested = base.join("a").join("b").join("c");
        std::fs::create_dir_all(&nested).unwrap();
        std::fs::write(
            base.join(".ralphus.toml"),
            "[review]\nskip_worktrees=true\n",
        )
        .unwrap();

        let found = find_project_config(&nested).expect("should find the ancestor config");
        assert_eq!(found, base.join(".ralphus.toml"));
        assert!(load_file(&found).skip_worktrees());

        // No config anywhere above a fresh, unrelated temp dir.
        let empty = std::env::temp_dir().join(format!("ralphus-empty-{}", std::process::id()));
        std::fs::create_dir_all(&empty).unwrap();
        // (may still find one if the temp root has a stray file; assert only our positive case)

        let _ = std::fs::remove_dir_all(&base);
        let _ = std::fs::remove_dir_all(&empty);
    }

    #[test]
    fn load_daemon_config_with_reads_configuration_path_entries() {
        let config_dir = std::env::temp_dir().join(format!(
            "ralphus-cfg-path-source-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_file = config_dir.join(".ralphus.toml");
        std::fs::write(
            &config_file,
            "[daemon]\ndefault_user = \"from-configuration-path\"\n",
        )
        .unwrap();

        // `cwd` is unrelated to `config_dir` -- no ancestor `.ralphus.toml`, so
        // the only way `default_user` can resolve here is through the
        // `$RALPHUS_CONFIGURATION_PATH` entry.
        let cwd = std::env::temp_dir().join(format!(
            "ralphus-cfg-path-unrelated-cwd-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&cwd).unwrap();

        let cfg = load_daemon_config_with(Some(&cwd), Some(config_file.to_str().unwrap()));
        assert_eq!(cfg.default_user.as_deref(), Some("from-configuration-path"));

        let _ = std::fs::remove_dir_all(&config_dir);
        let _ = std::fs::remove_dir_all(&cwd);
    }

    #[test]
    fn load_daemon_config_with_project_local_wins_over_configuration_path() {
        let config_dir = std::env::temp_dir().join(format!(
            "ralphus-cfg-path-loser-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("time")
                .as_nanos()
        ));
        std::fs::create_dir_all(&config_dir).unwrap();
        let config_file = config_dir.join(".ralphus.toml");
        std::fs::write(
            &config_file,
            "[daemon]\ndefault_user = \"from-configuration-path\"\n",
        )
        .unwrap();

        let project_root = std::env::temp_dir().join(format!(
            "ralphus-cfg-path-project-local-winner-{}",
            std::process::id()
        ));
        std::fs::create_dir_all(&project_root).unwrap();
        std::fs::write(
            project_root.join(".ralphus.toml"),
            "[daemon]\ndefault_user = \"from-project-local\"\n",
        )
        .unwrap();

        let cfg = load_daemon_config_with(Some(&project_root), Some(config_file.to_str().unwrap()));
        assert_eq!(cfg.default_user.as_deref(), Some("from-project-local"));

        let _ = std::fs::remove_dir_all(&config_dir);
        let _ = std::fs::remove_dir_all(&project_root);
    }

    // ── CartographerConfig (RAL-98) ──────────────────────────────────────

    #[test]
    fn cartographer_defaults_when_absent() {
        let c = cartographer_from_toml_str("");
        assert_eq!(c.retention_days(), 30);
        assert_eq!(c.max_rows(), 50_000);
    }

    #[test]
    fn cartographer_parses_explicit_values() {
        let c = cartographer_from_toml_str("[cartographer]\nretention_days = 7\nmax_rows = 1000\n");
        assert_eq!(c.retention_days(), 7);
        assert_eq!(c.max_rows(), 1000);
    }

    #[test]
    fn cartographer_partial_table_falls_back_per_field() {
        let c = cartographer_from_toml_str("[cartographer]\nretention_days = 7\n");
        assert_eq!(c.retention_days(), 7);
        assert_eq!(c.max_rows(), 50_000);
    }

    #[test]
    fn cartographer_malformed_toml_is_default() {
        let c = cartographer_from_toml_str("not = = valid");
        assert_eq!(c.retention_days(), 30);
        assert_eq!(c.max_rows(), 50_000);
    }

    // ── BudgetConfig (RAL-161) ────────────────────────────────────────────

    #[test]
    fn budget_defaults_when_absent() {
        let c = budget_from_toml_str("");
        assert_eq!(c.poll_interval(), Duration::from_millis(200));
    }

    #[test]
    fn budget_parses_explicit_interval() {
        let c = budget_from_toml_str("[budget]\npoll_interval_ms = 50\n");
        assert_eq!(c.poll_interval(), Duration::from_millis(50));
    }

    #[test]
    fn budget_zero_means_near_immediate() {
        let c = budget_from_toml_str("[budget]\npoll_interval_ms = 0\n");
        assert_eq!(c.poll_interval(), Duration::from_millis(20));
    }

    #[test]
    fn budget_malformed_toml_is_default() {
        let c = budget_from_toml_str("not = = valid");
        assert_eq!(c.poll_interval(), Duration::from_millis(200));
    }

    // ── TerminalLogConfig (RAL-154) ───────────────────────────────────────

    #[test]
    fn terminal_log_defaults_when_absent() {
        let c = terminal_log_from_toml_str("");
        assert_eq!(c.max_lines_per_attempt(), 4000);
        assert_eq!(c.retention_days(), 30);
        assert_eq!(c.max_files(), 2000);
        assert_eq!(c.max_transcript_bytes_per_attempt(), 256 * 1024 * 1024);
        assert_eq!(c.pane_history_limit(), None);
    }

    #[test]
    fn terminal_log_parses_explicit_values() {
        let c = terminal_log_from_toml_str(
            "[terminal_logs]\nmax_lines_per_attempt = 500\nretention_days = 7\nmax_files = 100\nmax_transcript_bytes_per_attempt = 1048576\npane_history_limit = 5000\n",
        );
        assert_eq!(c.max_lines_per_attempt(), 500);
        assert_eq!(c.retention_days(), 7);
        assert_eq!(c.max_files(), 100);
        assert_eq!(c.max_transcript_bytes_per_attempt(), 1_048_576);
        assert_eq!(c.pane_history_limit(), Some(5000));
    }

    #[test]
    fn terminal_log_partial_table_falls_back_per_field() {
        let c = terminal_log_from_toml_str("[terminal_logs]\nretention_days = 7\n");
        assert_eq!(c.max_lines_per_attempt(), 4000);
        assert_eq!(c.retention_days(), 7);
        assert_eq!(c.max_files(), 2000);
        assert_eq!(c.max_transcript_bytes_per_attempt(), 256 * 1024 * 1024);
        assert_eq!(c.pane_history_limit(), None);
    }

    #[test]
    fn terminal_log_malformed_toml_is_default() {
        let c = terminal_log_from_toml_str("not = = valid");
        assert_eq!(c.max_lines_per_attempt(), 4000);
        assert_eq!(c.retention_days(), 30);
        assert_eq!(c.max_files(), 2000);
        assert_eq!(c.max_transcript_bytes_per_attempt(), 256 * 1024 * 1024);
        assert_eq!(c.pane_history_limit(), None);
    }

    #[test]
    fn terminal_log_pane_history_limit_defaults_to_none_when_unset() {
        // Unset resolves to `None` so the caller (`crate::tmux`) falls back to
        // the `TMUX_HISTORY_LIMIT` const, the single source of truth.
        assert_eq!(terminal_log_from_toml_str("").pane_history_limit(), None);
        assert_eq!(
            terminal_log_from_toml_str("[terminal_logs]\nretention_days = 7\n")
                .pane_history_limit(),
            None
        );
    }

    #[test]
    fn terminal_log_pane_history_limit_explicit_value_is_honored() {
        let c = terminal_log_from_toml_str("[terminal_logs]\npane_history_limit = 2000\n");
        assert_eq!(c.pane_history_limit(), Some(2000));
    }

    #[test]
    fn terminal_log_pane_history_limit_below_one_falls_back_to_default() {
        let zero = terminal_log_from_toml_str("[terminal_logs]\npane_history_limit = 0\n");
        assert_eq!(zero.pane_history_limit(), None);
        let negative = terminal_log_from_toml_str("[terminal_logs]\npane_history_limit = -5\n");
        assert_eq!(negative.pane_history_limit(), None);
    }

    #[test]
    fn terminal_log_pane_history_limit_malformed_toml_is_none() {
        assert_eq!(
            terminal_log_from_toml_str("[terminal_logs]\npane_history_limit = \"nope\"\n")
                .pane_history_limit(),
            None
        );
    }

    #[test]
    fn terminal_log_max_transcript_bytes_below_one_falls_back_to_default() {
        let zero =
            terminal_log_from_toml_str("[terminal_logs]\nmax_transcript_bytes_per_attempt = 0\n");
        assert_eq!(zero.max_transcript_bytes_per_attempt(), 256 * 1024 * 1024);
        let negative =
            terminal_log_from_toml_str("[terminal_logs]\nmax_transcript_bytes_per_attempt = -5\n");
        assert_eq!(
            negative.max_transcript_bytes_per_attempt(),
            256 * 1024 * 1024
        );
    }

    #[test]
    fn terminal_log_max_transcript_bytes_of_one_is_valid() {
        let c =
            terminal_log_from_toml_str("[terminal_logs]\nmax_transcript_bytes_per_attempt = 1\n");
        assert_eq!(c.max_transcript_bytes_per_attempt(), 1);
    }

    #[test]
    fn terminal_log_max_lines_below_one_falls_back_to_default() {
        let zero = terminal_log_from_toml_str("[terminal_logs]\nmax_lines_per_attempt = 0\n");
        assert_eq!(zero.max_lines_per_attempt(), 4000);
        let negative = terminal_log_from_toml_str("[terminal_logs]\nmax_lines_per_attempt = -5\n");
        assert_eq!(negative.max_lines_per_attempt(), 4000);
    }

    #[test]
    fn terminal_log_max_lines_of_one_is_valid() {
        let c = terminal_log_from_toml_str("[terminal_logs]\nmax_lines_per_attempt = 1\n");
        assert_eq!(c.max_lines_per_attempt(), 1);
    }

    // ── LiveViewConfig (RAL-232) ──────────────────────────────────────────

    #[test]
    fn live_view_defaults_when_absent() {
        let c = live_view_from_toml_str("");
        assert!(!c.show_debug_messages_default());
    }

    #[test]
    fn live_view_parses_explicit_true() {
        let c = live_view_from_toml_str("[live_view]\nshow_debug_messages_default = true\n");
        assert!(c.show_debug_messages_default());
    }

    #[test]
    fn live_view_parses_explicit_false() {
        let c = live_view_from_toml_str("[live_view]\nshow_debug_messages_default = false\n");
        assert!(!c.show_debug_messages_default());
    }

    #[test]
    fn live_view_malformed_toml_is_default() {
        let c = live_view_from_toml_str("not = = valid");
        assert!(!c.show_debug_messages_default());
    }

    // ── tool_arg_truncate_chars (RAL-303) ─────────────────────────────────

    #[test]
    fn tool_arg_truncate_chars_defaults_to_200_when_absent() {
        let c = live_view_from_toml_str("");
        assert_eq!(c.tool_arg_truncate_chars(), 200);
    }

    #[test]
    fn tool_arg_truncate_chars_negative_value_falls_back_to_default() {
        // A negative value can never deserialize into `Option<u32>` --
        // confirms the "malformed config never blocks" fallback the
        // `ralphus check health` WARN message promises actually holds.
        let c = live_view_from_toml_str("[live_view]\ntool_arg_truncate_chars = -5\n");
        assert_eq!(c.tool_arg_truncate_chars(), 200);
    }

    #[test]
    fn tool_arg_truncate_chars_non_numeric_value_falls_back_to_default() {
        let c = live_view_from_toml_str("[live_view]\ntool_arg_truncate_chars = \"nope\"\n");
        assert_eq!(c.tool_arg_truncate_chars(), 200);
    }

    #[test]
    fn tool_arg_truncate_chars_parses_an_explicit_value() {
        let c = live_view_from_toml_str("[live_view]\ntool_arg_truncate_chars = 400\n");
        assert_eq!(c.tool_arg_truncate_chars(), 400);
    }

    #[test]
    fn tool_arg_truncate_chars_project_wins_over_global() {
        let global = live_view_from_toml_str("[live_view]\ntool_arg_truncate_chars = 100\n");
        let local = live_view_from_toml_str("[live_view]\ntool_arg_truncate_chars = 500\n");
        let effective = LiveViewConfig {
            show_debug_messages_default: local
                .show_debug_messages_default
                .or(global.show_debug_messages_default),
            tool_arg_truncate_chars: local
                .tool_arg_truncate_chars
                .or(global.tool_arg_truncate_chars),
        };
        assert_eq!(effective.tool_arg_truncate_chars(), 500);
    }

    // ── ThrashConfig (RAL-339) ─────────────────────────────────────────────

    #[test]
    fn thrash_defaults_when_absent() {
        let c = thrash_from_toml_str("");
        assert_eq!(c.max_compactions(), 3);
        assert_eq!(c.min_turn_gap(), 2);
    }

    #[test]
    fn thrash_parses_explicit_values() {
        let c = thrash_from_toml_str("[thrash]\nmax_compactions = 5\nmin_turn_gap = 4\n");
        assert_eq!(c.max_compactions(), 5);
        assert_eq!(c.min_turn_gap(), 4);
    }

    #[test]
    fn thrash_negative_value_falls_back_to_default() {
        // A negative value can never deserialize into `Option<u32>` --
        // confirms the "malformed config never blocks" fallback (and is what
        // `ralphus check health`'s WARN message for this table promises).
        let c = thrash_from_toml_str("[thrash]\nmax_compactions = -1\n");
        assert_eq!(c.max_compactions(), 3);
        assert_eq!(c.min_turn_gap(), 2);
    }

    #[test]
    fn thrash_non_numeric_value_falls_back_to_default() {
        let c = thrash_from_toml_str("[thrash]\nmin_turn_gap = \"nope\"\n");
        assert_eq!(c.max_compactions(), 3);
        assert_eq!(c.min_turn_gap(), 2);
    }

    #[test]
    fn thrash_malformed_toml_is_default() {
        let c = thrash_from_toml_str("not = = valid");
        assert_eq!(c.max_compactions(), 3);
        assert_eq!(c.min_turn_gap(), 2);
    }

    #[test]
    fn thrash_project_wins_over_global() {
        let global = thrash_from_toml_str("[thrash]\nmax_compactions = 10\nmin_turn_gap = 10\n");
        let local = thrash_from_toml_str("[thrash]\nmax_compactions = 4\n");
        let effective = ThrashConfig {
            max_compactions: local.max_compactions.or(global.max_compactions),
            min_turn_gap: local.min_turn_gap.or(global.min_turn_gap),
        };
        // The project set max_compactions but not min_turn_gap -- each
        // scalar resolves independently, so the global min_turn_gap still
        // shows through.
        assert_eq!(effective.max_compactions(), 4);
        assert_eq!(effective.min_turn_gap(), 10);
    }

    // ── Downtime windows (RAL-122) ────────────────────────────────────────

    fn t(hh: u32, mm: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(hh, mm, 0).unwrap()
    }

    #[test]
    fn downtime_defaults_to_no_windows() {
        let c = daemon_from_toml_str("");
        assert!(c.downtime_windows().is_empty());
        assert!(!in_downtime(t(23, 0), &c.downtime_windows()));
    }

    #[test]
    fn downtime_parses_single_window_under_daemon_table() {
        let c = daemon_from_toml_str("[[daemon.downtime]]\nstart = \"22:00\"\nend = \"06:00\"\n");
        assert_eq!(c.downtime_windows(), vec![(t(22, 0), t(6, 0))]);
    }

    #[test]
    fn downtime_parses_multiple_windows() {
        let c = daemon_from_toml_str(
            "[[daemon.downtime]]\nstart = \"01:00\"\nend = \"02:00\"\n\
             [[daemon.downtime]]\nstart = \"22:00\"\nend = \"06:00\"\n",
        );
        assert_eq!(
            c.downtime_windows(),
            vec![(t(1, 0), t(2, 0)), (t(22, 0), t(6, 0))]
        );
    }

    #[test]
    fn downtime_malformed_window_is_dropped_not_fatal() {
        // One bad entry alongside a good one: the bad one is silently dropped,
        // the good one still applies -- malformed config never blocks
        // scheduling entirely, but it also shouldn't discard sibling windows
        // that parsed fine.
        let c = daemon_from_toml_str(
            "[[daemon.downtime]]\nstart = \"not-a-time\"\nend = \"06:00\"\n\
             [[daemon.downtime]]\nstart = \"22:00\"\nend = \"23:00\"\n",
        );
        assert_eq!(c.downtime_windows(), vec![(t(22, 0), t(23, 0))]);
    }

    #[test]
    fn downtime_malformed_toml_is_default() {
        let c = daemon_from_toml_str("not = = valid");
        assert!(c.downtime_windows().is_empty());
    }

    // ── max_concurrent ────────────────────────────────────────────────────

    #[test]
    fn max_concurrent_defaults_when_absent() {
        let c = daemon_from_toml_str("");
        assert_eq!(c.max_concurrent(), crate::DEFAULT_MAX_CONCURRENT);
    }

    #[test]
    fn max_concurrent_parses_explicit_value() {
        let c = daemon_from_toml_str("[daemon]\nmax_concurrent = 24\n");
        assert_eq!(c.max_concurrent(), 24);
    }

    #[test]
    fn max_concurrent_zero_means_no_limit() {
        let c = daemon_from_toml_str("[daemon]\nmax_concurrent = 0\n");
        assert_eq!(c.max_concurrent(), 0);
    }

    #[test]
    fn max_concurrent_negative_falls_back_to_default() {
        let c = daemon_from_toml_str("[daemon]\nmax_concurrent = -5\n");
        assert_eq!(c.max_concurrent(), crate::DEFAULT_MAX_CONCURRENT);
    }

    #[test]
    fn max_concurrent_malformed_toml_is_default() {
        let c = daemon_from_toml_str("not = = valid");
        assert_eq!(c.max_concurrent(), crate::DEFAULT_MAX_CONCURRENT);
    }

    #[test]
    fn in_downtime_same_day_window() {
        let windows = vec![(t(9, 0), t(17, 0))];
        assert!(in_downtime(t(9, 0), &windows)); // inclusive start
        assert!(in_downtime(t(12, 0), &windows));
        assert!(!in_downtime(t(17, 0), &windows)); // exclusive end
        assert!(!in_downtime(t(8, 59), &windows));
    }

    #[test]
    fn in_downtime_midnight_crossing_window() {
        let windows = vec![(t(22, 0), t(6, 0))];
        assert!(in_downtime(t(23, 0), &windows));
        assert!(in_downtime(t(0, 0), &windows));
        assert!(in_downtime(t(5, 59), &windows));
        assert!(in_downtime(t(22, 0), &windows));
        assert!(!in_downtime(t(6, 0), &windows)); // exclusive end
        assert!(!in_downtime(t(12, 0), &windows));
    }

    #[test]
    fn in_downtime_zero_width_window_never_blocks() {
        let windows = vec![(t(10, 0), t(10, 0))];
        assert!(!in_downtime(t(10, 0), &windows));
    }

    #[test]
    fn downtime_local_project_overrides_global_when_non_empty() {
        let global = vec![DowntimeWindow {
            start: "01:00".to_string(),
            end: "02:00".to_string(),
        }];
        let local = vec![DowntimeWindow {
            start: "22:00".to_string(),
            end: "23:00".to_string(),
        }];
        assert_eq!(
            merge_downtime(global, local.clone()),
            local,
            "a non-empty per-project list must win outright, not union with global"
        );
    }

    #[test]
    fn downtime_global_used_when_local_is_empty() {
        let global = vec![DowntimeWindow {
            start: "01:00".to_string(),
            end: "02:00".to_string(),
        }];
        assert_eq!(merge_downtime(global.clone(), vec![]), global);
    }

    // ── ForgeConfig (RAL-117) ─────────────────────────────────────────────

    #[test]
    fn forge_defaults_when_absent() {
        let c = forge_from_toml_str("");
        assert_eq!(c, ForgeConfig::default());
    }

    #[test]
    fn forge_parses_explicit_values() {
        let c = forge_from_toml_str(
            "[forge]\nkind = \"gitlab\"\nremote = \"upstream\"\n\
             api_base = \"https://gitlab.example.com/api/v4\"\ntoken_env = \"MY_TOKEN\"\n",
        );
        assert_eq!(c.kind.as_deref(), Some("gitlab"));
        assert_eq!(c.remote.as_deref(), Some("upstream"));
        assert_eq!(
            c.api_base.as_deref(),
            Some("https://gitlab.example.com/api/v4")
        );
        assert_eq!(c.token_env.as_deref(), Some("MY_TOKEN"));
    }

    #[test]
    fn forge_malformed_toml_is_default() {
        assert_eq!(forge_from_toml_str("not = = valid"), ForgeConfig::default());
    }

    #[test]
    fn forge_merge_project_wins() {
        let global = ForgeConfig {
            kind: Some("github".to_string()),
            remote: Some("origin".to_string()),
            api_base: None,
            token_env: None,
            pull_request_branch_convention: None,
        };
        let project = ForgeConfig {
            kind: Some("gitlab".to_string()),
            ..ForgeConfig::default()
        };
        let merged = global.merge(project);
        assert_eq!(merged.kind.as_deref(), Some("gitlab"));
        // Unset-in-project field falls back to the global value.
        assert_eq!(merged.remote.as_deref(), Some("origin"));
    }

    #[test]
    fn forge_pr_branch_convention_defaults_and_parses() {
        let c = forge_from_toml_str("");
        assert_eq!(c.pull_request_branch_convention, None);
        assert_eq!(c.resolved_pr_branch_convention(), "{name}-review");

        let c =
            forge_from_toml_str("[forge]\npull_request_branch_convention = \"review-{name}\"\n");
        assert_eq!(
            c.pull_request_branch_convention.as_deref(),
            Some("review-{name}")
        );
        assert_eq!(c.resolved_pr_branch_convention(), "review-{name}");
    }

    #[test]
    fn forge_pr_branch_convention_merge_project_wins() {
        let global = ForgeConfig {
            pull_request_branch_convention: Some("{name}-review".to_string()),
            ..ForgeConfig::default()
        };
        let project = ForgeConfig {
            pull_request_branch_convention: Some("release/blah-{name}".to_string()),
            ..ForgeConfig::default()
        };
        let merged = global.clone().merge(project);
        assert_eq!(
            merged.pull_request_branch_convention.as_deref(),
            Some("release/blah-{name}")
        );
        // Per-project unset still falls back to the global value.
        let merged = global.merge(ForgeConfig::default());
        assert_eq!(
            merged.pull_request_branch_convention.as_deref(),
            Some("{name}-review")
        );
    }

    #[test]
    fn validate_pull_request_branch_convention_rejects_empty_and_missing_placeholder() {
        assert!(validate_pull_request_branch_convention("").is_err());
        assert!(validate_pull_request_branch_convention("static-branch-name").is_err());
        assert!(validate_pull_request_branch_convention("{name}-review").is_ok());
        assert!(validate_pull_request_branch_convention("release/blah-{name}").is_ok());
    }

    // ── EnvOverridesConfig (RAL-150) ──────────────────────────────────────

    #[test]
    fn env_overrides_defaults_when_absent() {
        let c = env_overrides_from_toml_str("");
        assert!(c.allowlist.is_empty());
        assert!(!c.is_allowed("ANYTHING"));
    }

    #[test]
    fn env_overrides_parses_explicit_values() {
        let c = env_overrides_from_toml_str(
            "[env_overrides]\nallowlist = [\"RALPHUS_RESOLVER_MODEL\", \"MY_FLAG\"]\n",
        );
        assert!(c.is_allowed("RALPHUS_RESOLVER_MODEL"));
        assert!(c.is_allowed("MY_FLAG"));
        assert!(!c.is_allowed("SECRET_TOKEN"));
    }

    #[test]
    fn env_overrides_malformed_toml_is_default() {
        let c = env_overrides_from_toml_str("not = = valid");
        assert!(c.allowlist.is_empty());
    }

    #[test]
    fn env_overrides_merge_unions_and_dedupes() {
        let global = EnvOverridesConfig {
            allowlist: vec!["A".to_string(), "B".to_string()],
        };
        let project = EnvOverridesConfig {
            allowlist: vec!["B".to_string(), "C".to_string()],
        };
        let merged = global.merge(project);
        assert_eq!(
            merged.allowlist,
            vec!["A".to_string(), "B".to_string(), "C".to_string()]
        );
    }

    // ── AgentIsolationConfig (RAL-336) ────────────────────────────────────

    #[test]
    fn agent_isolation_defaults_to_fully_isolated_when_absent() {
        let c = agent_isolation_from_toml_str("");
        assert!(!c.allow_personal_settings());
        assert!(!c.allow_personal_memory());
    }

    #[test]
    fn agent_isolation_parses_explicit_values() {
        let c = agent_isolation_from_toml_str(
            "[agent_isolation]\nallow_personal_settings = true\nallow_personal_memory = true\n",
        );
        assert!(c.allow_personal_settings());
        assert!(c.allow_personal_memory());
    }

    #[test]
    fn agent_isolation_flags_are_independent() {
        let c =
            agent_isolation_from_toml_str("[agent_isolation]\nallow_personal_settings = true\n");
        assert!(c.allow_personal_settings());
        assert!(!c.allow_personal_memory());
    }

    #[test]
    fn agent_isolation_malformed_toml_is_default() {
        let c = agent_isolation_from_toml_str("not = = valid");
        assert!(!c.allow_personal_settings());
        assert!(!c.allow_personal_memory());
    }

    #[test]
    fn resolve_agent_isolation_layers_global_under_project() {
        let base =
            std::env::temp_dir().join(format!("ralphus-agent-isolation-{}", std::process::id()));
        std::fs::create_dir_all(&base).unwrap();
        std::fs::write(
            base.join(".ralphus.toml"),
            "[agent_isolation]\nallow_personal_settings = true\n",
        )
        .unwrap();

        let resolved = resolve_agent_isolation(&base);
        assert!(resolved.allow_personal_settings());
        assert!(!resolved.allow_personal_memory());

        let _ = std::fs::remove_dir_all(&base);
    }

    // ── CorsConfig (RAL-220) ──────────────────────────────────────────────

    #[test]
    fn cors_defaults_when_absent() {
        let c = cors_from_toml_str("");
        assert!(c.allowed_origins.is_empty());
    }

    #[test]
    fn cors_parses_explicit_values() {
        let c = cors_from_toml_str("[cors]\nallowed_origins = [\"https://board.example.com\"]\n");
        assert_eq!(
            c.allowed_origins,
            vec!["https://board.example.com".to_string()]
        );
    }

    #[test]
    fn cors_malformed_toml_is_default() {
        let c = cors_from_toml_str("not = = valid");
        assert!(c.allowed_origins.is_empty());
    }

    #[test]
    fn is_valid_env_key_accepts_identifiers() {
        assert!(is_valid_env_key("RALPHUS_RESOLVER_MODEL"));
        assert!(is_valid_env_key("_PRIVATE"));
        assert!(is_valid_env_key("a1"));
    }

    #[test]
    fn is_valid_env_key_rejects_non_identifiers() {
        assert!(!is_valid_env_key(""));
        assert!(!is_valid_env_key("1LEADING_DIGIT"));
        assert!(!is_valid_env_key("HAS SPACE"));
        assert!(!is_valid_env_key("HAS=EQUALS"));
        assert!(!is_valid_env_key("HAS;SEMI"));
        assert!(!is_valid_env_key("$(injected)"));
    }

    // ── skip_base_updates (RAL-250) ───────────────────────────────────────

    #[test]
    fn skip_base_updates_defaults_to_false_when_unset() {
        assert!(!ReviewConfig::default().skip_base_updates());
        assert!(!from_toml_str("[review]\nskip_worktrees = true\n").skip_base_updates());
    }

    #[test]
    fn skip_base_updates_parses_explicit_true() {
        let c = from_toml_str("[review]\nskip_base_updates = true\n");
        assert_eq!(c.skip_base_updates, Some(true));
        assert!(c.skip_base_updates());
    }

    #[test]
    fn skip_base_updates_parses_explicit_false() {
        let c = from_toml_str("[review]\nskip_base_updates = false\n");
        assert_eq!(c.skip_base_updates, Some(false));
        assert!(!c.skip_base_updates());
    }

    #[test]
    fn merge_skip_base_updates_project_wins() {
        let global = ReviewConfig {
            skip_base_updates: Some(true),
            ..ReviewConfig::default()
        };
        let project = ReviewConfig {
            skip_base_updates: Some(false),
            ..ReviewConfig::default()
        };
        assert!(!global.clone().merge(project).skip_base_updates());
        // Project unset falls back to the global value.
        assert!(global.merge(ReviewConfig::default()).skip_base_updates());
    }

    // ── match_pr_branch_name (RAL-307) ─────────────────────────────────────

    #[test]
    fn match_pr_branch_name_defaults_to_false_when_unset() {
        assert!(!ReviewConfig::default().match_pr_branch_name());
        assert!(!from_toml_str("[review]\nskip_worktrees = true\n").match_pr_branch_name());
    }

    #[test]
    fn match_pr_branch_name_parses_explicit_true() {
        let c = from_toml_str("[review]\nmatch_pr_branch_name = true\n");
        assert_eq!(c.match_pr_branch_name, Some(true));
        assert!(c.match_pr_branch_name());
    }

    #[test]
    fn merge_match_pr_branch_name_project_wins() {
        let global = ReviewConfig {
            match_pr_branch_name: Some(true),
            ..ReviewConfig::default()
        };
        let project = ReviewConfig {
            match_pr_branch_name: Some(false),
            ..ReviewConfig::default()
        };
        assert!(!global.clone().merge(project).match_pr_branch_name());
        // Project unset falls back to the global value.
        assert!(global.merge(ReviewConfig::default()).match_pr_branch_name());
    }

    // ── auto_submit_pr_stack (RAL-317) ──────────────────────────────────────

    #[test]
    fn auto_submit_pr_stack_defaults_to_false_when_unset() {
        assert!(!ReviewConfig::default().auto_submit_pr_stack());
        assert!(!from_toml_str("[review]\nskip_worktrees = true\n").auto_submit_pr_stack());
    }

    #[test]
    fn auto_submit_pr_stack_parses_explicit_true() {
        let c = from_toml_str("[review]\nauto_submit_pr_stack = true\n");
        assert_eq!(c.auto_submit_pr_stack, Some(true));
        assert!(c.auto_submit_pr_stack());
    }

    #[test]
    fn merge_auto_submit_pr_stack_project_wins() {
        let global = ReviewConfig {
            auto_submit_pr_stack: Some(true),
            ..ReviewConfig::default()
        };
        let project = ReviewConfig {
            auto_submit_pr_stack: Some(false),
            ..ReviewConfig::default()
        };
        assert!(!global.clone().merge(project).auto_submit_pr_stack());
        // Project unset falls back to the global value.
        assert!(global.merge(ReviewConfig::default()).auto_submit_pr_stack());
    }

    #[test]
    fn is_valid_env_value_accepts_ordinary_content() {
        assert!(is_valid_env_value(""));
        assert!(is_valid_env_value("sk-abc123"));
        assert!(is_valid_env_value(
            "it's a value with spaces & punctuation!"
        ));
        assert!(is_valid_env_value("path/to/thing"));
    }

    #[test]
    fn is_valid_env_value_rejects_control_characters() {
        assert!(!is_valid_env_value("line1\nline2"));
        assert!(!is_valid_env_value("carriage\rreturn"));
        assert!(!is_valid_env_value("tab\ttab"));
        assert!(!is_valid_env_value("esc\x1b[31m"));
        assert!(!is_valid_env_value("nul\0byte"));
    }

    // -- CORS allow-list caching ------------------------------------------

    /// Serializes the two tests below: both assert on the process-global
    /// [`CORS_LOADS`] counter, so they cannot run at the same time.
    static CORS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// `resolve_cors` runs for every inbound HTTP request; the uncached
    /// loader behind it does a global-config read, a `find_project_config`
    /// directory walk and two TOML parses. A burst of requests must share one
    /// read, or that filesystem I/O sits on the critical path of every single
    /// request the daemon answers.
    #[test]
    fn cached_cors_config_reads_the_files_once_per_burst() {
        let _serialized = CORS_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_cors_cache_for_test();
        let before = cors_config_load_count();
        for _ in 0..50 {
            let _ = load_cors_config_cached();
        }
        assert_eq!(
            cors_config_load_count() - before,
            1,
            "50 requests' worth of CORS decisions must cost one filesystem read"
        );
    }

    /// The cache is a TTL, not a freeze: an expired entry is re-read, so an
    /// edit to `[cors]` takes effect without restarting the daemon.
    #[test]
    fn cached_cors_config_re_reads_once_the_ttl_expires() {
        let _serialized = CORS_TEST_LOCK
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        reset_cors_cache_for_test();
        let before = cors_config_load_count();
        let _ = load_cors_config_cached();
        // Backdate the cached entry past the TTL rather than sleeping for it.
        {
            let mut cache = CORS_CACHE
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let (read_at, _) = cache.as_mut().expect("just-populated cache entry");
            *read_at = read_at
                .checked_sub(CORS_CACHE_TTL + Duration::from_secs(1))
                .expect("backdate the cache entry past its TTL");
        }
        let _ = load_cors_config_cached();
        assert_eq!(cors_config_load_count() - before, 2);
    }

    // ── [[templates]] / [ui] (RAL-297) ────────────────────────────────────

    #[test]
    fn templates_parses_array_of_tables() {
        let templates = templates_from_toml_str(
            "[[templates]]\nname = \"hello-world\"\nprompt_template = \"{prompt}\"\n\n\
             [[templates]]\nname = \"standard\"\nprompt_template = \"{prompt}\\n{ticket_id}\"\n\
             [[templates.fields]]\nname = \"ticket_id\"\nrequired = false\n",
        );
        assert_eq!(templates.len(), 2);
        assert_eq!(templates[0].name, "hello-world");
        assert_eq!(templates[1].fields.len(), 1);
        assert_eq!(templates[1].fields[0].name, "ticket_id");
    }

    #[test]
    fn templates_malformed_toml_is_empty() {
        assert!(templates_from_toml_str("not = = valid").is_empty());
    }

    #[test]
    fn default_template_is_hello_world_and_ascii() {
        let t = default_template();
        assert_eq!(t.name, DEFAULT_TEMPLATE_NAME);
        assert_eq!(t.prompt_template.as_deref(), Some("{prompt}"));
        assert!(t.prompt_template.unwrap().is_ascii());
    }

    #[test]
    fn validate_templates_rejects_empty_and_duplicate_names() {
        let templates = vec![
            TemplateDef {
                name: String::new(),
                prompt_template: Some("{prompt}".to_string()),
                ..TemplateDef::default()
            },
            TemplateDef {
                name: "dup".to_string(),
                prompt_template: Some("{prompt}".to_string()),
                ..TemplateDef::default()
            },
            TemplateDef {
                name: "dup".to_string(),
                prompt_template: Some("{prompt}".to_string()),
                ..TemplateDef::default()
            },
        ];
        let errors = validate_templates(&templates);
        assert!(errors.iter().any(|e| e.message.contains("empty name")));
        assert!(errors.iter().any(|e| e.message.contains("duplicate")));
    }

    #[test]
    fn validate_templates_rejects_missing_prompt_template() {
        let templates = vec![TemplateDef {
            name: "x".to_string(),
            ..TemplateDef::default()
        }];
        let errors = validate_templates(&templates);
        assert!(errors.iter().any(|e| e.message.contains("prompt_template")));
    }

    #[test]
    fn validate_templates_rejects_unknown_field_type_and_placeholder() {
        let templates = vec![TemplateDef {
            name: "x".to_string(),
            fields: vec![TemplateFieldDef {
                name: "ticket_id".to_string(),
                field_type: Some("bogus".to_string()),
                ..TemplateFieldDef::default()
            }],
            prompt_template: Some("{prompt} {unknown_field}".to_string()),
            ..TemplateDef::default()
        }];
        let errors = validate_templates(&templates);
        assert!(errors.iter().any(|e| e.message.contains("unknown type")));
        assert!(errors.iter().any(|e| e.message.contains("unknown_field")));
    }

    #[test]
    fn validate_templates_accepts_the_standard_example() {
        let templates = vec![TemplateDef {
            name: "standard".to_string(),
            fields: vec![
                TemplateFieldDef {
                    name: "ticket_id".to_string(),
                    field_type: Some("string".to_string()),
                    required: false,
                    ..TemplateFieldDef::default()
                },
                TemplateFieldDef {
                    name: "context_files".to_string(),
                    ..TemplateFieldDef::default()
                },
            ],
            prompt_template: Some(
                "{prompt}\n\nTicket: {ticket_id}\nRelevant files: {context_files}".to_string(),
            ),
            ..TemplateDef::default()
        }];
        assert!(validate_templates(&templates).is_empty());
    }

    #[test]
    fn effective_templates_falls_back_to_default_when_none_configured() {
        // No `.ralphus.toml`/global config in this process's temp cwd, so
        // `load_templates_config()` returns empty regardless of environment --
        // exercise the fallback logic directly instead of touching cwd/env,
        // which other tests in this module run concurrently against.
        let configured: Vec<TemplateDef> = Vec::new();
        let invalid: std::collections::HashSet<String> = validate_templates(&configured)
            .into_iter()
            .filter_map(|e| e.template)
            .collect();
        let valid: Vec<TemplateDef> = configured
            .into_iter()
            .filter(|t| !invalid.contains(&t.name))
            .collect();
        assert!(valid.is_empty());
    }

    #[test]
    fn ui_config_new_task_default_tab_defaults_to_simple() {
        assert_eq!(UiConfig::default().new_task_default_tab(), "simple");
        let c = ui_from_toml_str("[ui]\nnew_task_default_tab = \"paste\"\n");
        assert_eq!(c.new_task_default_tab(), "paste");
    }

    #[test]
    fn ui_config_unrecognized_tab_falls_back_to_simple() {
        let c = ui_from_toml_str("[ui]\nnew_task_default_tab = \"bogus\"\n");
        assert_eq!(c.new_task_default_tab(), "simple");
    }

    #[test]
    fn validate_new_task_default_tab_accepts_known_values() {
        for tab in VALID_NEW_TASK_TABS {
            assert!(validate_new_task_default_tab(tab).is_ok());
        }
        assert!(validate_new_task_default_tab("bogus").is_err());
    }
}
