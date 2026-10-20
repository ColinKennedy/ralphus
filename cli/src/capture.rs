//! Screen capture for `ralphus exhibit screenshot|record`: a still or a short
//! GIF/MP4 of one program's window (by process id) or a whole screen, using
//! only what each OS already ships.
//!
//! * Windows: PowerShell with .NET `System.Drawing`; a window is captured with
//!   `PrintWindow`, so it does not have to be in front.
//! * macOS: `screencapture` (whole screen only).
//! * Linux: the first of `grim` (Wayland), ImageMagick `import` (X11, which
//!   can also capture one window found with `xdotool`) and
//!   `gnome-screenshot`.
//!
//! Recording uses `ffmpeg` when it is on PATH. Without it, recording takes
//! stills at the requested rate and encodes them into a GIF in-process.
//! Every value handed to a capture script travels in an environment
//! variable, never inside the script text.

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// What to capture.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Target {
    /// The main window of this process.
    Pid(u32),
    /// The window with this title (an exact match first, then the first
    /// title containing it). For programs whose windows do not record their
    /// process id (Tk under X11, for one).
    Window(String),
    /// The whole primary screen.
    Screen,
}

fn on_path(program: &str) -> bool {
    ralphus_core::process::which(program).is_some()
}

fn run(mut cmd: Command, what: &str) -> Result<(), String> {
    let out = cmd
        .output()
        .map_err(|e| format!("could not run {what}: {e}"))?;
    if out.status.success() {
        Ok(())
    } else {
        Err(format!(
            "{what} failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

fn require_file(out: &Path) -> Result<(), String> {
    match std::fs::metadata(out) {
        Ok(m) if m.len() > 0 => Ok(()),
        _ => Err(format!("the capture produced no file at {}", out.display())),
    }
}

// ---- stills ------------------------------------------------------------------

/// The PowerShell that writes a PNG of `$env:RALPHUS_CAPTURE_PID`'s main
/// window, or of the window titled `$env:RALPHUS_CAPTURE_TITLE`, or of the
/// primary screen when neither is set, to `$env:RALPHUS_CAPTURE_OUT`.
const WINDOWS_STILL: &str = r#"
$ErrorActionPreference = 'Stop'
Add-Type -AssemblyName System.Drawing
Add-Type -AssemblyName System.Windows.Forms
Add-Type @"
using System;
using System.Runtime.InteropServices;
public static class RalphusCapture {
  [StructLayout(LayoutKind.Sequential)] public struct RECT { public int L, T, R, B; }
  [DllImport("user32.dll")] public static extern bool GetWindowRect(IntPtr h, out RECT r);
  [DllImport("user32.dll")] public static extern bool PrintWindow(IntPtr h, IntPtr dc, uint flags);
  [DllImport("user32.dll")] public static extern bool SetProcessDPIAware();
}
"@
[RalphusCapture]::SetProcessDPIAware() | Out-Null
$out = $env:RALPHUS_CAPTURE_OUT
$h = [IntPtr]::Zero
if ($env:RALPHUS_CAPTURE_PID) {
  $p = Get-Process -Id ([int]$env:RALPHUS_CAPTURE_PID)
  $h = $p.MainWindowHandle
  if ($h -eq [IntPtr]::Zero) { throw "process $($p.Id) has no visible main window" }
} elseif ($env:RALPHUS_CAPTURE_TITLE) {
  $t = $env:RALPHUS_CAPTURE_TITLE
  $p = Get-Process | Where-Object { $_.MainWindowTitle -eq $t } | Select-Object -First 1
  if (-not $p) {
    $like = '*' + [System.Management.Automation.WildcardPattern]::Escape($t) + '*'
    $p = Get-Process | Where-Object { $_.MainWindowTitle -like $like } | Select-Object -First 1
  }
  if (-not $p) { throw "no window is titled `"$t`"" }
  $h = $p.MainWindowHandle
}
if ($h -ne [IntPtr]::Zero) {
  $r = New-Object RalphusCapture+RECT
  [RalphusCapture]::GetWindowRect($h, [ref]$r) | Out-Null
  $w = $r.R - $r.L; $hgt = $r.B - $r.T
  if ($w -le 0 -or $hgt -le 0) { throw "the window has no area (minimized?)" }
  $bmp = New-Object System.Drawing.Bitmap $w, $hgt
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $dc = $g.GetHdc()
  $ok = [RalphusCapture]::PrintWindow($h, $dc, 2)
  $g.ReleaseHdc($dc)
  if (-not $ok) { $g.CopyFromScreen($r.L, $r.T, 0, 0, $bmp.Size) }
} else {
  $b = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
  $bmp = New-Object System.Drawing.Bitmap $b.Width, $b.Height
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.CopyFromScreen($b.Left, $b.Top, 0, 0, $bmp.Size)
}
$bmp.Save($out, [System.Drawing.Imaging.ImageFormat]::Png)
$g.Dispose(); $bmp.Dispose()
"#;

/// Write a PNG of `target` to `out`.
///
/// # Errors
///
/// When no capture tool is available for this OS/target, or the capture
/// fails.
pub fn screenshot(target: &Target, out: &Path) -> Result<(), String> {
    if cfg!(windows) {
        let mut cmd = Command::new("powershell");
        cmd.args(["-NoProfile", "-NonInteractive", "-Command", WINDOWS_STILL])
            .env("RALPHUS_CAPTURE_OUT", out);
        cmd.env_remove("RALPHUS_CAPTURE_PID")
            .env_remove("RALPHUS_CAPTURE_TITLE");
        match target {
            Target::Pid(pid) => cmd.env("RALPHUS_CAPTURE_PID", pid.to_string()),
            Target::Window(title) => cmd.env("RALPHUS_CAPTURE_TITLE", title),
            Target::Screen => &mut cmd,
        };
        run(cmd, "PowerShell screen capture")?;
    } else if cfg!(target_os = "macos") {
        if !matches!(target, Target::Screen) {
            return Err(
                "capturing one window is not supported on macOS yet; use --screen".to_string(),
            );
        }
        let mut cmd = Command::new("screencapture");
        cmd.arg("-x").arg(out);
        run(cmd, "screencapture")?;
    } else {
        linux_screenshot(target, out)?;
    }
    require_file(out)
}

/// The first visible X11 window `xdotool search` finds for `args`.
fn xdotool_first(args: &[&str]) -> Result<Option<String>, String> {
    let found = Command::new("xdotool")
        .arg("search")
        .arg("--onlyvisible")
        .args(args)
        .output()
        .map_err(|e| format!("could not run xdotool: {e}"))?;
    Ok(String::from_utf8_lossy(&found.stdout)
        .lines()
        .map(str::trim)
        .find(|w| !w.is_empty())
        .map(str::to_string))
}

/// `title` as an extended regular expression matching itself literally.
fn regex_literal(title: &str) -> String {
    let mut out = String::new();
    for c in title.chars() {
        if "\\.^$|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The X11 window `target` names: by the `_NET_WM_PID` its toolkit records,
/// or by title (an exact match first, then a title containing it).
fn x11_window(target: &Target) -> Result<String, String> {
    match target {
        Target::Pid(pid) => xdotool_first(&["--pid", &pid.to_string()])?.ok_or_else(|| {
            format!(
                "no visible window records process {pid} (some toolkits, such as Tk, never \
                 set _NET_WM_PID); capture it by title with --window instead"
            )
        }),
        Target::Window(title) => {
            let literal = regex_literal(title);
            match xdotool_first(&["--name", &format!("^{literal}$")])? {
                Some(w) => Ok(w),
                None => xdotool_first(&["--name", &literal])?
                    .ok_or_else(|| format!("no visible window is titled \"{title}\"")),
            }
        }
        Target::Screen => Ok("root".to_string()),
    }
}

fn linux_screenshot(target: &Target, out: &Path) -> Result<(), String> {
    if !matches!(target, Target::Screen) {
        if !(on_path("xdotool") && on_path("import")) {
            return Err(
                "capturing one window needs xdotool and ImageMagick's import (X11); use --screen"
                    .to_string(),
            );
        }
        let window = x11_window(target)?;
        let mut cmd = Command::new("import");
        // `-frame` includes the window manager's frame: the title bar.
        cmd.args(["-frame", "-window", &window]).arg(out);
        return run(cmd, "import");
    }
    if on_path("grim") && std::env::var_os("WAYLAND_DISPLAY").is_some() {
        let mut cmd = Command::new("grim");
        cmd.arg(out);
        return run(cmd, "grim");
    }
    if on_path("import") && std::env::var_os("DISPLAY").is_some() {
        let mut cmd = Command::new("import");
        cmd.args(["-window", "root"]).arg(out);
        return run(cmd, "import");
    }
    if on_path("gnome-screenshot") {
        let mut cmd = Command::new("gnome-screenshot");
        cmd.arg("-f").arg(out);
        return run(cmd, "gnome-screenshot");
    }
    Err("no screen capture tool found: install grim (Wayland), ImageMagick (X11) or gnome-screenshot".to_string())
}

// ---- recordings ----------------------------------------------------------------

/// A recording's format, from the output file's extension.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordFormat {
    Gif,
    Mp4,
}

impl RecordFormat {
    /// The format `out`'s extension asks for.
    #[must_use]
    pub fn for_path(out: &Path) -> Option<Self> {
        match out
            .extension()
            .and_then(|e| e.to_str())
            .map(str::to_ascii_lowercase)
            .as_deref()
        {
            Some("gif") => Some(Self::Gif),
            Some("mp4") => Some(Self::Mp4),
            _ => None,
        }
    }
}

/// Record `target` for `seconds` at `fps` into `out` (a `.gif` or `.mp4`).
///
/// # Errors
///
/// When the format is unsupported, no capture path is available, or the
/// capture fails.
pub fn record(target: &Target, seconds: u32, fps: u32, out: &Path) -> Result<(), String> {
    let format =
        RecordFormat::for_path(out).ok_or_else(|| "record to a .gif or .mp4 file".to_string())?;
    if on_path("ffmpeg") {
        if let Some(input) = ffmpeg_input(target)? {
            return record_with_ffmpeg(&input, seconds, fps, format, out);
        }
    }
    if format == RecordFormat::Mp4 {
        return Err("recording an .mp4 needs ffmpeg on PATH; record a .gif instead".to_string());
    }
    record_stills(target, seconds, fps, out)
}

/// ffmpeg's input arguments for `target` on this OS, or `None` when ffmpeg
/// cannot grab it here (the still-frame path is used instead).
fn ffmpeg_input(target: &Target) -> Result<Option<Vec<String>>, String> {
    let v = |items: &[&str]| items.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
    if cfg!(windows) {
        return Ok(Some(match target {
            Target::Screen => v(&["-f", "gdigrab", "-i", "desktop"]),
            Target::Pid(pid) => {
                let title = windows_window_title(*pid)?;
                vec![
                    "-f".into(),
                    "gdigrab".into(),
                    "-i".into(),
                    format!("title={title}"),
                ]
            }
            Target::Window(title) => vec![
                "-f".into(),
                "gdigrab".into(),
                "-i".into(),
                format!("title={title}"),
            ],
        }));
    }
    if cfg!(target_os = "macos") {
        return Ok(match target {
            Target::Screen => Some(v(&["-f", "avfoundation", "-i", "1:none"])),
            Target::Pid(_) | Target::Window(_) => None,
        });
    }
    Ok(match (target, std::env::var("DISPLAY")) {
        (Target::Screen, Ok(display)) => {
            Some(vec!["-f".into(), "x11grab".into(), "-i".into(), display])
        }
        _ => None,
    })
}

fn windows_window_title(pid: u32) -> Result<String, String> {
    let out = Command::new("powershell")
        .args([
            "-NoProfile",
            "-NonInteractive",
            "-Command",
            "(Get-Process -Id ([int]$env:RALPHUS_CAPTURE_PID)).MainWindowTitle",
        ])
        .env("RALPHUS_CAPTURE_PID", pid.to_string())
        .output()
        .map_err(|e| format!("could not run PowerShell: {e}"))?;
    let title = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if title.is_empty() {
        Err(format!("process {pid} has no titled main window"))
    } else {
        Ok(title)
    }
}

fn record_with_ffmpeg(
    input: &[String],
    seconds: u32,
    fps: u32,
    format: RecordFormat,
    out: &Path,
) -> Result<(), String> {
    let mut cmd = Command::new("ffmpeg");
    // `-t` must limit the *input*: as an output option the grab never ends,
    // and the GIF palette filter, which waits for the end of its input,
    // buffers frames forever.
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-y",
        "-framerate",
    ])
    .arg(fps.to_string())
    .arg("-t")
    .arg(seconds.to_string())
    .args(input);
    match format {
        RecordFormat::Gif => cmd.args([
            "-vf",
            &format!(
                "fps={fps},scale='min(960,iw)':-2:flags=lanczos,split[a][b];[a]palettegen=max_colors=128[p];[b][p]paletteuse"
            ),
            "-loop",
            "0",
        ]),
        RecordFormat::Mp4 => cmd.args([
            "-vf",
            "scale='min(1280,iw)':-2",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
        ]),
    };
    cmd.arg(out);
    run(cmd, "ffmpeg")?;
    require_file(out)
}

/// Record by taking stills at `fps` and encoding them into a GIF.
fn record_stills(target: &Target, seconds: u32, fps: u32, out: &Path) -> Result<(), String> {
    let frames_dir = scratch_dir()?;
    let result = (|| {
        let interval = Duration::from_millis(1000 / u64::from(fps.max(1)));
        let total = seconds.max(1) * fps.max(1);
        let started = Instant::now();
        let mut frames: Vec<PathBuf> = Vec::new();
        for i in 0..total {
            let frame = frames_dir.join(format!("{i:05}.png"));
            screenshot(target, &frame)?;
            frames.push(frame);
            let due = interval * (i + 1);
            if let Some(wait) = due.checked_sub(started.elapsed()) {
                std::thread::sleep(wait);
            }
        }
        // A still capture can be slower than the requested rate; the GIF
        // plays at the rate the frames were actually taken.
        let per_frame = started.elapsed() / u32::try_from(frames.len()).unwrap_or(1).max(1);
        encode_gif(&frames, per_frame, out)
    })();
    let _ = std::fs::remove_dir_all(&frames_dir);
    result
}

fn scratch_dir() -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join(format!(
        "ralphus-capture-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("could not create {}: {e}", dir.display()))?;
    Ok(dir)
}

/// The width a recorded frame is scaled down to.
const MAX_GIF_WIDTH: u32 = 960;

/// One decoded frame as RGBA.
struct Rgba {
    width: u32,
    height: u32,
    pixels: Vec<u8>,
}

fn decode_png(path: &Path) -> Result<Rgba, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("{}: {e}", path.display()))?;
    let mut decoder = png::Decoder::new(std::io::BufReader::new(file));
    decoder.set_transformations(png::Transformations::normalize_to_color8());
    let mut reader = decoder
        .read_info()
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let mut buf = vec![0; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buf)
        .map_err(|e| format!("{}: {e}", path.display()))?;
    let bytes = &buf[..info.buffer_size()];
    let pixels = match info.color_type {
        png::ColorType::Rgba => bytes.to_vec(),
        png::ColorType::Rgb => bytes
            .chunks_exact(3)
            .flat_map(|p| [p[0], p[1], p[2], 255])
            .collect(),
        png::ColorType::GrayscaleAlpha => bytes
            .chunks_exact(2)
            .flat_map(|p| [p[0], p[0], p[0], p[1]])
            .collect(),
        png::ColorType::Grayscale => bytes.iter().flat_map(|&g| [g, g, g, 255]).collect(),
        png::ColorType::Indexed => {
            return Err(format!("{}: unexpected indexed PNG", path.display()));
        }
    };
    Ok(Rgba {
        width: info.width,
        height: info.height,
        pixels,
    })
}

/// Nearest-neighbour downscale to at most `max_width` wide.
fn shrink(frame: Rgba, max_width: u32) -> Rgba {
    if frame.width <= max_width {
        return frame;
    }
    let width = max_width;
    let height =
        (u64::from(frame.height) * u64::from(width) / u64::from(frame.width)).max(1) as u32;
    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        let sy = (u64::from(y) * u64::from(frame.height) / u64::from(height)) as usize;
        for x in 0..width {
            let sx = (u64::from(x) * u64::from(frame.width) / u64::from(width)) as usize;
            let i = (sy * frame.width as usize + sx) * 4;
            pixels.extend_from_slice(&frame.pixels[i..i + 4]);
        }
    }
    Rgba {
        width,
        height,
        pixels,
    }
}

/// Encode PNG frames into a looping GIF, each shown for `per_frame`.
///
/// # Errors
///
/// When a frame cannot be read or the GIF cannot be written.
pub fn encode_gif(frames: &[PathBuf], per_frame: Duration, out: &Path) -> Result<(), String> {
    let mut decoded: Vec<Rgba> = Vec::new();
    for frame in frames {
        decoded.push(shrink(decode_png(frame)?, MAX_GIF_WIDTH));
    }
    let first = decoded
        .first()
        .ok_or_else(|| "no frames were captured".to_string())?;
    let (width, height) = (first.width, first.height);
    let w16 = u16::try_from(width).map_err(|_| "frame too wide for a GIF".to_string())?;
    let h16 = u16::try_from(height).map_err(|_| "frame too tall for a GIF".to_string())?;
    let file = std::fs::File::create(out).map_err(|e| format!("{}: {e}", out.display()))?;
    let mut encoder = gif::Encoder::new(std::io::BufWriter::new(file), w16, h16, &[])
        .map_err(|e| format!("{}: {e}", out.display()))?;
    encoder
        .set_repeat(gif::Repeat::Infinite)
        .map_err(|e| e.to_string())?;
    let delay = u16::try_from((per_frame.as_millis() / 10).max(2)).unwrap_or(u16::MAX);
    for mut frame in decoded {
        // A window resized mid-recording is cut or padded to the first size.
        if frame.width != width || frame.height != height {
            frame = fit(&frame, width, height);
        }
        let mut gif_frame = gif::Frame::from_rgba_speed(w16, h16, &mut frame.pixels, 10);
        gif_frame.delay = delay;
        encoder.write_frame(&gif_frame).map_err(|e| e.to_string())?;
    }
    Ok(())
}

fn fit(frame: &Rgba, width: u32, height: u32) -> Rgba {
    let mut pixels = vec![0u8; (width * height * 4) as usize];
    for y in 0..height.min(frame.height) {
        for x in 0..width.min(frame.width) {
            let src = ((y * frame.width + x) * 4) as usize;
            let dst = ((y * width + x) * 4) as usize;
            pixels[dst..dst + 4].copy_from_slice(&frame.pixels[src..src + 4]);
        }
    }
    Rgba {
        width,
        height,
        pixels,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_png(path: &Path, width: u32, height: u32, shade: u8) {
        let file = std::fs::File::create(path).unwrap();
        let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
        encoder.set_color(png::ColorType::Rgb);
        encoder.set_depth(png::BitDepth::Eight);
        let mut writer = encoder.write_header().unwrap();
        let data: Vec<u8> = (0..width * height)
            .flat_map(|_| [shade, 0, 255 - shade])
            .collect();
        writer.write_image_data(&data).unwrap();
    }

    #[test]
    fn stills_encode_into_a_looping_gif_scaled_to_the_first_frame() {
        let dir = scratch_dir().unwrap();
        let frames: Vec<PathBuf> = (0..3)
            .map(|i| {
                let path = dir.join(format!("{i}.png"));
                // The last frame is a different size and must be fitted.
                let (w, h) = if i == 2 { (30, 10) } else { (40, 20) };
                write_png(&path, w, h, (i * 80) as u8);
                path
            })
            .collect();
        let out = dir.join("out.gif");
        encode_gif(&frames, Duration::from_millis(100), &out).unwrap();
        let bytes = std::fs::read(&out).unwrap();
        assert!(bytes.starts_with(b"GIF89a"));
        let mut decoder = gif::DecodeOptions::new().read_info(&bytes[..]).unwrap();
        assert_eq!((decoder.width(), decoder.height()), (40, 20));
        let mut count = 0;
        while let Some(frame) = decoder.read_next_frame().unwrap() {
            assert_eq!(frame.delay, 10);
            count += 1;
        }
        assert_eq!(count, 3);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn wide_frames_shrink_keeping_their_aspect() {
        let frame = Rgba {
            width: 1920,
            height: 1080,
            pixels: vec![7; 1920 * 1080 * 4],
        };
        let small = shrink(frame, 960);
        assert_eq!((small.width, small.height), (960, 540));
        assert_eq!(small.pixels.len(), 960 * 540 * 4);
    }

    /// Live captures on a real desktop. `#[ignore]`d: they need a display
    /// (run with `--run-ignored only -E 'test(/live_capture/)'`).
    #[test]
    #[ignore = "needs a desktop session"]
    fn live_capture_screen_still_and_still_based_gif() {
        let dir = scratch_dir().unwrap();
        let png = dir.join("screen.png");
        screenshot(&Target::Screen, &png).unwrap();
        assert!(std::fs::read(&png).unwrap().starts_with(b"\x89PNG"));
        let gif = dir.join("screen.gif");
        record_stills(&Target::Screen, 1, 2, &gif).unwrap();
        assert!(std::fs::read(&gif).unwrap().starts_with(b"GIF89a"));
        if on_path("ffmpeg") {
            let mp4 = dir.join("screen.mp4");
            record(&Target::Screen, 1, 5, &mp4).unwrap();
            assert_eq!(&std::fs::read(&mp4).unwrap()[4..8], b"ftyp");
            let gif2 = dir.join("screen-ffmpeg.gif");
            record(&Target::Screen, 1, 5, &gif2).unwrap();
            assert!(std::fs::read(&gif2).unwrap().starts_with(b"GIF89a"));
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    #[ignore = "needs a desktop session"]
    #[cfg(windows)]
    fn live_capture_one_windows_process_window() {
        // A window owned by the very process whose id is captured.
        let mut child = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                "Add-Type -AssemblyName System.Windows.Forms; \
                 $f = New-Object System.Windows.Forms.Form; $f.Text = 'ralphus capture probe'; \
                 $f.Width = 320; $f.Height = 200; $f.ShowDialog() | Out-Null",
            ])
            .spawn()
            .unwrap();
        let dir = scratch_dir().unwrap();
        let png = dir.join("window.png");
        // Wait for the window to appear.
        let mut result = Err(String::new());
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(500));
            result = screenshot(&Target::Pid(child.id()), &png);
            if result.is_ok() {
                break;
            }
        }
        let gif = dir.join("window.gif");
        let recorded = record_stills(&Target::Pid(child.id()), 1, 2, &gif);
        let by_title = dir.join("by-title.png");
        let titled = screenshot(
            &Target::Window("ralphus capture probe".to_string()),
            &by_title,
        );
        let _ = child.kill();
        let _ = child.wait();
        result.unwrap();
        recorded.unwrap();
        titled.unwrap();
        let titled_frame = decode_png(&by_title).unwrap();
        assert!(
            titled_frame.width < 1000,
            "the titled window, not the screen"
        );
        let frame = decode_png(&png).unwrap();
        // The form, not the whole screen.
        assert!(
            frame.width < 1000 && frame.height < 1000,
            "{}x{}",
            frame.width,
            frame.height
        );
        assert!(std::fs::read(&gif).unwrap().starts_with(b"GIF89a"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn record_format_comes_from_the_extension() {
        assert_eq!(
            RecordFormat::for_path(Path::new("a.GIF")),
            Some(RecordFormat::Gif)
        );
        assert_eq!(
            RecordFormat::for_path(Path::new("a.mp4")),
            Some(RecordFormat::Mp4)
        );
        assert_eq!(RecordFormat::for_path(Path::new("a.mov")), None);
    }
}
