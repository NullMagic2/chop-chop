//! FFmpeg-backed cutting engine. Cuts are frame-accurate and almost lossless:
//!
//! Only the frames between a cut and the nearest keyframe inside the range are re-encoded
//! (with the source's own codec, profile and pixel format); the video between those
//! keyframes is copied untouched, the audio is copied, and the pieces are joined without
//! re-encoding. So a cut takes about as long as copying the file.
//!
//! When that isn't possible — a codec other than H.264/HEVC, an open-GOP stream, or a range
//! with no keyframe inside — the range is re-encoded instead: on the GPU with its maker's
//! encoder (NVIDIA NVENC, AMD AMF, Intel Quick Sync; VA-API is the Linux fallback for
//! AMD/Intel), or with libx264 on the CPU when there is no usable GPU. The GPU is detected
//! once and checked with a short test encode. If a GPU encode fails, that piece is redone on
//! the CPU and the GPU is not used again.
//!
//! In a batch split every part is cut the same way, several parts at a time.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::Sender;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::thread;

#[derive(Clone, Debug, Default)]
pub struct VideoInfo {
    pub path: PathBuf,
    pub duration: f64,
    pub width: u32,
    pub height: u32,
    pub vcodec: String,
    pub acodec: String,
    pub size_bytes: u64,
    pub fps: f64,
    /// Timestamp of the file's first packet; FFmpeg's `-ss` is relative to it.
    pub start_time: f64,
    /// Video profile and pixel format, matched when the edges of a smart cut are re-encoded.
    pub profile: String,
    pub pix_fmt: String,
    /// How many frames the decoder may hold back for B-frame reordering.
    pub has_b_frames: u32,
}

/// Audio-only export formats.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AudioFormat {
    Wav,
    Mp3,
    Ogg,
    Flac,
    M4a,
    Opus,
}

impl AudioFormat {
    pub const ALL: [AudioFormat; 6] =
        [AudioFormat::Wav, AudioFormat::Mp3, AudioFormat::Ogg, AudioFormat::Flac, AudioFormat::M4a, AudioFormat::Opus];

    pub fn ext(self) -> &'static str {
        match self {
            AudioFormat::Wav => "wav",
            AudioFormat::Mp3 => "mp3",
            AudioFormat::Ogg => "ogg",
            AudioFormat::Flac => "flac",
            AudioFormat::M4a => "m4a",
            AudioFormat::Opus => "opus",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            AudioFormat::Wav => "WAV",
            AudioFormat::Mp3 => "MP3",
            AudioFormat::Ogg => "OGG",
            AudioFormat::Flac => "FLAC",
            AudioFormat::M4a => "M4A",
            AudioFormat::Opus => "OPUS",
        }
    }

    pub fn describe(self) -> &'static str {
        match self {
            AudioFormat::Wav => "PCM 16-bit, uncompressed",
            AudioFormat::Mp3 => "LAME VBR ~190 kb/s",
            AudioFormat::Ogg => "Vorbis q6 ~192 kb/s",
            AudioFormat::Flac => "Lossless",
            AudioFormat::M4a => "AAC 256 kb/s",
            AudioFormat::Opus => "Opus 160 kb/s",
        }
    }

    fn codec_args(self) -> &'static [&'static str] {
        match self {
            AudioFormat::Wav => &["-c:a", "pcm_s16le"],
            AudioFormat::Mp3 => &["-c:a", "libmp3lame", "-q:a", "2"],
            AudioFormat::Ogg => &["-c:a", "libvorbis", "-q:a", "6"],
            AudioFormat::Flac => &["-c:a", "flac"],
            AudioFormat::M4a => &["-c:a", "aac", "-b:a", "256k", "-movflags", "+faststart"],
            AudioFormat::Opus => &["-c:a", "libopus", "-b:a", "160k"],
        }
    }
}

#[derive(Clone, Debug)]
pub enum TaskKind {
    /// Standalone audio file export of the range.
    AudioExport { format: AudioFormat, start: f64, len: f64, file: PathBuf },
    /// A clip or batch part [start, start+len), cut as described at the top of this file.
    Clip { start: f64, len: f64, file: PathBuf },
}

#[derive(Clone, Debug)]
pub struct Task {
    pub title: String,
    pub detail: String,
    pub kind: TaskKind,
    /// Share of the total work, used for the overall progress bar.
    pub weight: f64,
}

#[derive(Clone, Debug)]
pub struct CutJob {
    pub input: PathBuf,
    pub start: f64,
    pub end: f64,
    pub workers: usize,
    /// Video clip destination, or None for audio-only exports.
    pub output: Option<PathBuf>,
    /// Batch parts: (start, length, file).
    pub parts: Vec<(f64, f64, PathBuf)>,
    /// Audio files: (format, start, length, file).
    pub audio_exports: Vec<(AudioFormat, f64, f64, PathBuf)>,
    pub has_audio: bool,
    pub fps: f64,
    pub tmp_dir: PathBuf,
    /// The probed source (timestamps and the codec details smart cuts match).
    pub src: VideoInfo,
}

#[derive(Debug)]
pub enum Msg {
    Started(usize),
    Progress(usize, f64),
    Finished(usize, Result<(), String>),
    AllDone { cancelled: bool, result: Result<Vec<PathBuf>, String> },
}

/// A command for one of the FFmpeg tools ("ffmpeg" / "ffprobe").
///
/// On Windows the copy installed next to `chop-chop.exe` (or in an `ffmpeg\bin` folder beside
/// it) wins over the PATH, and the console window FFmpeg would otherwise open is suppressed.
fn tool(name: &str) -> Command {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        let exe = format!("{name}.exe");
        let bundled = std::env::current_exe().ok().and_then(|p| p.parent().map(Path::to_path_buf)).and_then(|dir| {
            [dir.join(&exe), dir.join("ffmpeg").join("bin").join(&exe)].into_iter().find(|p| p.is_file())
        });
        let mut c = Command::new(bundled.unwrap_or_else(|| PathBuf::from(exe)));
        c.creation_flags(CREATE_NO_WINDOW);
        c
    }
    #[cfg(not(windows))]
    {
        Command::new(name)
    }
}

