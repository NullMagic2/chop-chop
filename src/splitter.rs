//! FFmpeg-backed cutting engine. All cuts are frame-accurate re-encodes.
//!
//! Video is encoded on the GPU with its maker's encoder — NVIDIA NVENC, AMD AMF or Intel
//! Quick Sync (VA-API is the Linux fallback for AMD/Intel) — and with libx264 on the CPU
//! when there is no usable GPU. The GPU is detected once and checked with a short test
//! encode; it is invisible to the user. If a GPU encode fails later, that piece is redone
//! on the CPU and the GPU is not used again.
//!
//! * Custom selection: the range is divided into N chunks that are encoded
//!   **in parallel** (one FFmpeg per CPU worker), the audio is encoded once,
//!   and everything is joined losslessly into a single clip.
//! * Batch split: the video is cut into many part files; the parts themselves
//!   are encoded in parallel across the workers.

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
    /// One complete batch part (video + audio) written straight to its own file.
    VideoPart { start: f64, len: f64, file: PathBuf },
    /// Video-only encode of [start, start+len).
    VideoChunk { start: f64, len: f64, file: PathBuf },
    /// Audio-only encode of the whole range.
    Audio { file: PathBuf },
    /// Concatenate chunks + mux audio into the final file.
    Join,
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
        .arg("format=duration:stream=codec_type,codec_name,width,height,avg_frame_rate")
        .args(["-of", "default=noprint_wrappers=1"])
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
    let (mut name, mut w, mut h) = (String::new(), 0u32, 0u32);
    for line in text.lines() {
        let Some((k, v)) = line.split_once('=') else { continue };
        match k {
            "codec_name" => name = v.to_string(),
            "width" => w = v.parse().unwrap_or(0),
            "height" => h = v.parse().unwrap_or(0),
            "codec_type" if v == "video" && info.vcodec.is_empty() => info.vcodec = name.clone(),
            "codec_type" if v == "audio" && info.acodec.is_empty() => info.acodec = name.clone(),
            "avg_frame_rate" if info.fps == 0.0 => {
                if let Some((n, d)) = v.split_once('/') {
                    let (n, d): (f64, f64) = (n.parse().unwrap_or(0.0), d.parse().unwrap_or(0.0));
                    if n > 0.0 && d > 0.0 {
                        info.fps = n / d;
                    }
                }
            }
            "duration" => {
                if let Ok(d) = v.parse::<f64>() {
                    info.duration = d;
                }
            }
            _ => {}
        }
        if k == "height" && info.width == 0 && w > 0 && !info.vcodec.is_empty() {
            info.width = w;
            info.height = h;
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

    pub fn label(self) -> &'static str {
        match self {
            VideoEncoder::Nvenc => "NVIDIA NVENC",
            VideoEncoder::Qsv => "Intel Quick Sync",
            VideoEncoder::Amf => "AMD AMF",
            VideoEncoder::Vaapi => "VA-API",
            VideoEncoder::X264 => "libx264 (CPU)",
        }
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
            VideoEncoder::Vaapi => &["-vf", "format=nv12,hwupload", "-c:v", "h264_vaapi", "-qp", "21"],
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
    fn encoders(self) -> &'static [VideoEncoder] {
        match self {
            GpuVendor::Nvidia => &[VideoEncoder::Nvenc],
            GpuVendor::Amd if cfg!(windows) => &[VideoEncoder::Amf],
            GpuVendor::Amd => &[VideoEncoder::Amf, VideoEncoder::Vaapi],
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

/// Decide how the job will be executed (also used by the UI to draw the task list).
pub fn plan_tasks(job: &CutJob) -> Vec<Task> {
    let len = (job.end - job.start).max(0.0);
    let mut tasks = Vec::new();
    if job.output.is_some() {
        {
            // One chunk per worker, but never chunks shorter than ~2 s. A GPU runs only a
            // few encodes at once, so there are no more chunks than it has slots.
            let n = job.workers.max(1).min(video_encoder().slots()).min(((len / 2.0).floor() as usize).max(1));
            let chunk = len / n as f64;
            // Chunk boundaries are snapped to whole frames so no frame is duplicated or lost.
            let snap = |t: f64| if job.fps > 0.0 { (t * job.fps).round() / job.fps } else { t };
            let bound = |i: usize| if i == 0 { job.start } else if i == n { job.end } else { snap(job.start + i as f64 * chunk) };
            tasks.extend((0..n)
                .map(|i| {
                    let s = bound(i);
                    let l = bound(i + 1) - s;
                    Task {
                        title: format!("Parallel chunk {}/{}", i + 1, n),
                        detail: format!("{} → {}", fmt_ts(s), fmt_ts(s + l)),
                        kind: TaskKind::VideoChunk { start: s, len: l, file: job.tmp_dir.join(format!("chunk{:03}.mp4", i)) },
                        weight: l,
                    }
                }));
            if job.has_audio {
                tasks.push(Task {
                    title: "Audio track".into(),
                    detail: "AAC 192 kb/s".into(),
                    kind: TaskKind::Audio { file: job.tmp_dir.join("audio.m4a") },
                    weight: len * 0.08,
                });
            }
            tasks.push(Task {
                title: "Join".into(),
                detail: "Lossless concat + mux".into(),
                kind: TaskKind::Join,
                weight: len * 0.04,
            });
        }
    }
    // Audio exports run in the same worker pool, in parallel with the video chunks.
    // They go before the Join so that Join stays last in the list.
    let join_pos = tasks.iter().position(|t| matches!(t.kind, TaskKind::Join)).unwrap_or(tasks.len());
    let digits = job.parts.len().max(job.audio_exports.len()).to_string().len().max(2);
    let mut extra: Vec<Task> = job
        .parts
        .iter()
        .enumerate()
        .map(|(i, (s, l, file))| Task {
            title: format!("Part {:0digits$}", i + 1),
            detail: format!("{} → {}", fmt_ts(*s), fmt_ts(s + l)),
            kind: TaskKind::VideoPart { start: *s, len: *l, file: file.clone() },
            weight: *l,
        })
        .collect();
    let many = job.audio_exports.len() > 1;
    extra.extend(job.audio_exports.iter().enumerate().map(|(i, (format, s, l, file))| Task {
        title: if many { format!("Part {:0digits$} · {}", i + 1, format.label()) } else { format!("{} audio", format.label()) },
        detail: if many { format!("{} → {}", fmt_ts(*s), fmt_ts(s + l)) } else { format.describe().into() },
        kind: TaskKind::AudioExport { format: *format, start: *s, len: *l, file: file.clone() },
        weight: l * 0.1,
    }));
    tasks.splice(join_pos..join_pos, extra);
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
        let parallel: Vec<usize> = tasks
            .iter()
            .enumerate()
            .filter(|(_, t)| !matches!(t.kind, TaskKind::Join))
            .map(|(i, _)| i)
            .collect();
        let video_jobs = tasks
            .iter()
            .filter(|t| matches!(t.kind, TaskKind::VideoChunk { .. } | TaskKind::VideoPart { .. }))
            .count()
            .max(1);
        let threads_per_job = (cores() / video_jobs.min(job.workers.max(1))).max(1);
        let encoder = video_encoder();
        let ctx = Arc::new(Ctx {
            encoder,
            slots: Slots::new(encoder.slots()),
            used: Mutex::new(vec![None; tasks.len()]),
        });
        let _ = std::fs::create_dir_all(&job.tmp_dir);

        // ---- parallel phase: worker pool pulls tasks from a shared queue ----
        let queue = Arc::new(Mutex::new(parallel.into_iter().collect::<VecDeque<_>>()));
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

        // ---- join phase ----
        let cancelled = cancel.load(Ordering::Relaxed);
        let mut result: Result<Vec<PathBuf>, String> = match first_err.lock().unwrap().take() {
            Some(e) => Err(e),
            None if cancelled => Err("Cancelled".into()),
            None => Ok(outputs(&job)),
        };
        if result.is_ok() {
            if let Some(j) = tasks.iter().position(|t| matches!(t.kind, TaskKind::Join)) {
                let _ = tx.send(Msg::Started(j));
                let r = join(&job, &tasks, &ctx);
                if let Err(e) = &r {
                    result = Err(e.clone());
                }
                let _ = tx.send(Msg::Finished(j, r));
            }
        }
        let _ = std::fs::remove_dir_all(&job.tmp_dir);
        if result.is_err() {
            for f in outputs(&job) {
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
    /// Which encoder actually produced each video task (indexed like `tasks`).
    used: Mutex<Vec<Option<VideoEncoder>>>,
}

/// FFmpeg command for a video chunk or part encoded with `enc`.
fn video_cmd(job: &CutJob, kind: &TaskKind, enc: VideoEncoder, threads: usize) -> (Command, f64) {
    let mut c = base_cmd();
    c.args(enc.input_args());
    match kind {
        TaskKind::VideoChunk { start, len, file } => {
            // Boundaries sit exactly on frame times; flooring to the millisecond keeps the
            // seek inside (previous frame, this frame], and an exact frame count avoids overlap.
            c.args(["-ss", &format!("{:.3}", (start * 1000.0).floor() / 1000.0)])
                .arg("-i")
                .arg(&job.input);
            if job.fps > 0.0 {
                c.args(["-frames:v", &((len * job.fps).round().max(1.0) as u64).to_string()]);
            } else {
                c.args(["-t", &format!("{:.3}", len)]);
            }
            c.args(["-map", "0:v:0", "-an", "-sn", "-dn"]).args(enc.output_args(threads)).arg(file);
            (c, *len)
        }
        TaskKind::VideoPart { start, len, file } => {
            c.args(["-ss", &format!("{:.3}", (start * 1000.0).floor() / 1000.0)])
                .arg("-i")
                .arg(&job.input)
                .args(["-t", &format!("{:.3}", len)]);
            if job.fps > 0.0 {
                c.args(["-frames:v", &((len * job.fps).round().max(1.0) as u64).to_string()]);
            }
            c.args(["-map", "0:v:0", "-map", "0:a:0?", "-sn", "-dn"])
                .args(enc.output_args(threads))
                .args(["-c:a", "aac", "-b:a", "192k", "-movflags", "+faststart"])
                .arg(file);
            (c, *len)
        }
        _ => unreachable!("video_cmd for a non-video task"),
    }
}

fn run_task(job: &CutJob, task: &Task, idx: usize, threads: usize, ctx: &Ctx, tx: &Sender<Msg>, cancel: &AtomicBool) -> Result<(), String> {
    let len = job.end - job.start;
    let (mut cmd, dur) = match &task.kind {
        TaskKind::VideoChunk { .. } | TaskKind::VideoPart { .. } => {
            // GPU first (a few at a time); on failure redo this piece on the CPU and stop
            // using the GPU for the rest of the session.
            if ctx.encoder.is_hardware() && !HW_BROKEN.load(Ordering::Relaxed) {
                if !ctx.slots.acquire(cancel) {
                    return Err("Cancelled".into());
                }
                let (mut c, d) = video_cmd(job, &task.kind, ctx.encoder, threads);
                let res = run_ffmpeg(&mut c, d, idx, tx, cancel);
                ctx.slots.release();
                match res {
                    Ok(()) => {
                        ctx.used.lock().unwrap()[idx] = Some(ctx.encoder);
                        return Ok(());
                    }
                    Err(e) if cancel.load(Ordering::Relaxed) => return Err(e),
                    Err(_) => {
                        HW_BROKEN.store(true, Ordering::Relaxed);
                        let _ = tx.send(Msg::Progress(idx, 0.0));
                    }
                }
            }
            let (mut c, d) = video_cmd(job, &task.kind, VideoEncoder::X264, threads.max(1));
            let res = run_ffmpeg(&mut c, d, idx, tx, cancel);
            if res.is_ok() {
                ctx.used.lock().unwrap()[idx] = Some(VideoEncoder::X264);
            }
            return res;
        }
        TaskKind::AudioExport { format, start, len, file } => {
            let mut c = base_cmd();
            c.args(["-ss", &format!("{:.3}", start)])
                .arg("-i")
                .arg(&job.input)
                .args(["-t", &format!("{:.3}", len)])
                .args(["-map", "0:a:0", "-vn", "-sn", "-dn", "-map_metadata", "0"])
                .args(format.codec_args())
                .arg(file);
            (c, *len)
        }
        TaskKind::Audio { file } => {
            let mut c = base_cmd();
            c.args(["-ss", &format!("{:.3}", job.start)])
                .arg("-i")
                .arg(&job.input)
                .args(["-t", &format!("{:.3}", len)])
                .args(["-map", "0:a:0", "-vn", "-c:a", "aac", "-b:a", "192k"])
                .arg(file);
            (c, len)
        }
        TaskKind::Join => unreachable!(),
    };
    run_ffmpeg(&mut cmd, dur, idx, tx, cancel)
}

fn join(job: &CutJob, tasks: &[Task], ctx: &Ctx) -> Result<(), String> {
    let list = job.tmp_dir.join("list.txt");
    let mut f = std::fs::File::create(&list).map_err(|e| e.to_string())?;
    for t in tasks {
        if let TaskKind::VideoChunk { file, .. } = &t.kind {
            let p = file.to_string_lossy().replace('\'', "'\\''");
            writeln!(f, "file '{p}'").map_err(|e| e.to_string())?;
        }
    }
    drop(f);
    // Chunks from one encoder are joined losslessly. If the GPU failed half-way and some
    // chunks came from the CPU instead, their streams differ, so the video is re-encoded.
    let used = ctx.used.lock().unwrap();
    let mut encoders = tasks.iter().enumerate().filter(|(_, t)| matches!(t.kind, TaskKind::VideoChunk { .. })).map(|(i, _)| used[i]);
    let first = encoders.next().flatten();
    let mixed = encoders.any(|e| e != first);
    drop(used);
    let mut c = tool("ffmpeg");
    c.args(["-hide_banner", "-nostdin", "-y", "-loglevel", "error"])
        .args(["-f", "concat", "-safe", "0", "-i"])
        .arg(&list);
    let audio = tasks.iter().find_map(|t| match &t.kind {
        TaskKind::Audio { file } => Some(file.clone()),
        _ => None,
    });
    if let Some(a) = &audio {
        c.arg("-i").arg(a).args(["-map", "0:v:0", "-map", "1:a:0"]);
    }
    if mixed {
        c.args(VideoEncoder::X264.output_args(cores())).args(["-c:a", "copy"]);
    } else {
        c.args(["-c", "copy"]);
    }
    c.args(["-movflags", "+faststart"]).arg(job.output.as_ref().expect("join without video output"));
    let out = c.output().map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).lines().last().unwrap_or("join failed").to_string())
    }
}

fn run_ffmpeg(cmd: &mut Command, dur: f64, idx: usize, tx: &Sender<Msg>, cancel: &AtomicBool) -> Result<(), String> {
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
                let _ = tx.send(Msg::Progress(idx, (us / 1e6 / dur.max(0.001)).clamp(0.0, 1.0)));
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
