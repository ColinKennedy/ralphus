//! Container mode's effect on the commands the provider sends over ssh, with
//! no live host: every remote command must be wrapped, and the few host-side
//! ones must not be.
//!
//! One test, because container mode is process-wide state that the first
//! `configure` call fixes for the whole test binary.

use ralphus_ssh_provider::container::{self, ContainerConfig};
use ralphus_ssh_provider::ssh;

#[test]
fn once_configured_every_command_args_call_runs_inside_the_container() {
    assert!(container::active().is_none(), "off until configured");
    let before = ssh::command_args("alice@host", 10, "echo hi", None);
    assert_eq!(before.last().unwrap(), "echo hi", "unwrapped when off");

    container::configure(ContainerConfig {
        image: "img:1".to_string(),
        name: Some("work".to_string()),
        ..ContainerConfig::default()
    });

    let wrapped = ssh::command_args("alice@host", 10, "cd /w && run 'x'", None);
    assert_eq!(
        wrapped.last().unwrap(),
        "docker exec -i 'work' sh -c 'cd /w && run '\\''x'\\'''"
    );
    // The ssh-level shape is unchanged: target, then `--`, then one command.
    assert_eq!(wrapped[wrapped.len() - 2], "--");
    assert_eq!(wrapped[wrapped.len() - 3], "alice@host");

    let host = ssh::host_command_args("alice@host", 10, "docker ps", None);
    assert_eq!(
        host.last().unwrap(),
        "docker ps",
        "host-side commands (container management) are never wrapped"
    );

    // The terminal path asks for a tty.
    let tty = container::wrap_active("alice@host", "claude --resume x", true);
    assert!(tty.starts_with("docker exec -it 'work' sh -c "), "{tty}");
}
