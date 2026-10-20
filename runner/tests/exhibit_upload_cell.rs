//! The `ralphus-runner` binary delivering a live run's exhibits: an `upload`
//! spec with the forge token only in its environment, against a stand-in
//! GitHub, as the daemon sends one to the machine that ran the live run.

use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};

use ralphus_runner::exhibit_upload::{
    ExhibitWork, RunRef, SlotOutcome, TOKEN_ENV, UploadFile, UploadForge, UploadRequest,
    UploadTarget,
};

/// The cache root `ralphus_core::cache_dir()` resolves to with `home` as every
/// candidate location.
fn cache_root(home: &Path) -> PathBuf {
    if cfg!(windows) {
        home.join("ralphus")
    } else if cfg!(target_os = "macos") {
        home.join("Library").join("Caches").join("ralphus")
    } else {
        home.join("ralphus")
    }
}

/// Answer one upload request with `reply` (the repository lookup is skipped:
/// the id is given), returning its request line and `Authorization` header.
fn stand_in_github(
    reply: &'static str,
) -> (String, std::thread::JoinHandle<(String, Option<String>)>) {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let handle = std::thread::spawn(move || {
        let (stream, _) = listener.accept().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let mut auth = None;
        let mut length = 0;
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).unwrap();
            let header = header.trim_end().to_string();
            if header.is_empty() {
                break;
            }
            let (name, value) = header.split_once(':').unwrap();
            match name.trim().to_ascii_lowercase().as_str() {
                "authorization" => auth = Some(value.trim().to_string()),
                "content-length" => length = value.trim().parse().unwrap(),
                _ => {}
            }
        }
        let mut body = vec![0; length];
        reader.read_exact(&mut body).unwrap();
        let mut stream = stream;
        write!(
            stream,
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{reply}",
            reply.len()
        )
        .unwrap();
        (line.trim_end().to_string(), auth)
    });
    (base, handle)
}

#[test]
fn the_runner_binary_uploads_a_runs_capture_with_the_token_from_its_environment() {
    let home = std::env::temp_dir().join(format!("ralphus-exhibit-cell-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&home);
    let exhibits = cache_root(&home).join("exhibits");
    let run_dir = ralphus_runner::exhibit_upload::run_dir(&exhibits, "g-1", "run-1");
    std::fs::create_dir_all(&run_dir).unwrap();
    std::fs::write(run_dir.join("view.png"), b"\x89PNG").unwrap();

    let (base, github) =
        stand_in_github(r#"{"url": "https://github.com/user-attachments/assets/xyz"}"#);
    let work = ExhibitWork::Upload(UploadRequest {
        run: RunRef {
            guardian_id: "g-1".into(),
            run_id: "run-1".into(),
        },
        target: Some(UploadTarget {
            forge: UploadForge::Github,
            api_base: base,
            repo_path: "acme/w".into(),
            repository_id: Some(9),
        }),
        target_problem: None,
        files: vec![UploadFile {
            action_idx: 0,
            slot_idx: 0,
            requested: ralphus_core::exhibit_recipes::ExhibitKind::Screenshot,
            file: "view.png".into(),
        }],
        kind_preference: vec![ralphus_core::exhibit_recipes::ExhibitKind::Screenshot],
        max_video_seconds: 15,
        keep_undelivered: false,
    });
    let spec = serde_json::json!({
        "squad_id": "live-g-1",
        "task": "live-action",
        "cell_id": "c-exhibit-upload",
        "cwd": home.to_string_lossy(),
        "exhibit": work,
    });
    let spec_path = home.join("spec.json");
    std::fs::write(&spec_path, spec.to_string()).unwrap();

    let out = std::process::Command::new(env!("CARGO_BIN_EXE_ralphus-runner"))
        .arg("send")
        .arg(&spec_path)
        .env(TOKEN_ENV, "forge-token")
        .env("LOCALAPPDATA", &home)
        .env("XDG_CACHE_HOME", &home)
        .env("HOME", &home)
        .output()
        .unwrap();
    let stdout = String::from_utf8_lossy(&out.stdout);
    let result: serde_json::Value = stdout
        .lines()
        .rev()
        .find_map(|l| serde_json::from_str(l).ok())
        .unwrap_or_else(|| panic!("no result in {stdout}"));
    assert_eq!(result["status"], "done", "{result}");
    let outcomes: Vec<SlotOutcome> =
        serde_json::from_str(result["summary"].as_str().unwrap()).unwrap();
    assert_eq!(
        outcomes[0].url.as_deref(),
        Some("https://github.com/user-attachments/assets/xyz")
    );
    let (line, auth) = github.join().unwrap();
    assert!(line.starts_with("POST /user-attachments/assets?"), "{line}");
    assert_eq!(auth.as_deref(), Some("Bearer forge-token"));
    assert!(!run_dir.exists(), "the capture is gone once uploaded");
    let _ = std::fs::remove_dir_all(&home);
}
