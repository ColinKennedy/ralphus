//! Making a captured exhibit file fit what a PR/MR description can embed:
//! its kind from the file type, conversion of types the forges reject, and
//! shrinking to the size limits, stepping down the review's kind preference
//! when a file cannot be made to fit.
//!
//! The limits are the stricter of GitHub's and GitLab's, measured live: GitHub
//! takes PNG/GIF/WebP/SVG images up to 10 MiB and MP4/WebM/MOV video up to
//! 100 MiB (paid plans); GitLab takes any file up to 100 MiB. Conversions use
//! `ffmpeg` when it is on PATH; without it a file that already fits is used
//! as is and one that does not is reported as not deliverable.

use std::path::{Path, PathBuf};

use crate::exhibits::ExhibitKind;

/// Largest image or GIF either forge embeds.
pub const MAX_IMAGE_BYTES: u64 = 10 * 1024 * 1024;
/// Largest video either forge embeds.
pub const MAX_VIDEO_BYTES: u64 = 100 * 1024 * 1024;

/// What a file is, from its extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaType {
    /// An image both forges embed (PNG, JPEG, WebP, SVG).
    Image,
    /// An image only GitLab takes or neither renders (EXR, TIFF, BMP, ...).
    OtherImage,
    Gif,
    /// A video both forges play (MP4, WebM, MOV).
    Video,
    /// A video type to re-encode (AVI, MKV, ...).
    OtherVideo,
    Text,
    Unknown,
}

/// The media type of `path`, from its extension.
#[must_use]
pub fn media_type(path: &Path) -> MediaType {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "webp" | "svg" => MediaType::Image,
        "exr" | "tif" | "tiff" | "bmp" | "tga" | "hdr" | "dpx" => MediaType::OtherImage,
        "gif" => MediaType::Gif,
        "mp4" | "webm" | "mov" => MediaType::Video,
        "avi" | "mkv" | "m4v" | "flv" | "wmv" => MediaType::OtherVideo,
        "txt" | "log" | "out" | "md" | "json" | "csv" => MediaType::Text,
        _ => MediaType::Unknown,
    }
}

/// The MIME type an upload of `path` is sent with.
#[must_use]
pub fn content_type(path: &Path) -> &'static str {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match ext.as_str() {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "svg" => "image/svg+xml",
        "gif" => "image/gif",
        "mp4" => "video/mp4",
        "webm" => "video/webm",
        "mov" => "video/quicktime",
        _ => "application/octet-stream",
    }
}

/// The exhibit kind a file delivers.
#[must_use]
pub fn delivered_kind(media: MediaType) -> Option<ExhibitKind> {
    match media {
        MediaType::Image | MediaType::OtherImage => Some(ExhibitKind::Screenshot),
        MediaType::Gif => Some(ExhibitKind::Gif),
        MediaType::Video | MediaType::OtherVideo => Some(ExhibitKind::Video),
        MediaType::Text => Some(ExhibitKind::Output),
        MediaType::Unknown => None,
    }
}

/// Whether a file of this type and size can be embedded as is.
#[must_use]
pub fn fits(media: MediaType, bytes: u64) -> bool {
    match media {
        MediaType::Image | MediaType::Gif => bytes <= MAX_IMAGE_BYTES,
        MediaType::Video => bytes <= MAX_VIDEO_BYTES,
        _ => false,
    }
}

/// Runs `ffmpeg`; a seam so tests never need the real binary.
pub trait Transcoder {
    /// Whether conversions are possible at all.
    fn available(&self) -> bool;
    /// Run ffmpeg with `args` (input/output paths included). `Err` carries
    /// the failure text.
    fn run(&self, args: &[String]) -> Result<(), String>;
    /// A video's duration in seconds, when it can be read.
    fn duration_seconds(&self, path: &Path) -> Option<f64>;
}

/// The real `ffmpeg`/`ffprobe` on PATH.
pub struct Ffmpeg;

