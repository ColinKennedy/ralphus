//! RAL-416: one shared catalog of every `ralphus check health` check, with a
//! stable `id`, its daemon/remote [`Applicability`], its [`CostTier`] (does it
//! run in the default/hourly-sweep path, or only on explicit opt-in?), its
//! [`RequirementLevel`] (must this be true for the system to work at all,
//! optional/best-effort, or only relevant as a fallback path?), how it's
//! actually invoked ([`Probe`]), and a short `impact` summary meant to agree
//! with `docs/dependencies.md`'s own prose for the same dependency.
//!
//! This crate (`ralphus-core`) is the one place `cli`, `daemon`, and `mcp` all
//! already depend on, so it's the natural home for a catalog every one of
//! them needs to agree on: `cli/src/health.rs`'s [`crate::CheckResult`]-
//! shaped local/daemon-delegated checks, `daemon/src/health_targets.rs`'s
//! per-remote-target checks, and any board/report surface built on top of
//! either.
//!
//! The catalog is deliberately data-only -- it does not itself run anything.
//! Each check's real implementation stays exactly where it already lives
//! (`cli/src/health.rs`, `daemon/src/health_targets.rs`); those call sites
//! reference a [`CatalogEntry`]'s `id` (and, in tests, its other fields) so
//! the catalog and the implementation can never silently drift apart -- see
//! each crate's own parity tests.

/// Which section of `check health`'s human-facing report a catalog entry
/// belongs to (`cli/src/health.rs`'s `CORE`/`HARNESS`/`MACHINE` constants --
/// duplicated here as string literals, not re-exported, so this
/// dependency-light crate never has to know about `cli`'s module layout;
/// each section's meaning is documented in full at `cli/src/health.rs`'s
/// module doc comment).
pub mod section {
    pub const CORE: &str = "core";
    pub const HARNESS: &str = "harness";
    pub const MACHINE: &str = "machine";
}

/// Does this check inspect the local/daemon machine, or a configured
/// `[machine.targets.*]` remote?
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Applicability {
    /// The daemon's own host (or, for CLI-local checks like `git`/`tmux`,
    /// whatever host the CLI process itself runs on -- normally the same
    /// machine).
    DaemonLocal,
    /// One configured `[machine.targets.*]` entry, probed over the target's
    /// machine provider (SSH today).
    Remote,
}

/// Does this check run by default (every plain `check health`, and the
/// daemon's hourly background sweep), or only when explicitly opted into
/// (a CLI flag, or a board "check now" action)? This is a cost/consent
/// distinction, not a correctness one -- some `OnDemand` checks (the
/// Arbiter round-trip) cost real money; others (`--all-remotes`,
/// `--enable-developer-checks`) just aren't universally relevant enough to
/// run unconditionally. The one invariant that must always hold: the hourly
/// background sweep (`daemon/src/scheduler.rs`) only ever runs `Free`
/// entries -- see that module's own test coverage.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CostTier {
    /// Runs unconditionally: plain `ralphus check health`, and the daemon's
    /// hourly background sweep.
    Free,
    /// Only runs on explicit opt-in -- a CLI flag (`--enable-developer-checks`/
    /// `--all-remotes`/`--enable-live-agent-check`) or a board "check now"
    /// action. Never included in the background sweep.
    OnDemand,
}

/// How load-bearing is this check to the system actually working?
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RequirementLevel {
    /// Must resolve for the dependent functionality to work at all.
    Required,
    /// Best-effort / degrades gracefully; missing or misconfigured is a
    /// `warn`, not a `fail`, or affects only a narrow, non-default path.
    Optional,
    /// Only relevant as a fallback path for another, primary mechanism (e.g.
    /// `gh`/`glab` as a fallback token source when the primary token env var
    /// is unset) -- a missing fallback is never itself a problem unless the
    /// primary path is *also* unavailable.
    FallbackOnly,
}

