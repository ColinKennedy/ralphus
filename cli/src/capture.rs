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

/// The PowerShell prologue every window capture shares: the Win32 calls,
/// per-monitor DPI awareness (so every rectangle is in physical pixels),
/// `Get-RalphusFrame`, `Save-RalphusWindow`, and `$h`, the handle of
/// `$env:RALPHUS_CAPTURE_PID`'s main window or of the window titled
/// `$env:RALPHUS_CAPTURE_TITLE` (zero when neither is set).
macro_rules! windows_find_window {
    () => {
        r#"
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
  [DllImport("user32.dll")] public static extern bool SetProcessDpiAwarenessContext(IntPtr c);
  [DllImport("user32.dll")] public static extern bool IsWindowVisible(IntPtr h);
  [DllImport("user32.dll")] public static extern bool IsIconic(IntPtr h);
  [DllImport("dwmapi.dll")] public static extern int DwmGetWindowAttribute(IntPtr h, int a, out RECT r, int size);
}
"@
# -4: DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2 (Windows 10 1703+).
$aware = $false
try { $aware = [RalphusCapture]::SetProcessDpiAwarenessContext([IntPtr]-4) } catch {}
if (-not $aware) { [RalphusCapture]::SetProcessDPIAware() | Out-Null }
# The window's whole rectangle and its visible frame (title bar included):
# on Windows 10 the whole rectangle also holds invisible resize borders.
function Get-RalphusFrame($h) {
  $r = New-Object RalphusCapture+RECT
  [RalphusCapture]::GetWindowRect($h, [ref]$r) | Out-Null
  $f = New-Object RalphusCapture+RECT
  # 9: DWMWA_EXTENDED_FRAME_BOUNDS.
  if ([RalphusCapture]::DwmGetWindowAttribute($h, 9, [ref]$f, 16) -ne 0 -or $f.R -le $f.L) { $f = $r }
  if ($f.R -le $f.L -or $f.B -le $f.T) { throw "the window has no area (minimized?)" }
  return @($r, $f)
}
# Writes the window's visible frame to $path as a PNG with PrintWindow, which
# renders it whether or not it is in front, cut or padded to $size when
# given. Returns the frame's size, or $null when Windows cannot render it.
function Save-RalphusWindow($h, $path, $size) {
  $r, $f = Get-RalphusFrame $h
  $full = New-Object System.Drawing.Bitmap ($r.R - $r.L), ($r.B - $r.T)
  $g = [System.Drawing.Graphics]::FromImage($full)
  $dc = $g.GetHdc()
  $ok = [RalphusCapture]::PrintWindow($h, $dc, 2)
  $g.ReleaseHdc($dc); $g.Dispose()
  if (-not $ok) { $full.Dispose(); return $null }
  $crop = New-Object System.Drawing.Rectangle ($f.L - $r.L), ($f.T - $r.T), ($f.R - $f.L), ($f.B - $f.T)
  if (-not $size) { $size = $crop.Size }
  $bmp = New-Object System.Drawing.Bitmap $size.Width, $size.Height
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.Clear([System.Drawing.Color]::Black)
  $g.DrawImage($full, (New-Object System.Drawing.Rectangle 0, 0, $crop.Width, $crop.Height), $crop, [System.Drawing.GraphicsUnit]::Pixel)
  $g.Dispose(); $full.Dispose()
  $bmp.Save($path, [System.Drawing.Imaging.ImageFormat]::Png)
  $bmp.Dispose()
  return $size
}
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
"#
    };
}

/// The PowerShell that writes a PNG of the window `windows_find_window!`
/// found, frame and title bar included, or of the primary screen when no
/// window was asked for, to `$env:RALPHUS_CAPTURE_OUT`.
const WINDOWS_STILL: &str = concat!(
    windows_find_window!(),
    r#"
$out = $env:RALPHUS_CAPTURE_OUT
if ($h -ne [IntPtr]::Zero) {
  if (Save-RalphusWindow $h $out $null) { return }
  # PrintWindow failed: copy the window's part of the screen instead.
  $r, $f = Get-RalphusFrame $h
  $bmp = New-Object System.Drawing.Bitmap ($f.R - $f.L), ($f.B - $f.T)
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.CopyFromScreen($f.L, $f.T, 0, 0, $bmp.Size)
} else {
  $b = [System.Windows.Forms.Screen]::PrimaryScreen.Bounds
  $bmp = New-Object System.Drawing.Bitmap $b.Width, $b.Height
  $g = [System.Drawing.Graphics]::FromImage($bmp)
  $g.CopyFromScreen($b.Left, $b.Top, 0, 0, $bmp.Size)
}
$g.Dispose()
$bmp.Save($out, [System.Drawing.Imaging.ImageFormat]::Png)
$bmp.Dispose()
"#
);