impl Transcoder for Ffmpeg {
    fn available(&self) -> bool {
        std::process::Command::new("ffmpeg")
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .is_ok_and(|s| s.success())
    }

    fn run(&self, args: &[String]) -> Result<(), String> {
        let out = std::process::Command::new("ffmpeg")
            .args(["-hide_banner", "-loglevel", "error", "-nostdin", "-y"])
            .args(args)
            .output()
            .map_err(|e| format!("could not run ffmpeg: {e}"))?;
        if out.status.success() {
            Ok(())
        } else {
            Err(String::from_utf8_lossy(&out.stderr).trim().to_string())
        }
    }

    fn duration_seconds(&self, path: &Path) -> Option<f64> {
        let out = std::process::Command::new("ffprobe")
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=duration",
                "-of",
                "csv=p=0",
            ])
            .arg(path)
            .output()
            .ok()?;
        String::from_utf8_lossy(&out.stdout).trim().parse().ok()
    }
}

/// A file ready to embed, or why there is none.
#[derive(Debug, Clone, PartialEq)]
pub enum Prepared {
    File { path: PathBuf, kind: ExhibitKind },
    Text { text: String },
    Undeliverable { reason: String },
}

fn size(path: &Path) -> u64 {
    std::fs::metadata(path).map(|m| m.len()).unwrap_or(u64::MAX)
}

fn p(path: &Path) -> String {
    path.to_string_lossy().to_string()
}

/// Make `source` embeddable, writing any converted file next to `out_stem`
/// (`<out_stem>.<ext>`). Visual files step down `preference` -- e.g. a GIF
/// that cannot be shrunk under the image limit becomes a video, then a
/// still -- skipping kinds the preference leaves out. Videos are trimmed to
/// `max_video_seconds`.
#[must_use]
pub fn prepare(
    source: &Path,
    out_stem: &Path,
    preference: &[ExhibitKind],
    max_video_seconds: u32,
    max_text_chars: usize,
    ffmpeg: &dyn Transcoder,
) -> Prepared {
    let media = media_type(source);
    if media == MediaType::Text {
        return match std::fs::read(source) {
            Ok(bytes) => Prepared::Text {
                text: String::from_utf8_lossy(&bytes)
                    .chars()
                    .take(max_text_chars)
                    .collect(),
            },
            Err(e) => Prepared::Undeliverable {
                reason: format!("could not read the captured text: {e}"),
            },
        };
    }
    if media == MediaType::Unknown {
        return Prepared::Undeliverable {
            reason: format!(
                "unsupported file type {:?}; capture a PNG, GIF, MP4 or text file",
                source.extension().unwrap_or_default()
            ),
        };
    }
    let has_ffmpeg = ffmpeg.available();
    let allowed = |kind: ExhibitKind| preference.contains(&kind);
    let start = delivered_kind(media).unwrap_or(ExhibitKind::Screenshot);
    // Kinds to try, from what was captured downwards in the preference.
    let mut ladder: Vec<ExhibitKind> = preference
        .iter()
        .copied()
        .skip_while(|k| *k != start)
        .collect();
    if ladder.is_empty() {
        ladder = preference.to_vec();
    }
    let mut last_error = String::new();
    for kind in ladder {
        if !allowed(kind) {
            continue;
        }
        match attempt(
            source,
            media,
            kind,
            out_stem,
            max_video_seconds,
            ffmpeg,
            has_ffmpeg,
        ) {
            Ok(path) => return Prepared::File { path, kind },
            Err(e) => last_error = e,
        }
    }
    Prepared::Undeliverable {
        reason: if last_error.is_empty() {
            "no allowed exhibit kind could be made from the captured file".to_string()
        } else {
            last_error
        },
    }
}