pub fn ffmpeg_available() -> bool {
    tool("ffmpeg")
        .arg("-version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

pub fn cores() -> usize {
    thread::available_parallelism().map(|n| n.get()).unwrap_or(1)
}

pub fn probe(path: &Path) -> Result<VideoInfo, String> {
    let out = tool("ffprobe")
        .args(["-v", "error", "-show_entries"])
        .arg(
            "format=duration,start_time:stream=codec_type,codec_name,profile,width,height,pix_fmt,has_b_frames,\
             avg_frame_rate,r_frame_rate:stream_disposition=attached_pic",
        )
        .args(["-of", "compact=p=0"])
        .arg(path)
        .output()
        .map_err(|e| format!("Could not run ffprobe: {e}"))?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut info = VideoInfo {
        path: path.to_path_buf(),
        size_bytes: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
        ..Default::default()
    };
    let rate = |v: &str| {
        let (n, d) = v.split_once('/')?;
        let (n, d): (f64, f64) = (n.parse().ok()?, d.parse().ok()?);
        (n > 0.0 && d > 0.0).then(|| n / d)
    };
    // One line per stream and one for the format, as "key=value|key=value…".
    for line in text.lines() {
        let f: std::collections::HashMap<&str, &str> = line.split('|').filter_map(|kv| kv.split_once('=')).collect();
        let get = |k: &str| f.get(k).copied().unwrap_or("");
        match get("codec_type") {
            // Cover art is stored as a one-frame video stream; it isn't the video.
            "video" if info.vcodec.is_empty() && get("disposition:attached_pic") != "1" => {
                info.vcodec = get("codec_name").to_string();
                info.profile = get("profile").to_string();
                info.pix_fmt = get("pix_fmt").to_string();
                info.width = get("width").parse().unwrap_or(0);
                info.height = get("height").parse().unwrap_or(0);
                info.has_b_frames = get("has_b_frames").parse().unwrap_or(0);
                info.fps = rate(get("avg_frame_rate")).or_else(|| rate(get("r_frame_rate"))).unwrap_or(0.0);
            }
            "audio" if info.acodec.is_empty() => info.acodec = get("codec_name").to_string(),
            "" => {
                info.duration = get("duration").parse().unwrap_or(0.0);
                info.start_time = get("start_time").parse().unwrap_or(0.0);
            }
            _ => {}
        }
    }
    if info.vcodec.is_empty() {
        return Err("No video stream found in this file.".into());
    }
    if info.duration <= 0.0 {
        return Err("Could not determine the video duration.".into());
    }
    Ok(info)
}

/// Grab one frame at `t` seconds as PNG bytes, scaled to fit `w`×`h`.
pub fn thumbnail(path: &Path, t: f64, w: u32, h: u32) -> Result<Vec<u8>, String> {
    let out = tool("ffmpeg")
        .args(["-v", "error", "-nostdin", "-ss", &format!("{:.3}", t.max(0.0))])
        .arg("-i")
        .arg(path)
        .args(["-frames:v", "1", "-an", "-sn"])
        .args(["-vf", &format!("scale={w}:{h}:force_original_aspect_ratio=decrease")])
        .args(["-f", "image2pipe", "-c:v", "png", "pipe:1"])
        .output()
        .map_err(|e| e.to_string())?;
    if out.stdout.is_empty() {
        return Err(String::from_utf8_lossy(&out.stderr).trim().to_string());
    }
    Ok(out.stdout)
}


// ───────────────────────── video encoder selection ─────────────────────────

/// The H.264 encoder used for video.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VideoEncoder {
    /// NVIDIA (NVENC)
    Nvenc,
    /// Intel Quick Sync
    Qsv,
    /// AMD (AMF, Windows)
    Amf,
    /// Intel / AMD through VA-API (Linux)
    Vaapi,
    /// libx264 on the CPU
    X264,
}

impl VideoEncoder {
    pub fn is_hardware(self) -> bool {
        self != VideoEncoder::X264
    }

    /// How many encodes the hardware runs at once without hitting session limits
    /// (consumer NVIDIA cards, for example, cap concurrent NVENC sessions).
    pub fn slots(self) -> usize {
        match self {
            VideoEncoder::Nvenc => 3,
            VideoEncoder::X264 => usize::MAX,
            _ => 2,
        }
    }

    fn from_name(name: &str) -> Option<VideoEncoder> {
        Some(match name.trim().to_lowercase().as_str() {
            "nvenc" | "nvidia" => VideoEncoder::Nvenc,
            "qsv" | "quicksync" | "intel" => VideoEncoder::Qsv,
            "amf" | "amd" => VideoEncoder::Amf,
            "vaapi" => VideoEncoder::Vaapi,
            "cpu" | "x264" | "libx264" | "software" => VideoEncoder::X264,
            _ => return None,
        })
    }

    /// Filter that hands frames to the encoder (VA-API encodes from GPU memory).
    fn upload_filter(self) -> Option<&'static str> {
        (self == VideoEncoder::Vaapi).then_some("format=nv12,hwupload")
    }

    /// Options that must come before `-i` (VA-API needs its device).
    fn input_args(self) -> Vec<String> {
        match self {
            VideoEncoder::Vaapi => vec!["-vaapi_device".into(), vaapi_device().to_string_lossy().into_owned()],
            _ => Vec::new(),
        }
    }

    /// Video codec options. Quality targets roughly match libx264 at CRF 20.
    fn output_args(self, threads: usize) -> Vec<String> {
        let a: &[&str] = match self {
            VideoEncoder::Nvenc => &[
                "-c:v", "h264_nvenc", "-preset", "p5", "-tune", "hq", "-rc", "vbr", "-cq", "21", "-b:v", "0",
                "-profile:v", "high", "-pix_fmt", "yuv420p",
            ],
            VideoEncoder::Qsv => &["-c:v", "h264_qsv", "-preset", "medium", "-global_quality", "21", "-pix_fmt", "nv12"],
            VideoEncoder::Amf => &[
                "-c:v", "h264_amf", "-quality", "balanced", "-rc", "cqp", "-qp_i", "20", "-qp_p", "22", "-qp_b", "24",
                "-pix_fmt", "nv12",
            ],
            VideoEncoder::Vaapi => &["-c:v", "h264_vaapi", "-qp", "21"],
            VideoEncoder::X264 => &["-c:v", "libx264", "-preset", "fast", "-crf", "20", "-pix_fmt", "yuv420p"],
        };
        let mut v: Vec<String> = a.iter().map(|s| s.to_string()).collect();
        if self == VideoEncoder::X264 {
            v.extend(["-threads".to_string(), threads.to_string()]);
        }
        v
    }
}

