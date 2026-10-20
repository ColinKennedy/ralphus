//! Getting exhibits from a live run into a PR/MR description: reading what a
//! run reported into its action's slots ([`record_run_reports`]), then, when a
//! description is published, uploading each captured file through the PR's
//! own forge client ([`upload_pending`]).
//!
//! Uploads go through the forge's REST API first. On GitHub, when that fails
//! and `gh` (2.99 or later, logged in) is available, `gh pr edit --attach`
//! is the fallback; any text `gh` adds to the description is removed again,
//! so the daemon stays the only writer of what the description says. When
//! nothing can upload a file, the slot says so and the file stays on the
//! board.

use std::path::{Path, PathBuf};

use crate::exhibit_media::{Prepared, Transcoder};
use crate::exhibits::{
    ExhibitFill, ExhibitKind, ExhibitSettings, ExhibitSlot, SlotKey, SlotReport,
};

/// Longest text an `output` exhibit keeps.
const MAX_TEXT_CHARS: usize = 4_000;

/// Where exhibits are kept: `state_dir()/exhibits`.
#[must_use]
pub fn exhibits_root() -> PathBuf {
    #[cfg(test)]
    {
        if let Some(root) = TEST_ROOT.with(|r| r.borrow().clone()) {
            return root;
        }
    }
    crate::state_dir().join("exhibits")
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

/// The directory a live run saves its exhibits into.
#[must_use]
pub fn run_dir(root: &Path, guardian_id: &str, run_id: &str) -> PathBuf {
    root.join(safe(guardian_id)).join("runs").join(safe(run_id))
}

fn kept_dir(root: &Path, guardian_id: &str) -> PathBuf {
    root.join(safe(guardian_id)).join("kept")
}

fn safe(value: &str) -> String {
    value
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// What [`record_run_reports`] did, for the run's log row.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
pub struct RecordSummary {
    pub delivered: Vec<String>,
    pub missing: Vec<String>,
}

/// Record a run's reports into its action's slots. Every slot of `slots`
/// gets a new fill -- a re-run replaces what an earlier run delivered -- and
/// a slot the run did not report is recorded as missing.
#[allow(clippy::too_many_arguments)]
pub fn record_run_reports(
    store: &crate::store_lock::StoreHandle,
    guardian_id: &str,
    run_id: &str,
    slots: &[ExhibitSlot],
    reports: &[SlotReport],
    dir: &Path,
    root: &Path,
    settings: &ExhibitSettings,
    ffmpeg: &dyn Transcoder,
) -> RecordSummary {
    let mut summary = RecordSummary::default();
    let kept = kept_dir(root, guardian_id);
    let _ = std::fs::create_dir_all(&kept);
    for slot in slots {
        let label = crate::exhibits::slot_label(slot.key);
        let report = reports.iter().find(|r| r.key() == slot.key);
        let mut fill = ExhibitFill {
            run_id: Some(run_id.to_string()),
            updated_at_ms: crate::store::now_ms(),
            ..ExhibitFill::default()
        };
        match report {
            None => {
                fill.missing_reason = Some("the live run did not report it".to_string());
            }
            Some(SlotReport::Missing { reason, .. }) => {
                fill.missing_reason = Some(reason.clone());
            }
            Some(SlotReport::File { file, caption, .. }) => {
                fill.caption.clone_from(caption);
                match crate::exhibits::resolve_reported_file(dir, file) {
                    None => {
                        fill.missing_reason = Some(format!(
                            "reported file {file:?} is not in the run's exhibit directory"
                        ));
                    }
                    Some(source) => {
                        let stem = kept.join(format!(
                            "{}-{}-{}",
                            safe(run_id),
                            slot.key.action_idx,
                            slot.key.slot_idx
                        ));
                        let preference = preference_for(slot.request.kind, settings);
                        match crate::exhibit_media::prepare(
                            &source,
                            &stem,
                            &preference,
                            settings.max_video_seconds,
                            MAX_TEXT_CHARS,
                            ffmpeg,
                        ) {
                            Prepared::Text { text } => {
                                fill.delivered_kind = Some(ExhibitKind::Output);
                                fill.text = Some(text);
                            }
                            Prepared::File { path, kind } => match keep(&path, &stem) {
                                Ok(kept_path) => {
                                    fill.delivered_kind = Some(kind);
                                    fill.local_path = Some(kept_path.to_string_lossy().to_string());
                                }
                                Err(e) => {
                                    fill.missing_reason =
                                        Some(format!("could not keep the captured file: {e}"));
                                }
                            },
                            Prepared::Undeliverable { reason } => {
                                fill.missing_reason = Some(reason);
                            }
                        }
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

/// The kinds a slot may be delivered as: an `output` slot only as text, a
/// visual one in the review's preference order.
fn preference_for(requested: ExhibitKind, settings: &ExhibitSettings) -> Vec<ExhibitKind> {
    if requested == ExhibitKind::Output {
        vec![ExhibitKind::Output]
    } else {
        settings.kind_preference.clone()
    }
}

/// Copy a prepared file into the kept directory (a converted file already is).
fn keep(path: &Path, stem: &Path) -> std::io::Result<PathBuf> {
    if path.parent() == stem.parent() {
        return Ok(path.to_path_buf());
    }
    let ext = path.extension().unwrap_or_default();
    let dest = stem.with_extension(ext);
    std::fs::copy(path, &dest)?;
    Ok(dest)
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
    let ctype = crate::exhibit_media::content_type(local);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exhibit_media::tests::TestDir;
    use crate::exhibits::{ExhibitOrigin, ExhibitRequest};

    struct NoFfmpeg;
    impl Transcoder for NoFfmpeg {
        fn available(&self) -> bool {
            false
        }
        fn run(&self, _: &[String]) -> Result<(), String> {
            Err("no ffmpeg".into())
        }
        fn duration_seconds(&self, _: &Path) -> Option<f64> {
            None
        }
    }

    #[test]
    fn attachment_urls_are_found_in_markdown_and_bare_lines() {
        let text = "x ![a](https://github.com/user-attachments/assets/1) y\nhttps://github.com/user-attachments/assets/2\n<img src=\"https://github.com/user-attachments/assets/3\">";
        assert_eq!(attachment_urls(text).len(), 3);
    }

    #[test]
    fn a_runs_reports_fill_its_slots_and_unreported_ones_are_missing() {
        let store = crate::store::Store::open_in_memory().unwrap();
        let gid = store.create_guardian("r", "main", "/repo").unwrap();
        store
            .add_exhibit_requests(
                &gid,
                0,
                ExhibitOrigin::Handoff,
                &[
                    (
                        ExhibitRequest {
                            kind: ExhibitKind::Gif,
                            describe: "moving".into(),
                        },
                        None,
                    ),
                    (
                        ExhibitRequest {
                            kind: ExhibitKind::Output,
                            describe: "cli".into(),
                        },
                        None,
                    ),
                    (
                        ExhibitRequest {
                            kind: ExhibitKind::Screenshot,
                            describe: "end".into(),
                        },
                        None,
                    ),
                ],
            )
            .unwrap();
        let handle = std::sync::Arc::new(crate::store_lock::StoreMutex::new(store));
        let slots = handle.lock().guardian_exhibit_slots(&gid).unwrap();
        let root = TestDir::new();
        let dir = run_dir(root.path(), &gid, "run-1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("a.gif"), b"GIF89a").unwrap();
        std::fs::write(dir.join("out.txt"), b"hello").unwrap();
        let reports = vec![
            SlotReport::File {
                key: slots[0].key,
                file: "a.gif".into(),
                caption: Some("see it".into()),
            },
            SlotReport::File {
                key: slots[1].key,
                file: "out.txt".into(),
                caption: None,
            },
        ];
        let summary = record_run_reports(
            &handle,
            &gid,
            "run-1",
            &slots,
            &reports,
            &dir,
            root.path(),
            &ExhibitSettings::default(),
            &NoFfmpeg,
        );
        assert_eq!(
            summary.delivered,
            vec!["0.0".to_string(), "0.1".to_string()]
        );
        assert_eq!(summary.missing, vec!["0.2".to_string()]);
        let now = handle.lock().guardian_exhibit_slots(&gid).unwrap();
        let gif = now[0].fill.as_ref().unwrap();
        assert_eq!(gif.delivered_kind, Some(ExhibitKind::Gif));
        let kept = gif.local_path.as_deref().unwrap();
        assert!(Path::new(kept).is_file());
        assert!(kept.contains("kept"));
        assert_eq!(now[1].fill.as_ref().unwrap().text.as_deref(), Some("hello"));
        assert_eq!(
            now[2].fill.as_ref().unwrap().missing_reason.as_deref(),
            Some("the live run did not report it")
        );
    }

    #[test]
    fn a_report_pointing_outside_the_run_directory_is_refused() {
        let store = crate::store::Store::open_in_memory().unwrap();
        let gid = store.create_guardian("r", "main", "/repo").unwrap();
        store
            .add_exhibit_requests(
                &gid,
                0,
                ExhibitOrigin::Handoff,
                &[(
                    ExhibitRequest {
                        kind: ExhibitKind::Screenshot,
                        describe: "end".into(),
                    },
                    None,
                )],
            )
            .unwrap();
        let handle = std::sync::Arc::new(crate::store_lock::StoreMutex::new(store));
        let slots = handle.lock().guardian_exhibit_slots(&gid).unwrap();
        let root = TestDir::new();
        let dir = run_dir(root.path(), &gid, "run-1");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(root.path().join("secret.png"), b"x").unwrap();
        let reports = vec![SlotReport::File {
            key: slots[0].key,
            file: "../../../secret.png".into(),
            caption: None,
        }];
        record_run_reports(
            &handle,
            &gid,
            "run-1",
            &slots,
            &reports,
            &dir,
            root.path(),
            &ExhibitSettings::default(),
            &NoFfmpeg,
        );
        let now = handle.lock().guardian_exhibit_slots(&gid).unwrap();
        let fill = now[0].fill.as_ref().unwrap();
        assert!(fill.local_path.is_none());
        assert!(
            fill.missing_reason
                .as_deref()
                .unwrap()
                .contains("not in the run's exhibit directory")
        );
    }
}
