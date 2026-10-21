//! `ralphus-outpost install` / `uninstall` / `status`: set a machine up
//! once so its outpost starts by itself from then on.
//!
//! `install --fleet <url> --code <code>` pairs with the fleet
//! ([`crate::pairing`]), keeps the machine credential in the OS's encrypted
//! per-user store ([`crate::settings`]), copies this executable into the
//! outpost's home, and registers it to start automatically:
//!
//! | Platform | Default (`install`) | `install --service` |
//! |---|---|---|
//! | Windows | a per-user Scheduled Task at logon, no admin needed | a Scheduled Task at boot that runs as you whether or not you are logged on (elevated prompt; Windows asks for your password once and keeps it, ralphus never sees it) |
//! | Linux | a `systemd --user` unit (falling back to an XDG autostart entry where there is no user systemd) | the same unit plus `loginctl enable-linger`, so it starts at boot |
//! | macOS | a launchd agent | not supported |
//!
//! Each started outpost runs `ralphus-outpost serve --home <home>`, which
//! reconnects forever and logs to `<home>/logs/outpost.log`. The registered
//! definitions are built by pure functions ([`plan_install`]), so tests (and
//! `install --dry-run <dir>`) can write them somewhere harmless and read
//! them back without registering anything.

use std::path::{Path, PathBuf};
use std::process::Command;

use ralphus_fleet::outpost_keys;
use ralphus_fleet::secret_store::{OsStore, os_store_name};

use crate::settings::{CREDENTIAL_SERVICE, Home, Settings};

/// The Scheduled Task / unit / agent name.
pub const SERVICE_NAME: &str = "ralphus-outpost";
const LAUNCHD_LABEL: &str = "com.ralphus.outpost";

/// Which operating system's autostart mechanism to plan for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Platform {
    Windows,
    Linux,
    Macos,
}

impl Platform {
    #[must_use]
    pub fn current() -> Self {
        if cfg!(windows) {
            Self::Windows
        } else if cfg!(target_os = "macos") {
            Self::Macos
        } else {
            Self::Linux
        }
    }
}

/// Where the user's per-user configuration lives, for autostart files.
#[derive(Debug, Clone)]
pub struct UserDirs {
    /// `$XDG_CONFIG_HOME` (`~/.config`) on Linux; `~` on macOS; unused on
    /// Windows.
    pub config_home: PathBuf,
    /// `DOMAIN\user` on Windows, the login name elsewhere.
    pub user: String,
}

impl UserDirs {
    /// This user's directories.
    ///
    /// # Errors
    /// `HOME` (or `USERNAME` on Windows) is unset.
    pub fn current() -> Result<Self, String> {
        let var = |name: &str| std::env::var(name).ok().filter(|v| !v.trim().is_empty());
        if cfg!(windows) {
            let user = var("USERNAME").ok_or("USERNAME is not set")?;
            let user = match var("USERDOMAIN") {
                Some(domain) => format!("{domain}\\{user}"),
                None => user,
            };
            return Ok(Self {
                config_home: PathBuf::new(),
                user,
            });
        }
        let home = var("HOME").ok_or("HOME is not set")?;
        let config_home = if cfg!(target_os = "macos") {
            PathBuf::from(&home)
        } else {
            var("XDG_CONFIG_HOME").map_or_else(|| Path::new(&home).join(".config"), PathBuf::from)
        };
        Ok(Self {
            config_home,
            user: var("USER").or_else(|| var("LOGNAME")).unwrap_or_default(),
        })
    }
}

/// A file an install writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlannedFile {
    pub path: PathBuf,
    pub bytes: Vec<u8>,
}

/// What an install or uninstall does: files, then commands.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub files: Vec<PlannedFile>,
    pub commands: Vec<Vec<String>>,
    /// Lines to tell the user.
    pub notes: Vec<String>,
}

fn xml_escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// `text` as UTF-16LE with a byte-order mark: the encoding `schtasks /XML`
/// reads reliably.
#[must_use]
pub fn utf16_with_bom(text: &str) -> Vec<u8> {
    let mut bytes = vec![0xFF, 0xFE];
    for unit in text.encode_utf16() {
        bytes.extend_from_slice(&unit.to_le_bytes());
    }
    bytes
}