/// First DRM render node, used by VA-API.
fn vaapi_device() -> PathBuf {
    (128..136)
        .map(|n| PathBuf::from(format!("/dev/dri/renderD{n}")))
        .find(|p| p.exists())
        .unwrap_or_else(|| PathBuf::from("/dev/dri/renderD128"))
}

/// GPU makers, in the order they are preferred (a dedicated card before integrated graphics).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum GpuVendor {
    Nvidia,
    Amd,
    Intel,
}

impl GpuVendor {
    /// From a PCI vendor id.
    pub fn from_pci_id(id: u32) -> Option<GpuVendor> {
        match id {
            0x10DE => Some(GpuVendor::Nvidia),
            0x1002 | 0x1022 => Some(GpuVendor::Amd),
            0x8086 => Some(GpuVendor::Intel),
            _ => None,
        }
    }

    /// The maker's own encoder: NVIDIA → NVENC, AMD → AMF, Intel → Quick Sync. On Linux,
    /// where AMF and Quick Sync often aren't available, AMD and Intel can also use VA-API.
    /// For AMD on Linux VA-API comes first: AMF there sets up a Vulkan device in every FFmpeg
    /// process, which made it about half as fast as VA-API on the same GPU (RX 7900 XT).
    fn encoders(self) -> &'static [VideoEncoder] {
        match self {
            GpuVendor::Nvidia => &[VideoEncoder::Nvenc],
            GpuVendor::Amd if cfg!(windows) => &[VideoEncoder::Amf],
            GpuVendor::Amd => &[VideoEncoder::Vaapi, VideoEncoder::Amf],
            GpuVendor::Intel if cfg!(windows) => &[VideoEncoder::Qsv],
            GpuVendor::Intel => &[VideoEncoder::Qsv, VideoEncoder::Vaapi],
        }
    }
}

static GPUS: OnceLock<Vec<GpuVendor>> = OnceLock::new();

/// Tell the engine which GPUs are installed. The Windows front end calls this at startup
/// (it asks DXGI); elsewhere the engine reads them from sysfs itself.
#[allow(dead_code)]
pub fn set_gpu_vendors(mut vendors: Vec<GpuVendor>) {
    vendors.sort();
    vendors.dedup();
    let _ = GPUS.set(vendors);
}

/// The installed GPUs, most preferred first.
pub fn gpu_vendors() -> &'static [GpuVendor] {
    GPUS.get_or_init(|| {
        let mut v = detect_gpus();
        v.sort();
        v.dedup();
        v
    })
}

/// Linux: the PCI vendor of every DRM card (`/sys/class/drm/cardN/device/vendor`).
fn detect_gpus() -> Vec<GpuVendor> {
    let Ok(dir) = std::fs::read_dir("/sys/class/drm") else { return Vec::new() };
    dir.filter_map(|e| e.ok())
        .filter(|e| {
            let n = e.file_name().to_string_lossy().to_string();
            n.strip_prefix("card").is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
        })
        .filter_map(|e| std::fs::read_to_string(e.path().join("device/vendor")).ok())
        .filter_map(|id| u32::from_str_radix(id.trim().trim_start_matches("0x"), 16).ok())
        .filter_map(GpuVendor::from_pci_id)
        .collect()
}

static DETECTED: OnceLock<VideoEncoder> = OnceLock::new();
/// Set when a GPU encode failed during a job: everything after that uses the CPU.
static HW_BROKEN: AtomicBool = AtomicBool::new(false);

/// The encoder to use: the installed GPU's own encoder (NVIDIA → NVENC, AMD → AMF,
/// Intel → Quick Sync) if a short test encode with it works, else libx264 on the CPU.
/// The first call runs the test (about a second); later calls are instant.
/// `CHOP_CHOP_ENCODER=nvenc|qsv|amf|vaapi|cpu` forces a choice (for testing).
pub fn video_encoder() -> VideoEncoder {
    if HW_BROKEN.load(Ordering::Relaxed) {
        return VideoEncoder::X264;
    }
    *DETECTED.get_or_init(|| {
        if let Some(forced) = std::env::var("CHOP_CHOP_ENCODER").ok().and_then(|v| VideoEncoder::from_name(&v)) {
            return forced;
        }
        gpu_vendors()
            .iter()
            .flat_map(|v| v.encoders().iter().copied())
            .find(|e| encoder_works(*e))
            .unwrap_or(VideoEncoder::X264)
    })
}

