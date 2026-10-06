//! Container-backed machines: run every remote command inside a Docker
//! container on the SSH host instead of directly in the host account. (Not the
//! daemon's own `docs/container-mode.md`, which confines the daemon host.)
//!
//! The provider owns provisioning end to end -- ralphus's daemon neither knows
//! nor cares that a machine is containerised. An operator registers the
//! provider with the container flags (see [`ContainerConfig`]) under its own
//! scheme, and every verb that reaches the machine does so through the
//! container:
//!
//! ```text
//! daemon -> provider verb -> ssh host -> docker exec -i <name> sh -c '<cmd>'
//! ```
//!
//! Every remote command in this crate is one shell string built for the
//! remote login shell and handed to [`crate::ssh::command_args`]. That function
//! asks [`wrap`] for the in-container form of the string, so the whole verb
//! set -- job launch/status/stream/cancel, provision, file operations, runner
//! upload -- runs in one PID namespace and one filesystem without any of them
//! knowing. The job supervisor's `/proc` start-time identity checks depend on
//! that: launch and status must be observed from the same namespace.
//!
//! ## Identity
//!
//! A container is addressed by [`container_name`], a pure function of a
//! [`ContainerScope`] and the machine's ssh target. Today every machine has
//! exactly one container ([`ContainerScope::Machine`]). Finer granularity
//! (per project, squad, or cell) is expected to arrive later by adding scope
//! variants here -- nothing else in the crate derives a container name, so
//! nothing else changes when it does. [`ContainerScope::Review`] already
//! exists to pin one rule: a review always owns exactly one container of its
//! own, however granular cell and proof containers become.
//!
//! ## What is delegated to the operator
//!
//! Image contents, mounts, users, networks, and credentials are the
//! operator's. The provider passes `--container-mount` and `--container-run-arg`
//! values straight to `docker run`. In particular, whatever is mounted must
//! cover each target's `remote_root` if provisioned clones, worktrees, and job
//! state are meant to outlive the container.

use std::process::{Command, Stdio};
use std::sync::OnceLock;

use crate::config::sanitize_slug;
use crate::exec::EffectiveConfig;
use crate::ssh;
use crate::transport::shell_quote_single;
use crate::uri;

/// How a machine's container is created and reached. All fields except
/// `image` have defaults; see each field.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContainerConfig {
    /// The Docker command as shell words, run on the remote host. Defaults to
    /// `docker`. Operator-trusted: this is where a privileged invocation such
    /// as `sudo docker` or a rootless-socket prefix goes.
    pub docker: Option<String>,
    /// Image the container is created from. Required.
    pub image: String,
    /// Explicit container name. Defaults to [`container_name`]'s derivation.
    pub name: Option<String>,
    /// `docker run -v` values (`HOST:CONTAINER[:opts]`).
    pub mounts: Vec<String>,
    /// Extra single arguments for `docker run`, passed verbatim (for example
    /// `--network=host` or `-e=KEY=value`), each as its own argv entry.
    pub run_args: Vec<String>,
}

/// What a container is dedicated to. See the module docs on identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ContainerScope {
    /// The machine as a whole -- the only scope cells and proofs use today.
    Machine,
    /// One review. A review always has exactly one container of its own.
    Review(String),
}

/// Process-wide container mode, set once from the provider's own arguments.
///
/// A global because the provider is a one-shot process per verb and the
/// setting must reach [`crate::ssh::command_args`] from a dozen call sites
/// that each already take an [`EffectiveConfig`] they would otherwise all have
/// to grow a field on.
static ACTIVE: OnceLock<ContainerConfig> = OnceLock::new();

/// Turn container mode on for this process. The first call wins.
pub fn configure(config: ContainerConfig) {
    let _ = ACTIVE.set(config);
}

/// The active container configuration, if container mode is on.
#[must_use]
pub fn active() -> Option<&'static ContainerConfig> {
    ACTIVE.get()
}

impl ContainerConfig {
    fn docker_cmd(&self) -> &str {
        self.docker
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or("docker")
    }

    /// The configuration problem that would make every verb fail, if any.
    ///
    /// # Errors
    /// An empty image, or a name Docker would reject.
    pub fn validate(&self) -> Result<(), String> {
        if self.image.trim().is_empty() {
            return Err("--container-image must not be empty".to_string());
        }
        if let Some(name) = &self.name {
            if !valid_container_name(name) {
                return Err(format!(
                    "--container-name {name:?} is not a valid Docker container name \
                     (letters, digits, `_`, `.`, `-`; must start with a letter or digit)"
                ));
            }
        }
        Ok(())
    }
}

