//! A live run's exhibits on the machine that ran it: the directory the agent
//! saves captures into, and delivering those captures -- trimmed to the
//! forges' size limits ([`crate::exhibit_media`]) and uploaded straight to the
//! forge -- so a capture leaves this machine only as the upload itself.
//!
//! The daemon asks for both through a cell spec's `exhibit` field
//! ([`ExhibitWork`]): `capture` on the live run's own spec (the run's
//! directory is created here and substituted for [`DIR_PLACEHOLDER`] in the
//! prompt), `upload` once the run is done. The forge token reaches this
//! process only through its environment ([`TOKEN_ENV`]), never the spec.

use std::path::{Path, PathBuf};
use std::time::Duration;

use ralphus_core::exhibit_recipes::ExhibitKind;
use serde::{Deserialize, Serialize};

use crate::exhibit_media::{Prepared, Transcoder};

/// The environment variable an `upload` request's forge token arrives in.
pub const TOKEN_ENV: &str = "RALPHUS_EXHIBIT_UPLOAD_TOKEN";

/// Stands for the run's exhibit directory in a `capture` spec's prompt; the
/// runner replaces it with the directory it created on its own machine.
pub const DIR_PLACEHOLDER: &str = "<<RALPHUS_EXHIBIT_DIR>>";

/// Longest text an `output` exhibit keeps.
pub const MAX_TEXT_CHARS: usize = 4_000;

/// How long a capture may stay in the cache before it is pruned, uploaded or
/// not.
pub const MAX_CACHE_AGE: Duration = Duration::from_secs(14 * 24 * 60 * 60);

/// What a cell spec asks of the runner about exhibits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExhibitWork {
    /// Create the run's exhibit directory and put its path into the prompt.
    Capture(RunRef),
    /// Prepare and upload what the run captured; no agent runs.
    Upload(UploadRequest),
}

/// Which live run's exhibits.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRef {
    pub guardian_id: String,
    pub run_id: String,
}

/// The forge an upload goes to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum UploadForge {
    Github,
    Gitlab,
}

/// Where uploads go: the repository the review's PRs/MRs are filed in.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadTarget {
    pub forge: UploadForge,
    /// The REST API base, e.g. `https://api.github.com` or
    /// `https://gitlab.com/api/v4`.
    pub api_base: String,
    /// `owner/repo` (GitHub) or the URL-encoded project path (GitLab).
    pub repo_path: String,
    /// GitHub's numeric repository id, when the daemon already knows it;
    /// looked up from `repo_path` otherwise.
    #[serde(default)]
    pub repository_id: Option<i64>,
}

/// One reported file to deliver.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadFile {
    pub action_idx: i64,
    pub slot_idx: i64,
    /// The kind the slot asked for.
    pub requested: ExhibitKind,
    /// The file name the agent reported, inside the run's directory.
    pub file: String,
}

/// Everything a run captured that should reach the PR/MR.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UploadRequest {
    pub run: RunRef,
    /// `None` when there is nowhere to upload to; `target_problem` says why.
    #[serde(default)]
    pub target: Option<UploadTarget>,
    #[serde(default)]
    pub target_problem: Option<String>,
    pub files: Vec<UploadFile>,
    /// Visual kinds in the review's preference order.
    pub kind_preference: Vec<ExhibitKind>,
    pub max_video_seconds: u32,
    /// Keep a prepared file that could not be uploaded (in the review's kept
    /// directory) instead of dropping it, so the daemon can retry once the
    /// PR exists. Only for runs on the daemon's own machine.
    #[serde(default)]
    pub keep_undelivered: bool,
}