/// The Scheduled Task definition. At logon (`boot == false`) it runs in the
/// user's session with their token, so the Credential Manager is
/// reachable; through `conhost --headless` so no console window opens. At
/// boot it runs as the user with a stored password (logon type `Password`).
///
/// A second trigger fires every minute from a start in the past, and a
/// second instance is ignored while one runs: so an outpost that exits or is
/// killed is started again within a minute, without waiting for the next
/// logon or boot. (`RestartOnFailure` alone does not do that -- Task
/// Scheduler reads a killed `conhost` as a run that ended; and a repetition
/// on the logon or boot trigger only starts counting when that trigger next
/// fires.) With the logon principal the minute trigger, like the task, runs
/// only while the user is logged on.
#[must_use]
pub fn windows_task_xml(exe: &Path, home: &Path, user: &str, boot: bool) -> String {
    let user = xml_escape(user);
    let (start, logon_type) = if boot {
        (
            "<BootTrigger><Enabled>true</Enabled></BootTrigger>".to_string(),
            "Password",
        )
    } else {
        (
            format!("<LogonTrigger><Enabled>true</Enabled><UserId>{user}</UserId></LogonTrigger>"),
            "InteractiveToken",
        )
    };
    let trigger = format!(
        "{start}\n    <TimeTrigger><StartBoundary>2000-01-01T00:00:00</StartBoundary>\
         <Enabled>true</Enabled><Repetition><Interval>PT1M</Interval>\
         <StopAtDurationEnd>false</StopAtDurationEnd></Repetition></TimeTrigger>"
    );
    let arguments = xml_escape(&format!(
        "--headless \"{}\" serve --home \"{}\"",
        exe.display(),
        home.display()
    ));
    format!(
        r#"<?xml version="1.0" encoding="UTF-16"?>
<Task version="1.2" xmlns="http://schemas.microsoft.com/windows/2004/02/mit/task">
  <RegistrationInfo>
    <Description>ralphus-outpost: serves this machine to ralphus-fleet</Description>
  </RegistrationInfo>
  <Triggers>
    {trigger}
  </Triggers>
  <Principals>
    <Principal id="Author">
      <UserId>{user}</UserId>
      <LogonType>{logon_type}</LogonType>
      <RunLevel>LeastPrivilege</RunLevel>
    </Principal>
  </Principals>
  <Settings>
    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>
    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>
    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>
    <AllowHardTerminate>true</AllowHardTerminate>
    <StartWhenAvailable>true</StartWhenAvailable>
    <RunOnlyIfNetworkAvailable>false</RunOnlyIfNetworkAvailable>
    <IdleSettings>
      <StopOnIdleEnd>false</StopOnIdleEnd>
      <RestartOnIdle>false</RestartOnIdle>
    </IdleSettings>
    <AllowStartOnDemand>true</AllowStartOnDemand>
    <Enabled>true</Enabled>
    <Hidden>true</Hidden>
    <RunOnlyIfIdle>false</RunOnlyIfIdle>
    <WakeToRun>false</WakeToRun>
    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>
    <Priority>7</Priority>
    <RestartOnFailure>
      <Interval>PT1M</Interval>
      <Count>999</Count>
    </RestartOnFailure>
  </Settings>
  <Actions Context="Author">
    <Exec>
      <Command>%SystemRoot%\System32\conhost.exe</Command>
      <Arguments>{arguments}</Arguments>
    </Exec>
  </Actions>
</Task>
"#
    )
}

fn systemd_quote(path: &Path) -> String {
    format!(
        "\"{}\"",
        path.display()
            .to_string()
            .replace('\\', "\\\\")
            .replace('"', "\\\"")
    )
}

/// The `systemd --user` unit.
#[must_use]
pub fn systemd_unit(exe: &Path, home: &Path) -> String {
    format!(
        "[Unit]\n\
         Description=ralphus-outpost: serves this machine to ralphus-fleet\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         ExecStart={} serve --home {}\n\
         Restart=always\n\
         RestartSec=5\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n",
        systemd_quote(exe),
        systemd_quote(home)
    )
}

/// The XDG autostart entry used where there is no user systemd.
#[must_use]
pub fn xdg_autostart(exe: &Path, home: &Path) -> String {
    format!(
        "[Desktop Entry]\n\
         Type=Application\n\
         Name=ralphus-outpost\n\
         Comment=Serves this machine to ralphus-fleet\n\
         Exec={} serve --home {}\n\
         NoDisplay=true\n\
         X-GNOME-Autostart-enabled=true\n",
        systemd_quote(exe),
        systemd_quote(home)
    )
}