/// Encode a few frames of a test pattern with `enc`; true if FFmpeg succeeds.
fn encoder_works(enc: VideoEncoder) -> bool {
    if enc == VideoEncoder::Vaapi && !vaapi_device().exists() {
        return false;
    }
    let mut c = tool("ffmpeg");
    c.args(["-hide_banner", "-nostdin", "-loglevel", "error"])
        .args(enc.input_args())
        .args(["-f", "lavfi", "-i", "testsrc2=size=640x360:rate=30", "-frames:v", "10"])
        .args(enc.upload_filter().map(|f| vec!["-vf", f]).unwrap_or_default())
        .args(enc.output_args(1))
        .args(["-f", "null", "-"])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    let Ok(mut child) = c.spawn() else { return false };
    // A broken driver can hang; give up after 15 s.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success(),
            Ok(None) if std::time::Instant::now() < deadline => thread::sleep(std::time::Duration::from_millis(50)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// Limits how many GPU encodes run at the same time.
struct Slots {
    free: Mutex<usize>,
    cv: Condvar,
}

impl Slots {
    fn new(n: usize) -> Slots {
        Slots { free: Mutex::new(n), cv: Condvar::new() }
    }

    /// Wait for a slot; returns false if the job was cancelled meanwhile.
    fn acquire(&self, cancel: &AtomicBool) -> bool {
        let mut free = self.free.lock().unwrap();
        loop {
            if cancel.load(Ordering::Relaxed) {
                return false;
            }
            if *free > 0 {
                *free -= 1;
                return true;
            }
            free = self.cv.wait_timeout(free, std::time::Duration::from_millis(200)).unwrap().0;
        }
    }

    fn release(&self) {
        *self.free.lock().unwrap() += 1;
        self.cv.notify_one();
    }
}

/// How far around a cut to look for keyframes, in seconds.
const KF_WINDOW: f64 = 60.0;

/// Seeking in some containers is imprecise (MPEG-TS can land after the target, Matroska well
/// before it), so every seek starts this many seconds early and the exact position is chosen
/// by timestamp afterwards.
const PREROLL: f64 = 5.0;

/// A video packet: presentation time (relative to the file start, like `-ss`), keyframe flag,
/// and decode time when the container stores it.
type Packet = (f64, bool, Option<f64>);

/// Video packets in decode order inside the given windows. Only those windows are read,
/// without decoding.
fn decode_order(src: &VideoInfo, windows: &[(f64, f64)]) -> Vec<Packet> {
    if windows.is_empty() {
        return Vec::new();
    }
    let st = src.start_time;
    let intervals: Vec<String> =
        windows.iter().map(|(a, b)| format!("{:.6}%{:.6}", (a - PREROLL).max(0.0) + st, b + st)).collect();
    let Ok(out) = tool("ffprobe")
        .args(["-v", "error", "-select_streams", "V:0", "-read_intervals"])
        .arg(intervals.join(","))
        .args(["-show_entries", "packet=pts_time,dts_time,flags", "-of", "csv=p=0"])
        .arg(&src.path)
        .output()
    else {
        return Vec::new();
    };
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| {
            let mut it = l.split(',');
            let t: f64 = it.next()?.trim().parse().ok()?;
            let dts = it.next().and_then(|d| d.trim().parse::<f64>().ok()).map(|d| d - st);
            Some((t - st, it.next().unwrap_or("").contains('K'), dts))
        })
        .collect()
}

/// The packets sorted by presentation time: (time, keyframe).
fn packets(src: &VideoInfo, windows: &[(f64, f64)]) -> Vec<(f64, bool)> {
    let mut v: Vec<(f64, bool)> = decode_order(src, windows).into_iter().map(|p| (p.0, p.1)).collect();
    v.sort_by(|a, b| a.0.total_cmp(&b.0));
    v.dedup_by(|a, b| (a.0 - b.0).abs() < 1e-6);
    v
}

/// Open GOP: frames decoded after a keyframe but shown before it, which depend on the
/// previous GOP. Such a stream can't be copied from a keyframe without losing those frames.
fn open_gop(order: &[Packet]) -> bool {
    let mut key: Option<f64> = None;
    order.iter().any(|(t, k, _)| {
        if *k {
            key = Some(*t);
            false
        } else {
            key.is_some_and(|kt| *t < kt - 1e-6)
        }
    })
}

/// Time of the first frame shown at or after `t`.
fn first_frame_at(src: &VideoInfo, t: f64) -> Option<f64> {
    packets(src, &[(t, t + 5.0)]).into_iter().map(|p| p.0).find(|p| *p >= t - 1e-6)
}

/// Decide how the job will be executed (also used by the UI to draw the task list).
pub fn plan_tasks(job: &CutJob) -> Vec<Task> {
    let clip = |title: String, start: f64, len: f64, file: PathBuf| Task {
        title,
        detail: format!("{} → {}", fmt_ts(start), fmt_ts(start + len)),
        kind: TaskKind::Clip { start, len, file },
        weight: len,
    };
    let mut tasks: Vec<Task> = job.output.iter().map(|out| clip("Clip".into(), job.start, (job.end - job.start).max(0.0), out.clone())).collect();
    let digits = job.parts.len().max(job.audio_exports.len()).to_string().len().max(2);
    tasks.extend(job.parts.iter().enumerate().map(|(i, (s, l, file))| clip(format!("Part {:0digits$}", i + 1), *s, *l, file.clone())));
    // Audio exports run in the same worker pool, in parallel with the video.
    let many = job.audio_exports.len() > 1;
    tasks.extend(job.audio_exports.iter().enumerate().map(|(i, (format, s, l, file))| Task {
        title: if many { format!("Part {:0digits$} · {}", i + 1, format.label()) } else { format!("{} audio", format.label()) },
        detail: if many { format!("{} → {}", fmt_ts(*s), fmt_ts(s + l)) } else { format.describe().into() },
        kind: TaskKind::AudioExport { format: *format, start: *s, len: *l, file: file.clone() },
        weight: l * 0.1,
    }));
    tasks
}

/// Frame-snapped (start, length) pairs for a batch split of `duration` seconds,
/// either every `every` seconds or into `parts` equal parts.
pub fn batch_bounds(duration: f64, fps: f64, every: Option<f64>, parts: Option<usize>) -> Vec<(f64, f64)> {
    let len = match (every, parts) {
        (Some(e), _) if e > 0.0 => e,
        (_, Some(n)) if n > 0 => duration / n as f64,
        _ => duration,
    };
    if duration <= 0.0 || len <= 0.0 {
        return Vec::new();
    }
    let mut count = (duration / len).ceil().max(1.0) as usize;
    // Don't leave a tiny sliver (< 0.5 s) at the end; fold it into the last part.
    if count > 1 && duration - len * ((count - 1) as f64) < 0.5 {
        count -= 1;
    }
    let snap = |t: f64| if fps > 0.0 { (t * fps).round() / fps } else { t };
    let bound = |i: usize| if i == 0 { 0.0 } else if i >= count { duration } else { snap(i as f64 * len).min(duration) };
    (0..count).map(|i| (bound(i), bound(i + 1) - bound(i))).filter(|(_, l)| *l > 0.0).collect()
}

/// Every file the job will create in the output folder.
pub fn outputs(job: &CutJob) -> Vec<PathBuf> {
    job.output
        .iter()
        .cloned()
        .chain(job.parts.iter().map(|(_, _, p)| p.clone()))
        .chain(job.audio_exports.iter().map(|(_, _, _, p)| p.clone()))
        .collect()
}

/// Execute the job on background threads; progress is reported through `tx`.
pub fn run(job: CutJob, tasks: Vec<Task>, tx: Sender<Msg>, cancel: Arc<AtomicBool>) {
    thread::spawn(move || {
        let video_jobs = tasks.iter().filter(|t| matches!(t.kind, TaskKind::Clip { .. })).count().max(1);
        let threads_per_job = (cores() / video_jobs.min(job.workers.max(1))).max(1);
        let encoder = video_encoder();
        let ctx = Arc::new(Ctx { encoder, slots: Slots::new(encoder.slots()) });
        let outputs = outputs(&job);
        let _ = std::fs::create_dir_all(&job.tmp_dir);

        // Worker pool pulls tasks from a shared queue.
        let queue = Arc::new(Mutex::new((0..tasks.len()).collect::<VecDeque<_>>()));
        let failed = Arc::new(AtomicBool::new(false));
        let first_err = Arc::new(Mutex::new(None::<String>));
        let tasks = Arc::new(tasks);
        let pool = job.workers.max(1);
        let handles: Vec<_> = (0..pool)
            .map(|_| {
                let (queue, tasks, tx, cancel, job) = (queue.clone(), tasks.clone(), tx.clone(), cancel.clone(), job.clone());
                let (failed, first_err, ctx) = (failed.clone(), first_err.clone(), ctx.clone());
                thread::spawn(move || loop {
                    if cancel.load(Ordering::Relaxed) || failed.load(Ordering::Relaxed) {
                        break;
                    }
                    let Some(i) = queue.lock().unwrap().pop_front() else { break };
                    let _ = tx.send(Msg::Started(i));
                    let res = run_task(&job, &tasks[i], i, threads_per_job, &ctx, &tx, &cancel);
                    if let Err(e) = &res {
                        if !cancel.load(Ordering::Relaxed) {
                            failed.store(true, Ordering::Relaxed);
                            first_err.lock().unwrap().get_or_insert(e.clone());
                        }
                    }
                    let _ = tx.send(Msg::Finished(i, res));
                })
            })
            .collect();
        for h in handles {
            let _ = h.join();
        }

        let cancelled = cancel.load(Ordering::Relaxed);
        let result: Result<Vec<PathBuf>, String> = match first_err.lock().unwrap().take() {
            Some(e) => Err(e),
            None if cancelled => Err("Cancelled".into()),
            None => Ok(outputs.clone()),
        };
        let _ = std::fs::remove_dir_all(&job.tmp_dir);
        if result.is_err() {
            for f in outputs {
                let _ = std::fs::remove_file(f);
            }
        }
        let _ = tx.send(Msg::AllDone { cancelled, result });
    });
}

fn base_cmd() -> Command {
    let mut c = tool("ffmpeg");
    c.args(["-hide_banner", "-nostdin", "-y", "-loglevel", "error", "-nostats", "-progress", "pipe:1"]);
    c
}

/// Shared by the worker threads of one job.
struct Ctx {
    /// The encoder chosen for this job.
    encoder: VideoEncoder,
    /// GPU encodes allowed at once.
    slots: Slots,
}

/// A time on the source's own clock, which `trim` sees when timestamps are kept (`-copyts`).
fn src_clock(job: &CutJob, t: f64) -> String {
    format!("{:.6}", t + job.src.start_time)
}

/// Input options that decode exactly the frames shown in [start, end): seek early, keep the
/// source timestamps, turn off FFmpeg's own trimming after the seek, and let `trim` choose.
/// A frame exactly on a boundary belongs to the piece that starts there, so adjacent pieces
/// never share or lose a frame.
fn exact_input(c: &mut Command, job: &CutJob, start: f64) {
    c.args(["-copyts", "-noaccurate_seek"]);
    if start - PREROLL > 0.0 {
        c.args(["-ss", &format!("{:.6}", start - PREROLL)]);
    }
    c.arg("-i").arg(&job.input);
}

fn video_trim(job: &CutJob, start: f64, end: f64) -> String {
    format!("trim=start={}:end={},setpts=PTS-STARTPTS", src_clock(job, start), src_clock(job, end))
}

fn audio_trim(job: &CutJob, start: f64, end: f64) -> String {
    format!("atrim=start={}:end={},asetpts=PTS-STARTPTS", src_clock(job, start), src_clock(job, end))
}

/// FFmpeg command that re-encodes [start, start+len) with `enc` into a finished file.
fn reencode_cmd(job: &CutJob, start: f64, len: f64, file: &Path, enc: VideoEncoder, threads: usize) -> Command {
    let mut c = base_cmd();
    c.args(enc.input_args());
    exact_input(&mut c, job, start);
    let mut vf = video_trim(job, start, start + len);
    if let Some(f) = enc.upload_filter() {
        vf = format!("{vf},{f}");
    }
    c.args(["-map", "0:V:0", "-sn", "-dn", "-vf", &vf, "-fps_mode", "passthrough"]);
    c.args(enc.output_args(threads));
    if job.has_audio {
        // The audio starts with the first frame.
        let a0 = first_frame_at(&job.src, start).unwrap_or(start);
        c.args(["-map", "0:a:0", "-af", &audio_trim(job, a0, start + len)])
            .args(["-c:a", "aac", "-b:a", "192k"]);
    } else {
        c.arg("-an");
    }
    c.args(["-movflags", "+faststart"]).arg(file);
    c
}

/// Re-encode a whole clip: on the GPU first (a few at a time); on failure redo it on the CPU
/// and stop using the GPU for the rest of the session.
#[allow(clippy::too_many_arguments)]
fn reencode(
    job: &CutJob,
    start: f64,
    len: f64,
    file: &Path,
    idx: usize,
    threads: usize,
    ctx: &Ctx,
    tx: &Sender<Msg>,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let _ = tx.send(Msg::Progress(idx, 0.0));
    if ctx.encoder.is_hardware() && !HW_BROKEN.load(Ordering::Relaxed) {
        if !ctx.slots.acquire(cancel) {
            return Err("Cancelled".into());
        }
        let res = run_ffmpeg(&mut reencode_cmd(job, start, len, file, ctx.encoder, threads), len, idx, tx, cancel, (0.0, 1.0));
        ctx.slots.release();
        match res {
            Ok(()) => return Ok(()),
            Err(e) if cancel.load(Ordering::Relaxed) => return Err(e),
            Err(_) => {
                HW_BROKEN.store(true, Ordering::Relaxed);
                let _ = tx.send(Msg::Progress(idx, 0.0));
            }
        }
    }
    run_ffmpeg(&mut reencode_cmd(job, start, len, file, VideoEncoder::X264, threads.max(1)), len, idx, tx, cancel, (0.0, 1.0))
}

/// FFmpeg muxer name for an output file extension.
fn muxer(path: &Path) -> &'static str {
    match path.extension().map(|e| e.to_string_lossy().to_lowercase()).as_deref() {
        Some("mov") => "mov",
        Some("webm") => "webm",
        Some("mkv") => "matroska",
        Some("ts") => "mpegts",
        _ => "mp4",
    }
}