/// What became of one reported file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SlotOutcome {
    pub action_idx: i64,
    pub slot_idx: i64,
    #[serde(default)]
    pub delivered_kind: Option<ExhibitKind>,
    /// The uploaded file's URL.
    #[serde(default)]
    pub url: Option<String>,
    /// The forge's id for the upload, when it can delete uploads (GitLab).
    #[serde(default)]
    pub upload_id: Option<String>,
    /// An `output` exhibit's text.
    #[serde(default)]
    pub text: Option<String>,
    /// Why nothing was delivered.
    #[serde(default)]
    pub missing_reason: Option<String>,
    /// A prepared file kept for a later upload (`keep_undelivered`).
    #[serde(default)]
    pub kept_path: Option<String>,
    /// Why the upload did not happen now, when the file was kept instead.
    #[serde(default)]
    pub upload_error: Option<String>,
}

/// An upload the forge accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Uploaded {
    /// The URL a description embeds.
    pub url: String,
    /// The forge's id for the upload, when it can be deleted (GitLab).
    pub upload_id: Option<String>,
}

// ---- where captures live ------------------------------------------------------

/// This machine's exhibit storage: the per-user cache directory
/// (`ralphus_core::cache_dir()`), never the state directory.
#[must_use]
pub fn exhibits_root() -> PathBuf {
    ralphus_core::cache_dir().join("exhibits")
}

/// The directory a live run saves its exhibits into.
#[must_use]
pub fn run_dir(root: &Path, guardian_id: &str, run_id: &str) -> PathBuf {
    root.join(safe(guardian_id)).join("runs").join(safe(run_id))
}

/// Where a review's prepared files wait for a later upload.
#[must_use]
pub fn kept_dir(root: &Path, guardian_id: &str) -> PathBuf {
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

/// Delete every file under `root` last modified more than `max_age` ago, and
/// any directory left empty. Best-effort: anything that cannot be read or
/// removed is skipped. Returns how many files were deleted.
pub fn prune_cache(root: &Path, max_age: Duration) -> usize {
    fn walk(dir: &Path, cutoff: std::time::SystemTime, removed: &mut usize) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(meta) = entry.metadata() else {
                continue;
            };
            if meta.is_dir() {
                walk(&path, cutoff, removed);
                // Only succeeds once the directory is empty.
                let _ = std::fs::remove_dir(&path);
            } else if meta.modified().is_ok_and(|m| m < cutoff)
                && std::fs::remove_file(&path).is_ok()
            {
                *removed += 1;
            }
        }
    }
    let Some(cutoff) = std::time::SystemTime::now().checked_sub(max_age) else {
        return 0;
    };
    let mut removed = 0;
    walk(root, cutoff, &mut removed);
    removed
}

/// The file a report names, when it really is a regular file directly inside
/// `dir` (no `..`, no absolute path, no symlink escaping it).
#[must_use]
pub fn resolve_reported_file(dir: &Path, file: &str) -> Option<PathBuf> {
    let name = Path::new(file).file_name()?;
    if Path::new(file).components().count() != 1 {
        return None;
    }
    let path = dir.join(name);
    let canonical_dir = dir.canonicalize().ok()?;
    let canonical = path.canonicalize().ok()?;
    (canonical.parent() == Some(canonical_dir.as_path()) && canonical.is_file())
        .then_some(canonical)
}

/// Create `run`'s exhibit directory under `root` and put its path into
/// `prompt` in place of [`DIR_PLACEHOLDER`].
///
/// # Errors
///
/// A message when the directory cannot be created.
pub fn prepare_capture(root: &Path, run: &RunRef, prompt: &str) -> Result<String, String> {
    prune_cache(root, MAX_CACHE_AGE);
    let dir = run_dir(root, &run.guardian_id, &run.run_id);
    std::fs::create_dir_all(&dir).map_err(|e| {
        format!(
            "could not create the exhibit directory {}: {e}",
            dir.display()
        )
    })?;
    Ok(prompt.replace(DIR_PLACEHOLDER, &dir.to_string_lossy()))
}

// ---- delivering ---------------------------------------------------------------