fn valid_container_name(name: &str) -> bool {
    let mut chars = name.chars();
    chars.next().is_some_and(|c| c.is_ascii_alphanumeric())
        && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// The container name for `scope` on the machine reached as `target`.
///
/// Deterministic, so every verb invocation (each a fresh process) addresses
/// the same container. An explicit `config.name` pins the machine-scope name.
#[must_use]
pub fn container_name(config: &ContainerConfig, scope: &ContainerScope, target: &str) -> String {
    match scope {
        ContainerScope::Machine => match &config.name {
            Some(name) => name.clone(),
            None => format!("ralphus-{}", stable_slug(target)),
        },
        ContainerScope::Review(id) => format!("ralphus-review-{}", stable_slug(id)),
    }
}

/// A short, Docker-name-safe slug that stays unique across inputs that
/// sanitise to the same text.
fn stable_slug(raw: &str) -> String {
    use std::hash::{DefaultHasher, Hash, Hasher};
    let mut hasher = DefaultHasher::new();
    raw.hash(&mut hasher);
    let digest = hasher.finish();
    format!("{}-{:08x}", sanitize_slug(raw), digest & 0xffff_ffff)
}

/// The in-container form of a remote shell command: `cmd` run by `sh -c`
/// inside `name`, with stdin attached. With `pty`, a terminal is allocated
/// too (for the `terminal` verb, whose ssh session already holds a pty).
#[must_use]
pub fn wrap(config: &ContainerConfig, name: &str, cmd: &str, pty: bool) -> String {
    let flags = if pty { "-it" } else { "-i" };
    format!(
        "{} exec {flags} {} sh -c {}",
        config.docker_cmd(),
        shell_quote_single(name),
        shell_quote_single(cmd)
    )
}

/// [`wrap`] under the process-wide configuration, or `cmd` unchanged when
/// container mode is off.
#[must_use]
pub fn wrap_active(target: &str, cmd: &str, pty: bool) -> String {
    match active() {
        Some(config) => {
            let name = container_name(config, &ContainerScope::Machine, target);
            wrap(config, &name, cmd, pty)
        }
        None => cmd.to_string(),
    }
}

/// Exit status [`ensure_script`] uses when the existing container was made
/// from a different image than configured.
const EXIT_IMAGE_MISMATCH: i32 = 42;
/// Exit status [`ensure_script`] uses when the container could not be created.
const EXIT_CREATE_FAILED: i32 = 41;

/// The host-side script that makes `name` exist and run, creating it from the
/// configured image when absent. Idempotent and safe to run concurrently:
/// Docker's name uniqueness is the lock, so a lost creation race falls through
/// to re-inspecting the winner's container -- polled briefly, because the name
/// is reserved a moment before the container becomes inspectable.
///
/// An existing container built from another image is refused rather than
/// adopted -- silently reusing it would run work in an environment the
/// operator did not configure.
#[must_use]
pub fn ensure_script(config: &ContainerConfig, name: &str) -> String {
    let docker = config.docker_cmd();
    let q_name = shell_quote_single(name);
    let q_image = shell_quote_single(&config.image);
    let mut run_args = String::new();
    for mount in &config.mounts {
        run_args.push_str(" -v ");
        run_args.push_str(&shell_quote_single(mount));
    }
    for arg in &config.run_args {
        run_args.push(' ');
        run_args.push_str(&shell_quote_single(arg));
    }
    let idle = shell_quote_single("while :; do sleep 3600; done");
    let fmt = shell_quote_single("{{.State.Running}} {{.Config.Image}}");
    format!(
        "N={q_name}; I={q_image}\n\
         st=$({docker} inspect -f {fmt} \"$N\" 2>/dev/null)\n\
         if [ -z \"$st\" ]; then\n\
           if ! out=$({docker} run -d --name \"$N\" --init --label ralphus.managed=1{run_args} --entrypoint sh \"$I\" -c {idle} 2>&1); then\n\
             n=0; while [ -z \"$st\" ] && [ $n -lt 20 ]; do st=$({docker} inspect -f {fmt} \"$N\" 2>/dev/null); n=$((n+1)); [ -n \"$st\" ] || sleep 0.5; done\n\
             if [ -z \"$st\" ]; then echo \"could not create container $N from $I: $out\" >&2; exit {EXIT_CREATE_FAILED}; fi\n\
           else\n\
             st=\"true $I\"\n\
           fi\n\
         fi\n\
         img=${{st#* }}\n\
         if [ \"$img\" != \"$I\" ]; then echo \"container $N exists but was created from image $img, not $I; remove it or fix the configuration\" >&2; exit {EXIT_IMAGE_MISMATCH}; fi\n\
         if [ \"${{st%% *}}\" != \"true\" ]; then\n\
           if ! out=$({docker} start \"$N\" 2>&1); then echo \"could not start container $N: $out\" >&2; exit {EXIT_CREATE_FAILED}; fi\n\
         fi\n\
         echo \"$N\"\n"
    )
}

/// Make sure the machine's container exists and is running; returns its name.
///
/// Runs on the SSH host itself (never through [`wrap`]).
///
/// # Errors
/// An unreachable host, missing Docker, a failed create/start, or an image
/// mismatch, each as an actionable message.
pub fn ensure(uri_str: &str, config: &EffectiveConfig) -> Result<String, String> {
    let Some(container) = active() else {
        return Err("container mode is not enabled".to_string());
    };
    let target = uri::parse(uri_str).map_err(|e| e.to_string())?;
    let name = container_name(container, &ContainerScope::Machine, &target.target_string());
    let script = ensure_script(container, &name);
    let args = ssh::host_command_args(
        &target.target_string(),
        config.connect_timeout_secs,
        &script,
        config.ssh_config_file.as_deref(),
    );
    let out = Command::new("ssh")
        .args(&args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("could not run ssh to reach {target}: {e}"))?;
    if out.status.success() {
        return Ok(name);
    }
    let stderr = String::from_utf8_lossy(&out.stderr);
    Err(interpret_ensure_failure(
        &name,
        out.status.code(),
        &stderr,
        container.docker_cmd(),
    ))
}

/// Turn a failed [`ensure_script`] run into an actionable message.
#[must_use]
pub fn interpret_ensure_failure(
    name: &str,
    code: Option<i32>,
    stderr: &str,
    docker: &str,
) -> String {
    let lower = stderr.to_lowercase();
    if code == Some(EXIT_IMAGE_MISMATCH) || code == Some(EXIT_CREATE_FAILED) {
        return format!("container {name}: {}", stderr.trim());
    }
    if lower.contains("command not found") || lower.contains("not found") && lower.contains(docker)
    {
        return format!(
            "`{docker}` is not installed or not on PATH for the ssh user on this host: {}",
            stderr.trim()
        );
    }
    if lower.contains("permission denied") && lower.contains("docker.sock") {
        return format!(
            "the ssh user cannot reach the Docker daemon (permission denied on its socket) -- \
             add the user to the `docker` group or point --container-docker at a working \
             invocation: {}",
            stderr.trim()
        );
    }
    if lower.contains("cannot connect to the docker daemon") {
        return format!(
            "the Docker daemon on this host is not running or not reachable: {}",
            stderr.trim()
        );
    }
    // Everything else is the ssh layer's (host key, auth, refused, ...).
    ssh::interpret_failure("ssh", code, stderr)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg() -> ContainerConfig {
        ContainerConfig {
            image: "img:1".to_string(),
            ..ContainerConfig::default()
        }
    }

    #[test]
    fn wrap_runs_the_command_via_sh_c_with_stdin_attached() {
        let wrapped = wrap(&cfg(), "box", "cd /w && run", false);
        assert_eq!(wrapped, "docker exec -i 'box' sh -c 'cd /w && run'");
    }

    #[test]
    fn wrap_allocates_a_tty_only_when_asked() {
        assert!(wrap(&cfg(), "box", "x", true).contains(" -it "));
        assert!(!wrap(&cfg(), "box", "x", false).contains(" -it "));
    }

    #[test]
    fn wrap_quotes_hostile_commands_as_one_word() {
        let wrapped = wrap(&cfg(), "box", "echo 'a b'; $(rm -rf /)", false);
        assert_eq!(
            wrapped,
            "docker exec -i 'box' sh -c 'echo '\\''a b'\\''; $(rm -rf /)'"
        );
    }

    #[test]
    fn docker_command_is_an_operator_supplied_prefix() {
        let mut c = cfg();
        c.docker = Some("sudo docker".to_string());
        assert!(wrap(&c, "box", "x", false).starts_with("sudo docker exec"));
        assert!(
            ensure_script(&c, "box").contains("$(sudo docker inspect"),
            "ensure must use the same docker prefix"
        );
    }

    #[test]
    fn blank_docker_command_falls_back_to_docker() {
        let mut c = cfg();
        c.docker = Some("   ".to_string());
        assert!(wrap(&c, "box", "x", false).starts_with("docker exec"));
    }

    #[test]
    fn machine_names_are_deterministic_and_docker_safe() {
        let a = container_name(&cfg(), &ContainerScope::Machine, "alice@build box.example");
        let b = container_name(&cfg(), &ContainerScope::Machine, "alice@build box.example");
        assert_eq!(a, b);
        assert!(valid_container_name(&a), "{a}");
        assert!(a.starts_with("ralphus-"), "{a}");
    }

    #[test]
    fn targets_that_sanitise_alike_still_get_distinct_names() {
        let a = container_name(&cfg(), &ContainerScope::Machine, "a@host");
        let b = container_name(&cfg(), &ContainerScope::Machine, "a_host");
        assert_ne!(a, b);
    }

    #[test]
    fn an_explicit_name_pins_the_machine_container() {
        let mut c = cfg();
        c.name = Some("my-box".to_string());
        assert_eq!(
            container_name(&c, &ContainerScope::Machine, "anything"),
            "my-box"
        );
    }

    #[test]
    fn a_review_scope_is_distinct_per_review_and_from_the_machine() {
        let c = cfg();
        let machine = container_name(&c, &ContainerScope::Machine, "host");
        let r1 = container_name(&c, &ContainerScope::Review("rev-1".into()), "host");
        let r2 = container_name(&c, &ContainerScope::Review("rev-2".into()), "host");
        assert_ne!(r1, r2);
        assert_ne!(r1, machine);
        assert_eq!(
            r1,
            container_name(&c, &ContainerScope::Review("rev-1".into()), "host"),
            "one review, one container: the name must be stable"
        );
        assert!(valid_container_name(&r1), "{r1}");
    }

    #[test]
    fn validate_rejects_an_empty_image_and_a_bad_name() {
        assert!(ContainerConfig::default().validate().is_err());
        let mut c = cfg();
        c.name = Some("-bad".to_string());
        assert!(c.validate().is_err());
        c.name = Some("good_name.1".to_string());
        assert!(c.validate().is_ok());
    }

    #[test]
    fn ensure_script_passes_mounts_and_run_args_as_separate_quoted_words() {
        let mut c = cfg();
        c.mounts = vec!["/srv/r:/srv/r".to_string()];
        c.run_args = vec!["--network=host".to_string(), "-e=A=b c".to_string()];
        let script = ensure_script(&c, "box");
        assert!(script.contains(" -v '/srv/r:/srv/r'"), "{script}");
        assert!(script.contains(" '--network=host'"), "{script}");
        assert!(script.contains(" '-e=A=b c'"), "{script}");
    }

    #[test]
    fn ensure_script_refuses_a_container_built_from_another_image() {
        let script = ensure_script(&cfg(), "box");
        assert!(script.contains("was created from image"), "{script}");
        assert!(
            script.contains(&format!("exit {EXIT_IMAGE_MISMATCH}")),
            "{script}"
        );
    }

    #[test]
    fn ensure_script_overrides_the_image_entrypoint_with_an_idle_shell() {
        let script = ensure_script(&cfg(), "box");
        assert!(
            script.contains("--entrypoint sh \"$I\" -c 'while :; do sleep 3600; done'"),
            "an image's own entrypoint must not swallow the idle command: {script}"
        );
    }

    #[test]
    fn ensure_script_waits_for_a_concurrent_creator_instead_of_failing() {
        let script = ensure_script(&cfg(), "box");
        assert!(
            script.contains("while [ -z \"$st\" ] && [ $n -lt 20 ]"),
            "a lost creation race must poll for the winner's container: {script}"
        );
    }

    #[test]
    fn ensure_script_starts_a_stopped_container_instead_of_recreating_it() {
        let script = ensure_script(&cfg(), "box");
        assert!(script.contains("docker start"), "{script}");
    }

    #[test]
    fn interpret_ensure_failure_is_actionable() {
        let m = interpret_ensure_failure("box", Some(42), "container box exists but...", "docker");
        assert!(m.starts_with("container box:"), "{m}");
        let m = interpret_ensure_failure("box", Some(127), "sh: 1: docker: not found", "docker");
        assert!(m.contains("not installed"), "{m}");
        let m = interpret_ensure_failure(
            "box",
            Some(1),
            "permission denied while trying to connect to the docker API at unix:///var/run/docker.sock",
            "docker",
        );
        assert!(
            m.contains("docker` group") || m.contains("docker group"),
            "{m}"
        );
        let m = interpret_ensure_failure(
            "box",
            Some(1),
            "Cannot connect to the Docker daemon at unix:///var/run/docker.sock.",
            "docker",
        );
        assert!(m.contains("not running"), "{m}");
    }
}