/// Copy the video from the keyframe at `start` up to (not including) the keyframe at `end`,
/// without re-encoding.
///
/// The input seek starts well before the keyframe (seeking isn't exact in every container).
/// With stream copy, an output `-ss` makes FFmpeg start at the first keyframe whose *decode*
/// time is at or after it, so it is set just below the keyframe's decode time. The video then
/// stops after exactly the packets that come before the end keyframe in decode order.
#[allow(clippy::too_many_arguments)]
fn copy_range(
    job: &CutJob,
    start: f64,
    end: f64,
    dest: &Path,
    idx: usize,
    tx: &Sender<Msg>,
    cancel: &AtomicBool,
    span: (f64, f64),
) -> Result<(), String> {
    const COPY_PREROLL: f64 = 10.0;
    let frame = 1.0 / if job.fps > 0.0 { job.fps } else { 30.0 };
    let order = decode_order(&job.src, &[(start - 1.0, end + 1.0)]);
    // The copy must begin above the previous keyframe's presentation time (so that one is
    // skipped) and below this keyframe's decode time, which trails its presentation time by
    // the reorder delay. Containers like Matroska don't store every decode time; then it is
    // worked out from the next packet that has one (decode times advance a frame per packet).
    let prev_key = order.iter().filter(|(p, k, _)| *k && *p < start - 1e-3).map(|p| p.0).fold(f64::MIN, f64::max);
    let key_dts = order.iter().position(|(p, k, _)| *k && (p - start).abs() < 1e-3).and_then(|i| {
        order[i..].iter().enumerate().take(32).find_map(|(n, p)| p.2.map(|d| d - n as f64 * frame))
    });
    let key_dts = key_dts.unwrap_or(start - (job.src.has_b_frames as f64 + 1.0) * frame).min(start);
    let from = (key_dts - frame / 2.0).max(prev_key + 0.0005);
    let mut c = base_cmd();
    let pre = (start - COPY_PREROLL).max(0.0);
    if pre > 0.0 {
        c.args(["-ss", &format!("{:.6}", pre)]);
    }
    c.arg("-i").arg(&job.input);
    if from - pre > 0.0 {
        c.args(["-ss", &format!("{:.6}", from - pre)]);
    }
    let at = |t: f64| order.iter().position(|(p, k, _)| *k && (p - t).abs() < 1e-3);
    let (Some(i1), Some(i2)) = (at(start), at(end)) else {
        return Err(format!("No keyframe at {} or {}", fmt_ts(start), fmt_ts(end)));
    };
    c.args(["-frames:v", &(i2 - i1).to_string(), "-map", "0:V:0", "-an", "-sn", "-dn", "-c", "copy"])
        .args(["-avoid_negative_ts", "make_zero", "-f", muxer(dest)])
        .arg(dest);
    run_ffmpeg(&mut c, end - start, idx, tx, cancel, span)
}