/// Prepare and upload every file of `request` from its run's directory under
/// `root`, then remove that directory. A file that cannot be uploaded is
/// reported missing, or kept for later when `request.keep_undelivered`.
#[must_use]
pub fn deliver(
    request: &UploadRequest,
    token: Option<&str>,
    root: &Path,
    transcoder: &dyn Transcoder,
) -> Vec<SlotOutcome> {
    let run = &request.run;
    let dir = run_dir(root, &run.guardian_id, &run.run_id);
    let outcomes = request
        .files
        .iter()
        .map(|file| deliver_one(request, file, token, root, &dir, transcoder))
        .collect();
    if dir.starts_with(root) && dir != root {
        let _ = std::fs::remove_dir_all(&dir);
    }
    prune_cache(root, MAX_CACHE_AGE);
    outcomes
}

fn deliver_one(
    request: &UploadRequest,
    file: &UploadFile,
    token: Option<&str>,
    root: &Path,
    dir: &Path,
    transcoder: &dyn Transcoder,
) -> SlotOutcome {
    let mut outcome = SlotOutcome {
        action_idx: file.action_idx,
        slot_idx: file.slot_idx,
        ..SlotOutcome::default()
    };
    let Some(source) = resolve_reported_file(dir, &file.file) else {
        outcome.missing_reason = Some(format!(
            "reported file {:?} is not in the run's exhibit directory",
            file.file
        ));
        return outcome;
    };
    let stem = dir.join(format!("prepared-{}-{}", file.action_idx, file.slot_idx));
    let preference = if file.requested == ExhibitKind::Output {
        vec![ExhibitKind::Output]
    } else {
        request.kind_preference.clone()
    };
    match crate::exhibit_media::prepare(
        &source,
        &stem,
        &preference,
        request.max_video_seconds,
        MAX_TEXT_CHARS,
        transcoder,
    ) {
        Prepared::Text { text } => {
            outcome.delivered_kind = Some(ExhibitKind::Output);
            outcome.text = Some(text);
        }
        Prepared::Undeliverable { reason } => outcome.missing_reason = Some(reason),
        Prepared::File { path, kind } => {
            outcome.delivered_kind = Some(kind);
            let uploaded = match (&request.target, token) {
                (Some(target), Some(token)) => upload_file(target, token, &path),
                (Some(_), None) => Err("no forge token reached the runner".to_string()),
                (None, _) => Err(request
                    .target_problem
                    .clone()
                    .unwrap_or_else(|| "there is no forge to upload to".to_string())),
            };
            match uploaded {
                Ok(up) => {
                    outcome.url = Some(up.url);
                    outcome.upload_id = up.upload_id;
                }
                Err(e) if request.keep_undelivered => {
                    let name = format!(
                        "{}-{}-{}",
                        safe(&request.run.run_id),
                        file.action_idx,
                        file.slot_idx
                    );
                    match keep(&path, &kept_dir(root, &request.run.guardian_id), &name) {
                        Ok(kept) => {
                            outcome.kept_path = Some(kept.to_string_lossy().to_string());
                            outcome.upload_error = Some(e);
                        }
                        Err(keep_error) => {
                            outcome.missing_reason = Some(format!(
                                "upload failed: {e}; could not keep the file: {keep_error}"
                            ));
                        }
                    }
                }
                Err(e) => outcome.missing_reason = Some(format!("upload failed: {e}")),
            }
        }
    }
    outcome
}

/// Copy a prepared file into `kept` as `<name>.<ext>`.
fn keep(path: &Path, kept: &Path, name: &str) -> std::io::Result<PathBuf> {
    std::fs::create_dir_all(kept)?;
    let dest = kept
        .join(name)
        .with_extension(path.extension().unwrap_or_default());
    std::fs::copy(path, &dest)?;
    Ok(dest)
}

fn upload_file(target: &UploadTarget, token: &str, path: &Path) -> Result<Uploaded, String> {
    let bytes =
        std::fs::read(path).map_err(|e| format!("could not read {}: {e}", path.display()))?;
    let name = path.file_name().map_or_else(
        || "exhibit".to_string(),
        |n| n.to_string_lossy().to_string(),
    );
    upload_attachment(
        target,
        token,
        &name,
        crate::exhibit_media::content_type(path),
        &bytes,
    )
}