/// The PowerShell that records the window `windows_find_window!` found:
/// `$env:RALPHUS_CAPTURE_FRAMES` frames, one every
/// `$env:RALPHUS_CAPTURE_INTERVAL_MS`, saved as `00000.png`, `00001.png`, ...
/// in `$env:RALPHUS_CAPTURE_DIR`, each cut or padded to the first frame's
/// size. A closed or minimized window ends the recording before its next
/// frame. Prints `<frames saved> <milliseconds taken>`.
const WINDOWS_RECORD_FRAMES: &str = concat!(
    windows_find_window!(),
    r#"
if ($h -eq [IntPtr]::Zero) { throw "no window was named" }
$interval = [int]$env:RALPHUS_CAPTURE_INTERVAL_MS
$total = [int]$env:RALPHUS_CAPTURE_FRAMES
$size = $null
$clock = [System.Diagnostics.Stopwatch]::StartNew()
$n = 0
while ($n -lt $total) {
  if (-not [RalphusCapture]::IsWindowVisible($h) -or [RalphusCapture]::IsIconic($h)) { break }
  $path = Join-Path $env:RALPHUS_CAPTURE_DIR ('{0:D5}.png' -f $n)
  try { $next = Save-RalphusWindow $h $path $size } catch { $next = $null }
  # A frame taken while the window was closing is dropped.
  if (-not $next -or -not [RalphusCapture]::IsWindowVisible($h) -or [RalphusCapture]::IsIconic($h)) {
    Remove-Item -LiteralPath $path -ErrorAction SilentlyContinue
    break
  }
  $size = $next
  $n++
  $wait = $n * $interval - $clock.ElapsedMilliseconds
  if ($wait -gt 0) { Start-Sleep -Milliseconds $wait }
}
"$n $($clock.ElapsedMilliseconds)"
"#
);

/// A PowerShell process running `script` against `target`.
fn windows_capture_command(script: &str, target: &Target) -> Command {
    let mut cmd = Command::new("powershell");
    cmd.args(["-NoProfile", "-NonInteractive", "-Command", script]);
    cmd.env_remove("RALPHUS_CAPTURE_PID")
        .env_remove("RALPHUS_CAPTURE_TITLE");
    match target {
        Target::Pid(pid) => cmd.env("RALPHUS_CAPTURE_PID", pid.to_string()),
        Target::Window(title) => cmd.env("RALPHUS_CAPTURE_TITLE", title),
        Target::Screen => &mut cmd,
    };
    cmd
}