/// The launchd agent.
#[must_use]
pub fn launchd_plist(exe: &Path, home: &Path) -> String {
    let log = home.join("logs").join("launchd.log");
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
  <key>Label</key><string>{LAUNCHD_LABEL}</string>
  <key>ProgramArguments</key>
  <array>
    <string>{}</string>
    <string>serve</string>
    <string>--home</string>
    <string>{}</string>
  </array>
  <key>RunAtLoad</key><true/>
  <key>KeepAlive</key><true/>
  <key>ThrottleInterval</key><integer>10</integer>
  <key>StandardErrorPath</key><string>{}</string>
</dict>
</plist>
"#,
        xml_escape(&exe.display().to_string()),
        xml_escape(&home.display().to_string()),
        xml_escape(&log.display().to_string()),
    )
}

fn strings(argv: &[&str]) -> Vec<String> {
    argv.iter().map(|a| (*a).to_string()).collect()
}

/// The unit file's path on Linux.
#[must_use]
pub fn systemd_unit_path(dirs: &UserDirs) -> PathBuf {
    dirs.config_home
        .join("systemd/user")
        .join(format!("{SERVICE_NAME}.service"))
}

/// The autostart entry's path on Linux.
#[must_use]
pub fn xdg_autostart_path(dirs: &UserDirs) -> PathBuf {
    dirs.config_home
        .join("autostart")
        .join(format!("{SERVICE_NAME}.desktop"))
}

fn launchd_path(dirs: &UserDirs) -> PathBuf {
    dirs.config_home
        .join("Library/LaunchAgents")
        .join(format!("{LAUNCHD_LABEL}.plist"))
}

/// What installing does on `platform`: the autostart definition for `exe`
/// (the installed copy) serving `home`, and the commands that register and
/// start it.
///
/// # Errors
/// `service` was asked for where it is not supported.
pub fn plan_install(
    platform: Platform,
    service: bool,
    exe: &Path,
    home: &Home,
    dirs: &UserDirs,
) -> Result<Plan, String> {
    let mut plan = Plan::default();
    match platform {
        Platform::Windows => {
            let xml_path = home.dir.join(format!("{SERVICE_NAME}-task.xml"));
            plan.files.push(PlannedFile {
                path: xml_path.clone(),
                bytes: utf16_with_bom(&windows_task_xml(exe, &home.dir, &dirs.user, service)),
            });
            let xml = xml_path.to_string_lossy().into_owned();
            let mut create = strings(&["schtasks", "/Create", "/TN", SERVICE_NAME, "/XML"]);
            create.push(xml);
            create.push("/F".to_string());
            if service {
                create.extend(strings(&["/RU", &dirs.user, "/RP", "*"]));
                plan.notes.push(
                    "Windows asks for your password once: the task runs as you at boot, logged \
                     on or not, and Windows (not ralphus) keeps the password"
                        .to_string(),
                );
            }
            plan.commands.push(create);
            plan.commands
                .push(strings(&["schtasks", "/Run", "/TN", SERVICE_NAME]));
        }
        Platform::Linux => {
            plan.files.push(PlannedFile {
                path: systemd_unit_path(dirs),
                bytes: systemd_unit(exe, &home.dir).into_bytes(),
            });
            plan.commands
                .push(strings(&["systemctl", "--user", "daemon-reload"]));
            let unit = format!("{SERVICE_NAME}.service");
            plan.commands
                .push(strings(&["systemctl", "--user", "enable", &unit]));
            // `restart` also starts it, and replaces an outpost a previous
            // install left running with the old credential.
            plan.commands
                .push(strings(&["systemctl", "--user", "restart", &unit]));
            if service {
                plan.commands
                    .push(strings(&["loginctl", "enable-linger", &dirs.user]));
                plan.notes.push(
                    "lingering is on: the outpost starts at boot. If the login keyring is \
                     locked then, it waits and retries until it can read its credential"
                        .to_string(),
                );
            }
        }
        Platform::Macos => {
            if service {
                return Err(
                    "--service is not supported on macOS yet: a launchd daemon runs outside your \
                     login and cannot read your Keychain. Install without --service (a launchd \
                     agent that starts at login)"
                        .to_string(),
                );
            }
            let plist = launchd_path(dirs);
            plan.files.push(PlannedFile {
                path: plist.clone(),
                bytes: launchd_plist(exe, &home.dir).into_bytes(),
            });
            plan.commands.push(vec![
                "launchctl".to_string(),
                "load".to_string(),
                "-w".to_string(),
                plist.to_string_lossy().into_owned(),
            ]);
        }
    }
    Ok(plan)
}

