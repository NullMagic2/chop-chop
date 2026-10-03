//! Application state and behaviour — a port of the GTK front end's `State` and signal
//! handlers onto native Win32 controls.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::Instant;

use windows::core::{HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::Graphics::Gdi::{InvalidateRect, HFONT};
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::*;

use crate::i18n::{tr, trf};
use crate::image::Image;
use crate::splitter::{self, AudioFormat, CutJob, Msg, TaskKind, VideoInfo};

pub const MIN_CLIP: f64 = 0.1;

pub const WM_APP_THUMB: u32 = WM_APP + 1;
pub const WM_APP_STARTUP: u32 = WM_APP + 2;

pub const TIMER_PREVIEW: usize = 1;
pub const TIMER_JOBS: usize = 2;

// Text entries (index into `Ui::edits`).
pub const E_START: usize = 0;
pub const E_END: usize = 1;
pub const E_NAME: usize = 2;
pub const E_MIN: usize = 3;
pub const E_SEC: usize = 4;
pub const E_PARTS: usize = 5;
pub const E_WORKERS: usize = 6;
pub const N_EDITS: usize = 7;
pub const EDIT_ID_BASE: usize = 100;

/// Spin entries and their ranges (workers' maximum depends on the CPU).
pub const SPINS: [usize; 4] = [E_MIN, E_SEC, E_PARTS, E_WORKERS];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Handle {
    Start,
    End,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Mode {
    Custom,
    Batch,
}

/// Every native control in the window.
#[derive(Default)]
pub struct Ui {
    pub hwnd: HWND,
    pub open_btn: HWND,
    pub lang_btn: HWND,
    pub dir_btn: HWND,
    pub video_grp: HWND,
    pub thumb: HWND,
    pub drop_title: HWND,
    pub drop_sub: HWND,
    pub drop_info: HWND,
    pub preview_grp: HWND,
    pub preview: HWND,
    pub tab: HWND,
    pub timeline: HWND,
    pub start_lbl: HWND,
    pub end_lbl: HWND,
    pub batch_head: HWND,
    pub every_radio: HWND,
    pub parts_radio: HWND,
    pub min_lbl: HWND,
    pub sec_lbl: HWND,
    pub parts_lbl: HWND,
    pub export_grp: HWND,
    pub audio_btn: HWND,
    pub workers_lbl: HWND,
    pub cores_lbl: HWND,
    pub out_lbl: HWND,
    pub out_edit: HWND,
    pub out_btn: HWND,
    pub name_lbl: HWND,
    pub ext_lbl: HWND,
    pub open_check: HWND,
    pub go_btn: HWND,
    pub progress_grp: HWND,
    pub bar: HWND,
    pub percent: HWND,
    pub list: HWND,
    pub status: HWND,
    pub edits: [HWND; N_EDITS],
    pub spins: [HWND; N_EDITS],
    pub tooltip: HWND,
}

pub struct Row {
    pub frac: f64,
    pub weight: f64,
}

pub struct App {
    pub ui: Ui,
    pub font: HFONT,
    pub bold_font: HFONT,
    pub scale: f32,

    pub info: Option<VideoInfo>,
    pub mode: Mode,
    pub split_every: bool,
    pub start: f64,
    pub end: f64,
    pub focus: Handle,
    pub drag: Option<Handle>,
    pub hot: Option<Handle>,
    pub batch: Vec<(f64, f64)>,
    pub preview_gen: u64,
    pub name_auto: bool,
    pub running: bool,
    pub cancelling: bool,
    pub cancel: Option<Arc<AtomicBool>>,
    pub rows: Vec<Row>,
    pub started: Option<Instant>,
    pub out_dir_user_set: bool,
    pub out_dir: Option<PathBuf>,
    pub last_audio: AudioFormat,
    pub spin_vals: [f64; N_EDITS],
    pub edit_error: [bool; N_EDITS],
    pub badge: String,
    pub preview_img: Option<Image>,
    pub card_img: Option<Image>,
    pub rx: Option<mpsc::Receiver<Msg>>,
    pub status: String,
    /// " · NVIDIA NVENC" etc. while a video export runs.
    pub encoder_note: String,
}

/// Thumbnails decoded on worker threads, handed over via `WM_APP_THUMB`.
pub enum ThumbKind {
    Preview,
    Card,
}
pub static THUMBS: Mutex<Vec<(ThumbKind, u64, Option<Image>)>> = Mutex::new(Vec::new());

// ───────────────────────── control helpers ─────────────────────────

pub fn text_of(h: HWND) -> String {
    unsafe {
        let n = GetWindowTextLengthW(h);
        let mut buf = vec![0u16; n as usize + 1];
        let got = GetWindowTextW(h, &mut buf);
        String::from_utf16_lossy(&buf[..got.max(0) as usize])
    }
}

pub fn set_text(h: HWND, s: &str) {
    if text_of(h) != s {
        unsafe {
            let _ = SetWindowTextW(h, &HSTRING::from(s));
        }
    }
}

pub fn enable(h: HWND, on: bool) {
    unsafe {
        let _ = EnableWindow(h, on);
    }
}

pub fn set_check(h: HWND, on: bool) {
    unsafe {
        SendMessageW(h, BM_SETCHECK, Some(WPARAM(if on { 1 } else { 0 })), None);
    }
}

pub fn is_checked(h: HWND) -> bool {
    unsafe { SendMessageW(h, BM_GETCHECK, None, None).0 == 1 }
}

pub fn redraw(h: HWND) {
    unsafe {
        let _ = InvalidateRect(Some(h), None, false);
    }
}

pub fn cores() -> usize {
    splitter::cores()
}

// ───────────────────────── list view ─────────────────────────

fn lv_set(list: HWND, row: usize, col: i32, text: &str) {
    let mut w: Vec<u16> = text.encode_utf16().chain(std::iter::once(0)).collect();
    let item = LVITEMW {
        mask: LVIF_TEXT,
        iItem: row as i32,
        iSubItem: col,
        pszText: PWSTR(w.as_mut_ptr()),
        ..Default::default()
    };
    unsafe {
        if col == 0 {
            SendMessageW(list, LVM_INSERTITEMW, Some(WPARAM(0)), Some(LPARAM(&item as *const _ as isize)));
        } else {
            SendMessageW(list, LVM_SETITEMTEXTW, Some(WPARAM(row)), Some(LPARAM(&item as *const _ as isize)));
        }
    }
}

impl App {
    // ───────────── spin entries ─────────────

    pub fn spin_range(&self, idx: usize) -> (f64, f64) {
        match idx {
            E_MIN => (0.0, 1440.0),
            E_SEC => (0.0, 59.0),
            E_PARTS => (2.0, 999.0),
            _ => (1.0, (cores() * 2).max(2) as f64),
        }
    }

    pub fn set_spin(&mut self, idx: usize, v: f64) {
        let (lo, hi) = self.spin_range(idx);
        let v = v.round().clamp(lo, hi);
        self.spin_vals[idx] = v;
        set_text(self.ui.edits[idx], &format!("{}", v as i64));
        unsafe {
            SendMessageW(self.ui.spins[idx], UDM_SETPOS32, Some(WPARAM(0)), Some(LPARAM(v as isize)));
        }
        self.edit_error[idx] = false;
        redraw(self.ui.edits[idx]);
        if idx != E_WORKERS {
            self.batch_changed();
        }
    }

    /// The user typed in a spin entry (or clicked its arrows).
    pub fn spin_typed(&mut self, idx: usize) {
        let (lo, hi) = self.spin_range(idx);
        match text_of(self.ui.edits[idx]).trim().parse::<f64>() {
            Ok(v) if v >= lo && v <= hi => {
                self.spin_vals[idx] = v;
                self.edit_error[idx] = false;
                if idx != E_WORKERS {
                    self.batch_changed();
                }
            }
            _ => self.edit_error[idx] = true,
        }
        redraw(self.ui.edits[idx]);
    }

    pub fn spin_commit(&mut self, idx: usize) {
        let v = text_of(self.ui.edits[idx]).trim().parse::<f64>().unwrap_or(self.spin_vals[idx]);
        self.set_spin(idx, v);
    }

    // ───────────── modes ─────────────

    pub fn set_mode(&mut self, mode: Mode) {
        self.mode = mode;
        self.mode_changed();
    }

    pub fn mode_changed(&mut self) {
        if self.mode == Mode::Batch {
            self.batch_changed();
        } else {
            self.update_auto_name();
            self.refresh_go();
        }
        crate::layout(self);
        self.request_preview(0);
    }

    pub fn batch_changed(&mut self) {
        let (dur, fps) = self.info.as_ref().map(|i| (i.duration, i.fps)).unwrap_or((0.0, 0.0));
        let every_secs = self.spin_vals[E_MIN] * 60.0 + self.spin_vals[E_SEC];
        self.batch = if self.split_every {
            splitter::batch_bounds(dur, fps, Some(every_secs), None)
        } else {
            splitter::batch_bounds(dur, fps, None, Some(self.spin_vals[E_PARTS].max(1.0) as usize))
        };
        self.update_auto_name();
        self.refresh_go();
    }

    /// Main button text + enabled state for the current mode.
    pub fn refresh_go(&self) {
        set_text(self.ui.ext_lbl, if self.mode == Mode::Custom { ".mp4" } else { "_partNN.mp4" });
        if self.running {
            set_text(self.ui.go_btn, &tr("Cancel"));
            enable(self.ui.go_btn, !self.cancelling);
            return;
        }
        let (text, ok) = match self.mode {
            Mode::Custom => (tr("Cut clip"), self.end - self.start >= MIN_CLIP),
            Mode::Batch => {
                let n = self.batch.len();
                (
                    match n {
                        0 => tr("Split video"),
                        1 => tr("Export 1 part"),
                        n => trf("Split into {n} parts", &[("n", n.to_string())]),
                    },
                    n > 0,
                )
            }
        };
        set_text(self.ui.go_btn, &text);
        enable(self.ui.go_btn, self.info.is_some() && ok);
    }

    pub fn has_audio(&self) -> bool {
        self.info.as_ref().map(|i| !i.acodec.is_empty()).unwrap_or(false)
    }

    pub fn audio_tooltip(&self) -> String {
        if self.info.is_some() && !self.has_audio() {
            return tr("This video has no audio track");
        }
        tr(match self.mode {
            Mode::Custom => "Save the selected range as WAV, MP3, OGG, FLAC, M4A or OPUS",
            Mode::Batch => "Save one audio file per part as WAV, MP3, OGG, FLAC, M4A or OPUS",
        })
    }

    // ───────────── start / end ─────────────

    fn set_handle(&mut self, h: Handle, t: f64) -> Option<f64> {
        let d = self.info.as_ref().map(|i| i.duration)?;
        let v = match h {
            Handle::Start => t.clamp(0.0, (self.end - MIN_CLIP).max(0.0)),
            Handle::End => t.clamp((self.start + MIN_CLIP).min(d), d),
        };
        match h {
            Handle::Start => self.start = v,
            Handle::End => self.end = v,
        }
        self.set_focus_handle(h, false);
        Some(v)
    }

    /// The preview follows the handle that was moved (or whose time entry got focus) last.
    pub fn set_focus_handle(&mut self, h: Handle, refresh: bool) {
        let changed = self.focus != h;
        self.focus = h;
        if changed && refresh {
            redraw(self.ui.timeline);
            self.request_preview(0);
        }
    }

    pub fn move_handle(&mut self, h: Handle, t: f64) {
        if self.set_handle(h, t).is_none() {
            return;
        }
        self.range_changed();
        self.request_preview(140);
    }

    /// Live update while the user types a time: move the bar, but don't rewrite the text.
    pub fn entry_typed(&mut self, idx: usize) {
        if self.info.is_none() {
            return;
        }
        let h = if idx == E_START { Handle::Start } else { Handle::End };
        let Some(t) = splitter::parse_ts(&text_of(self.ui.edits[idx])) else {
            self.edit_error[idx] = true;
            redraw(self.ui.edits[idx]);
            return;
        };
        let dur = self.info.as_ref().map(|i| i.duration).unwrap_or(0.0);
        let valid = match h {
            Handle::Start => t <= self.end - MIN_CLIP,
            Handle::End => t >= self.start + MIN_CLIP && t <= dur + 0.0005,
        };
        self.edit_error[idx] = !valid;
        redraw(self.ui.edits[idx]);
        self.set_handle(h, t);
        redraw(self.ui.timeline);
        self.update_auto_name();
        self.refresh_go();
        self.request_preview(250);
    }

    pub fn apply_entry(&mut self, idx: usize) {
        if self.info.is_none() {
            return;
        }
        let h = if idx == E_START { Handle::Start } else { Handle::End };
        match splitter::parse_ts(&text_of(self.ui.edits[idx])) {
            Some(t) => {
                let cur = if h == Handle::Start { self.start } else { self.end };
                if (cur - t).abs() > 0.0005 {
                    self.move_handle(h, t);
                } else {
                    self.range_changed();
                }
            }
            None => {
                self.edit_error[idx] = true;
                redraw(self.ui.edits[idx]);
            }
        }
    }

    pub fn range_changed(&mut self) {
        for (idx, v) in [(E_START, self.start), (E_END, self.end)] {
            set_text(self.ui.edits[idx], &splitter::fmt_ts(v));
            self.edit_error[idx] = false;
            redraw(self.ui.edits[idx]);
        }
        redraw(self.ui.timeline);
        self.update_auto_name();
        self.refresh_go();
    }

    pub fn update_auto_name(&mut self) {
        if !self.name_auto {
            return;
        }
        let Some(info) = &self.info else { return };
        let stem = info.path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "clip".into());
        let name = match self.mode {
            Mode::Batch => stem,
            Mode::Custom => format!(
                "{stem}_{}-{}",
                splitter::fmt_ts(self.start)[..8].replace(':', "."),
                splitter::fmt_ts(self.end)[..8].replace(':', ".")
            ),
        };
        // EN_CHANGE from this call is ignored (the app is borrowed), so name_auto stays set.
        set_text(self.ui.edits[E_NAME], &name);
    }

    pub fn clean_name(&self) -> String {
        let n: String = text_of(self.ui.edits[E_NAME]).trim().replace(['/', '\\', ':', '*', '?', '"', '<', '>', '|'], "_");
        if n.is_empty() {
            "clip".into()
        } else {
            n
        }
    }

    // ───────────── thumbnails ─────────────

    /// Debounced frame grab for the big preview.
    pub fn request_preview(&mut self, delay_ms: u32) {
        if self.info.is_none() {
            return;
        }
        self.preview_gen += 1;
        self.badge = match self.mode {
            Mode::Batch => format!("{} · {}", tr("START"), splitter::fmt_ts(0.0)),
            Mode::Custom => match self.focus {
                Handle::Start => format!("{} · {}", tr("START"), splitter::fmt_ts(self.start)),
                Handle::End => format!("{} · {}", tr("END"), splitter::fmt_ts(self.end)),
            },
        };
        redraw(self.ui.preview);
        unsafe {
            SetTimer(Some(self.ui.hwnd), TIMER_PREVIEW, delay_ms.max(1), None);
        }
    }

    /// TIMER_PREVIEW fired: grab the frame on a worker thread.
    pub fn grab_preview(&mut self) {
        unsafe {
            let _ = KillTimer(Some(self.ui.hwnd), TIMER_PREVIEW);
        }
        let Some(info) = &self.info else { return };
        let dur = info.duration;
        let t = match self.mode {
            Mode::Batch => 0.0,
            Mode::Custom => match self.focus {
                Handle::Start => self.start,
                // The very last frame often can't be decoded; back off a little.
                Handle::End => (self.end - 0.05).max(0.0).min((dur - 0.1).max(0.0)),
            },
        };
        let mut rc = Default::default();
        unsafe {
            let _ = GetClientRect(self.ui.preview, &mut rc);
        }
        let (w, h) = ((rc.right - rc.left).max(16) as u32, (rc.bottom - rc.top).max(16) as u32);
        spawn_thumb(self.ui.hwnd, ThumbKind::Preview, self.preview_gen, info.path.clone(), t, w, h);
    }

    pub fn thumbs_ready(&mut self) {
        let items: Vec<_> = std::mem::take(&mut *THUMBS.lock().unwrap());
        for (kind, gen, img) in items {
            match kind {
                ThumbKind::Preview if gen == self.preview_gen => {
                    if img.is_some() {
                        self.preview_img = img;
                    }
                    redraw(self.ui.preview);
                }
                ThumbKind::Card => {
                    self.card_img = img;
                    redraw(self.ui.thumb);
                }
                _ => {}
            }
        }
    }

    // ───────────── files ─────────────

    pub fn set_out_dir(&mut self, dir: &Path) {
        self.out_dir = Some(dir.to_path_buf());
        set_text(self.ui.out_edit, &dir.display().to_string());
    }

    /// Returns an error (title, body) to show once the app is no longer borrowed.
    pub fn load_file(&mut self, path: &Path) -> Result<(), (String, String)> {
        // Paths from the command line can be relative; the output folder needs the real one.
        let abs = std::path::absolute(path).unwrap_or_else(|_| path.to_path_buf());
        let path = abs.as_path();
        self.set_status(&tr("Reading video information…"));
        let info = match splitter::probe(path) {
            Ok(i) => i,
            Err(e) => {
                self.set_status("");
                return Err((tr("Can't open this video"), e));
            }
        };
        set_text(self.ui.drop_title, &path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default());
        set_text(self.ui.drop_sub, &path.parent().map(|p| p.display().to_string()).unwrap_or_default());
        let mut tags = vec![splitter::fmt_time(info.duration)];
        if info.width > 0 {
            tags.push(format!("{}×{}", info.width, info.height));
        }
        tags.push(info.vcodec.to_uppercase());
        if !info.acodec.is_empty() {
            tags.push(info.acodec.to_uppercase());
        }
        tags.push(splitter::fmt_size(info.size_bytes));
        set_text(self.ui.drop_info, &tags.join("   ·   "));
        if !self.out_dir_user_set {
            if let Some(parent) = path.parent() {
                self.set_out_dir(parent);
            }
        }
        unsafe {
            SendMessageW(self.ui.list, LVM_DELETEALLITEMS, None, None);
        }
        self.rows.clear();
        self.set_overall(0.0, PBST_NORMAL);
        self.set_status("");

        let dur = info.duration;
        self.card_img = None;
        self.preview_img = None;
        spawn_thumb(
            self.ui.hwnd,
            ThumbKind::Card,
            0,
            info.path.clone(),
            (dur * 0.1).min(30.0),
            (128.0 * self.scale) as u32,
            (72.0 * self.scale) as u32,
        );
        self.info = Some(info);
        self.start = 0.0;
        self.end = dur;
        self.name_auto = true;
        self.set_focus_handle(Handle::Start, false);
        for idx in [E_START, E_END] {
            enable(self.ui.edits[idx], true);
        }
        enable(self.ui.audio_btn, self.has_audio());
        self.range_changed();
        self.batch_changed();
        self.mode_changed();
        Ok(())
    }

    pub fn set_status(&mut self, s: &str) {
        self.status = s.to_string();
        set_text(self.ui.status, s);
    }

    fn set_overall(&self, frac: f64, state: u32) {
        unsafe {
            SendMessageW(self.ui.bar, PBM_SETSTATE, Some(WPARAM(state as usize)), None);
            SendMessageW(self.ui.bar, PBM_SETPOS, Some(WPARAM((frac * 1000.0).round() as usize)), None);
        }
        set_text(self.ui.percent, &format!("{:.0}%", frac * 100.0));
    }

    pub fn set_running_ui(&mut self, running: bool) {
        self.running = running;
        if !running {
            self.cancelling = false;
        }
        let loaded = self.info.is_some();
        for idx in 0..N_EDITS {
            let on = !running && (loaded || !matches!(idx, E_START | E_END));
            enable(self.ui.edits[idx], on);
            enable(self.ui.spins[idx], on);
        }
        let u = &self.ui;
        for h in [u.open_btn, u.thumb, u.drop_title, u.tab, u.timeline, u.every_radio, u.parts_radio, u.out_btn] {
            enable(h, !running);
        }
        enable(u.audio_btn, !running && self.has_audio());
        redraw(u.timeline);
        self.refresh_go();
    }

    /// Plan the export and, if `files_ok` approves the files it would write, launch it.
    /// `audio == None` exports video.
    pub fn start_job(
        &mut self,
        audio: Option<(AudioFormat, PathBuf)>,
        files_ok: impl FnOnce(&[PathBuf]) -> bool,
    ) -> Result<(), (String, String)> {
        let Some(info) = self.info.clone() else { return Ok(()) };
        let (mode, start_t, end_t, bounds) = (self.mode, self.start, self.end, self.batch.clone());
        if mode == Mode::Custom && end_t - start_t < MIN_CLIP {
            return Err((tr("The clip is too short"), tr("Choose an end time after the start time.")));
        }
        if mode == Mode::Batch && bounds.is_empty() {
            return Err((tr("Nothing to split"), tr("Choose a part length or a number of parts.")));
        }
        let Some(out_dir) = self.out_dir.clone() else {
            return Err((tr("No output folder"), tr("Please choose where the files should be saved.")));
        };
        if let Err(e) = std::fs::create_dir_all(&out_dir) {
            return Err((tr("Output folder is not writable"), e.to_string()));
        }
        let name = self.clean_name();
        let n = bounds.len();
        let mut output = None;
        let mut parts = Vec::new();
        let mut audio_exports = Vec::new();
        match (&audio, mode) {
            (None, Mode::Custom) => output = Some(out_dir.join(format!("{name}.mp4"))),
            (None, Mode::Batch) => {
                parts = bounds
                    .iter()
                    .enumerate()
                    .map(|(i, (s, l))| (*s, *l, out_dir.join(part_name(&name, i, n, "mp4"))))
                    .collect()
            }
            (Some((fmt, path)), Mode::Custom) => audio_exports.push((*fmt, start_t, end_t - start_t, path.clone())),
            (Some((fmt, path)), Mode::Batch) => {
                let dir = path.parent().map(Path::to_path_buf).unwrap_or_else(|| out_dir.clone());
                let stem = path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| name.clone());
                audio_exports = bounds
                    .iter()
                    .enumerate()
                    .map(|(i, (s, l))| (*fmt, *s, *l, dir.join(part_name(&stem, i, n, fmt.ext()))))
                    .collect();
            }
        }
        let job_preview = CutJob {
            input: info.path.clone(),
            start: start_t,
            end: end_t,
            workers: 1,
            output,
            parts,
            audio_exports,
            has_audio: false,
            fps: 0.0,
            tmp_dir: PathBuf::new(),
        };
        let files = splitter::outputs(&job_preview);
        if files.iter().any(|p| same_file(p, &info.path)) {
            return Err((tr("Choose another file name"), tr("The export would overwrite the original video.")));
        }
        if !files_ok(&files) {
            return Ok(());
        }
        let work_dir = files.first().and_then(|p| p.parent()).map(Path::to_path_buf).unwrap_or_else(|| out_dir.clone());
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        let job = CutJob {
            workers: self.spin_vals[E_WORKERS].max(1.0) as usize,
            has_audio: !info.acodec.is_empty(),
            fps: info.fps,
            tmp_dir: work_dir.join(format!(".chop-chop-{}-{stamp}", std::process::id())),
            ..job_preview
        };
        let tasks = splitter::plan_tasks(&job);

        // Job list: Job | Range | Progress | Status
        unsafe {
            SendMessageW(self.ui.list, LVM_DELETEALLITEMS, None, None);
        }
        for (i, t) in tasks.iter().enumerate() {
            lv_set(self.ui.list, i, 0, &loc_title(&t.title));
            lv_set(self.ui.list, i, 1, &tr(&t.detail));
            lv_set(self.ui.list, i, 2, "0%");
            lv_set(self.ui.list, i, 3, &tr("Queued"));
        }
        self.rows = tasks.iter().map(|t| Row { frac: 0.0, weight: t.weight }).collect();

        let cancel = Arc::new(AtomicBool::new(false));
        self.cancel = Some(cancel.clone());
        self.started = Some(Instant::now());
        self.set_overall(0.0, PBST_NORMAL);
        let chunks = tasks.iter().filter(|t| matches!(t.kind, TaskKind::VideoChunk { .. })).count();
        let w = job.workers.min(n).to_string();
        let mut status = match (&audio, mode) {
            (None, Mode::Custom) => trf(
                "Encoding {len} — split into {n} chunk(s) so every core helps…",
                &[("len", splitter::fmt_ts(end_t - start_t)), ("n", chunks.to_string())],
            ),
            (None, Mode::Batch) => trf("Splitting into {n} parts, {w} at a time…", &[("n", n.to_string()), ("w", w)]),
            (Some((f, _)), Mode::Custom) => trf("Exporting {fmt} audio…", &[("fmt", f.label().to_string())]),
            (Some((f, _)), Mode::Batch) => trf(
                "Exporting {n} {fmt} files, {w} at a time…",
                &[("n", n.to_string()), ("fmt", f.label().to_string()), ("w", w)],
            ),
        };
        // Which encoder does the work (the GPU's, or libx264 on the CPU), shown while it runs.
        self.encoder_note = if audio.is_none() { format!(" · {}", splitter::video_encoder().label()) } else { String::new() };
        status.push_str(&self.encoder_note);
        self.set_status(&status);
        self.set_running_ui(true);
        let (tx, rx) = mpsc::channel::<Msg>();
        splitter::run(job, tasks, tx, cancel);
        self.rx = Some(rx);
        unsafe {
            SetTimer(Some(self.ui.hwnd), TIMER_JOBS, 80, None);
        }
        Ok(())
    }

    pub fn cancel_job(&mut self) {
        if let Some(c) = &self.cancel {
            c.store(true, Ordering::Relaxed);
        }
        self.cancelling = true;
        self.set_status(&tr("Cancelling…"));
        self.refresh_go();
    }

    /// TIMER_JOBS: drain engine messages. Returns the folder to open when the job finished.
    pub fn poll_jobs(&mut self) -> Option<PathBuf> {
        let mut finished = None;
        let list = self.ui.list;
        if let Some(rx) = &self.rx {
            while let Ok(msg) = rx.try_recv() {
                match msg {
                    Msg::Started(i) => {
                        lv_set(list, i, 3, &tr("Working"));
                        unsafe {
                            SendMessageW(list, LVM_ENSUREVISIBLE, Some(WPARAM(i)), Some(LPARAM(0)));
                        }
                    }
                    Msg::Progress(i, f) => {
                        self.rows[i].frac = f;
                        lv_set(list, i, 2, &format!("{:.0}%", f * 100.0));
                    }
                    Msg::Finished(i, res) => match res {
                        Ok(()) => {
                            self.rows[i].frac = 1.0;
                            lv_set(list, i, 2, "100%");
                            lv_set(list, i, 3, &tr("Done"));
                        }
                        Err(e) if e == "Cancelled" => lv_set(list, i, 3, &tr("Stopped")),
                        Err(e) => lv_set(list, i, 3, &format!("{} — {e}", tr("Failed"))),
                    },
                    Msg::AllDone { cancelled, result } => finished = Some((cancelled, result)),
                }
            }
        }
        let total: f64 = self.rows.iter().map(|r| r.weight).sum();
        let done: f64 = self.rows.iter().map(|r| r.frac * r.weight).sum();
        let frac = if total > 0.0 { (done / total).clamp(0.0, 1.0) } else { 0.0 };
        self.set_overall(frac, PBST_NORMAL);
        let elapsed = self.started.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
        if let Some((cancelled, result)) = finished {
            unsafe {
                let _ = KillTimer(Some(self.ui.hwnd), TIMER_JOBS);
            }
            self.rx = None;
            self.cancel = None;
            self.set_running_ui(false);
            match result {
                Ok(paths) => {
                    self.set_overall(1.0, PBST_NORMAL);
                    let size: u64 = paths.iter().map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)).sum();
                    let what = if paths.len() == 1 {
                        format!("“{}”", paths[0].file_name().unwrap_or_default().to_string_lossy())
                    } else {
                        trf("{n} files", &[("n", paths.len().to_string())])
                    };
                    self.set_status(&trf(
                        "Saved {what} · {size} · done in {secs} s",
                        &[("what", what), ("size", splitter::fmt_size(size)), ("secs", format!("{elapsed:.1}"))],
                    ));
                    if is_checked(self.ui.open_check) {
                        return paths.first().and_then(|p| p.parent()).map(Path::to_path_buf);
                    }
                }
                Err(_) if cancelled => self.set_status(&tr("Cancelled — no files were kept.")),
                Err(e) => {
                    self.set_overall(frac, PBST_ERROR);
                    self.set_status(&trf("Failed: {e}", &[("e", e)]));
                }
            }
            return None;
        }
        if frac > 0.02 && elapsed > 1.0 && !self.cancelling {
            let eta = elapsed / frac * (1.0 - frac);
            let eta_text = trf(
                "{t} elapsed · about {left} left",
                &[("t", splitter::fmt_time(elapsed)), ("left", splitter::fmt_time(eta))],
            );
            let note = self.encoder_note.clone();
            self.set_status(&format!("{eta_text}{note}"));
        }
        None
    }
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy()),
    }
}