/// How a catalog entry is actually invoked -- kept close to
/// `cli/src/health.rs`'s own opt-in-flag/daemon-delegation shape rather than
/// abstracted further, so a reader can map a catalog entry straight back to
/// the code that runs it.
#[derive(Debug, Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case", tag = "kind", content = "detail")]
pub enum Probe {
    /// Runs unconditionally, entirely inside the calling process (no daemon
    /// round trip beyond what daemon reachability itself already requires).
    Local,
    /// Runs unconditionally, delegated to a daemon HTTP endpoint (named
    /// here) because the check needs the daemon process's own
    /// state/environment.
    DaemonApi(&'static str),
    /// Only runs when the named CLI flag is passed.
    OptIn(&'static str),
    /// One probe per configured `[machine.targets.*]` entry, delegated to
    /// the daemon's remote-target sweep (`daemon/src/health_targets.rs`),
    /// gated behind `--all-remotes`.
    RemoteTarget,
}

/// One check's catalog metadata. Deliberately does not carry a live result
/// (status/detail/etc.) -- that stays in `cli::health::CheckResult` /
/// `daemon::health_targets::TargetCheck`, each of which carries this entry's
/// `id` so a result can always be joined back to its catalog metadata.
#[derive(Debug, Clone, Copy, serde::Serialize)]
pub struct CatalogEntry {
    /// Stable, kebab-case identifier -- never renamed once shipped, since
    /// external consumers (the board, `--json` output) may key off it.
    pub id: &'static str,
    /// Short human label (what a report/board row's name column shows).
    pub label: &'static str,
    pub section: &'static str,
    pub applicability: Applicability,
    pub cost_tier: CostTier,
    pub requirement: RequirementLevel,
    pub probe: Probe,
    /// One-line summary of what's at stake if this check isn't a clean
    /// pass -- meant to agree with `docs/dependencies.md`'s prose for the
    /// same dependency (that doc is the "why", this is the index entry).
    /// Individual check results still carry their own, more specific,
    /// per-status `impact` string; this is the catalog-level summary shown
    /// when browsing the catalog itself (e.g. `ralphus check catalog`),
    /// independent of any particular run's outcome.
    pub impact: &'static str,
    /// Generic, status-agnostic next step for when this check is not a
    /// clean pass -- the catalog-level counterpart to `impact`, for the
    /// same "browsing the catalog itself, independent of any particular
    /// run" contexts (e.g. the board's copy-remediation affordance on a
    /// cached sweep/report row that has no richer per-run remediation text
    /// of its own, unlike `cli::health::CheckResult::remediation`).
    pub remediation: &'static str,
}

macro_rules! catalog_ids {
    ($($const_name:ident => $id:literal),+ $(,)?) => {
        $(pub const $const_name: &str = $id;)+

        /// Every `ID_*` constant's own Rust symbol name paired with its
        /// string value -- e.g. `("ID_GIT", "git")`. Exists solely so a
        /// parity test in `cli`/`daemon` can grep their own source for each
        /// constant's *name* (not its string value, which could coincidence-
        /// ally appear in an unrelated string) and flag a catalog entry that
        /// no check call site actually references -- see
        /// `cli/tests/health_catalog_parity.rs`.
        pub const CATALOG_ID_CONST_NAMES: &[(&str, &str)] = &[
            $((stringify!($const_name), $id)),+
        ];
    };
}

catalog_ids! {
    ID_DAEMON => "daemon",
    ID_CONFIG_SOURCES => "config-sources",
    ID_CONFIG_SOURCES_PROJECT => "config-sources-project",
    ID_GIT => "git",
    ID_PROJECT_PATH => "project-path",
    ID_PROJECT_CLONE_URL => "project-clone-url",
    ID_PROJECT_FORK => "project-fork",
    ID_RUNNER => "runner",
    ID_OLLAMA => "ollama",
    ID_GH => "gh",
    ID_GLAB => "glab",
    ID_TMUX => "tmux",
    ID_CLAUDE_COMMAND => "claude-command",
    ID_CODEX_COMMAND => "codex-command",
    ID_PI_COMMAND => "pi-command",
    ID_CONFIG => "config",
    ID_DAEMON_MAX_CONCURRENT => "daemon-max-concurrent",
    ID_TOOL_ARG_TRUNCATE_CHARS => "tool-arg-truncate-chars",
    ID_THRASH_MAX_COMPACTIONS => "thrash-max-compactions",
    ID_THRASH_MIN_TURN_GAP => "thrash-min-turn-gap",
    ID_PULL_REQUEST_BRANCH_CONVENTION => "pull-request-branch-convention",
    ID_TEMPLATES => "templates",
    ID_NEW_TASK_DEFAULT_TAB => "new-task-default-tab",
    ID_AGENT_PROFILES => "agent-profiles",
    ID_DEFAULT_RESOLVER_AGENT => "default-resolver-agent",
    ID_ARBITER => "arbiter",
    ID_NVIDIA_SMI => "nvidia-smi",
    ID_CARGO => "cargo",
    ID_REMOTE_RESOLVE => "remote-resolve",
    ID_REMOTE_SSH_REACHABLE => "remote-ssh-reachable",
    ID_REMOTE_CAPABILITIES => "remote-capabilities",
    ID_REMOTE_ROOT => "remote-root",
    ID_REMOTE_GIT_VERSION => "remote-git-version",
    ID_REMOTE_GIT_IDENTITY_NAME => "remote-git-identity-name",
    ID_REMOTE_GIT_IDENTITY_EMAIL => "remote-git-identity-email",
    ID_REMOTE_PUSH_CREDENTIALS => "remote-push-credentials",
    ID_REMOTE_RUNNER => "remote-runner",
}

/// Every check `ralphus check health` (CLI-local + daemon-delegated) and the
/// daemon's remote-target sweep can produce, in the same order
/// `cli::health::run_checks` runs its daemon-local ones and
/// `daemon::health_targets::check_one_target` runs its per-target ones.
pub const CATALOG: &[CatalogEntry] = &[
    CatalogEntry {
        id: ID_DAEMON,
        label: "daemon reachability",
        section: section::CORE,
        applicability: Applicability::DaemonLocal,
        cost_tier: CostTier::Free,
        requirement: RequirementLevel::Required,
        probe: Probe::Local,
        impact: "No task can be submitted, monitored, or managed without a reachable daemon.",
        remediation: "Start the daemon (`ralphus-daemon`), or fix --daemon-url/$RALPHUS_DAEMON_URL to point at a running one.",
    },
    CatalogEntry {
        id: ID_CONFIG_SOURCES,
        label: "CLI config sources",
        section: section::CORE,
        applicability: Applicability::DaemonLocal,
        cost_tier: CostTier::Free,
        requirement: RequirementLevel::Optional,
        probe: Probe::Local,
        impact: "Purely informational -- names which .ralphus.toml files layer into task.*/[daemon] settings.",
        remediation: "Confirm the file exists and is named .ralphus.toml if you expect an override to apply.",
    },
    CatalogEntry {
        id: ID_CONFIG_SOURCES_PROJECT,
        label: "daemon config sources",
        section: section::CORE,
        applicability: Applicability::DaemonLocal,
        cost_tier: CostTier::Free,
        requirement: RequirementLevel::Optional,
        probe: Probe::Local,
        impact: "Purely informational -- names which .ralphus.toml files layer into [forge]/[live_view]/[thrash]/[[templates]]/[ui] settings.",
        remediation: "Confirm the file exists and is named .ralphus.toml if you expect an override to apply.",
    },
    CatalogEntry {
        id: ID_GIT,
        label: "git",
        section: section::HARNESS,
        applicability: Applicability::DaemonLocal,
        cost_tier: CostTier::Free,
        requirement: RequirementLevel::Required,
        probe: Probe::Local,
        impact: "Guardian reviews, worktree creation, and Git-based project validation all shell out to git; required whenever any registered project uses vcs=\"git\".",
        remediation: "Install git and ensure it resolves on PATH.",
    },
    CatalogEntry {
        id: ID_PROJECT_PATH,
        label: "project path/repo validity",
        section: section::CORE,
        applicability: Applicability::DaemonLocal,
        cost_tier: CostTier::Free,
        requirement: RequirementLevel::Required,
        probe: Probe::DaemonApi("GET /api/projects"),
        impact: "Every task/cell routed to a project whose on-disk path or git repo is invalid fails before it can even start.",
        remediation: "Create the missing path, or re-point the registration (`ralphus project git --name <name> --path <path>`).",
    },
    CatalogEntry {
        id: ID_PROJECT_CLONE_URL,
        label: "project clone URL",
        section: section::CORE,
        applicability: Applicability::DaemonLocal,
        cost_tier: CostTier::Free,
        requirement: RequirementLevel::Optional,
        probe: Probe::DaemonApi("GET /api/projects"),
        impact: "A git project with no registered clone URL fails remote provisioning the moment a cell routes to a non-local machine.",
        remediation: "Register a clone URL: `ralphus project git --name <name> --path <path> --url <clone-url>`.",
    },
    CatalogEntry {
        id: ID_PROJECT_FORK,
        label: "project fork registration",
        section: section::CORE,
        applicability: Applicability::DaemonLocal,
        cost_tier: CostTier::Free,
        requirement: RequirementLevel::Optional,
        probe: Probe::DaemonApi("GET /api/health/project-forks"),
        impact: "A misregistered fork breaks automated PR routing for that project/user.",
        remediation: "Re-run that project/user's fork registration, or see the check's own detail for specifics.",
    },
    CatalogEntry {
        id: ID_RUNNER,
        label: "ralphus-runner binary",
        section: section::HARNESS,
        applicability: Applicability::DaemonLocal,
        cost_tier: CostTier::Free,
        requirement: RequirementLevel::Required,
        probe: Probe::Local,
        impact: "The daemon cannot launch cells without a resolvable runner binary; tasks fail to start.",
        remediation: "Build/install ralphus-runner and put it on PATH, or set RALPHUS_RUNNER_CMD to its full path.",
    },
    CatalogEntry {
        id: ID_OLLAMA,
        label: "Ollama server",
        section: section::HARNESS,
        applicability: Applicability::DaemonLocal,
        cost_tier: CostTier::Free,
        requirement: RequirementLevel::Optional,
        probe: Probe::Local,
        impact: "Tasks using the ollama agent backend (and the Arbiter's default resolver, unless reconfigured) cannot run without a reachable server.",
        remediation: "Start Ollama (`ollama serve`), or point $RALPHUS_OLLAMA_URL at a reachable server.",
    },
    CatalogEntry {
        id: ID_GH,
        label: "gh CLI",
        section: section::HARNESS,
        applicability: Applicability::DaemonLocal,
        cost_tier: CostTier::Free,
        requirement: RequirementLevel::FallbackOnly,
        probe: Probe::Local,
        impact: "Optional fallback token source for GitHub auth, used only when RALPHUS_GITHUB_TOKEN/[forge].token_env is unset.",
        remediation: "Optional: install the GitHub CLI (https://cli.github.com) and run `gh auth login`, or set RALPHUS_GITHUB_TOKEN directly.",
    },
    CatalogEntry {
        id: ID_GLAB,
        label: "glab CLI",
        section: section::HARNESS,
        applicability: Applicability::DaemonLocal,
        cost_tier: CostTier::Free,
        requirement: RequirementLevel::FallbackOnly,
        probe: Probe::Local,
        impact: "Optional fallback token source for GitLab auth, used only when RALPHUS_GITLAB_TOKEN/[forge].token_env is unset.",
        remediation: "Optional: install the GitLab CLI (https://gitlab.com/gitlab-org/cli) and run `glab auth login`, or set RALPHUS_GITLAB_TOKEN directly.",
    },
    CatalogEntry {
        id: ID_TMUX,
        label: "tmux/psmux",
        section: section::HARNESS,
        applicability: Applicability::DaemonLocal,
        cost_tier: CostTier::Free,
        requirement: RequirementLevel::Required,
        probe: Probe::Local,
        impact: "Live sessions/panes for running cells depend on this binary; a missing one fails every cell that starts a session.",
        remediation: "Install tmux/psmux and put it on PATH, or set RALPHUS_TMUX_CMD to its full path (see docs/dependencies.md).",
    },
    CatalogEntry {
        id: ID_CLAUDE_COMMAND,
        label: "claude-code command override",
        section: section::HARNESS,
        applicability: Applicability::DaemonLocal,
        cost_tier: CostTier::Free,
        requirement: RequirementLevel::Optional,
        probe: Probe::Local,
        impact: "Only relevant if $RALPHUS_CLAUDE_COMMAND is set; an invalid override breaks tasks using this backend.",
        remediation: "Fix or unset $RALPHUS_CLAUDE_COMMAND so it points at a real executable.",
    },
    CatalogEntry {
        id: ID_CODEX_COMMAND,
        label: "codex command override",
        section: section::HARNESS,
        applicability: Applicability::DaemonLocal,
        cost_tier: CostTier::Free,
        requirement: RequirementLevel::Optional,
        probe: Probe::Local,
        impact: "Only relevant if $RALPHUS_CODEX_COMMAND is set; an invalid override breaks tasks using this backend.",
        remediation: "Fix or unset $RALPHUS_CODEX_COMMAND so it points at a real executable.",
    },
    CatalogEntry {
        id: ID_PI_COMMAND,
        label: "pi command override",
        section: section::HARNESS,
        applicability: Applicability::DaemonLocal,
        cost_tier: CostTier::Free,
        requirement: RequirementLevel::Optional,
        probe: Probe::Local,
        impact: "Only relevant if $RALPHUS_PI_COMMAND is set; an invalid override breaks tasks using this backend.",
        remediation: "Fix or unset $RALPHUS_PI_COMMAND so it points at a real executable.",
    },
    CatalogEntry {
        id: ID_CONFIG,
        label: "task.maximum_timeout_seconds",
        section: section::CORE,
        applicability: Applicability::DaemonLocal,
        cost_tier: CostTier::Free,
        requirement: RequirementLevel::Optional,
        probe: Probe::Local,
        impact: "Every subprocess timeout computation depends on this value being well-formed.",
        remediation: "Set task.maximum_timeout_seconds to 0 (unbounded) or a positive number of seconds.",
    },
    CatalogEntry {
        id: ID_DAEMON_MAX_CONCURRENT,
        label: "[daemon] max_concurrent",
        section: section::CORE,
        applicability: Applicability::DaemonLocal,
        cost_tier: CostTier::Free,
        requirement: RequirementLevel::Optional,
        probe: Probe::Local,
        impact: "An invalid value is silently ignored in favor of the built-in default concurrency cap.",
        remediation: "Set daemon.max_concurrent to 0 (no limit) or a positive concurrency cap.",
    },
    CatalogEntry {
        id: ID_TOOL_ARG_TRUNCATE_CHARS,
        label: "[live_view] tool_arg_truncate_chars",
        section: section::CORE,
        applicability: Applicability::DaemonLocal,
        cost_tier: CostTier::Free,
        requirement: RequirementLevel::Optional,
        probe: Probe::Local,
        impact: "An invalid value is silently ignored in favor of the built-in default Live View truncation length.",
        remediation: "Set live_view.tool_arg_truncate_chars to an integer >= 0.",
    },
    CatalogEntry {
        id: ID_THRASH_MAX_COMPACTIONS,
        label: "[thrash] max_compactions",
        section: section::CORE,
        applicability: Applicability::DaemonLocal,
        cost_tier: CostTier::Free,
        requirement: RequirementLevel::Optional,
        probe: Probe::Local,
        impact: "An invalid value is silently ignored in favor of the built-in thrash-detection default.",
        remediation: "Set thrash.max_compactions to a non-negative integer.",
    },
    CatalogEntry {
        id: ID_THRASH_MIN_TURN_GAP,
        label: "[thrash] min_turn_gap",
        section: section::CORE,
        applicability: Applicability::DaemonLocal,
        cost_tier: CostTier::Free,
        requirement: RequirementLevel::Optional,
        probe: Probe::Local,
        impact: "An invalid value is silently ignored in favor of the built-in thrash-detection default.",
        remediation: "Set thrash.min_turn_gap to a non-negative integer.",
    },
    CatalogEntry {
        id: ID_PULL_REQUEST_BRANCH_CONVENTION,
        label: "[forge] pull_request_branch_convention",
        section: section::CORE,
        applicability: Applicability::DaemonLocal,
        cost_tier: CostTier::Free,
        requirement: RequirementLevel::Optional,
        probe: Probe::Local,
        impact: "An explicitly-set-but-invalid convention fails PR submission outright -- unlike most .ralphus.toml scalars, this one has no safe default to fall back to.",
        remediation: "Fix [forge].pull_request_branch_convention so it contains the required {name} placeholder.",
    },
    CatalogEntry {
        id: ID_TEMPLATES,
        label: "[[templates]]",
        section: section::CORE,
        applicability: Applicability::DaemonLocal,
        cost_tier: CostTier::Free,
        requirement: RequirementLevel::Optional,
        probe: Probe::Local,
        impact: "A malformed template entry breaks the Simple task form's template picker.",
        remediation: "Fix the [[templates]] entry named in the check's own detail.",
    },
    CatalogEntry {
        id: ID_NEW_TASK_DEFAULT_TAB,
        label: "[ui] new_task_default_tab",
        section: section::CORE,
        applicability: Applicability::DaemonLocal,
        cost_tier: CostTier::Free,
        requirement: RequirementLevel::Optional,
        probe: Probe::Local,
        impact: "An unknown tab name breaks the New Task form's default-tab setting.",
        remediation: "Set [ui].new_task_default_tab to a known tab name.",
    },
    CatalogEntry {
        id: ID_AGENT_PROFILES,
        label: "[agent.profiles.*]",
        section: section::CORE,
        applicability: Applicability::DaemonLocal,
        cost_tier: CostTier::Free,
        requirement: RequirementLevel::Optional,
        probe: Probe::DaemonApi("GET /api/health/agent-profiles"),
        impact: "A broken agent profile fails any task that selects it, at submission time.",
        remediation: "Fix the profile in .ralphus.toml's [agent.profiles.*] per the check's own detail.",
    },
    CatalogEntry {
        id: ID_DEFAULT_RESOLVER_AGENT,
        label: "[review].default_resolver_agent",
        section: section::CORE,
        applicability: Applicability::DaemonLocal,
        cost_tier: CostTier::Free,
        requirement: RequirementLevel::Required,
        probe: Probe::DaemonApi("GET /api/agents"),
        impact: "Reviews without an explicit resolver fail outright instead of falling back to a working agent.",
        remediation: "Fix [review].default_resolver_agent to name a built-in backend or a configured [agent.profiles.*] entry.",
    },
    CatalogEntry {
        id: ID_ARBITER,
        label: "Arbiter live round-trip",
        section: section::CORE,
        applicability: Applicability::DaemonLocal,
        cost_tier: CostTier::OnDemand,
        requirement: RequirementLevel::Required,
        probe: Probe::OptIn("--enable-live-agent-check"),
        impact: "Reviews/tasks that depend on the Arbiter fail the same way a live round-trip does; this is the only check that spends model budget, so it never runs by default.",
        remediation: "See the check's own detail for the underlying agent/model error, and fix the Arbiter's configuration or credentials.",
    },
    CatalogEntry {
        id: ID_NVIDIA_SMI,
        label: "nvidia-smi",
        section: section::MACHINE,
        applicability: Applicability::DaemonLocal,
        cost_tier: CostTier::Free,
        requirement: RequirementLevel::Optional,
        probe: Probe::Local,
        impact: "The resource view's GPU column shows N/A instead of live usage; nothing else is affected.",
        remediation: "Optional: install NVIDIA drivers/nvidia-smi if you want GPU usage reported.",
    },
    CatalogEntry {
        id: ID_CARGO,
        label: "cargo (developer toolchain)",
        section: section::MACHINE,
        applicability: Applicability::DaemonLocal,
        cost_tier: CostTier::OnDemand,
        requirement: RequirementLevel::Optional,
        probe: Probe::OptIn("--enable-developer-checks"),
        impact: "The Rust binaries in this workspace cannot be built from source; irrelevant unless you're developing ralphus itself.",
        remediation: "Install it via https://rustup.rs.",
    },
    CatalogEntry {
        id: ID_REMOTE_RESOLVE,
        label: "remote target resolution",
        section: section::MACHINE,
        applicability: Applicability::Remote,
        cost_tier: CostTier::OnDemand,
        requirement: RequirementLevel::Required,
        probe: Probe::RemoteTarget,
        impact: "A target that doesn't resolve to a remote provider fails any cell routed to it before any other remote check can even run.",
        remediation: "Fix this target's machine reference to name a registered remote provider.",
    },
    CatalogEntry {
        id: ID_REMOTE_SSH_REACHABLE,
        label: "remote SSH reachability",
        section: section::MACHINE,
        applicability: Applicability::Remote,
        cost_tier: CostTier::OnDemand,
        requirement: RequirementLevel::Required,
        probe: Probe::RemoteTarget,
        impact: "An unreachable target fails any cell routed to it; every later check for that target is skipped, since it would fail the identical way.",
        remediation: "Ensure the remote host is reachable and its SSH/provider credentials are valid.",
    },
    CatalogEntry {
        id: ID_REMOTE_CAPABILITIES,
        label: "remote capabilities",
        section: section::MACHINE,
        applicability: Applicability::Remote,
        cost_tier: CostTier::OnDemand,
        requirement: RequirementLevel::Optional,
        probe: Probe::RemoteTarget,
        impact: "A provider that can't report capabilities just means less detail is shown; it does not by itself block cell execution.",
        remediation: "No action needed -- this only affects how much detail is shown.",
    },
    CatalogEntry {
        id: ID_REMOTE_ROOT,
        label: "remote root readiness",
        section: section::MACHINE,
        applicability: Applicability::Remote,
        cost_tier: CostTier::OnDemand,
        requirement: RequirementLevel::Required,
        probe: Probe::RemoteTarget,
        impact: "Worktree provisioning on this target fails if create/read/rename/delete don't all succeed under the configured remote root.",
        remediation: "Ensure the configured remote_root exists and is writable by the provider's account.",
    },
    CatalogEntry {
        id: ID_REMOTE_GIT_VERSION,
        label: "remote git version",
        section: section::MACHINE,
        applicability: Applicability::Remote,
        cost_tier: CostTier::OnDemand,
        requirement: RequirementLevel::Required,
        probe: Probe::RemoteTarget,
        impact: "Cells routed to this target cannot perform any git-based operation without a working remote git binary.",
        remediation: "Install git on the remote host and ensure it resolves for the provider's account.",
    },
    CatalogEntry {
        id: ID_REMOTE_GIT_IDENTITY_NAME,
        label: "remote git user.name",
        section: section::MACHINE,
        applicability: Applicability::Remote,
        cost_tier: CostTier::OnDemand,
        requirement: RequirementLevel::Optional,
        probe: Probe::RemoteTarget,
        impact: "Commits made on this target fail until a global git user.name is set for the remote account, unless every repository sets it per-repo instead.",
        remediation: "Set a global git user.name for the remote account, or configure it per-repository.",
    },
    CatalogEntry {
        id: ID_REMOTE_GIT_IDENTITY_EMAIL,
        label: "remote git user.email",
        section: section::MACHINE,
        applicability: Applicability::Remote,
        cost_tier: CostTier::OnDemand,
        requirement: RequirementLevel::Optional,
        probe: Probe::RemoteTarget,
        impact: "Commits made on this target fail until a global git user.email is set for the remote account, unless every repository sets it per-repo instead.",
        remediation: "Set a global git user.email for the remote account, or configure it per-repository.",
    },
    CatalogEntry {
        id: ID_REMOTE_PUSH_CREDENTIALS,
        label: "remote push credentials",
        section: section::MACHINE,
        applicability: Applicability::Remote,
        cost_tier: CostTier::OnDemand,
        requirement: RequirementLevel::Optional,
        probe: Probe::RemoteTarget,
        impact: "Documented-unverified: no safe, non-mutating way to confirm push authorization exists yet, so this always reports 'not verified' rather than a real pass/fail.",
        remediation: "No automated remediation -- manually verify push credentials are valid for this target.",
    },
    CatalogEntry {
        id: ID_REMOTE_RUNNER,
        label: "remote runner executable",
        section: section::MACHINE,
        applicability: Applicability::Remote,
        cost_tier: CostTier::OnDemand,
        requirement: RequirementLevel::Optional,
        probe: Probe::RemoteTarget,
        impact: "Documented-unverified: confirming the configured runner_command resolves on the remote would need the SSH provider's run verb to accept an arbitrary program, which is deliberately scoped to git only -- so this always reports 'not verified' rather than a real pass/fail.",
        remediation: "No automated remediation -- manually confirm the configured runner_command resolves on the remote.",
    },
];

/// Looks up one catalog entry by its stable `id`.
#[must_use]
pub fn get(id: &str) -> Option<&'static CatalogEntry> {
    CATALOG.iter().find(|e| e.id == id)
}

/// Every entry that runs unconditionally on the local/daemon machine --
/// exactly what the hourly background sweep is allowed to run
/// (`daemon/src/health_sweep.rs`). Deliberately excludes every `Remote` entry
/// too: even though remote-target checks aren't individually opted into via
/// the same per-check flag mechanism as `arbiter`/`cargo`, probing a remote
/// target still costs a live SSH round trip the background sweep has no
/// business spending on its own, so every `Remote` entry is `OnDemand`.
pub fn free_daemon_local_entries() -> impl Iterator<Item = &'static CatalogEntry> {
    CATALOG
        .iter()
        .filter(|e| e.cost_tier == CostTier::Free && e.applicability == Applicability::DaemonLocal)
}