fn attempt(
    source: &Path,
    media: MediaType,
    kind: ExhibitKind,
    out_stem: &Path,
    max_video_seconds: u32,
    ffmpeg: &dyn Transcoder,
    has_ffmpeg: bool,
) -> Result<PathBuf, String> {
    let need = |what: &str| format!("{what}, and ffmpeg is not on PATH to convert it");
    match (kind, media) {
        (ExhibitKind::Screenshot, MediaType::Image) if fits(media, size(source)) => {
            Ok(source.to_path_buf())
        }
        (ExhibitKind::Gif, MediaType::Gif) if fits(media, size(source)) => Ok(source.to_path_buf()),
        (ExhibitKind::Video, MediaType::Video)
            if fits(media, size(source))
                && ffmpeg
                    .duration_seconds(source)
                    .is_none_or(|d| d <= f64::from(max_video_seconds) + 0.5) =>
        {
            Ok(source.to_path_buf())
        }
        _ if !has_ffmpeg => Err(need(&format!(
            "the captured {} does not fit as a {}",
            source.extension().unwrap_or_default().to_string_lossy(),
            kind.as_str()
        ))),
        (ExhibitKind::Screenshot, _) => {
            let out = out_stem.with_extension("png");
            let mut args = vec!["-i".to_string(), p(source)];
            if matches!(
                media,
                MediaType::Gif | MediaType::Video | MediaType::OtherVideo
            ) {
                // The last frame shows the end state an exhibit is about.
                args = vec!["-sseof".into(), "-0.1".into(), "-i".into(), p(source)];
            }
            args.extend([
                "-frames:v".into(),
                "1".into(),
                "-vf".into(),
                "scale='min(1920,iw)':-2".into(),
                p(&out),
            ]);
            ffmpeg.run(&args)?;
            check(&out, MediaType::Image)
        }
        (ExhibitKind::Gif, MediaType::Gif | MediaType::Video | MediaType::OtherVideo) => {
            for (width, fps) in [(960, 12), (720, 10), (480, 8)] {
                let out = out_stem.with_extension("gif");
                let filter = format!(
                    "fps={fps},scale='min({width},iw)':-2:flags=lanczos,split[a][b];[a]palettegen=max_colors=128[p];[b][p]paletteuse"
                );
                ffmpeg.run(&[
                    "-t".into(),
                    max_video_seconds.to_string(),
                    "-i".into(),
                    p(source),
                    "-vf".into(),
                    filter,
                    "-loop".into(),
                    "0".into(),
                    p(&out),
                ])?;
                if fits(MediaType::Gif, size(&out)) {
                    return Ok(out);
                }
            }
            Err("the GIF stays over 10 MiB even shrunk".to_string())
        }
        (ExhibitKind::Video, MediaType::Gif | MediaType::Video | MediaType::OtherVideo) => {
            for crf in [28, 34, 40] {
                let out = out_stem.with_extension("mp4");
                ffmpeg.run(&[
                    "-t".into(),
                    max_video_seconds.to_string(),
                    "-i".into(),
                    p(source),
                    "-vf".into(),
                    "scale='min(1280,iw)':-2".into(),
                    "-c:v".into(),
                    "libx264".into(),
                    "-crf".into(),
                    crf.to_string(),
                    "-pix_fmt".into(),
                    "yuv420p".into(),
                    "-an".into(),
                    p(&out),
                ])?;
                if fits(MediaType::Video, size(&out)) {
                    return Ok(out);
                }
            }
            Err("the video stays over 100 MiB even re-encoded".to_string())
        }
        _ => Err(format!(
            "a {} cannot be made from a still image",
            kind.as_str()
        )),
    }
}

