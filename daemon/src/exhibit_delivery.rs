//! Getting exhibits from a live run into a PR/MR description.
//!
//! The machine that ran the live run prepares and uploads its captures itself
//! (`ralphus_runner::exhibit_upload`), so a capture reaches the forge
//! straight from where it was made. The daemon only says which files to
//! deliver ([`upload_files`]) and records what came back ([`record_run`]):
//! a URL, a text excerpt, or why a slot has nothing.
//!
//! A capture stays on disk only for a run on the daemon's own machine whose
//! upload could not happen yet (no forge resolved, the upload failed): it is
//! kept in this machine's exhibit cache and [`upload_pending`] retries it
//! when a PR/MR description is published. On GitHub, when that REST upload
//! fails and `gh` (2.99 or later, logged in) is available, `gh pr edit
//! --attach` is the fallback; any text `gh` adds to the description is
//! removed again, so the daemon stays the only writer of what the
//! description says. When nothing can upload a file, the slot says so.

use std::path::{Path, PathBuf};

use ralphus_runner::exhibit_upload::{SlotOutcome, UploadFile};

use crate::exhibits::{ExhibitFill, ExhibitSlot, SlotKey, SlotReport};

/// This machine's exhibit cache (`ralphus_runner::exhibit_upload::exhibits_root`),
/// where a live run on the daemon's own machine saves its captures.
#[must_use]
pub fn exhibits_root() -> PathBuf {
    #[cfg(test)]
    {
        if let Some(root) = TEST_ROOT.with(|r| r.borrow().clone()) {
            return root;
        }
    }
    ralphus_runner::exhibit_upload::exhibits_root()
}

#[cfg(test)]
thread_local! {
    static TEST_ROOT: std::cell::RefCell<Option<PathBuf>> = const { std::cell::RefCell::new(None) };
}

/// Redirect this thread's exhibit storage to `root` for a test.
#[cfg(test)]
pub fn set_test_root(root: &Path) {
    TEST_ROOT.with(|r| *r.borrow_mut() = Some(root.to_path_buf()));
}

/// What [`record_run`] did, for the run's log row.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct RecordSummary {
    pub delivered: Vec<String>,
    pub missing: Vec<String>,
}

/// The reported files among `reports` that belong to `slots`, to deliver.
#[must_use]
pub fn upload_files(slots: &[ExhibitSlot], reports: &[SlotReport]) -> Vec<UploadFile> {
    reports
        .iter()
        .filter_map(|report| match report {
            SlotReport::File { key, file, .. } => {
                let slot = slots.iter().find(|s| s.key == *key)?;
                Some(UploadFile {
                    action_idx: key.action_idx,
                    slot_idx: key.slot_idx,
                    requested: slot.request.kind,
                    file: file.clone(),
                })
            }
            SlotReport::Missing { .. } => None,
        })
        .collect()
}

/// Record a run's exhibits into its action's slots: every slot of `slots`
/// gets a new fill -- a re-run replaces what an earlier run delivered. A
/// reported file takes what its delivery `outcomes` say; one with no outcome
/// is missing for `undelivered` (why the delivery step did not answer), and
/// a slot the run did not report is missing too.
pub fn record_run(
    store: &crate::store_lock::StoreHandle,
    guardian_id: &str,
    run_id: &str,
    slots: &[ExhibitSlot],
    reports: &[SlotReport],
    outcomes: &[SlotOutcome],
    undelivered: &str,
) -> RecordSummary {
    let mut summary = RecordSummary::default();
    for slot in slots {
        let label = crate::exhibits::slot_label(slot.key);
        let mut fill = ExhibitFill {
            run_id: Some(run_id.to_string()),
            updated_at_ms: crate::store::now_ms(),
            ..ExhibitFill::default()
        };
        match reports.iter().find(|r| r.key() == slot.key) {
            None => fill.missing_reason = Some("the live run did not report it".to_string()),
            Some(SlotReport::Missing { reason, .. }) => fill.missing_reason = Some(reason.clone()),
            Some(SlotReport::File { caption, .. }) => {
                fill.caption.clone_from(caption);
                match outcomes.iter().find(|o| {
                    o.action_idx == slot.key.action_idx && o.slot_idx == slot.key.slot_idx
                }) {
                    None => fill.missing_reason = Some(undelivered.to_string()),
                    Some(outcome) => {
                        fill.delivered_kind = outcome.delivered_kind;
                        fill.url.clone_from(&outcome.url);
                        fill.forge_upload_id.clone_from(&outcome.upload_id);
                        fill.text.clone_from(&outcome.text);
                        fill.missing_reason.clone_from(&outcome.missing_reason);
                        fill.local_path.clone_from(&outcome.kept_path);
                    }
                }
            }
        }
        if fill.missing_reason.is_some() {
            summary.missing.push(label);
        } else {
            summary.delivered.push(label);
        }
        let _ = store.lock().set_exhibit_fill(guardian_id, slot.key, &fill);
    }
    summary
}