/// What uninstalling does on `platform`: stop and unregister, then remove
/// the definitions.
#[must_use]
pub fn plan_uninstall(platform: Platform, home: &Home, dirs: &UserDirs) -> Plan {
    let mut plan = Plan::default();
    match platform {
        Platform::Windows => {
            plan.commands
                .push(strings(&["schtasks", "/End", "/TN", SERVICE_NAME]));
            plan.commands
                .push(strings(&["schtasks", "/Delete", "/TN", SERVICE_NAME, "/F"]));
            plan.files.push(PlannedFile {
                path: home.dir.join(format!("{SERVICE_NAME}-task.xml")),
                bytes: Vec::new(),
            });
        }
        Platform::Linux => {
            plan.commands.push(strings(&[
                "systemctl",
                "--user",
                "disable",
                "--now",
                &format!("{SERVICE_NAME}.service"),
            ]));
            plan.files.push(PlannedFile {
                path: systemd_unit_path(dirs),
                bytes: Vec::new(),
            });
            plan.files.push(PlannedFile {
                path: xdg_autostart_path(dirs),
                bytes: Vec::new(),
            });
        }
        Platform::Macos => {
            let plist = launchd_path(dirs);
            plan.commands.push(vec![
                "launchctl".to_string(),
                "unload".to_string(),
                "-w".to_string(),
                plist.to_string_lossy().into_owned(),
            ]);
            plan.files.push(PlannedFile {
                path: plist,
                bytes: Vec::new(),
            });
        }
    }
    plan
}

/// Run `argv`, returning its stdout, or why it failed (its stderr).
fn run_command(argv: &[String]) -> Result<String, String> {
    let output = Command::new(&argv[0])
        .args(&argv[1..])
        .output()
        .map_err(|e| format!("could not run {}: {e}", argv[0]))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        Err(format!(
            "`{}` failed: {}",
            argv.join(" "),
            if stderr.trim().is_empty() {
                stdout.trim()
            } else {
                stderr.trim()
            }
        ))
    }
}

/// Run `argv` with this terminal attached (for a password prompt).
fn run_interactive(argv: &[String]) -> Result<(), String> {
    let status = Command::new(&argv[0])
        .args(&argv[1..])
        .status()
        .map_err(|e| format!("could not run {}: {e}", argv[0]))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("`{}` failed ({status})", argv.join(" ")))
    }
}

fn write_file(file: &PlannedFile) -> Result<(), String> {
    if let Some(parent) = file.path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("could not create {}: {e}", parent.display()))?;
    }
    std::fs::write(&file.path, &file.bytes)
        .map_err(|e| format!("could not write {}: {e}", file.path.display()))
}

/// Write `plan`'s files under `dir` (by file name) instead of their real
/// places, and describe its commands: `install --dry-run`.
///
/// # Errors
/// A file could not be written.
pub fn write_dry_run(plan: &Plan, dir: &Path) -> Result<Vec<String>, String> {
    let mut lines = Vec::new();
    for file in &plan.files {
        let name = file.path.file_name().unwrap_or_default();
        let target = PlannedFile {
            path: dir.join(name),
            bytes: file.bytes.clone(),
        };
        write_file(&target)?;
        lines.push(format!(
            "would write {} (written to {})",
            file.path.display(),
            target.path.display()
        ));
    }
    for command in &plan.commands {
        lines.push(format!("would run: {}", command.join(" ")));
    }
    lines.extend(plan.notes.iter().cloned());
    Ok(lines)
}

/// What `install` was asked to do.
#[derive(Debug, Clone)]
pub struct InstallOptions {
    pub fleet_url: String,
    pub code: String,
    /// The `remote_root` the machine's owner expects; must match the
    /// fleet's when given.
    pub remote_root: Option<String>,
    pub service: bool,
    /// Write the definitions here and pair nothing.
    pub dry_run: Option<PathBuf>,
    pub home: Home,
}

fn say(line: &str) {
    eprintln!("ralphus-outpost: {line}");
}

