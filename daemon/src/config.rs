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
//! avoid a git worktree copy per branch, and `auto_build` (RAL-101), the
//! project-level default build/test command run when a review declares no
//! explicit `checks`.

use std::path::{Path, PathBuf};

use chrono::{NaiveTime, Utc};
use serde::Deserialize;

/// Resolved review configuration (after layering global under per-project).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
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
}

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
        }
    }
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

/// Forge routing configuration (`[forge]` table, RAL-117). Lets a project pin
/// which forge (GitHub/GitLab) and remote to submit PRs against, instead of
/// relying purely on `git remote get-url` autodetection. `None` fields fall
/// back to autodetection/defaults at the point of use (see `crate::forge`).
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct ForgeConfig {
    /// `"github"` or `"gitlab"`. `None` means autodetect from the remote URL's host.
    #[serde(default)]
    pub kind: Option<String>,
    /// Git remote name to read the repository URL from. Defaults to `"origin"`.
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
        }
    }
}

/// The on-disk file shape: either a `[review]` or a `[defaults]` table.
#[derive(Debug, Default, Deserialize)]
struct ConfigFile {
    #[serde(default)]
    review: Option<ReviewConfig>,
    #[serde(default)]
    defaults: Option<ReviewConfig>,
    #[serde(default)]
    daemon: Option<DaemonConfig>,
    #[serde(default)]
    cartographer: Option<CartographerConfig>,
    #[serde(default)]
    forge: Option<ForgeConfig>,
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

/// Load the effective daemon config by layering the global config file under
/// the nearest per-project `.ralphus.toml` (per-project scalars win).
#[must_use]
pub fn load_daemon_config() -> DaemonConfig {
    let global = global_config_path()
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| daemon_from_toml_str(&s))
        .unwrap_or_default();
    let local = std::env::current_dir()
        .ok()
        .as_deref()
        .and_then(find_project_config)
        .and_then(|p| std::fs::read_to_string(p).ok())
        .map(|s| daemon_from_toml_str(&s))
        .unwrap_or_default();
    DaemonConfig {
        log_path: local.log_path.or(global.log_path),
        log_level: local.log_level.or(global.log_level),
        downtime: merge_downtime(global.downtime, local.downtime),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(skip: Option<bool>, checks: &[&str]) -> ReviewConfig {
        ReviewConfig {
            skip_worktrees: skip,
            checks: checks.iter().map(|s| (*s).to_string()).collect(),
            auto_build: None,
            summary_format: None,
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
}