// ---- uploading at publish time ---------------------------------------------

/// Upload every captured, not-yet-uploaded file among `slots` through
/// `client`, and delete uploads those files replaced once the new ones are
/// in. Called just before a PR/MR description is rendered, so a slot shown
/// on a PR carries a URL that PR's forge serves.
pub fn upload_pending(
    store: &crate::store_lock::StoreHandle,
    guardian_id: &str,
    pr_number: i64,
    client: &crate::forge::ForgeClient,
    slots: &[ExhibitSlot],
) {
    for slot in slots {
        let Some(fill) = &slot.fill else {
            continue;
        };
        match (fill.url.as_deref(), fill.local_path.as_deref()) {
            (None, Some(local)) => {
                let uploaded = upload_one(
                    store,
                    guardian_id,
                    pr_number,
                    client,
                    slot.key,
                    Path::new(local),
                );
                if uploaded && !fill.stale_upload_ids.is_empty() {
                    delete_stale(store, guardian_id, client, slot.key, &fill.stale_upload_ids);
                }
            }
            // Delivered (or replaced by text / a missing note): old uploads
            // can go.
            _ if !fill.stale_upload_ids.is_empty() => {
                delete_stale(store, guardian_id, client, slot.key, &fill.stale_upload_ids);
            }
            _ => {}
        }
    }
}

fn upload_one(
    store: &crate::store_lock::StoreHandle,
    guardian_id: &str,
    pr_number: i64,
    client: &crate::forge::ForgeClient,
    key: SlotKey,
    local: &Path,
) -> bool {
    let bytes = match std::fs::read(local) {
        Ok(bytes) => bytes,
        Err(e) => {
            set_upload_problem(
                store,
                guardian_id,
                key,
                &format!("the captured file is gone: {e}"),
            );
            return false;
        }
    };
    let name = local
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "exhibit".to_string());
    let ctype = ralphus_runner::exhibit_media::content_type(local);
    let direct = client.upload_attachment(&name, ctype, &bytes);
    let uploaded = match direct {
        Ok(up) => Ok((up.url, up.upload_id, "api")),
        Err(api_error) => match client.kind() {
            crate::forge::ForgeKind::GitHub => match gh_attach_status(&client.web_host()) {
                Ok(()) => gh_attach(client, pr_number, local)
                    .map(|url| (url, None, "gh"))
                    .map_err(|gh_error| format!("{api_error}; gh --attach: {gh_error}")),
                Err(why) => Err(format!("{api_error}; gh fallback unavailable: {why}")),
            },
            crate::forge::ForgeKind::GitLab => Err(api_error),
        },
    };
    let guard = store.lock();
    match uploaded {
        Ok((url, upload_id, via)) => {
            let _ = guard.record_exhibit_upload(guardian_id, key, &url, upload_id.as_deref());
            // The forge has it now; the cached copy is no longer needed.
            let _ = std::fs::remove_file(local);
            crate::cartographer::Note::new("guardian")
                .guardian(guardian_id)
                .scope("live_action")
                .emit(
                    &guard,
                    "exhibit uploaded",
                    serde_json::json!({ "slot": key, "url": url, "via": via, "bytes": bytes.len() }),
                );
            true
        }
        Err(message) => {
            crate::rlog!(
                WARNING,
                "ralphus [guardian] review {guardian_id} exhibit {} upload failed: {message}",
                crate::exhibits::slot_label(key)
            );
            crate::cartographer::Note::new("guardian")
                .guardian(guardian_id)
                .scope("live_action")
                .emit(
                    &guard,
                    "exhibit upload failed",
                    serde_json::json!({ "slot": key, "error": message }),
                );
            drop(guard);
            set_upload_problem(
                store,
                guardian_id,
                key,
                &format!("upload failed: {message}"),
            );
            false
        }
    }
}

fn set_upload_problem(
    store: &crate::store_lock::StoreHandle,
    guardian_id: &str,
    key: SlotKey,
    reason: &str,
) {
    let guard = store.lock();
    let Ok(slots) = guard.guardian_exhibit_slots(guardian_id) else {
        return;
    };
    let Some(mut fill) = slots
        .into_iter()
        .find(|s| s.key == key)
        .and_then(|s| s.fill)
    else {
        return;
    };
    fill.missing_reason = Some(reason.to_string());
    let _ = guard.set_exhibit_fill(guardian_id, key, &fill);
}