pub fn part_name(stem: &str, i: usize, n: usize, ext: &str) -> String {
    let digits = n.to_string().len().max(2);
    format!("{stem}_part{:0digits$}.{ext}", i + 1)
}

/// Translate the engine's job titles ("Parallel chunk 1/2", "Part 03 · MP3", "MP3 audio", …).
pub fn loc_title(t: &str) -> String {
    if let Some(rest) = t.strip_prefix("Parallel chunk ") {
        if let Some((i, n)) = rest.split_once('/') {
            return trf("Parallel chunk {i}/{n}", &[("i", i.to_string()), ("n", n.to_string())]);
        }
    }
    if let Some(rest) = t.strip_prefix("Part ") {
        return match rest.split_once(" · ") {
            Some((i, fmt)) => trf("Part {i} · {fmt}", &[("i", i.to_string()), ("fmt", fmt.to_string())]),
            None => trf("Part {i}", &[("i", rest.to_string())]),
        };
    }
    if let Some(fmt) = t.strip_suffix(" audio") {
        return trf("{fmt} audio", &[("fmt", fmt.to_string())]);
    }
    tr(t)
}

/// Grab a frame with FFmpeg and decode it with WIC, off the UI thread.
pub fn spawn_thumb(hwnd: HWND, kind: ThumbKind, gen: u64, path: PathBuf, t: f64, w: u32, h: u32) {
    let hw = hwnd.0 as usize;
    std::thread::spawn(move || {
        use windows::Win32::System::Com::{CoInitializeEx, CoUninitialize, COINIT_MULTITHREADED};
        let img = splitter::thumbnail(&path, t, w, h).ok().and_then(|bytes| unsafe {
            let _ = CoInitializeEx(None, COINIT_MULTITHREADED);
            let img = crate::image::decode_png(&bytes).ok();
            CoUninitialize();
            img
        });
        THUMBS.lock().unwrap().push((kind, gen, img));
        unsafe {
            let _ = PostMessageW(Some(HWND(hw as _)), WM_APP_THUMB, WPARAM(0), LPARAM(0));
        }
    });
}

pub fn open_folder(dir: &Path) {
    unsafe {
        windows::Win32::UI::Shell::ShellExecuteW(
            None,
            windows::core::w!("open"),
            &HSTRING::from(dir.as_os_str()),
            PCWSTR::null(),
            PCWSTR::null(),
            SW_SHOWNORMAL,
        );
    }
}