/// Run an `upload` request as a cell: the token comes from [`TOKEN_ENV`], and
/// the outcomes are the result's `summary`, as a JSON array.
#[must_use]
pub fn run_upload_cell(request: &UploadRequest) -> crate::spec::CellResult {
    let token = std::env::var(TOKEN_ENV).ok().filter(|t| !t.is_empty());
    let outcomes = deliver(
        request,
        token.as_deref(),
        &exhibits_root(),
        &crate::exhibit_media::Ffmpeg,
    );
    match serde_json::to_string(&outcomes) {
        Ok(json) => crate::spec::CellResult::done(json),
        Err(e) => crate::spec::CellResult::failed(format!("could not report the uploads: {e}"), ""),
    }
}

// ---- the forge ----------------------------------------------------------------

fn http_agent() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(Duration::from_secs(30))
        .timeout(Duration::from_secs(600))
        .build()
}

/// Upload a file a PR/MR description can embed.
///
/// GitHub: the `user-attachments` upload a description's drag-and-drop uses,
/// sent with the token (`uploads.github.com/user-attachments/assets`). It
/// accepts PNG, GIF, WebP, SVG, MP4, WebM and MOV only, images up to 10 MiB
/// and video up to 100 MiB (paid plans), and returns no deletable id.
/// GitLab: project uploads (`POST /projects/:id/uploads`), any type up to 100
/// MiB, deletable by id. Neither needs the PR/MR to exist yet. GitLab's
/// upload path is made absolute (see [`gitlab_upload_url`]), so the file shows
/// in an MR filed in another project of the same fork network too.
///
/// # Errors
///
/// A message naming the forge's answer when the upload is rejected.
pub fn upload_attachment(
    target: &UploadTarget,
    token: &str,
    name: &str,
    content_type: &str,
    bytes: &[u8],
) -> Result<Uploaded, String> {
    let api_base = target.api_base.trim_end_matches('/');
    match target.forge {
        UploadForge::Github => {
            let repository_id = match target.repository_id {
                Some(id) => id,
                None => github_repository_id(api_base, &target.repo_path, token)?,
            };
            let url = format!(
                "{}/user-attachments/assets?name={}&content_type={}&repository_id={repository_id}",
                github_uploads_base(api_base),
                percent_encode_query(name),
                percent_encode_query(content_type),
            );
            let value = send_bytes(
                http_agent()
                    .post(&url)
                    .set("Authorization", &format!("Bearer {token}"))
                    .set("Accept", "application/json"),
                content_type,
                bytes,
            )?;
            let url = value["url"]
                .as_str()
                .or_else(|| value["href"].as_str())
                .ok_or_else(|| "GitHub attachment upload returned no URL".to_string())?;
            Ok(Uploaded {
                url: url.to_string(),
                upload_id: None,
            })
        }
        UploadForge::Gitlab => {
            let url = format!("{api_base}/projects/{}/uploads", target.repo_path);
            let boundary = format!(
                "ralphus-{:x}",
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map_or(0, |d| d.as_nanos())
            );
            let safe_name = name.replace(['"', '\r', '\n'], "_");
            let mut body = format!(
                "--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{safe_name}\"\r\nContent-Type: {content_type}\r\n\r\n"
            )
            .into_bytes();
            body.extend_from_slice(bytes);
            body.extend_from_slice(format!("\r\n--{boundary}--\r\n").as_bytes());
            let value = send_bytes(
                http_agent().post(&url).set("PRIVATE-TOKEN", token),
                &format!("multipart/form-data; boundary={boundary}"),
                &body,
            )?;
            let url = gitlab_upload_url(api_base, &target.repo_path, &value)
                .ok_or_else(|| "GitLab upload returned no URL".to_string())?;
            Ok(Uploaded {
                url,
                upload_id: value["id"].as_i64().map(|id| id.to_string()),
            })
        }
    }
}