/// Encoder options for re-encoding the edges of a smart cut so they fit the copied middle:
/// same codec, profile and pixel format. None if smart cuts can't handle this codec.
fn edge_encoder(src: &VideoInfo) -> Option<Vec<String>> {
    let pix = if src.pix_fmt.is_empty() { "yuv420p" } else { src.pix_fmt.as_str() };
    let args: Vec<&str> = match src.vcodec.as_str() {
        "h264" => {
            let profile = match src.profile.to_lowercase().as_str() {
                "high" => "high",
                "main" => "main",
                "baseline" | "constrained baseline" => "baseline",
                "high 10" => "high10",
                "high 4:2:2" => "high422",
                "high 4:4:4 predictive" => "high444",
                _ => return None,
            };
            vec!["-c:v", "libx264", "-preset", "medium", "-crf", "16", "-profile:v", profile, "-pix_fmt", pix]
        }
        "hevc" => {
            let profile = match src.profile.to_lowercase().as_str() {
                "main" => "main",
                "main 10" => "main10",
                _ => return None,
            };
            vec!["-c:v", "libx265", "-preset", "fast", "-crf", "18", "-profile:v", profile, "-pix_fmt", pix, "-x265-params", "log-level=error"]
        }
        _ => return None,
    };
    Some(args.into_iter().map(String::from).collect())
}

/// Presentation span of an MPEG-TS piece (first frame to the end of the last one).
fn ts_span(path: &Path, frame: f64) -> Option<f64> {
    let out = tool("ffprobe")
        .args(["-v", "error", "-select_streams", "v:0", "-show_entries", "packet=pts_time", "-of", "csv=p=0"])
        .arg(path)
        .output()
        .ok()?;
    let pts: Vec<f64> = String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter_map(|l| l.trim().trim_end_matches(',').parse().ok())
        .collect();
    let (lo, hi) = pts.iter().fold((f64::MAX, f64::MIN), |(a, b), p| (a.min(*p), b.max(*p)));
    (!pts.is_empty()).then(|| hi - lo + frame)
}