fn delete_stale(
    store: &crate::store_lock::StoreHandle,
    guardian_id: &str,
    client: &crate::forge::ForgeClient,
    key: SlotKey,
    stale: &[String],
) {
    let mut removed = Vec::new();
    for id in stale {
        match client.kind() {
            // GitHub cannot delete an attachment; the old file stays
            // reachable only to people who can see the repository.
            crate::forge::ForgeKind::GitHub => removed.push(id.clone()),
            crate::forge::ForgeKind::GitLab => {
                if client.delete_attachment(id).is_ok() {
                    removed.push(id.clone());
                }
            }
        }
    }
    if !removed.is_empty() {
        let _ = store
            .lock()
            .clear_stale_exhibit_uploads(guardian_id, key, &removed);
    }
}

// ---- the gh fallback --------------------------------------------------------

/// Whether `gh pr edit --attach` can be used against `host`: `gh` is on PATH,
/// at least `ralphus_core::gh_attach::MIN_VERSION` and logged in there. `Err` says which
/// check failed.
///
/// # Errors
///
/// The first check that failed, in words.
pub fn gh_attach_status(host: &str) -> Result<(), String> {
    let out = std::process::Command::new("gh")
        .arg("--version")
        .output()
        .map_err(|_| "gh is not installed (not on PATH)".to_string())?;
    let text = String::from_utf8_lossy(&out.stdout);
    let version = ralphus_core::gh_attach::parse_version(&text)
        .ok_or_else(|| "could not read gh's version".to_string())?;
    if !ralphus_core::gh_attach::supports_attach(version) {
        return Err(format!(
            "gh {} is older than {}, the first with --attach",
            ralphus_core::gh_attach::format_version(version),
            ralphus_core::gh_attach::format_version(ralphus_core::gh_attach::MIN_VERSION)
        ));
    }
    let auth = std::process::Command::new("gh")
        .args(["auth", "status", "--hostname", host])
        .output()
        .map_err(|e| format!("could not run gh auth status: {e}"))?;
    if !auth.status.success() {
        return Err(format!(
            "gh is not logged in to {host} (run `gh auth login`)"
        ));
    }
    Ok(())
}

/// The `user-attachments` URLs in `text`.
fn attachment_urls(text: &str) -> Vec<String> {
    text.split(|c: char| c.is_whitespace() || matches!(c, '(' | ')' | '"' | '<' | '>' | '\''))
        .filter(|w| w.starts_with("https://github.com/user-attachments/"))
        .map(str::to_string)
        .collect()
}