/// Write a PNG of `target` to `out`.
///
/// # Errors
///
/// When no capture tool is available for this OS/target, or the capture
/// fails.
pub fn screenshot(target: &Target, out: &Path) -> Result<(), String> {
    if cfg!(windows) {
        let mut cmd = windows_capture_command(WINDOWS_STILL, target);
        cmd.env("RALPHUS_CAPTURE_OUT", out);
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
    let ffmpeg = on_path("ffmpeg");
    if format == RecordFormat::Mp4 && !ffmpeg {
        return Err("recording an .mp4 needs ffmpeg on PATH; record a .gif instead".to_string());
    }
    if cfg!(windows) && !matches!(target, Target::Screen) {
        return record_windows_window(target, seconds, fps, format, out);
    }
    if ffmpeg {
        if let Some(input) = ffmpeg_input(target)? {
            let rate = fps.to_string();
            run(
                ffmpeg_encode_command(&input, &rate, seconds, fps, format, out),
                "ffmpeg",
            )?;
            return require_file(out);
        }
    }
    record_stills(target, seconds, fps, out)
}

/// ffmpeg's input arguments for `target` on this OS, or `None` when ffmpeg
/// cannot grab it here (the still-frame path is used instead).
fn ffmpeg_input(target: &Target) -> Result<Option<Vec<String>>, String> {
    let v = |items: &[&str]| items.iter().map(|s| (*s).to_string()).collect::<Vec<_>>();
    if cfg!(windows) {
        return Ok(match target {
            Target::Screen => Some(v(&["-f", "gdigrab", "-i", "desktop"])),
            // Recorded by `record_windows_window`.
            Target::Pid(_) | Target::Window(_) => None,
        });
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

/// Record the window `target` names on Windows. One PowerShell process
/// captures its visible frame, title bar included, with `PrintWindow` -- the
/// same image as a still, whether or not the window is in front -- and the
/// frames are encoded with ffmpeg when it is on PATH, else in-process.
/// (`gdigrab -i title=...` grabs only the client area, and grabbing the
/// screen under the window records whatever covers it and, ffmpeg being
/// slow to start, whatever is behind it once it closes.) A window that is
/// closed or minimized ends the recording.
fn record_windows_window(
    target: &Target,
    seconds: u32,
    fps: u32,
    format: RecordFormat,
    out: &Path,
) -> Result<(), String> {
    let fps = fps.max(1);
    let total = seconds.max(1) * fps;
    let frames_dir = scratch_dir()?;
    let result = (|| {
        let mut cmd = windows_capture_command(WINDOWS_RECORD_FRAMES, target);
        cmd.env("RALPHUS_CAPTURE_DIR", &frames_dir)
            .env("RALPHUS_CAPTURE_INTERVAL_MS", (1000 / fps).to_string())
            .env("RALPHUS_CAPTURE_FRAMES", total.to_string());
        let done = cmd
            .output()
            .map_err(|e| format!("could not run PowerShell: {e}"))?;
        if !done.status.success() {
            return Err(format!(
                "PowerShell window recording failed: {}",
                String::from_utf8_lossy(&done.stderr).trim()
            ));
        }
        let (count, millis) = parse_recorded(&String::from_utf8_lossy(&done.stdout))?;
        if count == 0 {
            return Err(
                "the window was closed or minimized before a frame was captured".to_string(),
            );
        }
        if count < total {
            eprintln!(
                "ralphus exhibit: the window was closed or minimized after {count} of {total} \
                 frames, so the recording stops there"
            );
        }
        if on_path("ffmpeg") {
            // The rate the frames were actually taken at, which a slow
            // capture keeps below `fps`.
            let rate = format!("{}/{}", u64::from(count) * 1000, millis.max(1));
            let input = vec![
                "-i".to_string(),
                frames_dir.join("%05d.png").display().to_string(),
            ];
            run(
                ffmpeg_encode_command(&input, &rate, seconds, fps, format, out),
                "ffmpeg",
            )?;
            return require_file(out);
        }
        let frames: Vec<PathBuf> = (0..count)
            .map(|i| frames_dir.join(format!("{i:05}.png")))
            .collect();
        encode_gif(&frames, Duration::from_millis(millis) / count, out)
    })();
    let _ = std::fs::remove_dir_all(&frames_dir);
    result
}

/// `<frames saved> <milliseconds taken>` as printed by
/// [`WINDOWS_RECORD_FRAMES`].
fn parse_recorded(text: &str) -> Result<(u32, u64), String> {
    let bad = || format!("unexpected window recording summary {:?}", text.trim());
    let fields: Vec<&str> = text.split_whitespace().collect();
    let [count, millis] = fields[..] else {
        return Err(bad());
    };
    Ok((
        count.parse().map_err(|_| bad())?,
        millis.parse().map_err(|_| bad())?,
    ))
}

/// The ffmpeg command that encodes `input`, read at `input_rate` frames a
/// second, into `out`: at most `seconds` long, at `fps`.
fn ffmpeg_encode_command(
    input: &[String],
    input_rate: &str,
    seconds: u32,
    fps: u32,
    format: RecordFormat,
    out: &Path,
) -> Command {
    let mut cmd = Command::new("ffmpeg");
    // `-t` must limit the *input*: as an output option a screen grab never
    // ends, and the GIF palette filter, which waits for the end of its
    // input, buffers frames forever.
    cmd.args([
        "-hide_banner",
        "-loglevel",
        "error",
        "-nostdin",
        "-y",
        "-framerate",
        input_rate,
    ])
    .arg("-t")
    .arg(seconds.to_string())
    .args(input);
    match format {
        // No dithering: it speckles the flat backgrounds of app windows.
        // `-1` keeps a narrow window's size exactly (`-2` would rescale an
        // odd height by a pixel).
        RecordFormat::Gif => cmd.args([
            "-vf",
            &format!(
                "fps={fps},scale='min(960,iw)':-1:flags=lanczos,split[a][b];[a]palettegen=max_colors=128[p];[b][p]paletteuse=dither=none"
            ),
            "-loop",
            "0",
        ]),
        // x264 needs an even width and height.
        RecordFormat::Mp4 => cmd.args([
            "-vf",
            "scale='trunc(min(1280,iw)/2)*2':-2",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
        ]),
    };
    cmd.arg(out);
    cmd
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
        let recording_gif = dir.join("window-recording.gif");
        let recording = record(&Target::Pid(child.id()), 1, 5, &recording_gif);
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
        // The form is 320x200 including its frame: the still keeps the title
        // bar but not the invisible resize borders.
        assert!(
            frame.height > 180 && frame.width > 300 && frame.width <= 320,
            "{}x{}",
            frame.width,
            frame.height
        );
        recording.unwrap();
        let bytes = std::fs::read(&recording_gif).unwrap();
        let decoder = gif::DecodeOptions::new().read_info(&bytes[..]).unwrap();
        // The recording is framed like the still.
        assert_eq!(
            (u32::from(decoder.width()), u32::from(decoder.height())),
            (frame.width, frame.height)
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Every frame of the GIF at `path` as RGBA, drawn over the frames
    /// before it (ffmpeg writes later frames as changed rectangles).
    #[cfg(windows)]
    fn composited_frames(path: &Path) -> Vec<Rgba> {
        let mut options = gif::DecodeOptions::new();
        options.set_color_output(gif::ColorOutput::RGBA);
        let bytes = std::fs::read(path).unwrap();
        let mut decoder = options.read_info(&bytes[..]).unwrap();
        let (width, height) = (u32::from(decoder.width()), u32::from(decoder.height()));
        let mut canvas = vec![0u8; (width * height * 4) as usize];
        let mut frames = Vec::new();
        while let Some(frame) = decoder.read_next_frame().unwrap() {
            for y in 0..u32::from(frame.height) {
                for x in 0..u32::from(frame.width) {
                    let src = ((y * u32::from(frame.width) + x) * 4) as usize;
                    if frame.buffer[src + 3] == 0 {
                        continue;
                    }
                    let (cx, cy) = (x + u32::from(frame.left), y + u32::from(frame.top));
                    let dst = ((cy * width + cx) * 4) as usize;
                    canvas[dst..dst + 4].copy_from_slice(&frame.buffer[src..src + 4]);
                }
            }
            frames.push(Rgba {
                width,
                height,
                pixels: canvas.clone(),
            });
        }
        frames
    }

    #[test]
    #[ignore = "needs a desktop session"]
    #[cfg(windows)]
    fn live_capture_a_window_closed_mid_recording_ends_the_recording() {
        if !on_path("ffmpeg") {
            return;
        }
        // A magenta form that closes itself 4 s after it is shown.
        let mut child = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-Command",
                "Add-Type -AssemblyName System.Windows.Forms; \
                 $f = New-Object System.Windows.Forms.Form; $f.Text = 'ralphus closing probe'; \
                 $f.Width = 320; $f.Height = 200; $f.BackColor = [System.Drawing.Color]::Magenta; \
                 $t = New-Object System.Windows.Forms.Timer; $t.Interval = 4000; \
                 $t.Add_Tick({ $f.Close() }); $f.Add_Shown({ $t.Start() }); \
                 $f.ShowDialog() | Out-Null",
            ])
            .spawn()
            .unwrap();
        let dir = scratch_dir().unwrap();
        let gif = dir.join("closing.gif");
        let probe = dir.join("probe.png");
        for _ in 0..20 {
            std::thread::sleep(Duration::from_millis(250));
            if screenshot(&Target::Pid(child.id()), &probe).is_ok() {
                break;
            }
        }
        let started = Instant::now();
        let recorded = record(&Target::Pid(child.id()), 8, 5, &gif);
        let took = started.elapsed();
        let _ = child.kill();
        let _ = child.wait();
        recorded.unwrap();
        assert!(
            took < Duration::from_secs(7),
            "the recording ran on: {took:?}"
        );
        let frames = composited_frames(&gif);
        assert!(!frames.is_empty());
        for (i, frame) in frames.iter().enumerate() {
            let centre = (((frame.height / 2) * frame.width + frame.width / 2) * 4) as usize;
            assert_eq!(
                frame.pixels[centre..centre + 3],
                [255, 0, 255],
                "frame {i} of {} does not show the window",
                frames.len()
            );
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_window_recording_summary_is_frames_and_milliseconds() {
        assert_eq!(parse_recorded("60 6012\r\n"), Ok((60, 6012)));
        assert_eq!(parse_recorded("0 3"), Ok((0, 3)));
        assert!(parse_recorded("").is_err());
        assert!(parse_recorded("60").is_err());
        assert!(parse_recorded("60 6012 7").is_err());
        assert!(parse_recorded("-1 20").is_err());
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