/// Copy this executable into `home`'s `bin/` (unless it already runs from
/// there); the installed path.
fn install_executable(home: &Home) -> Result<PathBuf, String> {
    let current =
        std::env::current_exe().map_err(|e| format!("could not find this executable: {e}"))?;
    let target = home.bin_dir().join(current.file_name().unwrap_or_default());
    if std::fs::canonicalize(&current).ok() == std::fs::canonicalize(&target).ok() {
        return Ok(target);
    }
    std::fs::create_dir_all(home.bin_dir())
        .map_err(|e| format!("could not create {}: {e}", home.bin_dir().display()))?;
    if std::fs::copy(&current, &target).is_err() {
        // A running copy cannot be overwritten on Windows, but it can be
        // renamed out of the way.
        let mut aside = target.as_os_str().to_owned();
        aside.push(format!(".old-{}", std::process::id()));
        std::fs::rename(&target, PathBuf::from(aside))
            .and_then(|()| std::fs::copy(&current, &target).map(|_| ()))
            .map_err(|e| {
                format!(
                    "could not copy this executable to {}: {e}",
                    target.display()
                )
            })?;
    }
    Ok(target)
}

/// `ralphus-outpost install`.
///
/// # Errors
/// Pairing, storing the credential, or registering the autostart failed.
pub fn install(options: &InstallOptions) -> Result<(), String> {
    outpost_keys::normalize_pairing_code(&options.code).ok_or(
        "--code is not a pairing code: expect 16 letters and digits like ABCD-EFGH-JKMN-PQRS \
         (from `ralphus-fleet pair <machine>`)",
    )?;
    let platform = Platform::current();
    let dirs = UserDirs::current()?;
    let home = &options.home;
    if let Some(dir) = &options.dry_run {
        let exe = home.bin_dir().join(
            std::env::current_exe()
                .ok()
                .and_then(|p| p.file_name().map(std::ffi::OsStr::to_os_string))
                .unwrap_or_else(|| SERVICE_NAME.into()),
        );
        let plan = plan_install(platform, options.service, &exe, home, &dirs)?;
        for line in write_dry_run(&plan, dir)? {
            say(&line);
        }
        say("dry run: nothing was paired, stored or registered");
        return Ok(());
    }

    let fleet_url = options.fleet_url.trim_end_matches('/').to_string();
    let paired = crate::pairing::pair(&fleet_url, &options.code)?;
    if let Some(expected) = &options.remote_root {
        let same = |a: &str, b: &str| {
            let (a, b) = (a.replace('\\', "/"), b.replace('\\', "/"));
            if cfg!(windows) {
                a.trim_end_matches('/')
                    .eq_ignore_ascii_case(b.trim_end_matches('/'))
            } else {
                a.trim_end_matches('/') == b.trim_end_matches('/')
            }
        };
        if !same(expected, &paired.remote_root) {
            return Err(format!(
                "the fleet serves {:?} from {:?}, not --remote-root {expected:?}; change \
                 remote_root in ralphus-fleet.toml (then `ralphus-fleet pair {}` again) or omit \
                 --remote-root",
                paired.name, paired.remote_root, paired.name
            ));
        }
    }
    say(&format!(
        "paired with {fleet_url} as {:?} (this machine's key {}, the fleet's {})",
        paired.name,
        outpost_keys::fingerprint(&paired.credential.machine.public_key()),
        outpost_keys::fingerprint(&paired.credential.fleet_public_key),
    ));
    let os = OsStore::new(CREDENTIAL_SERVICE);
    let (kind, warning) = home.store_credential(&os, &paired.credential)?;
    match warning {
        Some(warning) => say(&format!("WARNING: {warning}")),
        None => say(&format!("the machine credential is in {}", os_store_name())),
    }
    let exe = install_executable(home)?;
    home.save(&Settings {
        fleet_url,
        name: paired.name.clone(),
        remote_root: paired.remote_root.clone(),
        credential_store: kind,
    })?;
    register(platform, options.service, &exe, home, &dirs)?;
    say(&format!(
        "installed: {:?} serves {} and starts by itself from now on; `ralphus-outpost status` \
         shows how it is doing, its log is {}",
        paired.name,
        paired.remote_root,
        home.log_file().display()
    ));
    Ok(())
}