/// Frame-accurate cut of [start, start+len) that re-encodes only the frames before the first
/// keyframe in the range and after the last one; the video between them is copied. If that
/// isn't possible (codec, no keyframe inside the range) or fails, the part is re-encoded.
#[allow(clippy::too_many_arguments)]
fn smart_part(
    job: &CutJob,
    start: f64,
    len: f64,
    file: &Path,
    idx: usize,
    threads: usize,
    ctx: &Ctx,
    tx: &Sender<Msg>,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let end = start + len;
    let frame = 1.0 / if job.fps > 0.0 { job.fps } else { 30.0 };
    let order = decode_order(&job.src, &[(start, start + KF_WINDOW), ((end - KF_WINDOW).max(start), end + 0.001)]);
    let mut ks: Vec<f64> = order.iter().filter(|p| p.1).map(|p| p.0).collect();
    ks.sort_by(f64::total_cmp);
    let k1 = ks.iter().copied().find(|k| *k >= start - 1e-4);
    let k2 = ks.iter().copied().rev().find(|k| *k <= end + 1e-4);
    // Open-GOP frames after the copied keyframes would refer to re-encoded ones; re-encode instead.
    let plan = match (edge_encoder(&job.src), k1, k2) {
        (Some(enc), Some(a), Some(b)) if b > a + frame / 2.0 && !open_gop(&order) => Some((enc, a, b)),
        _ => None,
    };
    let Some((enc, k1, k2)) = plan else { return reencode(job, start, len, file, idx, threads, ctx, tx, cancel) };
    match smart_pieces(job, start, end, k1, k2, &enc, frame, file, idx, tx, cancel) {
        Err(e) if cancel.load(Ordering::Relaxed) => Err(e),
        Err(_) => reencode(job, start, len, file, idx, threads, ctx, tx, cancel),
        Ok(()) => Ok(()),
    }
}

#[allow(clippy::too_many_arguments)]
fn smart_pieces(
    job: &CutJob,
    start: f64,
    end: f64,
    k1: f64,
    k2: f64,
    enc: &[String],
    frame: f64,
    file: &Path,
    idx: usize,
    tx: &Sender<Msg>,
    cancel: &AtomicBool,
) -> Result<(), String> {
    let dir = job.tmp_dir.join(format!("smart{idx:03}"));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    let first = first_frame_at(&job.src, start).unwrap_or(k1);
    let (head, mid, tail, audio) = (dir.join("head.ts"), dir.join("mid.ts"), dir.join("tail.ts"), dir.join("audio.m4a"));
    let has_head = first < k1 - 1e-4;
    let has_tail = end - k2 > 1e-4;

    // Head: from the first frame up to the first keyframe. Its decode timestamps are moved
    // back by the source's reorder delay so they stay below those of the copied keyframe.
    let head_step = |q: &Sender<Msg>| {
        let shift = (job.src.has_b_frames as f64 + 1.0) * frame;
        let mut c = base_cmd();
        exact_input(&mut c, job, start);
        c.args(["-map", "0:V:0", "-an", "-sn", "-dn", "-vf", &video_trim(job, start, k1 - 0.0005), "-fps_mode", "passthrough"])
            .args(enc)
            .args(["-bsf:v", &format!("setts=pts=PTS:dts=DTS-round({shift:.6}/TB)"), "-f", "mpegts"])
            .arg(&head);
        run_ffmpeg(&mut c, k1 - start, idx, q, cancel, (0.0, 1.0))
    };
    // Middle: copied as is.
    let mid_step = |q: &Sender<Msg>| copy_range(job, k1, k2, &mid, idx, q, cancel, (0.0, 1.0));
    // Tail: from the last keyframe to the end, without B-frames, so its timestamps follow on.
    let tail_step = |q: &Sender<Msg>| {
        let mut c = base_cmd();
        exact_input(&mut c, job, k2 - 0.0005);
        c.args(["-map", "0:V:0", "-an", "-sn", "-dn", "-vf", &video_trim(job, k2 - 0.0005, end), "-fps_mode", "passthrough"])
            .args(enc)
            .args(["-bf", "0", "-f", "mpegts"])
            .arg(&tail);
        run_ffmpeg(&mut c, end - k2, idx, q, cancel, (0.0, 1.0))
    };
    // Audio that MP4 can hold is copied: lossless and instant, and it starts within half an
    // audio packet (about ±10 ms) of the first frame. Anything else is encoded to AAC.
    let audio_step = |q: &Sender<Msg>| {
        let mut c = base_cmd();
        if matches!(job.src.acodec.as_str(), "aac" | "mp3" | "ac3" | "eac3" | "opus" | "flac" | "alac") {
            let pre = (first - PREROLL).max(0.0);
            if pre > 0.0 {
                c.args(["-ss", &format!("{:.6}", pre)]);
            }
            c.arg("-i").arg(&job.input);
            let from = first - pre - 0.0107;
            if from > 0.0 {
                c.args(["-ss", &format!("{:.6}", from)]);
            }
            c.args(["-t", &format!("{:.6}", end - first), "-map", "0:a:0", "-vn", "-sn", "-dn", "-c:a", "copy"]);
        } else {
            exact_input(&mut c, job, first);
            c.args(["-map", "0:a:0", "-vn", "-sn", "-dn", "-af", &audio_trim(job, first, end), "-c:a", "aac", "-b:a", "192k"]);
        }
        // An MP4 holds the audio until the join. Recent FFmpeg versions treat the first AAC
        // packet in Matroska as encoder priming, which moved the audio 21 ms early.
        c.args(["-f", "mp4"]).arg(&audio);
        run_ffmpeg(&mut c, end - first, idx, q, cancel, (0.0, 1.0))
    };

    // The four pieces don't depend on each other, so they run at the same time; the task's
    // progress moves on as each one finishes.
    let mut steps: Vec<&(dyn Fn(&Sender<Msg>) -> Result<(), String> + Sync)> = vec![&mid_step];
    if has_head {
        steps.push(&head_step);
    }
    if has_tail {
        steps.push(&tail_step);
    }
    if job.has_audio {
        steps.push(&audio_step);
    }
    let total = steps.len() as f64 + 1.0;
    let done = std::sync::atomic::AtomicUsize::new(0);
    let results: Vec<Result<(), String>> = thread::scope(|scope| {
        let handles: Vec<_> = steps
            .iter()
            .map(|step| {
                let done = &done;
                let tx = tx.clone();
                scope.spawn(move || {
                    let (quiet, _) = std::sync::mpsc::channel();
                    let r = step(&quiet);
                    let n = done.fetch_add(1, Ordering::Relaxed) + 1;
                    let _ = tx.send(Msg::Progress(idx, n as f64 / total));
                    r
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err("smart cut step panicked".into()))).collect()
    });
    results.into_iter().collect::<Result<Vec<()>, String>>()?;

    // (piece, duration to the next piece)
    let mut pieces: Vec<(PathBuf, Option<f64>)> = Vec::new();
    if has_head {
        let span = ts_span(&head, frame).ok_or("smart cut: empty head")?;
        pieces.push((head, Some(span)));
    }
    pieces.push((mid, Some(k2 - k1)));
    if has_tail {
        pieces.push((tail, None));
    }

    let list = dir.join("list.txt");
    let mut f = std::fs::File::create(&list).map_err(|e| e.to_string())?;
    for (p, d) in &pieces {
        let name = concat_path(p);
        writeln!(f, "file '{name}'").map_err(|e| e.to_string())?;
        if let Some(d) = d {
            writeln!(f, "duration {d:.6}").map_err(|e| e.to_string())?;
        }
    }
    drop(f);
    let mut c = tool("ffmpeg");
    c.args(["-hide_banner", "-nostdin", "-y", "-loglevel", "error"])
        .args(["-f", "concat", "-safe", "0", "-i"])
        .arg(&list);
    if job.has_audio {
        c.arg("-i").arg(&audio).args(["-map", "0:v:0", "-map", "1:a:0"]);
    }
    c.args(["-c", "copy", "-movflags", "+faststart"]).arg(file);
    let out = c.output().map_err(|e| e.to_string())?;
    let _ = tx.send(Msg::Progress(idx, 1.0));
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("join failed").to_string())
    }
}