fn github_repository_id(api_base: &str, repo_path: &str, token: &str) -> Result<i64, String> {
    let url = format!("{api_base}/repos/{repo_path}");
    let response = http_agent()
        .get(&url)
        .set("Authorization", &format!("Bearer {token}"))
        .set("Accept", "application/vnd.github+json")
        .call()
        .map_err(describe)?;
    let value = read_json(response)?;
    value["id"]
        .as_i64()
        .ok_or_else(|| format!("GitHub repo {repo_path} has no numeric id"))
}

fn send_bytes(
    request: ureq::Request,
    content_type: &str,
    bytes: &[u8],
) -> Result<serde_json::Value, String> {
    let response = request
        .set("Content-Type", content_type)
        .send_bytes(bytes)
        .map_err(describe)?;
    read_json(response)
}

fn read_json(response: ureq::Response) -> Result<serde_json::Value, String> {
    let text = response
        .into_string()
        .map_err(|e| format!("could not read the forge's answer: {e}"))?;
    serde_json::from_str(&text).map_err(|e| format!("the forge returned unreadable JSON: {e}"))
}

fn describe(error: ureq::Error) -> String {
    match error {
        ureq::Error::Status(code, response) => {
            let url = response.get_url().to_string();
            let body: String = response
                .into_string()
                .unwrap_or_default()
                .chars()
                .take(300)
                .collect();
            format!("forge API {code} from {url}: {}", body.trim())
        }
        ureq::Error::Transport(t) => format!("could not reach the forge: {t}"),
    }
}

/// Where GitHub takes attachment uploads for `api_base`: `uploads.github.com`
/// for github.com, otherwise `api_base` itself (a test forge).
fn github_uploads_base(api_base: &str) -> String {
    if api_base == "https://api.github.com" {
        "https://uploads.github.com".to_string()
    } else {
        api_base.to_string()
    }
}

/// The absolute URL of a GitLab upload, from the upload's API answer.
///
/// `full_path` is the path the web server actually serves the file at
/// (`/-/project/<id>/uploads/<secret>/<file>` on current GitLab, where the
/// `<namespace>/<project>/uploads/...` form answers 404), so it is preferred
/// and only needs the web base in front. A server that sends no `full_path`
/// gets its project-relative `url` (`/uploads/<secret>/<file>`) placed under
/// the project's web URL. A URL that is already absolute is kept as is.
fn gitlab_upload_url(api_base: &str, repo_path: &str, value: &serde_json::Value) -> Option<String> {
    let web_base = api_base.strip_suffix("/api/v4").unwrap_or(api_base);
    if let Some(full_path) = value["full_path"].as_str().filter(|p| !p.is_empty()) {
        if !full_path.starts_with('/') {
            return Some(full_path.to_string());
        }
        return Some(format!("{web_base}{full_path}"));
    }
    let url = value["url"].as_str()?;
    if !url.starts_with('/') {
        return Some(url.to_string());
    }
    let project = repo_path.replace("%2F", "/").replace("%2f", "/");
    Some(format!("{web_base}/{project}{url}"))
}