/// Write the autostart definition and run its commands; on Linux without a
/// user systemd, fall back to an XDG autostart entry and start it now.
fn register(
    platform: Platform,
    service: bool,
    exe: &Path,
    home: &Home,
    dirs: &UserDirs,
) -> Result<(), String> {
    let plan = plan_install(platform, service, exe, home, dirs)?;
    if platform == Platform::Windows {
        // Stop an outpost a previous install left running with the old
        // credential; the new task's `/Run` starts the new one.
        let _ = run_command(&strings(&["schtasks", "/End", "/TN", SERVICE_NAME]));
    }
    for file in &plan.files {
        write_file(file)?;
    }
    for (i, command) in plan.commands.iter().enumerate() {
        let interactive = platform == Platform::Windows && service && i == 0;
        let result = if interactive {
            run_interactive(command)
        } else {
            run_command(command).map(|_| ())
        };
        if let Err(e) = result {
            if platform == Platform::Linux && command.get(1).map(String::as_str) == Some("--user") {
                say(&format!(
                    "{e}; no user systemd here, so falling back to an XDG autostart entry"
                ));
                return register_xdg_autostart(exe, home, dirs);
            }
            if platform == Platform::Windows && service {
                return Err(format!(
                    "{e}\n--service needs an elevated (Run as administrator) prompt; or install \
                     without --service for a task that starts when you log on"
                ));
            }
            return Err(e);
        }
    }
    for note in &plan.notes {
        say(note);
    }
    Ok(())
}

fn register_xdg_autostart(exe: &Path, home: &Home, dirs: &UserDirs) -> Result<(), String> {
    let _ = std::fs::remove_file(systemd_unit_path(dirs));
    write_file(&PlannedFile {
        path: xdg_autostart_path(dirs),
        bytes: xdg_autostart(exe, &home.dir).into_bytes(),
    })?;
    Command::new(exe)
        .arg("serve")
        .arg("--home")
        .arg(&home.dir)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .map_err(|e| format!("could not start {}: {e}", exe.display()))?;
    say(
        "it starts when you log in to a desktop session (XDG autostart) and is running now; for \
         start at boot install a user systemd",
    );
    Ok(())
}

/// `ralphus-outpost uninstall`: stop and unregister the outpost and forget
/// its credential. Logs, the runner cache and `remote_root` stay.
///
/// # Errors
/// The home directory is unusable.
pub fn uninstall(home: &Home) -> Result<(), String> {
    let platform = Platform::current();
    let dirs = UserDirs::current()?;
    let plan = plan_uninstall(platform, home, &dirs);
    for command in &plan.commands {
        if let Err(e) = run_command(command) {
            say(&format!("{e} (continuing)"));
        }
    }
    for file in &plan.files {
        let _ = std::fs::remove_file(&file.path);
    }
    let name = home.load().ok().flatten().map(|s| s.name);
    home.delete_credential(&OsStore::new(CREDENTIAL_SERVICE));
    let _ = std::fs::remove_file(home.settings_file());
    let _ = std::fs::remove_file(home.status_file());
    say(&format!(
        "uninstalled; the credential is deleted. {} keeps its logs and runner cache; delete it by \
         hand if you like.",
        home.dir.display()
    ));
    if let Some(name) = name {
        say(&format!(
            "on the fleet host, `ralphus-fleet revoke {name}` forgets this machine there too"
        ));
    }
    Ok(())
}

/// Whether the autostart is registered and running, as one line.
fn autostart_state(platform: Platform, dirs: &UserDirs) -> String {
    match platform {
        Platform::Windows => {
            match run_command(&strings(&[
                "schtasks",
                "/Query",
                "/TN",
                SERVICE_NAME,
                "/FO",
                "LIST",
            ])) {
                Ok(text) => text
                    .lines()
                    .find_map(|l| l.trim().strip_prefix("Status:").map(str::trim))
                    .map_or_else(
                        || "Scheduled Task registered".to_string(),
                        |s| format!("Scheduled Task registered, {s}"),
                    ),
                Err(_) => "no Scheduled Task registered".to_string(),
            }
        }
        Platform::Linux => {
            if systemd_unit_path(dirs).exists() {
                let state = run_command(&strings(&[
                    "systemctl",
                    "--user",
                    "is-active",
                    &format!("{SERVICE_NAME}.service"),
                ]))
                .unwrap_or_else(|e| e);
                format!("systemd --user unit, {}", state.trim())
            } else if xdg_autostart_path(dirs).exists() {
                "XDG autostart entry".to_string()
            } else {
                "no autostart registered".to_string()
            }
        }
        Platform::Macos => {
            if launchd_path(dirs).exists() {
                "launchd agent registered".to_string()
            } else {
                "no launchd agent registered".to_string()
            }
        }
    }
}