fn run_task(job: &CutJob, task: &Task, idx: usize, threads: usize, ctx: &Ctx, tx: &Sender<Msg>, cancel: &AtomicBool) -> Result<(), String> {
    match &task.kind {
        TaskKind::Clip { start, len, file } => smart_part(job, *start, *len, file, idx, threads, ctx, tx, cancel),
        TaskKind::AudioExport { format, start, len, file } => {
            let mut c = base_cmd();
            c.args(["-ss", &format!("{:.3}", start)])
                .arg("-i")
                .arg(&job.input)
                .args(["-t", &format!("{:.3}", len)])
                .args(["-map", "0:a:0", "-vn", "-sn", "-dn", "-map_metadata", "0"])
                .args(format.codec_args())
                .arg(file);
            run_ffmpeg(&mut c, *len, idx, tx, cancel, (0.0, 1.0))
        }
    }
}

/// A path for an FFmpeg concat list. Relative paths there are resolved against the list's
/// own folder, so it is made absolute first.
fn concat_path(p: &Path) -> String {
    std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf()).to_string_lossy().replace('\'', "'\\''")
}

/// Run one FFmpeg and report its progress as `span.0..span.1` of task `idx`.
fn run_ffmpeg(cmd: &mut Command, dur: f64, idx: usize, tx: &Sender<Msg>, cancel: &AtomicBool, span: (f64, f64)) -> Result<(), String> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped()).stdin(Stdio::null());
    let mut child = cmd.spawn().map_err(|e| format!("Could not start ffmpeg: {e}"))?;
    let mut stderr = child.stderr.take().unwrap();
    let err_reader = thread::spawn(move || {
        let mut s = String::new();
        let _ = stderr.read_to_string(&mut s);
        s
    });
    let stdout = child.stdout.take().unwrap();
    for line in BufReader::new(stdout).lines().map_while(Result::ok) {
        if cancel.load(Ordering::Relaxed) {
            let _ = child.kill();
            break;
        }
        if let Some(v) = line.strip_prefix("out_time_us=").or_else(|| line.strip_prefix("out_time_ms=")) {
            if let Ok(us) = v.trim().parse::<f64>() {
                let f = (us / 1e6 / dur.max(0.001)).clamp(0.0, 1.0);
                let _ = tx.send(Msg::Progress(idx, span.0 + f * (span.1 - span.0)));
            }
        }
    }
    let status = child.wait().map_err(|e| e.to_string())?;
    let err = err_reader.join().unwrap_or_default();
    if cancel.load(Ordering::Relaxed) {
        return Err("Cancelled".into());
    }
    if status.success() {
        Ok(())
    } else {
        Err(err.lines().last().unwrap_or("ffmpeg failed").to_string())
    }
}

/// HH:MM:SS.mmm
pub fn fmt_ts(secs: f64) -> String {
    let ms = (secs.max(0.0) * 1000.0).round() as u64;
    format!("{:02}:{:02}:{:02}.{:03}", ms / 3_600_000, (ms / 60_000) % 60, (ms / 1000) % 60, ms % 1000)
}

/// HH:MM:SS
pub fn fmt_time(secs: f64) -> String {
    let s = secs.max(0.0).round() as u64;
    format!("{:02}:{:02}:{:02}", s / 3600, (s / 60) % 60, s % 60)
}

/// Accepts "SS", "SS.mmm", "MM:SS", "HH:MM:SS(.mmm)".
pub fn parse_ts(text: &str) -> Option<f64> {
    let parts: Vec<&str> = text.trim().split(':').collect();
    if parts.is_empty() || parts.len() > 3 {
        return None;
    }
    let mut total = 0.0;
    for p in &parts {
        let v: f64 = p.trim().parse().ok()?;
        if v < 0.0 {
            return None;
        }
        total = total * 60.0 + v;
    }
    Some(total)
}

pub fn fmt_size(bytes: u64) -> String {
    let b = bytes as f64;
    if b >= 1e9 {
        format!("{:.2} GB", b / 1e9)
    } else if b >= 1e6 {
        format!("{:.1} MB", b / 1e6)
    } else {
        format!("{:.0} KB", b / 1e3)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ts_roundtrip() {
        assert_eq!(parse_ts("01:02:03.500"), Some(3723.5));
        assert_eq!(parse_ts("90"), Some(90.0));
        assert_eq!(parse_ts("1:30"), Some(90.0));
        assert_eq!(parse_ts("abc"), None);
        assert_eq!(fmt_ts(3723.5), "01:02:03.500");
    }
}