/// Percent-encode a query-string value.
fn percent_encode_query(value: &str) -> String {
    let mut out = String::new();
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Read, Write};

    /// A scratch directory removed when dropped.
    struct TestDir(PathBuf);

    impl TestDir {
        fn new() -> Self {
            static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let dir = std::env::temp_dir().join(format!(
                "ralphus-exhibit-upload-{}-{}",
                std::process::id(),
                N.fetch_add(1, std::sync::atomic::Ordering::SeqCst)
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            Self(dir)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

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

    /// One request the mock forge received.
    #[derive(Debug)]
    struct Seen {
        line: String,
        headers: Vec<(String, String)>,
        body: Vec<u8>,
    }

    impl Seen {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(name))
                .map(|(_, v)| v.as_str())
        }
    }

    /// Serve `replies` in order, one request per connection, and hand back
    /// what was asked.
    fn mock_forge(replies: Vec<&'static str>) -> (String, std::thread::JoinHandle<Vec<Seen>>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let base = format!("http://{}", listener.local_addr().unwrap());
        let handle = std::thread::spawn(move || {
            let mut seen = Vec::new();
            for reply in replies {
                let (stream, _) = listener.accept().unwrap();
                let mut reader = BufReader::new(stream.try_clone().unwrap());
                let mut line = String::new();
                reader.read_line(&mut line).unwrap();
                let mut headers = Vec::new();
                loop {
                    let mut header = String::new();
                    reader.read_line(&mut header).unwrap();
                    let header = header.trim_end();
                    if header.is_empty() {
                        break;
                    }
                    let (name, value) = header.split_once(':').unwrap();
                    headers.push((name.trim().to_string(), value.trim().to_string()));
                }
                let length = headers
                    .iter()
                    .find(|(n, _)| n.eq_ignore_ascii_case("content-length"))
                    .map_or(0, |(_, v)| v.parse().unwrap());
                let mut body = vec![0; length];
                reader.read_exact(&mut body).unwrap();
                let mut stream = stream;
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
                    reply.len()
                )
                .unwrap();
                seen.push(Seen {
                    line: line.trim_end().to_string(),
                    headers,
                    body,
                });
            }
            seen
        });
        (base, handle)
    }

    fn run() -> RunRef {
        RunRef {
            guardian_id: "g-1".into(),
            run_id: "run-1".into(),
        }
    }

    fn request(target: Option<UploadTarget>, files: Vec<UploadFile>) -> UploadRequest {
        UploadRequest {
            run: run(),
            target,
            target_problem: Some("no forge here".into()),
            files,
            kind_preference: vec![
                ExhibitKind::Gif,
                ExhibitKind::Video,
                ExhibitKind::Screenshot,
            ],
            max_video_seconds: 15,
            keep_undelivered: false,
        }
    }

    fn file(slot_idx: i64, requested: ExhibitKind, name: &str) -> UploadFile {
        UploadFile {
            action_idx: 0,
            slot_idx,
            requested,
            file: name.into(),
        }
    }

    /// The run's directory under `root`, holding `files`.
    fn captured(root: &Path, files: &[(&str, &[u8])]) -> PathBuf {
        let dir = run_dir(root, "g-1", "run-1");
        std::fs::create_dir_all(&dir).unwrap();
        for (name, bytes) in files {
            std::fs::write(dir.join(name), bytes).unwrap();
        }
        dir
    }

    #[test]
    fn capture_creates_the_runs_directory_and_names_it_in_the_prompt() {
        let root = TestDir::new();
        let prompt = format!("save into {DIR_PLACEHOLDER} please");
        let got = prepare_capture(root.path(), &run(), &prompt).unwrap();
        let dir = run_dir(root.path(), "g-1", "run-1");
        assert!(dir.is_dir());
        assert_eq!(got, format!("save into {} please", dir.display()));
    }

    #[test]
    fn a_reported_file_must_sit_directly_in_the_run_directory() {
        let dir = TestDir::new();
        std::fs::write(dir.path().join("ok.png"), b"x").unwrap();
        std::fs::create_dir_all(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("sub").join("deep.png"), b"x").unwrap();
        assert!(resolve_reported_file(dir.path(), "ok.png").is_some());
        assert!(resolve_reported_file(dir.path(), "sub/deep.png").is_none());
        assert!(resolve_reported_file(dir.path(), "../ok.png").is_none());
        assert!(resolve_reported_file(dir.path(), "missing.png").is_none());
        assert!(
            resolve_reported_file(dir.path(), "sub").is_none(),
            "a directory is not a file"
        );
    }

    #[test]
    fn pruning_removes_only_old_files_and_empty_folders() {
        let root = TestDir::new();
        let old_dir = root.path().join("g").join("kept");
        std::fs::create_dir_all(&old_dir).unwrap();
        let old = old_dir.join("old.png");
        let fresh = root.path().join("g").join("fresh.png");
        std::fs::write(&old, b"x").unwrap();
        std::fs::write(&fresh, b"x").unwrap();
        let long_ago = std::time::SystemTime::now() - Duration::from_secs(30 * 24 * 3600);
        std::fs::File::options()
            .write(true)
            .open(&old)
            .unwrap()
            .set_modified(long_ago)
            .unwrap();
        assert_eq!(prune_cache(root.path(), MAX_CACHE_AGE), 1);
        assert!(!old.exists());
        assert!(!old_dir.exists(), "the emptied folder goes too");
        assert!(fresh.exists());
    }

    #[test]
    fn github_uploads_go_straight_to_the_forge_and_the_capture_is_removed() {
        let root = TestDir::new();
        let dir = captured(root.path(), &[("view.png", b"\x89PNG")]);
        let (base, forge) = mock_forge(vec![
            r#"{"id": 9}"#,
            r#"{"url": "https://github.com/user-attachments/assets/abc"}"#,
        ]);
        let target = UploadTarget {
            forge: UploadForge::Github,
            api_base: base,
            repo_path: "acme/w".into(),
            repository_id: None,
        };
        let req = request(
            Some(target),
            vec![file(0, ExhibitKind::Screenshot, "view.png")],
        );
        let got = deliver(&req, Some("tok"), root.path(), &NoFfmpeg);
        assert_eq!(
            got,
            vec![SlotOutcome {
                action_idx: 0,
                slot_idx: 0,
                delivered_kind: Some(ExhibitKind::Screenshot),
                url: Some("https://github.com/user-attachments/assets/abc".into()),
                ..SlotOutcome::default()
            }]
        );
        let seen = forge.join().unwrap();
        assert_eq!(seen[0].line, "GET /repos/acme/w HTTP/1.1");
        assert!(
            seen[1].line.starts_with(
                "POST /user-attachments/assets?name=view.png&content_type=image%2Fpng&repository_id=9 "
            ),
            "{}",
            seen[1].line
        );
        assert_eq!(seen[1].header("authorization"), Some("Bearer tok"));
        assert_eq!(seen[1].body, b"\x89PNG");
        assert!(!dir.exists(), "the run's captures are gone once delivered");
    }

    #[test]
    fn gitlab_uploads_go_straight_to_the_forge_with_an_absolute_url() {
        let root = TestDir::new();
        captured(root.path(), &[("a.gif", b"GIF89a")]);
        let (base, forge) = mock_forge(vec![
            r#"{"id": 5, "url": "/uploads/s3cr3t/a.gif", "full_path": "/-/project/42/uploads/s3cr3t/a.gif"}"#,
        ]);
        let target = UploadTarget {
            forge: UploadForge::Gitlab,
            api_base: format!("{base}/api/v4"),
            repo_path: "acme%2Fw".into(),
            repository_id: None,
        };
        let req = request(Some(target), vec![file(0, ExhibitKind::Gif, "a.gif")]);
        let got = deliver(&req, Some("tok"), root.path(), &NoFfmpeg);
        assert_eq!(
            got[0].url.as_deref(),
            Some(format!("{base}/-/project/42/uploads/s3cr3t/a.gif").as_str())
        );
        assert_eq!(got[0].upload_id.as_deref(), Some("5"));
        let seen = forge.join().unwrap();
        assert_eq!(
            seen[0].line,
            "POST /api/v4/projects/acme%2Fw/uploads HTTP/1.1"
        );
        assert_eq!(seen[0].header("private-token"), Some("tok"));
        let body = String::from_utf8_lossy(&seen[0].body);
        assert!(
            body.contains("filename=\"a.gif\"") && body.contains("GIF89a"),
            "{body}"
        );
    }

    #[test]
    fn text_is_delivered_inline_and_bad_reports_are_missing() {
        let root = TestDir::new();
        captured(root.path(), &[("out.txt", b"hello")]);
        std::fs::write(root.path().join("secret.png"), b"x").unwrap();
        let req = request(
            None,
            vec![
                file(0, ExhibitKind::Output, "out.txt"),
                file(1, ExhibitKind::Screenshot, "../../../secret.png"),
            ],
        );
        let got = deliver(&req, None, root.path(), &NoFfmpeg);
        assert_eq!(got[0].text.as_deref(), Some("hello"));
        assert_eq!(got[0].delivered_kind, Some(ExhibitKind::Output));
        assert!(
            got[1]
                .missing_reason
                .as_deref()
                .unwrap()
                .contains("not in the run's exhibit directory")
        );
    }

    #[test]
    fn an_undelivered_file_is_kept_only_when_asked() {
        let root = TestDir::new();
        captured(root.path(), &[("a.gif", b"GIF89a")]);
        let mut req = request(None, vec![file(0, ExhibitKind::Gif, "a.gif")]);
        let dropped = deliver(&req, Some("tok"), root.path(), &NoFfmpeg);
        assert_eq!(
            dropped[0].missing_reason.as_deref(),
            Some("upload failed: no forge here")
        );
        assert!(dropped[0].kept_path.is_none());

        captured(root.path(), &[("a.gif", b"GIF89a")]);
        req.keep_undelivered = true;
        let kept = deliver(&req, Some("tok"), root.path(), &NoFfmpeg);
        assert!(kept[0].missing_reason.is_none());
        assert_eq!(kept[0].upload_error.as_deref(), Some("no forge here"));
        let path = PathBuf::from(kept[0].kept_path.as_deref().unwrap());
        assert!(path.is_file());
        assert!(path.starts_with(kept_dir(root.path(), "g-1")));
    }

    #[test]
    fn gitlab_upload_url_prefers_full_path() {
        let answer = serde_json::json!({
            "url": "/uploads/x/a.png",
            "full_path": "/-/project/86439269/uploads/x/a.png",
        });
        assert_eq!(
            gitlab_upload_url("https://gitlab.com/api/v4", "g%2Fp", &answer).as_deref(),
            Some("https://gitlab.com/-/project/86439269/uploads/x/a.png")
        );
        assert_eq!(
            gitlab_upload_url(
                "https://gitlab.example/sub/api/v4",
                "grp%2Fproj",
                &serde_json::json!({"full_path": "/-/project/7/uploads/x/a.png"})
            )
            .as_deref(),
            Some("https://gitlab.example/sub/-/project/7/uploads/x/a.png")
        );
    }

    #[test]
    fn gitlab_upload_url_without_full_path_uses_the_project_url() {
        assert_eq!(
            gitlab_upload_url(
                "https://gitlab.example/sub/api/v4",
                "grp%2Fproj",
                &serde_json::json!({"url": "/uploads/x/a.png"})
            )
            .as_deref(),
            Some("https://gitlab.example/sub/grp/proj/uploads/x/a.png")
        );
        assert_eq!(
            gitlab_upload_url(
                "https://gitlab.com/api/v4",
                "g%2Fp",
                &serde_json::json!({"url": "https://cdn/a.png"})
            )
            .as_deref(),
            Some("https://cdn/a.png")
        );
        assert_eq!(
            gitlab_upload_url("https://gitlab.com/api/v4", "g%2Fp", &serde_json::json!({})),
            None
        );
    }

    #[test]
    fn the_work_round_trips_through_json() {
        let work = ExhibitWork::Upload(request(
            Some(UploadTarget {
                forge: UploadForge::Github,
                api_base: "https://api.github.com".into(),
                repo_path: "acme/w".into(),
                repository_id: Some(1),
            }),
            vec![file(0, ExhibitKind::Gif, "a.gif")],
        ));
        let json = serde_json::to_value(&work).unwrap();
        assert_eq!(json["upload"]["target"]["forge"], "github");
        assert_eq!(serde_json::from_value::<ExhibitWork>(json).unwrap(), work);
        let capture = serde_json::to_value(ExhibitWork::Capture(run())).unwrap();
        assert_eq!(capture["capture"]["run_id"], "run-1");
    }
}