/// Catalog entries that correspond to a genuine *runtime dependency*
/// `docs/dependencies.md` already has its own dedicated section for --
/// deliberately a small, curated subset, not every catalog entry: most
/// entries (`config`, `templates`, `thrash-*`, ...) validate `.ralphus.toml`
/// *shape*, not an external runtime dependency, and have no natural home in
/// that doc. `docs/dependencies.md`'s own drift test
/// (`dependencies_md_names_every_documented_catalog_id`, below) asserts each
/// of these ids is cross-referenced by its exact catalog id, so the two
/// documents can never silently disagree about which check backs which
/// dependency section.
pub const DOCUMENTED_IN_DEPENDENCIES_MD: &[&str] =
    &[ID_GIT, ID_TMUX, ID_GH, ID_GLAB, ID_NVIDIA_SMI];

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn every_id_is_unique() {
        let mut seen = HashSet::new();
        for entry in CATALOG {
            assert!(seen.insert(entry.id), "duplicate catalog id: {}", entry.id);
        }
    }

    #[test]
    fn every_id_is_kebab_case_ascii() {
        for entry in CATALOG {
            assert!(
                !entry.id.is_empty()
                    && entry
                        .id
                        .chars()
                        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-'),
                "catalog id {:?} must be non-empty kebab-case ascii",
                entry.id
            );
        }
    }

    #[test]
    fn every_entry_has_a_label_and_impact() {
        for entry in CATALOG {
            assert!(!entry.label.is_empty(), "{} has an empty label", entry.id);
            assert!(!entry.impact.is_empty(), "{} has an empty impact", entry.id);
            assert!(
                !entry.remediation.is_empty(),
                "{} has an empty remediation",
                entry.id
            );
        }
    }

    #[test]
    fn every_entry_has_a_known_section() {
        for entry in CATALOG {
            assert!(
                [section::CORE, section::HARNESS, section::MACHINE].contains(&entry.section),
                "{} has an unrecognized section {:?}",
                entry.id,
                entry.section
            );
        }
    }

    /// Architectural invariant the hourly background sweep depends on: no
    /// `Remote` check is ever `Free`, so filtering on cost tier alone (not
    /// applicability) is never enough to accidentally open a remote/SSH
    /// connection from a sweep meant to be free/local-only. See
    /// [`free_daemon_local_entries`].
    #[test]
    fn no_remote_entry_is_free() {
        for entry in CATALOG {
            if entry.applicability == Applicability::Remote {
                assert_eq!(
                    entry.cost_tier,
                    CostTier::OnDemand,
                    "{} is Remote but Free -- the hourly sweep must never probe remote targets",
                    entry.id
                );
            }
        }
    }

    #[test]
    fn free_daemon_local_entries_excludes_every_opt_in_and_remote_check() {
        let free: Vec<&str> = free_daemon_local_entries().map(|e| e.id).collect();
        assert!(!free.contains(&ID_ARBITER));
        assert!(!free.contains(&ID_CARGO));
        assert!(!free.contains(&ID_REMOTE_SSH_REACHABLE));
        assert!(free.contains(&ID_DAEMON));
        assert!(free.contains(&ID_GIT));
        assert!(free.contains(&ID_TMUX));
    }

    #[test]
    fn get_finds_a_known_id_and_none_for_unknown() {
        assert!(get(ID_DAEMON).is_some());
        assert!(get("does-not-exist").is_none());
    }

    /// RAL-416 doc/catalog parity: every id in [`DOCUMENTED_IN_DEPENDENCIES_MD`]
    /// must be cross-referenced, by its exact catalog id, in
    /// `docs/dependencies.md` -- a "catalog id `<id>`" marker next to the
    /// existing prose for that dependency (see that doc's own section for
    /// each). Named/actionable on failure: reports exactly which id is
    /// missing its marker rather than "the docs and the catalog disagree
    /// somewhere."
    #[test]
    fn dependencies_md_names_every_documented_catalog_id() {
        let docs = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../docs/dependencies.md"
        ));
        let missing: Vec<&str> = DOCUMENTED_IN_DEPENDENCIES_MD
            .iter()
            .filter(|id| !docs.contains(&format!("catalog id `{id}`")))
            .copied()
            .collect();
        assert!(
            missing.is_empty(),
            "docs/dependencies.md is missing a `catalog id `<id>`` cross-reference for: \
             {missing:?} -- add one next to that dependency's existing prose"
        );
    }
}