/// `ralphus-outpost status`: settings, whether the credential is present
/// (never its value), the autostart, the last connection state and the
/// log's tail.
///
/// # Errors
/// The settings are unreadable.
#[allow(clippy::print_stdout)] // the report is this command's product
pub fn status(home: &Home) -> Result<(), String> {
    use std::io::Write as _;
    let mut out = std::io::stdout().lock();
    let Some(settings) = home.load()? else {
        let _ = writeln!(
            out,
            "not installed (no {}); set it up with `ralphus-outpost install --fleet <url> --code \
             <code>`",
            home.settings_file().display()
        );
        return Ok(());
    };
    let _ = writeln!(out, "machine      {}", settings.name);
    let _ = writeln!(out, "fleet        {}", settings.fleet_url);
    let _ = writeln!(out, "remote_root  {}", settings.remote_root);
    let credential =
        home.load_credential(&OsStore::new(CREDENTIAL_SERVICE), settings.credential_store);
    let _ = writeln!(
        out,
        "credential   {}",
        match &credential {
            Ok(c) => format!(
                "present ({:?} store, key {})",
                settings.credential_store,
                outpost_keys::fingerprint(&c.machine.public_key())
            ),
            Err(e) => format!("MISSING: {e}"),
        }
    );
    if let Ok(dirs) = UserDirs::current() {
        let _ = writeln!(
            out,
            "autostart    {}",
            autostart_state(Platform::current(), &dirs)
        );
    }
    let state: serde_json::Value = std::fs::read(home.status_file())
        .ok()
        .and_then(|b| serde_json::from_slice(&b).ok())
        .unwrap_or_default();
    let connection = if state["connected"] == true {
        "connected".to_string()
    } else if state.is_null() {
        "never ran".to_string()
    } else {
        format!(
            "not connected: {}",
            state["error"].as_str().unwrap_or("unknown")
        )
    };
    let _ = writeln!(
        out,
        "connection   {connection} (as of {} ms since the epoch, pid {})",
        state["at_ms"], state["pid"]
    );
    let _ = writeln!(out, "log          {}", home.log_file().display());
    for line in crate::logfile::tail(&home.log_file(), 5) {
        let _ = writeln!(out, "  {line}");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dirs(root: &Path) -> UserDirs {
        UserDirs {
            config_home: root.join("config"),
            user: "DESK\\me".to_string(),
        }
    }

    fn utf16_text(bytes: &[u8]) -> String {
        assert_eq!(&bytes[..2], &[0xFF, 0xFE], "UTF-16LE with a BOM");
        let units: Vec<u16> = bytes[2..]
            .chunks(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        String::from_utf16(&units).unwrap()
    }

    #[test]
    fn a_windows_install_writes_a_logon_task_that_needs_no_admin() {
        let dir = ralphus_test_support::TestDir::new("install-windows");
        let home = Home::new(dir.join("home"));
        let exe = dir.join("home/bin/ralphus-outpost.exe");
        let plan = plan_install(Platform::Windows, false, &exe, &home, &dirs(&dir)).unwrap();
        let lines = write_dry_run(&plan, &dir.join("dry")).unwrap();
        let xml = utf16_text(&std::fs::read(dir.join("dry/ralphus-outpost-task.xml")).unwrap());
        assert!(
            xml.contains("<LogonTrigger><Enabled>true</Enabled>"),
            "{xml}"
        );
        assert!(
            xml.contains(
                "<TimeTrigger><StartBoundary>2000-01-01T00:00:00</StartBoundary>\
                 <Enabled>true</Enabled><Repetition><Interval>PT1M</Interval>"
            ),
            "a stopped outpost is started again within a minute: {xml}"
        );
        assert!(
            xml.contains("<MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>"),
            "a repeating trigger must not start a second outpost: {xml}"
        );
        assert!(xml.contains("<UserId>DESK\\me</UserId>"), "{xml}");
        assert!(
            xml.contains("<LogonType>InteractiveToken</LogonType>"),
            "{xml}"
        );
        assert!(xml.contains("<RunLevel>LeastPrivilege</RunLevel>"), "{xml}");
        assert!(
            xml.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"),
            "{xml}"
        );
        assert!(xml.contains("<RestartOnFailure>"), "{xml}");
        assert!(xml.contains("conhost.exe</Command>"), "{xml}");
        assert!(
            xml.contains(&format!(
                "--headless &quot;{}&quot; serve --home &quot;{}&quot;",
                exe.display(),
                home.dir.display()
            )),
            "{xml}"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("schtasks /Create /TN ralphus-outpost /XML")),
            "{lines:?}"
        );
        assert!(!lines.iter().any(|l| l.contains("/RU")), "{lines:?}");
    }

    #[test]
    fn a_windows_service_install_runs_at_boot_with_a_stored_password() {
        let dir = ralphus_test_support::TestDir::new("install-windows-service");
        let home = Home::new(dir.join("home"));
        let plan = plan_install(
            Platform::Windows,
            true,
            &dir.join("o.exe"),
            &home,
            &dirs(&dir),
        )
        .unwrap();
        let xml = utf16_text(&plan.files[0].bytes);
        assert!(xml.contains("<BootTrigger>"), "{xml}");
        assert!(xml.contains("<LogonType>Password</LogonType>"), "{xml}");
        assert!(plan.commands[0].ends_with(&strings(&["/RU", "DESK\\me", "/RP", "*"])));
    }

    #[test]
    fn a_linux_install_writes_a_restarting_user_unit() {
        let dir = ralphus_test_support::TestDir::new("install-linux");
        let home = Home::new(PathBuf::from("/home/me/.local/state/ralphus/outpost"));
        let exe = PathBuf::from("/home/me/.local/state/ralphus/outpost/bin/ralphus-outpost");
        let user_dirs = dirs(&dir);
        let plan = plan_install(Platform::Linux, false, &exe, &home, &user_dirs).unwrap();
        write_dry_run(&plan, &dir.join("dry")).unwrap();
        let unit = std::fs::read_to_string(dir.join("dry/ralphus-outpost.service")).unwrap();
        assert!(unit.contains(
            "ExecStart=\"/home/me/.local/state/ralphus/outpost/bin/ralphus-outpost\" serve --home \"/home/me/.local/state/ralphus/outpost\""
        ), "{unit}");
        assert!(unit.contains("Restart=always"), "{unit}");
        assert!(unit.contains("WantedBy=default.target"), "{unit}");
        assert_eq!(
            plan.files[0].path,
            user_dirs
                .config_home
                .join("systemd/user/ralphus-outpost.service")
        );
        assert!(plan.commands.contains(&strings(&[
            "systemctl",
            "--user",
            "enable",
            "ralphus-outpost.service"
        ])));
        assert!(plan.commands.contains(&strings(&[
            "systemctl",
            "--user",
            "restart",
            "ralphus-outpost.service"
        ])));

        let service = plan_install(Platform::Linux, true, &exe, &home, &user_dirs).unwrap();
        assert!(
            service
                .commands
                .contains(&strings(&["loginctl", "enable-linger", "DESK\\me"]))
        );
        let autostart = xdg_autostart(&exe, &home.dir);
        assert!(
            autostart.contains(
                "Exec=\"/home/me/.local/state/ralphus/outpost/bin/ralphus-outpost\" serve"
            ),
            "{autostart}"
        );
    }

    #[test]
    fn macos_gets_a_launchd_agent_and_refuses_service() {
        let dir = ralphus_test_support::TestDir::new("install-macos");
        let home = Home::new(dir.join("home"));
        let plan =
            plan_install(Platform::Macos, false, &dir.join("o"), &home, &dirs(&dir)).unwrap();
        let plist = String::from_utf8(plan.files[0].bytes.clone()).unwrap();
        assert!(plist.contains("<key>KeepAlive</key><true/>"), "{plist}");
        assert!(plan_install(Platform::Macos, true, &dir.join("o"), &home, &dirs(&dir)).is_err());
    }

    #[test]
    fn uninstall_unregisters_on_every_platform() {
        let dir = ralphus_test_support::TestDir::new("uninstall-plan");
        let home = Home::new(dir.join("home"));
        let windows = plan_uninstall(Platform::Windows, &home, &dirs(&dir));
        assert!(windows.commands.contains(&strings(&[
            "schtasks",
            "/Delete",
            "/TN",
            "ralphus-outpost",
            "/F"
        ])));
        let linux = plan_uninstall(Platform::Linux, &home, &dirs(&dir));
        assert_eq!(linux.files.len(), 2, "the unit and the autostart fallback");
    }
}
