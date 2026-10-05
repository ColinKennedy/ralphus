//! Optional verbs this provider skips must say so in the one form the daemon
//! recognises ("does not implement"), not as a bare unknown verb. Needs no
//! SSH host: the reply is decided before any connection is opened.

use std::process::{Command, Stdio};

fn reply(verb: &str) -> serde_json::Value {
    let output = Command::new(env!("CARGO_BIN_EXE_ralphus-ssh-provider"))
        .args([verb, "--uri", "ssh:nowhere.invalid"])
        .stdin(Stdio::null())
        .output()
        .expect("run ralphus-ssh-provider");
    serde_json::from_slice(&output.stdout).expect("one JSON envelope on stdout")
}

#[test]
fn retire_is_declined_in_the_form_the_daemon_reads_as_an_opt_out() {
    // `remote_runner::ProviderRunner::retire` maps an error containing "does
    // not implement" to `OptedOut`; anything else is recorded as a failed
    // retirement and retried on every daily sweep.
    let value = reply("retire");
    assert_eq!(value["ok"], false);
    let error = value["error"].as_str().unwrap_or_default();
    assert!(error.contains("does not implement"), "{error}");
}

#[test]
fn channel_is_declined_the_same_way() {
    let value = reply("channel");
    assert!(
        value["error"]
            .as_str()
            .unwrap_or_default()
            .contains("does not implement")
    );
}