/// Upload `file` with `gh pr edit --attach`, returning its URL. `gh` writes
/// the file into the description; the description is put back exactly as it
/// was before, so only the daemon's own rendering decides what it says.
fn gh_attach(
    client: &crate::forge::ForgeClient,
    pr_number: i64,
    file: &Path,
) -> Result<String, String> {
    let before = client.get_pull_request_description(pr_number)?;
    let out = std::process::Command::new("gh")
        .args([
            "pr",
            "edit",
            &pr_number.to_string(),
            "-R",
            client.repo_label(),
            "--attach",
        ])
        .arg(file)
        .output()
        .map_err(|e| format!("could not run gh: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let after = client.get_pull_request_description(pr_number)?;
    let known = attachment_urls(&before);
    let url = attachment_urls(&after)
        .into_iter()
        .find(|u| !known.contains(u))
        .ok_or_else(|| "gh --attach added no attachment URL to the description".to_string())?;
    client.update_pull_request_description(pr_number, &before)?;
    Ok(url)
}

/// A scratch directory removed when dropped, for tests.
#[cfg(test)]
pub(crate) struct TestDir(PathBuf);

#[cfg(test)]
impl TestDir {
    pub(crate) fn new() -> Self {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = std::env::temp_dir().join(format!(
            "ralphus-exhibits-{}-{}-{}",
            std::process::id(),
            crate::store::now_ms(),
            N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Self(dir)
    }

    pub(crate) fn path(&self) -> &Path {
        &self.0
    }
}

#[cfg(test)]
impl Drop for TestDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exhibits::{ExhibitKind, ExhibitOrigin, ExhibitRequest};

    #[test]
    fn the_cache_root_is_never_the_state_directory() {
        assert!(!ralphus_core::cache_dir().starts_with(crate::state_dir()));
    }

    #[test]
    fn attachment_urls_are_found_in_markdown_and_bare_lines() {
        let text = "x ![a](https://github.com/user-attachments/assets/1) y
https://github.com/user-attachments/assets/2
<img src=\"https://github.com/user-attachments/assets/3\">";
        assert_eq!(attachment_urls(text).len(), 3);
    }

    fn review_with_slots(kinds: &[ExhibitKind]) -> (crate::store_lock::StoreHandle, String) {
        let store = crate::store::Store::open_in_memory().unwrap();
        let gid = store.create_guardian("r", "main", "/repo").unwrap();
        let requests: Vec<_> = kinds
            .iter()
            .map(|kind| {
                (
                    ExhibitRequest {
                        kind: *kind,
                        describe: kind.as_str().into(),
                    },
                    None,
                )
            })
            .collect();
        store
            .add_exhibit_requests(&gid, 0, ExhibitOrigin::Handoff, &requests)
            .unwrap();
        (
            std::sync::Arc::new(crate::store_lock::StoreMutex::new(store)),
            gid,
        )
    }

    #[test]
    fn only_reported_files_are_sent_for_delivery() {
        let (handle, gid) = review_with_slots(&[ExhibitKind::Gif, ExhibitKind::TerminalText]);
        let slots = handle.lock().guardian_exhibit_slots(&gid).unwrap();
        let reports = vec![
            SlotReport::File {
                key: slots[0].key,
                file: "a.gif".into(),
                caption: None,
            },
            SlotReport::Missing {
                key: slots[1].key,
                reason: "nothing to show".into(),
            },
        ];
        assert_eq!(
            upload_files(&slots, &reports),
            vec![UploadFile {
                action_idx: 0,
                slot_idx: 0,
                requested: ExhibitKind::Gif,
                file: "a.gif".into(),
            }]
        );
    }

    #[test]
    fn a_runs_outcomes_fill_its_slots_and_unreported_ones_are_missing() {
        let (handle, gid) = review_with_slots(&[
            ExhibitKind::Gif,
            ExhibitKind::TerminalText,
            ExhibitKind::Screenshot,
            ExhibitKind::Video,
        ]);
        handle
            .lock()
            .add_exhibit_requests(
                &gid,
                1,
                ExhibitOrigin::Handoff,
                &[(
                    ExhibitRequest {
                        kind: ExhibitKind::Screenshot,
                        describe: "another action's".into(),
                    },
                    None,
                )],
            )
            .unwrap();
        let slots = handle.lock().guardian_exhibit_slots(&gid).unwrap();
        let file = |i: usize, caption: Option<&str>| SlotReport::File {
            key: slots[i].key,
            file: format!("{i}.bin"),
            caption: caption.map(str::to_string),
        };
        let reports = vec![
            file(0, Some("see it")),
            file(1, None),
            file(3, None),
            file(4, None),
        ];
        let outcomes = vec![
            SlotOutcome {
                action_idx: 0,
                slot_idx: 0,
                delivered_kind: Some(ExhibitKind::Gif),
                url: Some("https://forge/a.gif".into()),
                upload_id: Some("7".into()),
                ..SlotOutcome::default()
            },
            SlotOutcome {
                action_idx: 0,
                slot_idx: 1,
                delivered_kind: Some(ExhibitKind::TerminalText),
                text: Some("hello".into()),
                ..SlotOutcome::default()
            },
            SlotOutcome {
                action_idx: 0,
                slot_idx: 3,
                delivered_kind: Some(ExhibitKind::Video),
                kept_path: Some("/cache/k.mp4".into()),
                upload_error: Some("no token".into()),
                ..SlotOutcome::default()
            },
        ];
        let summary = record_run(
            &handle,
            &gid,
            "run-1",
            &slots,
            &reports,
            &outcomes,
            "the upload step failed",
        );
        assert_eq!(summary.delivered, vec!["0.0", "0.1", "0.3"]);
        assert_eq!(summary.missing, vec!["0.2", "1.0"]);
        let now = handle.lock().guardian_exhibit_slots(&gid).unwrap();
        let gif = now[0].fill.as_ref().unwrap();
        assert_eq!(gif.url.as_deref(), Some("https://forge/a.gif"));
        assert_eq!(gif.forge_upload_id.as_deref(), Some("7"));
        assert_eq!(gif.caption.as_deref(), Some("see it"));
        assert!(gif.local_path.is_none(), "an uploaded file is not kept");
        assert_eq!(now[1].fill.as_ref().unwrap().text.as_deref(), Some("hello"));
        assert_eq!(
            now[2].fill.as_ref().unwrap().missing_reason.as_deref(),
            Some("the live run did not report it")
        );
        let kept = now[3].fill.as_ref().unwrap();
        assert_eq!(kept.local_path.as_deref(), Some("/cache/k.mp4"));
        assert!(kept.url.is_none() && kept.missing_reason.is_none());
        assert_eq!(
            now[4].fill.as_ref().unwrap().missing_reason.as_deref(),
            Some("the upload step failed")
        );
    }
}