fn check(out: &Path, media: MediaType) -> Result<PathBuf, String> {
    if fits(media, size(out)) {
        Ok(out.to_path_buf())
    } else {
        Err(format!("{} is still too large", out.display()))
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// A scratch directory removed when dropped.
    pub(crate) struct TestDir(PathBuf);

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

    impl Drop for TestDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Writes a small file for every conversion instead of running ffmpeg.
    struct FakeFfmpeg {
        available: bool,
        output_bytes: usize,
        duration: Option<f64>,
        calls: std::cell::RefCell<Vec<Vec<String>>>,
    }

    impl Transcoder for FakeFfmpeg {
        fn available(&self) -> bool {
            self.available
        }
        fn run(&self, args: &[String]) -> Result<(), String> {
            self.calls.borrow_mut().push(args.to_vec());
            std::fs::write(args.last().unwrap(), vec![0u8; self.output_bytes]).unwrap();
            Ok(())
        }
        fn duration_seconds(&self, _: &Path) -> Option<f64> {
            self.duration
        }
    }

    fn fake(available: bool, output_bytes: usize) -> FakeFfmpeg {
        FakeFfmpeg {
            available,
            output_bytes,
            duration: Some(5.0),
            calls: std::cell::RefCell::new(Vec::new()),
        }
    }

    fn file(dir: &Path, name: &str, bytes: u64) -> PathBuf {
        let path = dir.join(name);
        let f = std::fs::File::create(&path).unwrap();
        f.set_len(bytes).unwrap();
        path
    }

    const PREF: [ExhibitKind; 3] = crate::exhibits::DEFAULT_KIND_PREFERENCE;

    #[test]
    fn a_fitting_file_is_used_as_is_without_ffmpeg() {
        let dir = TestDir::new();
        let gif = file(dir.path(), "a.gif", 1000);
        let ff = fake(false, 0);
        let out = prepare(&gif, &dir.path().join("o"), &PREF, 15, 100, &ff);
        assert_eq!(
            out,
            Prepared::File {
                path: gif,
                kind: ExhibitKind::Gif
            }
        );
    }

    #[test]
    fn an_oversized_gif_steps_down_to_video() {
        let dir = TestDir::new();
        let gif = file(dir.path(), "a.gif", MAX_IMAGE_BYTES + 1);
        // Every shrunk GIF is still too big; the mp4 fits.
        struct Sized(FakeFfmpeg);
        impl Transcoder for Sized {
            fn available(&self) -> bool {
                true
            }
            fn run(&self, args: &[String]) -> Result<(), String> {
                let out = args.last().unwrap();
                let bytes = if out.ends_with(".gif") {
                    MAX_IMAGE_BYTES as usize + 10
                } else {
                    1000
                };
                std::fs::write(out, vec![0u8; bytes]).unwrap();
                self.0.calls.borrow_mut().push(args.to_vec());
                Ok(())
            }
            fn duration_seconds(&self, _: &Path) -> Option<f64> {
                Some(5.0)
            }
        }
        let ff = Sized(fake(true, 0));
        let out = prepare(&gif, &dir.path().join("o"), &PREF, 15, 100, &ff);
        match out {
            Prepared::File { path, kind } => {
                assert_eq!(kind, ExhibitKind::Video);
                assert_eq!(path.extension().unwrap(), "mp4");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_preference_without_video_steps_a_gif_straight_to_a_still() {
        let dir = TestDir::new();
        let gif = file(dir.path(), "a.gif", MAX_IMAGE_BYTES + 1);
        struct GifsTooBig;
        impl Transcoder for GifsTooBig {
            fn available(&self) -> bool {
                true
            }
            fn run(&self, args: &[String]) -> Result<(), String> {
                let out = args.last().unwrap();
                let bytes = if out.ends_with(".gif") {
                    MAX_IMAGE_BYTES as usize + 10
                } else {
                    500
                };
                std::fs::write(out, vec![0u8; bytes]).unwrap();
                Ok(())
            }
            fn duration_seconds(&self, _: &Path) -> Option<f64> {
                None
            }
        }
        let pref = [ExhibitKind::Gif, ExhibitKind::Screenshot];
        let out = prepare(&gif, &dir.path().join("o"), &pref, 15, 100, &GifsTooBig);
        assert!(matches!(
            out,
            Prepared::File {
                kind: ExhibitKind::Screenshot,
                ..
            }
        ));
    }

    #[test]
    fn a_long_video_is_trimmed() {
        let dir = TestDir::new();
        let mp4 = file(dir.path(), "a.mp4", 1000);
        let mut ff = fake(true, 900);
        ff.duration = Some(40.0);
        let pref = [ExhibitKind::Video];
        let out = prepare(&mp4, &dir.path().join("o"), &pref, 15, 100, &ff);
        assert!(matches!(
            out,
            Prepared::File {
                kind: ExhibitKind::Video,
                ..
            }
        ));
        let calls = ff.calls.borrow();
        assert_eq!(calls[0][0..2], ["-t".to_string(), "15".to_string()]);
    }

    #[test]
    fn an_exr_needs_ffmpeg_and_becomes_a_png() {
        let dir = TestDir::new();
        let exr = file(dir.path(), "render.exr", 2000);
        let without = prepare(&exr, &dir.path().join("o"), &PREF, 15, 100, &fake(false, 0));
        assert!(matches!(without, Prepared::Undeliverable { reason } if reason.contains("ffmpeg")));
        let with = prepare(
            &exr,
            &dir.path().join("o"),
            &PREF,
            15,
            100,
            &fake(true, 800),
        );
        match with {
            Prepared::File { path, kind } => {
                assert_eq!(kind, ExhibitKind::Screenshot);
                assert_eq!(path.extension().unwrap(), "png");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn text_is_read_and_clipped_never_uploaded() {
        let dir = TestDir::new();
        let txt = dir.path().join("out.txt");
        std::fs::write(&txt, "abcdef").unwrap();
        let out = prepare(&txt, &dir.path().join("o"), &PREF, 15, 3, &fake(false, 0));
        assert_eq!(
            out,
            Prepared::Text {
                text: "abc".to_string()
            }
        );
    }

    #[test]
    fn an_unknown_type_is_undeliverable() {
        let dir = TestDir::new();
        let zip = file(dir.path(), "a.zip", 10);
        let out = prepare(&zip, &dir.path().join("o"), &PREF, 15, 100, &fake(true, 10));
        assert!(matches!(out, Prepared::Undeliverable { .. }));
    }

    #[test]
    fn a_real_ffmpeg_turns_a_gif_into_a_still_when_the_preference_allows_only_stills() {
        // Runs the real ffmpeg (tagged in proc_counts/tags.toml); without it
        // only the availability probe runs.
        if !Ffmpeg.available() {
            return;
        }
        let dir = TestDir::new();
        let gif = dir.path().join("in.gif");
        // A 2x2, 2-frame GIF.
        {
            let file = std::fs::File::create(&gif).unwrap();
            let mut enc = gif::Encoder::new(file, 2, 2, &[0, 0, 0, 255, 255, 255]).unwrap();
            for shade in [0u8, 1] {
                let frame = gif::Frame {
                    width: 2,
                    height: 2,
                    buffer: std::borrow::Cow::Owned(vec![shade; 4]),
                    ..gif::Frame::default()
                };
                enc.write_frame(&frame).unwrap();
            }
        }
        // A GIF fits as is, so ask for a still only to force a conversion.
        let out = prepare(
            &gif,
            &dir.path().join("o"),
            &[ExhibitKind::Screenshot],
            15,
            100,
            &Ffmpeg,
        );
        match out {
            Prepared::File { path, kind } => {
                assert_eq!(kind, ExhibitKind::Screenshot);
                assert!(std::fs::read(path).unwrap().starts_with(b"\x89PNG"));
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn content_types_match_what_github_accepts() {
        assert_eq!(content_type(Path::new("a.PNG")), "image/png");
        assert_eq!(content_type(Path::new("a.mov")), "video/quicktime");
        assert_eq!(content_type(Path::new("a.webm")), "video/webm");
    }
}
