//! Chop Chop Splitter for Windows — a native Win32 front end built with windows-rs.
//!
//! It shares the FFmpeg engine (`splitter.rs`) and the translations (`i18n.rs`) with the
//! GTK build and keeps its layout — the video and preview on the left; export settings,
//! the main button and progress on the right — using standard Windows controls: a menu
//! bar, group boxes, a tab control, edit + up-down spinners, a progress bar and a list view.
//! Only the video preview and the start/end range slider are drawn by hand (Windows has
//! no two-handle slider); the slider uses the system's trackbar theme parts.

#![windows_subsystem = "windows"]

#[path = "../../src/i18n.rs"]
#[allow(dead_code)]
mod i18n;
#[path = "../../src/splitter.rs"]
#[allow(dead_code)]
mod splitter;

mod app;
mod dialogs;
mod image;

use std::cell::RefCell;
use std::path::PathBuf;

use windows::core::{w, BOOL, HSTRING, PCWSTR, PWSTR};
use windows::Win32::Foundation::*;
use windows::Win32::Globalization::GetUserDefaultLocaleName;
use windows::Win32::Graphics::Gdi::*;
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::SystemServices::{SS_ENDELLIPSIS, SS_LEFT, SS_NOPREFIX, SS_NOTIFY, SS_PATHELLIPSIS, SS_RIGHT};
use windows::Win32::UI::Controls::*;
use windows::Win32::UI::HiDpi::{GetDpiForSystem, GetDpiForWindow, SystemParametersInfoForDpi};
use windows::Win32::UI::Input::KeyboardAndMouse::*;
use windows::Win32::UI::Shell::{DragAcceptFiles, DragFinish, DragQueryFileW, SHGetKnownFolderPath, FOLDERID_Videos, HDROP, KF_FLAG_DEFAULT};
use windows::Win32::UI::WindowsAndMessaging::*;

use app::*;
use i18n::{tr, trf, Lang};
use splitter::AudioFormat;

thread_local! {
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
    static TIPBUF: RefCell<Vec<u16>> = const { RefCell::new(Vec::new()) };
    static STARTUP_FILE: RefCell<Option<PathBuf>> = const { RefCell::new(None) };
}

/// Run `f` with the app, unless it is already borrowed further up the stack. That happens
/// when our own calls (e.g. SetWindowText on an edit) send notifications back synchronously;
/// those programmatic changes are deliberately ignored, like GTK's `updating_entries` flag.
fn with_app<R>(f: impl FnOnce(&mut App) -> R) -> Option<R> {
    APP.with(|a| {
        let mut b = a.try_borrow_mut().ok()?;
        b.as_mut().map(f)
    })
}

// Control and menu ids.
const ID_THUMB: usize = 200;
const ID_DROP_TITLE: usize = 201;
const ID_PREVIEW: usize = 202;
const ID_TAB: usize = 212;
const ID_TIMELINE: usize = 213;
const ID_EVERY: usize = 220;
const ID_PARTS: usize = 221;
const ID_AUDIO: usize = 230;
const ID_OUT_BTN: usize = 231;
const ID_OPEN_CHECK: usize = 232;
const ID_GO: usize = 240;
const ID_LIST: usize = 250;
const ID_OPEN: usize = 260;
const ID_SHOW_DIR: usize = 261;
const ID_LANG: usize = 262;
const IDM_LANG: usize = 1100;

fn hinstance() -> HINSTANCE {
    unsafe { GetModuleHandleW(None).map(|m| HINSTANCE(m.0)).unwrap_or_default() }
}

fn system_locale() -> Option<String> {
    let mut buf = [0u16; 85];
    let n = unsafe { GetUserDefaultLocaleName(&mut buf) };
    (n > 1).then(|| String::from_utf16_lossy(&buf[..(n - 1) as usize]))
}

fn videos_dir() -> PathBuf {
    unsafe {
        if let Ok(p) = SHGetKnownFolderPath(&FOLDERID_Videos, KF_FLAG_DEFAULT, None) {
            let s = p.to_string().ok();
            windows::Win32::System::Com::CoTaskMemFree(Some(p.0 as _));
            if let Some(s) = s {
                return PathBuf::from(s);
            }
        }
    }
    std::env::var_os("USERPROFILE").map(PathBuf::from).unwrap_or_else(|| PathBuf::from("."))
}

fn lparam_xy(lp: LPARAM) -> (i32, i32) {
    ((lp.0 & 0xffff) as i16 as i32, ((lp.0 >> 16) & 0xffff) as i16 as i32)
}

fn main() {
    unsafe {
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let icc = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_STANDARD_CLASSES | ICC_WIN95_CLASSES,
        };
        let _ = InitCommonControlsEx(&icc);
    }
    i18n::init_with_locale(system_locale());
    let hinst = hinstance();
    unsafe {
        let icon = LoadImageW(Some(hinst), PCWSTR(1 as _), IMAGE_ICON, 0, 0, LR_DEFAULTSIZE | LR_SHARED)
            .map(|h| HICON(h.0))
            .unwrap_or_default();
        let arrow = LoadCursorW(None, IDC_ARROW).unwrap_or_default();
        let procs: [(PCWSTR, WNDPROC, HBRUSH); 3] = [
            (w!("ChopChopSplitter"), Some(wndproc as unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT), HBRUSH((COLOR_WINDOW.0 + 1) as _)),
            (w!("ChopChopPreview"), Some(preview_proc as unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT), HBRUSH::default()),
            (w!("ChopChopTimeline"), Some(timeline_proc as unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT), HBRUSH::default()),
        ];
        for (name, proc_, bg) in procs {
            let wc = WNDCLASSEXW {
                cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
                style: CS_HREDRAW | CS_VREDRAW,
                lpfnWndProc: proc_,
                hInstance: hinst,
                hIcon: icon,
                hCursor: arrow,
                hbrBackground: bg,
                lpszClassName: name,
                ..Default::default()
            };
            RegisterClassExW(&wc);
        }

        let s = GetDpiForSystem() as f32 / 96.0;
        let Ok(hwnd) = CreateWindowExW(
            WINDOW_EX_STYLE(0),
            w!("ChopChopSplitter"),
            w!("Chop Chop Splitter"),
            WS_OVERLAPPEDWINDOW,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            (1240.0 * s) as i32,
            (900.0 * s) as i32,
            None,
            None,
            Some(hinst),
            None,
        ) else {
            return;
        };
        create_app(hwnd);
        let _ = ShowWindow(hwnd, SW_SHOWDEFAULT);
        let _ = UpdateWindow(hwnd);
        STARTUP_FILE.with(|f| *f.borrow_mut() = std::env::args_os().nth(1).map(PathBuf::from));
        let _ = PostMessageW(Some(hwnd), WM_APP_STARTUP, WPARAM(0), LPARAM(0));

        let mut msg = MSG::default();
        while GetMessageW(&mut msg, None, 0, 0).as_bool() {
            if pre_translate(&msg) || IsDialogMessageW(hwnd, &msg).as_bool() {
                continue;
            }
            let _ = TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn edit_index(hwnd: HWND) -> Option<usize> {
    let id = unsafe { GetDlgCtrlID(hwnd) } as usize;
    (EDIT_ID_BASE..EDIT_ID_BASE + N_EDITS).contains(&id).then(|| id - EDIT_ID_BASE)
}

/// Ctrl+O anywhere; Enter in an entry applies it (instead of pressing the default button).
fn pre_translate(msg: &MSG) -> bool {
    if msg.message != WM_KEYDOWN {
        return false;
    }
    let ctrl = unsafe { GetKeyState(VK_CONTROL.0 as i32) } < 0;
    let key = msg.wParam.0 as u16;
    if ctrl && key == b'O' as u16 {
        pick_and_open();
        return true;
    }
    if let Some(idx) = edit_index(msg.hwnd) {
        if key == VK_RETURN.0 {
            with_app(|a| entry_commit(a, idx));
            return true;
        }
        if ctrl && key == b'A' as u16 {
            unsafe {
                SendMessageW(msg.hwnd, EM_SETSEL, Some(WPARAM(0)), Some(LPARAM(-1)));
            }
            return true;
        }
    }
    false
}

fn entry_commit(a: &mut App, idx: usize) {
    match idx {
        E_START | E_END => a.apply_entry(idx),
        E_MIN | E_SEC | E_PARTS | E_WORKERS => a.spin_commit(idx),
        _ => {}
    }
}

// ───────────────────────── controls ─────────────────────────

fn ctl(parent: HWND, class: PCWSTR, text: &str, style: u32, ex: WINDOW_EX_STYLE, id: usize) -> HWND {
    unsafe {
        CreateWindowExW(
            ex,
            class,
            &HSTRING::from(text),
            WS_CHILD | WS_VISIBLE | WS_CLIPSIBLINGS | WINDOW_STYLE(style),
            0,
            0,
            10,
            10,
            Some(parent),
            Some(HMENU(id as _)),
            Some(hinstance()),
            None,
        )
        .unwrap_or_default()
    }
}

const NONE: WINDOW_EX_STYLE = WINDOW_EX_STYLE(0);

fn group(p: HWND) -> HWND {
    ctl(p, w!("BUTTON"), "", BS_GROUPBOX as u32, NONE, 0)
}
fn label(p: HWND, style: u32) -> HWND {
    ctl(p, w!("STATIC"), "", SS_LEFT.0 | SS_NOPREFIX.0 | style, NONE, 0)
}
fn button(p: HWND, id: usize, style: i32) -> HWND {
    ctl(p, w!("BUTTON"), "", WS_TABSTOP.0 | style as u32, NONE, id)
}

/// How much larger than the system message font the UI text is.
const FONT_SCALE: f32 = 1.25;

fn make_fonts(dpi: u32) -> (HFONT, HFONT) {
    unsafe {
        let mut ncm = NONCLIENTMETRICSW { cbSize: std::mem::size_of::<NONCLIENTMETRICSW>() as u32, ..Default::default() };
        let _ = SystemParametersInfoForDpi(SPI_GETNONCLIENTMETRICS.0, ncm.cbSize, Some(&mut ncm as *mut _ as _), 0, dpi);
        // The system message font (Segoe UI 9 pt), enlarged for readability.
        let mut lf = ncm.lfMessageFont;
        lf.lfHeight = (lf.lfHeight as f32 * FONT_SCALE).round() as i32;
        let font = CreateFontIndirectW(&lf);
        lf.lfWeight = 600;
        lf.lfHeight = (lf.lfHeight as f32 * 1.3).round() as i32;
        (font, CreateFontIndirectW(&lf))
    }
}

fn create_app(hwnd: HWND) {
    let dpi = unsafe { GetDpiForWindow(hwnd) };
    let (font, bold_font) = make_fonts(dpi);
    let mut u = Ui { hwnd, ..Default::default() };

    // Group boxes first, so they sit below their contents in the z-order.
    u.video_grp = group(hwnd);
    u.preview_grp = group(hwnd);
    u.export_grp = group(hwnd);
    u.progress_grp = group(hwnd);

    // ── Language flag (top right) ──
    u.lang_btn = button(hwnd, ID_LANG, BS_PUSHBUTTON | BS_ICON);

    // ── Video ──
    u.open_btn = button(hwnd, ID_OPEN, BS_PUSHBUTTON);
    u.thumb = ctl(hwnd, w!("ChopChopPreview"), "", 0, NONE, ID_THUMB);
    u.drop_title = ctl(hwnd, w!("STATIC"), "", SS_LEFT.0 | SS_NOPREFIX.0 | SS_ENDELLIPSIS.0 | SS_NOTIFY.0, NONE, ID_DROP_TITLE);
    u.drop_sub = label(hwnd, SS_PATHELLIPSIS.0);
    u.drop_info = label(hwnd, SS_ENDELLIPSIS.0);

    // ── Preview ──
    u.preview = ctl(hwnd, w!("ChopChopPreview"), "", 0, NONE, ID_PREVIEW);
    u.tab = ctl(hwnd, WC_TABCONTROLW, "", WS_TABSTOP.0 | WS_GROUP.0, NONE, ID_TAB);
    for i in 0..2 {
        let mut t: Vec<u16> = vec![0];
        let item = TCITEMW { mask: TCIF_TEXT, pszText: PWSTR(t.as_mut_ptr()), ..Default::default() };
        unsafe {
            SendMessageW(u.tab, TCM_INSERTITEMW, Some(WPARAM(i)), Some(LPARAM(&item as *const _ as isize)));
        }
    }
    // Custom selection page
    u.timeline = ctl(hwnd, w!("ChopChopTimeline"), "", WS_TABSTOP.0, NONE, ID_TIMELINE);
    u.start_lbl = label(hwnd, 0);
    u.end_lbl = label(hwnd, 0);
    // Batch split page
    u.batch_head = label(hwnd, 0);
    u.every_radio = button(hwnd, ID_EVERY, BS_AUTORADIOBUTTON | WS_GROUP.0 as i32);
    u.parts_radio = button(hwnd, ID_PARTS, BS_AUTORADIOBUTTON);
    u.min_lbl = label(hwnd, 0);
    u.sec_lbl = label(hwnd, 0);
    u.parts_lbl = label(hwnd, 0);

    // ── Export ──
    u.workers_lbl = label(hwnd, 0);
    u.cores_lbl = label(hwnd, 0);
    u.out_lbl = label(hwnd, 0);
    u.out_edit = ctl(hwnd, w!("EDIT"), "", (ES_AUTOHSCROLL | ES_READONLY) as u32 | WS_TABSTOP.0, WS_EX_CLIENTEDGE, 0);
    u.out_btn = button(hwnd, ID_OUT_BTN, BS_PUSHBUTTON);
    u.name_lbl = label(hwnd, 0);
    u.ext_lbl = label(hwnd, 0);
    u.open_check = button(hwnd, ID_OPEN_CHECK, BS_AUTOCHECKBOX);
    u.audio_btn = button(hwnd, ID_AUDIO, BS_PUSHBUTTON);
    u.dir_btn = button(hwnd, ID_SHOW_DIR, BS_PUSHBUTTON);

    // Entries (created in tab order) and their up-down spinners.
    for i in [E_START, E_END, E_MIN, E_SEC, E_PARTS, E_WORKERS, E_NAME] {
        let mut style = ES_AUTOHSCROLL as u32 | WS_TABSTOP.0;
        if matches!(i, E_START | E_END) {
            style |= ES_CENTER as u32;
        }
        if SPINS.contains(&i) {
            style |= ES_NUMBER as u32;
        }
        u.edits[i] = ctl(hwnd, w!("EDIT"), "", style, WS_EX_CLIENTEDGE, EDIT_ID_BASE + i);
        if SPINS.contains(&i) {
            u.spins[i] = ctl(
                hwnd,
                UPDOWN_CLASSW,
                "",
                UDS_SETBUDDYINT | UDS_ALIGNRIGHT | UDS_ARROWKEYS | UDS_NOTHOUSANDS | UDS_HOTTRACK,
                NONE,
                0,
            );
            unsafe {
                SendMessageW(u.spins[i], UDM_SETBUDDY, Some(WPARAM(u.edits[i].0 as usize)), None);
            }
        }
    }
    u.go_btn = button(hwnd, ID_GO, BS_DEFPUSHBUTTON);

    // ── Progress ──
    u.bar = ctl(hwnd, PROGRESS_CLASSW, "", PBS_SMOOTH, NONE, 0);
    u.percent = label(hwnd, SS_RIGHT.0);
    u.list = ctl(
        hwnd,
        WC_LISTVIEWW,
        "",
        LVS_REPORT | LVS_SINGLESEL | LVS_NOSORTHEADER | LVS_SHOWSELALWAYS | WS_TABSTOP.0,
        WS_EX_CLIENTEDGE,
        ID_LIST,
    );
    u.status = label(hwnd, 0);
    unsafe {
        SendMessageW(u.bar, PBM_SETRANGE32, Some(WPARAM(0)), Some(LPARAM(1000)));
        let ex = LVS_EX_FULLROWSELECT | LVS_EX_DOUBLEBUFFER;
        SendMessageW(u.list, LVM_SETEXTENDEDLISTVIEWSTYLE, Some(WPARAM(ex as usize)), Some(LPARAM(ex as isize)));
        let _ = SetWindowTheme(u.list, w!("Explorer"), PCWSTR::null());
        for c in 0..4 {
            let mut t: Vec<u16> = vec![0];
            let col = LVCOLUMNW { mask: LVCF_TEXT | LVCF_WIDTH, cx: 80, pszText: PWSTR(t.as_mut_ptr()), ..Default::default() };
            SendMessageW(u.list, LVM_INSERTCOLUMNW, Some(WPARAM(c)), Some(LPARAM(&col as *const _ as isize)));
        }
        DragAcceptFiles(hwnd, true);
    }
    // Z-order = Tab order: inputs top to bottom, then the tab control, then the group boxes
    // (containers must sit *below* the controls drawn on them).
    unsafe {
        let order = [
            u.lang_btn, u.open_btn, u.timeline, u.edits[E_START], u.edits[E_END], u.every_radio,
            u.edits[E_MIN], u.edits[E_SEC], u.parts_radio, u.edits[E_PARTS], u.edits[E_WORKERS], u.out_edit, u.out_btn,
            u.edits[E_NAME], u.open_check, u.audio_btn, u.dir_btn, u.go_btn, u.list,
        ];
        let mut after = HWND_TOP;
        for h in order {
            let _ = SetWindowPos(h, Some(after), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
            after = h;
        }
        for h in [u.tab, u.video_grp, u.preview_grp, u.export_grp, u.progress_grp] {
            let _ = SetWindowPos(h, Some(HWND_BOTTOM), 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE);
        }
    }
    u.tooltip = create_tooltip(hwnd, &[u.lang_btn, u.dir_btn, u.timeline, u.audio_btn, u.out_edit, u.thumb]);

    // Fonts for every child.
    unsafe {
        let _ = EnumChildWindows(Some(hwnd), Some(set_font_proc), LPARAM(font.0 as isize));
        SendMessageW(u.drop_title, WM_SETFONT, Some(WPARAM(bold_font.0 as usize)), Some(LPARAM(1)));
    }

    let cores = cores();
    let mut app = App {
        ui: u,
        font,
        bold_font,
        scale: dpi as f32 / 96.0,
        info: None,
        mode: Mode::Custom,
        split_every: true,
        start: 0.0,
        end: 0.0,
        focus: Handle::Start,
        drag: None,
        hot: None,
        batch: Vec::new(),
        preview_gen: 0,
        name_auto: true,
        running: false,
        cancelling: false,
        cancel: None,
        rows: Vec::new(),
        started: None,
        out_dir_user_set: false,
        out_dir: None,
        last_audio: AudioFormat::Mp3,
        spin_vals: [0.0; N_EDITS],
        edit_error: [false; N_EDITS],
        badge: String::new(),
        preview_img: None,
        card_img: None,
        rx: None,
        status: String::new(),
    };
    for (i, v) in [(E_MIN, 5.0), (E_SEC, 0.0), (E_PARTS, 4.0), (E_WORKERS, cores as f64)] {
        let (lo, hi) = app.spin_range(i);
        unsafe {
            SendMessageW(app.ui.spins[i], UDM_SETRANGE32, Some(WPARAM(lo as usize)), Some(LPARAM(hi as isize)));
        }
        app.set_spin(i, v);
    }
    for i in [E_START, E_END] {
        set_text(app.ui.edits[i], "00:00:00.000");
        enable(app.ui.edits[i], false);
    }
    unsafe {
        let cue = HSTRING::from("clip");
        SendMessageW(app.ui.edits[E_NAME], EM_SETCUEBANNER, Some(WPARAM(1)), Some(LPARAM(cue.as_ptr() as isize)));
    }
    set_check(app.ui.open_check, true);
    set_check(app.ui.every_radio, true);
    enable(app.ui.audio_btn, false);
    app.set_out_dir(&videos_dir());
    set_flag(&app);
    apply_texts(&mut app);
    APP.with(|a| *a.borrow_mut() = Some(app));
    with_app(|a| a.mode_changed());
}

extern "system" fn set_font_proc(h: HWND, lp: LPARAM) -> BOOL {
    unsafe {
        SendMessageW(h, WM_SETFONT, Some(WPARAM(lp.0 as usize)), Some(LPARAM(1)));
    }
    TRUE
}

/// (Re)apply every translated text, then lay the window out again (label widths change).
fn apply_texts(a: &mut App) {
    let u = &a.ui;
    set_text(u.video_grp, &tr("Video"));
    set_text(u.preview_grp, &tr("Preview"));
    set_text(u.export_grp, &tr("Export"));
    set_text(u.progress_grp, &tr("Progress"));
    if a.info.is_none() {
        set_text(u.drop_title, &tr("Drop a video here…"));
        set_text(u.drop_sub, &tr("Open a video (Ctrl+O)"));
    }
    set_text(u.start_lbl, &format!("{}:", tr("Start time")));
    set_text(u.end_lbl, &format!("{}:", tr("End time")));
    set_text(u.batch_head, &format!("{}:", tr("Split the whole video")));
    set_text(u.every_radio, &tr("Every…"));
    set_text(u.parts_radio, &tr("Equal parts"));
    set_text(u.min_lbl, &tr("min"));
    set_text(u.sec_lbl, &tr("sec"));
    set_text(u.parts_lbl, &tr("equal parts"));
    set_text(u.workers_lbl, &format!("{}:", tr("Parallel workers")));
    set_text(u.cores_lbl, &trf("{n} cores detected", &[("n", cores().to_string())]));
    set_text(u.out_lbl, &format!("{}:", tr("Output folder")));
    set_text(u.out_btn, &tr("Change…"));
    set_text(u.name_lbl, &format!("{}:", tr("File name")));
    set_text(u.open_check, &tr("Open the output folder when finished"));
    set_text(u.audio_btn, &tr("Export audio…"));
    set_text(u.open_btn, &format!("{}…", tr("Open")));
    set_text(u.dir_btn, &tr("Show output folder"));
    for (i, key) in ["Custom selection", "Batch split"].iter().enumerate() {
        let mut t: Vec<u16> = tr(key).encode_utf16().chain(std::iter::once(0)).collect();
        let item = TCITEMW { mask: TCIF_TEXT, pszText: PWSTR(t.as_mut_ptr()), ..Default::default() };
        unsafe {
            SendMessageW(u.tab, TCM_SETITEMW, Some(WPARAM(i)), Some(LPARAM(&item as *const _ as isize)));
        }
    }
    for (i, key) in ["Job", "Range", "Progress", "Status"].iter().enumerate() {
        let mut t: Vec<u16> = tr(key).encode_utf16().chain(std::iter::once(0)).collect();
        let col = LVCOLUMNW { mask: LVCF_TEXT, pszText: PWSTR(t.as_mut_ptr()), ..Default::default() };
        unsafe {
            SendMessageW(u.list, LVM_SETCOLUMNW, Some(WPARAM(i)), Some(LPARAM(&col as *const _ as isize)));
        }
    }
    a.refresh_go();
    layout(a);
}

// ───────────────────────── layout ─────────────────────────

fn text_width(a: &App, s: &str) -> i32 {
    unsafe {
        let dc = GetDC(Some(a.ui.hwnd));
        let old = SelectObject(dc, HGDIOBJ(a.font.0));
        let w: Vec<u16> = s.encode_utf16().collect();
        let mut sz = SIZE::default();
        let _ = GetTextExtentPoint32W(dc, &w, &mut sz);
        SelectObject(dc, old);
        ReleaseDC(Some(a.ui.hwnd), dc);
        sz.cx
    }
}

fn place(h: HWND, x: f32, y: f32, w: f32, hh: f32, s: f32) {
    unsafe {
        let _ = MoveWindow(h, (x * s).round() as i32, (y * s).round() as i32, (w * s).round().max(1.0) as i32, (hh * s).round().max(1.0) as i32, true);
    }
}

fn show(h: HWND, on: bool) {
    unsafe {
        let _ = ShowWindow(h, if on { SW_SHOWNA } else { SW_HIDE });
    }
}

/// Lay out every control for the current client size, mode and language (sizes in DIPs).
pub fn layout(a: &mut App) {
    let s = a.scale;
    let u = &a.ui;
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(u.hwnd, &mut rc);
    }
    let (cw, ch) = (rc.right as f32 / s, rc.bottom as f32 / s);
    let tw = |t: &str| text_width(a, t) as f32 / s;
    let txt = |h: HWND| text_of(h);
    const M: f32 = 12.0; // window margin
    const G: f32 = 10.0; // gap between groups
    const EH: f32 = 29.0; // edit / radio row height
    const BH: f32 = 32.0; // button height
    const LH: f32 = 22.0; // label height
    const LO: f32 = (EH - LH) / 2.0; // label offset that centres it on an edit row
    const CAP: f32 = 30.0; // group box caption → first row
    const PAD: f32 = 12.0; // group box inner padding
    const WIDE_PAD: f32 = 20.0; // inner side padding of the Video and Export groups
    let avail = cw - 2.0 * M;
    let right_w = (avail * 0.4).clamp(420.0, 560.0);
    let left_w = avail - G - right_w;
    let (lx, rx) = (M, M + left_w + G);

    // ── Language flag, top right ──
    place(u.lang_btn, cw - M - 48.0, 8.0, 48.0, 36.0, s);
    let top = 8.0 + 36.0 + 4.0;

    // ── Video ──
    let vh = CAP + 88.0 + 14.0;
    place(u.video_grp, lx, top, left_w, vh, s);
    let vx = lx + WIDE_PAD;
    let vw = left_w - 2.0 * WIDE_PAD;
    let ty0 = top + CAP;
    place(u.thumb, vx, ty0 + 4.0, 142.0, 80.0, s);
    let ow = (tw(&txt(u.open_btn)) + 36.0).max(100.0);
    place(u.open_btn, vx + vw - ow, ty0, ow, BH, s);
    let tx = vx + 142.0 + 16.0;
    let full = vx + vw - tx;
    place(u.drop_title, tx, ty0, full - ow - 12.0, 32.0, s);
    place(u.drop_sub, tx, ty0 + 38.0, full, LH, s);
    place(u.drop_info, tx, ty0 + 64.0, full, LH, s);

    // ── Preview ──
    let py = top + vh + G;
    let ph = ch - M - py;
    place(u.preview_grp, lx, py, left_w, ph, s);
    let ix = lx + PAD;
    let iw = left_w - 2.0 * PAD;
    let itop = py + CAP;
    let ibot = py + ph - PAD;
    let tab_h = 214.0;
    // The preview shows the frame at the handle that was moved last.
    let frame_h = (iw * 9.0 / 16.0).min(ibot - itop - G - tab_h).max(120.0);
    place(u.preview, ix, itop, iw, frame_h, s);
    let custom = a.mode == Mode::Custom;
    let ty = itop + frame_h + G;
    let th = (ibot - ty).max(170.0);
    place(u.tab, ix, ty, iw, th, s);
    // The tab's page area.
    let mut page = RECT {
        left: (ix * s) as i32,
        top: (ty * s) as i32,
        right: ((ix + iw) * s) as i32,
        bottom: ((ty + th) * s) as i32,
    };
    unsafe {
        SendMessageW(u.tab, TCM_SETCURSEL, Some(WPARAM(if custom { 0 } else { 1 })), None);
        SendMessageW(u.tab, TCM_ADJUSTRECT, Some(WPARAM(0)), Some(LPARAM(&mut page as *mut _ as isize)));
    }
    let (px, pyy, pw) = (page.left as f32 / s + 10.0, page.top as f32 / s + 8.0, (page.right - page.left) as f32 / s - 20.0);
    // custom page
    place(u.timeline, px, pyy, pw, 70.0, s);
    let ly = pyy + 70.0 + 8.0;
    let time_w: f32 = 170.0;
    let tcol = time_w.max(tw(&txt(u.start_lbl)) + 4.0).max(tw(&txt(u.end_lbl)) + 4.0);
    place(u.start_lbl, px, ly, tcol, LH, s);
    place(u.edits[E_START], px, ly + LH + 4.0, time_w, EH, s);
    place(u.end_lbl, px + tcol + 20.0, ly, tcol, LH, s);
    place(u.edits[E_END], px + tcol + 20.0, ly + LH + 4.0, time_w, EH, s);
    for h in [u.timeline, u.start_lbl, u.end_lbl, u.edits[E_START], u.edits[E_END]] {
        show(h, custom);
    }
    // batch page
    place(u.batch_head, px, pyy + 4.0, pw, LH, s);
    let rw = (tw(&txt(u.every_radio)).max(tw(&txt(u.parts_radio))) + 32.0).max(100.0);
    let r1 = pyy + LH + 18.0;
    let r2 = r1 + EH + 14.0;
    place(u.every_radio, px, r1, rw, EH, s);
    place(u.parts_radio, px, r2, rw, EH, s);
    let sx = px + rw + 10.0;
    let spin_w = 92.0;
    place(u.edits[E_MIN], sx, r1, spin_w, EH, s);
    let mw = tw(&txt(u.min_lbl)) + 4.0;
    place(u.min_lbl, sx + spin_w + 8.0, r1 + LO, mw, LH, s);
    let sx2 = sx + spin_w + 8.0 + mw + 14.0;
    place(u.edits[E_SEC], sx2, r1, spin_w, EH, s);
    place(u.sec_lbl, sx2 + spin_w + 8.0, r1 + LO, tw(&txt(u.sec_lbl)) + 4.0, LH, s);
    place(u.edits[E_PARTS], sx, r2, spin_w, EH, s);
    place(u.parts_lbl, sx + spin_w + 8.0, r2 + LO, tw(&txt(u.parts_lbl)) + 4.0, LH, s);
    for h in [u.batch_head, u.every_radio, u.parts_radio, u.min_lbl, u.sec_lbl, u.parts_lbl, u.edits[E_MIN], u.edits[E_SEC], u.edits[E_PARTS]] {
        show(h, !custom);
    }
    for i in [E_MIN, E_SEC, E_PARTS] {
        show(u.spins[i], !custom);
    }

    // ── Export ──
    let ey = top;
    let gx = rx + WIDE_PAD;
    let gw = right_w - 2.0 * WIDE_PAD;
    let lw = [u.workers_lbl, u.out_lbl, u.name_lbl].iter().map(|h| tw(&txt(*h))).fold(0.0, f32::max) + 12.0;
    let fx = gx + lw;
    let fw = gw - lw;
    let mut y = ey + CAP;
    place(u.workers_lbl, gx, y + LO, lw, LH, s);
    place(u.edits[E_WORKERS], fx, y, spin_w, EH, s);
    place(u.cores_lbl, fx + spin_w + 12.0, y + LO, fw - spin_w - 12.0, LH, s);
    y += EH + 12.0;
    let cbw = tw(&txt(u.out_btn)) + 28.0;
    place(u.out_lbl, gx, y + LO, lw, LH, s);
    place(u.out_edit, fx, y, fw - cbw - 8.0, EH, s);
    place(u.out_btn, fx + fw - cbw, y - 1.0, cbw, EH + 2.0, s);
    y += EH + 12.0;
    let xw = tw(&txt(u.ext_lbl)) + 6.0;
    place(u.name_lbl, gx, y + LO, lw, LH, s);
    place(u.edits[E_NAME], fx, y, fw - xw - 6.0, EH, s);
    place(u.ext_lbl, fx + fw - xw, y + LO, xw, LH, s);
    y += EH + 12.0;
    place(u.open_check, fx, y, fw, 26.0, s);
    y += 26.0 + 14.0;
    let aw = (tw(&txt(u.audio_btn)) + 36.0).max(130.0);
    place(u.audio_btn, fx, y, aw, BH, s);
    let dw = tw(&txt(u.dir_btn)) + 36.0;
    place(u.dir_btn, gx + gw - dw, y, dw, BH, s);
    y += BH + 16.0;
    place(u.export_grp, rx, ey, right_w, y - ey, s);
    // Up-down buddies follow their edits.
    for i in SPINS {
        unsafe {
            SendMessageW(u.spins[i], UDM_SETBUDDY, Some(WPARAM(u.edits[i].0 as usize)), None);
        }
    }
    show(u.spins[E_WORKERS], true);

    // ── Main button ──
    let gy = y + G;
    let go_h = 44.0;
    place(u.go_btn, rx, gy, right_w, go_h, s);

    // ── Progress ──
    let pgy = gy + go_h + G;
    let pgh = ch - M - pgy;
    place(u.progress_grp, rx, pgy, right_w, pgh, s);
    let qx = rx + PAD;
    let qw = right_w - 2.0 * PAD;
    let pct_w = 60.0;
    place(u.bar, qx, pgy + CAP + 2.0, qw - pct_w - 8.0, 18.0, s);
    place(u.percent, qx + qw - pct_w, pgy + CAP, pct_w, LH, s);
    let st_h = 2.0 * LH + 4.0;
    let st_y = pgy + pgh - PAD - st_h;
    place(u.status, qx, st_y, qw, st_h, s);
    let ly = pgy + CAP + 2.0 + 18.0 + 12.0;
    place(u.list, qx, ly, qw, (st_y - 8.0 - ly).max(40.0), s);
    let lwp = ((qw - 4.0) * s) as i32 - unsafe { GetSystemMetrics(SM_CXVSCROLL) };
    for (c, frac) in [(0, 0.17), (1, 0.48), (2, 0.17), (3, 0.18)] {
        unsafe {
            SendMessageW(u.list, LVM_SETCOLUMNWIDTH, Some(WPARAM(c)), Some(LPARAM((lwp as f32 * frac) as isize)));
        }
    }
    unsafe {
        let _ = InvalidateRect(Some(a.ui.hwnd), None, true);
    }
}

// ───────────────────────── tooltips ─────────────────────────

fn create_tooltip(hwnd: HWND, tools: &[HWND]) -> HWND {
    unsafe {
        let tt = CreateWindowExW(
            WS_EX_TOPMOST,
            TOOLTIPS_CLASSW,
            PCWSTR::null(),
            WS_POPUP | WINDOW_STYLE(TTS_ALWAYSTIP | TTS_NOPREFIX),
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            Some(hwnd),
            None,
            Some(hinstance()),
            None,
        )
        .unwrap_or_default();
        for t in tools {
            let ti = TTTOOLINFOW {
                cbSize: std::mem::size_of::<TTTOOLINFOW>() as u32,
                uFlags: TTF_IDISHWND | TTF_SUBCLASS,
                hwnd,
                uId: t.0 as usize,
                hinst: hinstance(),
                lpszText: PWSTR(usize::MAX as *mut u16), // LPSTR_TEXTCALLBACKW
                ..Default::default()
            };
            SendMessageW(tt, TTM_ADDTOOLW, Some(WPARAM(0)), Some(LPARAM(&ti as *const _ as isize)));
        }
        SendMessageW(tt, TTM_SETMAXTIPWIDTH, Some(WPARAM(0)), Some(LPARAM(420)));
        tt
    }
}

fn tooltip_text(tool: usize) -> String {
    with_app(|a| {
        let u = &a.ui;
        let h = HWND(tool as _);
        if h == u.lang_btn {
            tr("Language")
        } else if h == u.dir_btn {
            a.out_dir.as_ref().map(|d| d.display().to_string()).unwrap_or_default()
        } else if h == u.timeline {
            tr("Drag the handles to choose the start and end of the clip")
        } else if h == u.audio_btn {
            a.audio_tooltip()
        } else if h == u.out_edit {
            a.out_dir.as_ref().map(|d| d.display().to_string()).unwrap_or_default()
        } else if h == u.thumb {
            tr("Open a video (Ctrl+O)")
        } else {
            String::new()
        }
    })
    .unwrap_or_default()
}

// ───────────────────────── flows that open dialogs ─────────────────────────
// These run with the app *not* borrowed, because modal dialogs pump messages.

fn show_error(title: &str, body: &str) {
    if let Some(h) = with_app(|a| a.ui.hwnd) {
        dialogs::error(h, title, body);
    }
}

fn load_file(path: PathBuf) {
    if let Some(Err((t, b))) = with_app(|a| a.load_file(&path)) {
        show_error(&t, &b);
    }
}

fn pick_and_open() {
    let Some(h) = with_app(|a| (!a.running).then_some(a.ui.hwnd)).flatten() else { return };
    let filters = [
        dialogs::Filter {
            name: tr("Videos"),
            spec: "*.mp4;*.mkv;*.mov;*.webm;*.avi;*.mpg;*.mpeg;*.m4v;*.ogv;*.flv;*.3gp;*.wmv;*.ts;*.mts;*.m2ts".into(),
        },
        dialogs::Filter { name: tr("All files"), spec: "*.*".into() },
    ];
    if let Some(p) = dialogs::open_file(h, &tr("Open a video"), &filters) {
        load_file(p);
    }
}

fn pick_out_dir() {
    let Some((h, cur)) = with_app(|a| (a.ui.hwnd, a.out_dir.clone())) else { return };
    if let Some(d) = dialogs::pick_folder(h, &tr("Choose the output folder"), cur.as_deref()) {
        with_app(|a| {
            a.set_out_dir(&d);
            a.out_dir_user_set = true;
        });
    }
}

/// Ask before replacing any of `files` that already exist.
fn confirm_overwrite(owner: HWND, files: &[PathBuf]) -> bool {
    let existing: Vec<String> = files
        .iter()
        .filter(|p| p.exists())
        .map(|p| format!("• {}", p.file_name().unwrap_or_default().to_string_lossy()))
        .collect();
    if existing.is_empty() {
        return true;
    }
    let mut shown = existing.iter().take(8).cloned().collect::<Vec<_>>().join("\n");
    if existing.len() > 8 {
        shown.push_str(&format!("\n{}", trf("…and {n} more", &[("n", (existing.len() - 8).to_string())])));
    }
    dialogs::confirm(
        owner,
        &tr(if existing.len() == 1 { "Replace existing file?" } else { "Replace existing files?" }),
        &format!("{}\n{shown}", tr("Already in this folder:")),
    )
}

/// Start an export. The overwrite question needs a modal dialog, so the job is planned
/// twice: a dry run collects the files it would write, then (after asking) it launches.
fn start_export(audio: Option<(AudioFormat, PathBuf)>) {
    let Some(h) = with_app(|a| a.ui.hwnd) else { return };
    let mut files: Option<Vec<PathBuf>> = None;
    match with_app(|a| {
        a.start_job(audio.clone(), |f| {
            files = Some(f.to_vec());
            false
        })
    }) {
        Some(Err((t, b))) => return show_error(&t, &b),
        None => return,
        _ => {}
    }
    let Some(files) = files else { return };
    // A single audio file was already confirmed by the save dialog.
    let asked = audio.is_some() && with_app(|a| a.mode == Mode::Custom).unwrap_or(false);
    if !asked && !confirm_overwrite(h, &files) {
        return;
    }
    if let Some(Err((t, b))) = with_app(|a| a.start_job(audio, |_| true)) {
        show_error(&t, &b);
    }
}

/// Save dialog for the audio export; the format comes from the type list or the typed extension.
fn export_audio() {
    let Some((h, batch, dir, name, last)) = with_app(|a| {
        (!a.running && a.has_audio()).then(|| (a.ui.hwnd, a.mode == Mode::Batch, a.out_dir.clone(), a.clean_name(), a.last_audio))
    })
    .flatten() else {
        return;
    };
    let filters: Vec<dialogs::Filter> = AudioFormat::ALL
        .iter()
        .map(|f| dialogs::Filter { name: format!("{} — {}", f.label(), tr(f.describe())), spec: format!("*.{}", f.ext()) })
        .collect();
    let sel = AudioFormat::ALL.iter().position(|f| *f == last).unwrap_or(1);
    let title = tr(if batch { "Export audio — one file per part" } else { "Export audio" });
    let Some((chosen, idx)) =
        dialogs::save_file(h, &title, dir.as_deref(), &format!("{name}.{}", last.ext()), &filters, sel, last.ext(), !batch)
    else {
        return;
    };
    let by_ext = chosen
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .and_then(|e| AudioFormat::ALL.iter().copied().find(|f| f.ext() == e));
    let fmt = by_ext.or(AudioFormat::ALL.get(idx).copied()).unwrap_or(last);
    let path = if by_ext.is_some() {
        chosen.clone()
    } else {
        let mut os = chosen.clone().into_os_string();
        os.push(format!(".{}", fmt.ext()));
        PathBuf::from(os)
    };
    if !batch && path != chosen && !confirm_overwrite(h, &[path.clone()]) {
        return;
    }
    with_app(|a| a.last_audio = fmt);
    start_export(Some((fmt, path)));
}

fn set_language(l: Lang) {
    if l == i18n::lang() {
        return;
    }
    i18n::set_lang(l);
    with_app(|a| {
        set_flag(a);
        apply_texts(a);
        if a.info.is_some() {
            a.request_preview(0); // translated badge
        }
    });
}

/// The flag button's popup: the four languages, the current one checked.
fn language_menu() {
    let Some((hwnd, btn)) = with_app(|a| (a.ui.hwnd, a.ui.lang_btn)) else { return };
    unsafe {
        let menu = CreatePopupMenu().unwrap_or_default();
        for (i, l) in Lang::ALL.iter().enumerate() {
            let _ = AppendMenuW(menu, MF_STRING, IDM_LANG + i, &HSTRING::from(l.native_name()));
        }
        let cur = Lang::ALL.iter().position(|l| *l == i18n::lang()).unwrap_or(0);
        let _ = CheckMenuRadioItem(menu, IDM_LANG as u32, (IDM_LANG + Lang::ALL.len() - 1) as u32, (IDM_LANG + cur) as u32, MF_BYCOMMAND.0);
        let mut r = RECT::default();
        let _ = GetWindowRect(btn, &mut r);
        let cmd = TrackPopupMenu(menu, TPM_RETURNCMD | TPM_RIGHTALIGN | TPM_TOPALIGN, r.right, r.bottom, Some(0), hwnd, None);
        let _ = DestroyMenu(menu);
        let i = cmd.0 as usize;
        if (IDM_LANG..IDM_LANG + Lang::ALL.len()).contains(&i) {
            set_language(Lang::ALL[i - IDM_LANG]);
        }
    }
}

fn flag_png(l: Lang) -> &'static [u8] {
    match l {
        Lang::En => include_bytes!("../assets/flags/en.png"),
        Lang::Pt => include_bytes!("../assets/flags/pt.png"),
        Lang::Es => include_bytes!("../assets/flags/es.png"),
        Lang::El => include_bytes!("../assets/flags/el.png"),
    }
}

/// Put the current language's flag on the language button.
fn set_flag(a: &App) {
    let px = (24.0 * a.scale).round() as u32;
    if let Some(icon) = image::decode_png(flag_png(i18n::lang())).ok().and_then(|img| image::icon_fit(&img, px)) {
        unsafe {
            let old = SendMessageW(a.ui.lang_btn, BM_SETIMAGE, Some(WPARAM(IMAGE_ICON.0 as usize)), Some(LPARAM(icon.0 as isize)));
            if old.0 != 0 {
                let _ = DestroyIcon(HICON(old.0 as _));
            }
        }
    }
}

fn command(id: usize, code: u32, ctl: HWND) {
    match id {
        ID_OPEN => pick_and_open(),
        ID_DROP_TITLE if code == STN_CLICKED => pick_and_open(),
        ID_SHOW_DIR => {
            if let Some(Some(d)) = with_app(|a| a.out_dir.clone()) {
                open_folder(&d);
            }
        }
        ID_LANG => language_menu(),
        i if (IDM_LANG..IDM_LANG + Lang::ALL.len()).contains(&i) => set_language(Lang::ALL[i - IDM_LANG]),
        ID_AUDIO => export_audio(),
        ID_OUT_BTN => pick_out_dir(),
        ID_GO | 1 /* IDOK: Enter on the default button */ => {
            let running = with_app(|a| a.running).unwrap_or(false);
            if running {
                with_app(|a| a.cancel_job());
            } else if unsafe { IsWindowEnabled(ctl_or(ctl, ID_GO)).as_bool() } {
                start_export(None);
            }
        }
        ID_EVERY | ID_PARTS if code == BN_CLICKED => {
            with_app(|a| {
                a.split_every = id == ID_EVERY;
                a.batch_changed();
            });
        }
        i if (EDIT_ID_BASE..EDIT_ID_BASE + N_EDITS).contains(&i) => {
            let idx = i - EDIT_ID_BASE;
            match code {
                EN_CHANGE => {
                    with_app(|a| match idx {
                        E_START | E_END => a.entry_typed(idx),
                        E_NAME => a.name_auto = false,
                        _ => a.spin_typed(idx),
                    });
                }
                EN_SETFOCUS if matches!(idx, E_START | E_END) => {
                    with_app(|a| a.set_focus_handle(if idx == E_START { Handle::Start } else { Handle::End }, true));
                }
                EN_KILLFOCUS => {
                    with_app(|a| entry_commit(a, idx));
                }
                _ => {}
            }
        }
        _ => {}
    }
}

fn ctl_or(ctl: HWND, id: usize) -> HWND {
    if !ctl.0.is_null() {
        return ctl;
    }
    with_app(|a| unsafe { GetDlgItem(Some(a.ui.hwnd), id as i32).unwrap_or_default() }).unwrap_or_default()
}

// ───────────────────────── main window ─────────────────────────

extern "system" fn wndproc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_COMMAND => {
                command(wp.0 & 0xffff, ((wp.0 >> 16) & 0xffff) as u32, HWND(lp.0 as _));
                return LRESULT(0);
            }
            WM_NOTIFY => {
                let hdr = &*(lp.0 as *const NMHDR);
                match hdr.code {
                    TTN_GETDISPINFOW => {
                        let info = &mut *(lp.0 as *mut NMTTDISPINFOW);
                        let text = tooltip_text(hdr.idFrom);
                        TIPBUF.with(|b| {
                            let mut b = b.borrow_mut();
                            *b = text.encode_utf16().chain(std::iter::once(0)).collect();
                            info.lpszText = PWSTR(b.as_mut_ptr());
                        });
                    }
                    TCN_SELCHANGE => {
                        let sel = SendMessageW(hdr.hwndFrom, TCM_GETCURSEL, None, None).0;
                        with_app(|a| a.set_mode(if sel == 1 { Mode::Batch } else { Mode::Custom }));
                    }
                    LVN_GETEMPTYMARKUP => {
                        let m = &mut *(lp.0 as *mut NMLVEMPTYMARKUP);
                        m.dwFlags = EMF_CENTERED;
                        let t: Vec<u16> = tr("Encoding jobs will appear here.").encode_utf16().collect();
                        let n = t.len().min(m.szMarkup.len() - 1);
                        m.szMarkup[..n].copy_from_slice(&t[..n]);
                        m.szMarkup[n] = 0;
                        return LRESULT(1);
                    }
                    _ => {}
                }
                return LRESULT(0);
            }
            WM_CTLCOLORSTATIC | WM_CTLCOLOREDIT => {
                let hdc = HDC(wp.0 as _);
                let ctl = HWND(lp.0 as _);
                let err = with_app(|a| a.ui.edits.iter().position(|e| *e == ctl).map(|i| a.edit_error[i]).unwrap_or(false))
                    .unwrap_or(false);
                let bg = GetSysColor(COLOR_WINDOW);
                SetBkColor(hdc, COLORREF(bg));
                if err {
                    SetTextColor(hdc, COLORREF(0x0000_1CC0)); // dark red (BGR)
                } else if msg == WM_CTLCOLORSTATIC {
                    let dim = with_app(|a| ctl == a.ui.drop_sub || ctl == a.ui.drop_info || ctl == a.ui.cores_lbl || ctl == a.ui.ext_lbl)
                        .unwrap_or(false);
                    SetTextColor(hdc, COLORREF(GetSysColor(if dim { COLOR_GRAYTEXT } else { COLOR_WINDOWTEXT })));
                }
                return LRESULT(GetSysColorBrush(COLOR_WINDOW).0 as isize);
            }
            WM_SIZE => {
                with_app(layout);
                return LRESULT(0);
            }
            WM_GETMINMAXINFO => {
                let mmi = &mut *(lp.0 as *mut MINMAXINFO);
                let s = GetDpiForWindow(hwnd) as f32 / 96.0;
                mmi.ptMinTrackSize = POINT { x: (1080.0 * s) as i32, y: (800.0 * s) as i32 };
                return LRESULT(0);
            }
            WM_DPICHANGED => {
                let dpi = (wp.0 & 0xffff) as u32;
                with_app(|a| {
                    let (f, b) = make_fonts(dpi);
                    let _ = DeleteObject(HGDIOBJ(a.font.0));
                    let _ = DeleteObject(HGDIOBJ(a.bold_font.0));
                    a.font = f;
                    a.bold_font = b;
                    a.scale = dpi as f32 / 96.0;
                    let _ = EnumChildWindows(Some(hwnd), Some(set_font_proc), LPARAM(f.0 as isize));
                    SendMessageW(a.ui.drop_title, WM_SETFONT, Some(WPARAM(b.0 as usize)), Some(LPARAM(1)));
                });
                let r = &*(lp.0 as *const RECT);
                let _ = SetWindowPos(hwnd, None, r.left, r.top, r.right - r.left, r.bottom - r.top, SWP_NOZORDER | SWP_NOACTIVATE);
                with_app(layout);
                return LRESULT(0);
            }
            WM_DROPFILES => {
                let hdrop = HDROP(wp.0 as _);
                let mut buf = vec![0u16; 32768];
                let n = DragQueryFileW(hdrop, 0, Some(&mut buf));
                DragFinish(hdrop);
                if n > 0 && with_app(|a| !a.running).unwrap_or(false) {
                    load_file(PathBuf::from(String::from_utf16_lossy(&buf[..n as usize])));
                }
                return LRESULT(0);
            }
            WM_TIMER => {
                match wp.0 {
                    TIMER_PREVIEW => {
                        with_app(|a| a.grab_preview());
                    }
                    TIMER_JOBS => {
                        if let Some(Some(dir)) = with_app(|a| a.poll_jobs()) {
                            open_folder(&dir);
                        }
                    }
                    _ => {}
                }
                return LRESULT(0);
            }
            WM_APP_THUMB => {
                with_app(|a| a.thumbs_ready());
                return LRESULT(0);
            }
            WM_APP_STARTUP => {
                if !splitter::ffmpeg_available() {
                    show_error(
                        &tr("FFmpeg was not found"),
                        &tr("Chop Chop Splitter uses FFmpeg to cut videos. Reinstall Chop Chop Splitter, or put ffmpeg.exe and ffprobe.exe next to chop-chop.exe or on your PATH."),
                    );
                }
                if let Some(p) = STARTUP_FILE.with(|f| f.borrow_mut().take()) {
                    load_file(p);
                }
                return LRESULT(0);
            }
            WM_CLOSE => {
                // Stop running encodes so no FFmpeg processes are left behind.
                with_app(|a| {
                    if let Some(c) = &a.cancel {
                        c.store(true, std::sync::atomic::Ordering::Relaxed);
                    }
                });
                let _ = DestroyWindow(hwnd);
                return LRESULT(0);
            }
            WM_DESTROY => {
                PostQuitMessage(0);
                return LRESULT(0);
            }
            _ => {}
        }
        DefWindowProcW(hwnd, msg, wp, lp)
    }
}

// ───────────────────────── video preview / thumbnail ─────────────────────────

/// Double-buffered paint: `f` draws into a memory DC the size of the client area.
fn buffered_paint(hwnd: HWND, f: impl FnOnce(HDC, RECT)) {
    unsafe {
        let mut ps = PAINTSTRUCT::default();
        let dc = BeginPaint(hwnd, &mut ps);
        let mut rc = RECT::default();
        let _ = GetClientRect(hwnd, &mut rc);
        let mem = CreateCompatibleDC(Some(dc));
        let bmp = CreateCompatibleBitmap(dc, rc.right.max(1), rc.bottom.max(1));
        let old = SelectObject(mem, HGDIOBJ(bmp.0));
        f(mem, rc);
        let _ = BitBlt(dc, 0, 0, rc.right, rc.bottom, Some(mem), 0, 0, SRCCOPY);
        SelectObject(mem, old);
        let _ = DeleteObject(HGDIOBJ(bmp.0));
        let _ = DeleteDC(mem);
        let _ = EndPaint(hwnd, &ps);
    }
}

fn draw_text(dc: HDC, s: &str, rc: &mut RECT, flags: DRAW_TEXT_FORMAT) {
    let mut w: Vec<u16> = s.encode_utf16().collect();
    unsafe {
        DrawTextW(dc, &mut w, rc, flags);
    }
}

extern "system" fn preview_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_PAINT => {
                let id = GetDlgCtrlID(hwnd) as usize;
                let mut drawn = false;
                buffered_paint(hwnd, |dc, rc| {
                    let _ = FillRect(dc, &rc, HBRUSH(GetStockObject(BLACK_BRUSH).0));
                    drawn = with_app(|a| {
                        let img = if id == ID_THUMB { a.card_img.as_ref() } else { a.preview_img.as_ref() };
                        let old = SelectObject(dc, HGDIOBJ(a.font.0));
                        SetBkMode(dc, TRANSPARENT);
                        match img {
                            Some(img) if a.info.is_some() => image::draw_fit(dc, img, rc),
                            _ if id == ID_PREVIEW => {
                                SetTextColor(dc, COLORREF(0x00A0A0A0));
                                let mut r = rc;
                                draw_text(dc, &tr("NO VIDEO"), &mut r, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
                            }
                            _ => {}
                        }
                        if id == ID_PREVIEW && a.info.is_some() && !a.badge.is_empty() {
                            // Which frame is shown, top-left, like the GTK badge.
                            let mut m = RECT::default();
                            draw_text(dc, &a.badge, &mut m, DT_CALCRECT | DT_SINGLELINE);
                            let pad = (6.0 * a.scale) as i32;
                            let br = RECT { left: pad, top: pad, right: pad * 3 + m.right, bottom: pad * 2 + m.bottom };
                            let _ = FillRect(dc, &br, HBRUSH(GetStockObject(BLACK_BRUSH).0));
                            let _ = FrameRect(dc, &br, GetSysColorBrush(COLOR_HIGHLIGHT));
                            SetTextColor(dc, COLORREF(0x00FFFFFF));
                            let mut tr_ = br;
                            draw_text(dc, &a.badge, &mut tr_, DT_CENTER | DT_VCENTER | DT_SINGLELINE);
                        }
                        SelectObject(dc, old);
                    })
                    .is_some();
                });
                if !drawn {
                    // The app was busy (borrowed); paint again once it is free.
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                return LRESULT(0);
            }
            WM_ERASEBKGND => return LRESULT(1),
            WM_SETCURSOR if GetDlgCtrlID(hwnd) as usize == ID_THUMB => {
                SetCursor(LoadCursorW(None, IDC_HAND).ok());
                return LRESULT(1);
            }
            WM_LBUTTONUP if GetDlgCtrlID(hwnd) as usize == ID_THUMB => {
                if let Ok(parent) = GetParent(hwnd) {
                    let _ = PostMessageW(Some(parent), WM_COMMAND, WPARAM(ID_OPEN), LPARAM(0));
                }
                return LRESULT(0);
            }
            _ => {}
        }
        DefWindowProcW(hwnd, msg, wp, lp)
    }
}

// ───────────────────────── start / end range slider ─────────────────────────

struct Track {
    x0: f32,
    w: f32,
    cy: i32,
}

fn track_geom(hwnd: HWND, a: &App) -> Track {
    let mut rc = RECT::default();
    unsafe {
        let _ = GetClientRect(hwnd, &mut rc);
    }
    let pad = 12.0 * a.scale;
    Track { x0: pad, w: (rc.right as f32 - 2.0 * pad).max(1.0), cy: (18.0 * a.scale) as i32 }
}

fn t_to_x(tr_: &Track, a: &App, t: f64) -> i32 {
    let d = a.info.as_ref().map(|i| i.duration).unwrap_or(1.0).max(0.001);
    (tr_.x0 + (t / d).clamp(0.0, 1.0) as f32 * tr_.w).round() as i32
}

fn x_to_t(tr_: &Track, a: &App, x: i32) -> f64 {
    let d = a.info.as_ref().map(|i| i.duration).unwrap_or(0.0);
    (((x as f32 - tr_.x0) / tr_.w).clamp(0.0, 1.0) as f64) * d
}

fn handle_at(hwnd: HWND, a: &App, x: i32) -> Handle {
    let g = track_geom(hwnd, a);
    let (xs, xe) = (t_to_x(&g, a, a.start), t_to_x(&g, a, a.end));
    if (x - xs).abs() < (x - xe).abs() || ((x - xs).abs() == (x - xe).abs() && x < xs) {
        Handle::Start
    } else {
        Handle::End
    }
}

fn paint_timeline(hwnd: HWND, dc: HDC, rc: RECT, a: &App) {
    unsafe {
        let _ = FillRect(dc, &rc, GetSysColorBrush(COLOR_WINDOW));
        let g = track_geom(hwnd, a);
        let enabled = a.info.is_some() && !a.running;
        let theme = OpenThemeData(Some(hwnd), w!("TRACKBAR"));
        let (x0, x1) = (g.x0 as i32, (g.x0 + g.w) as i32);
        let s = a.scale;

        // Track
        let track = RECT { left: x0, top: g.cy - (2.0 * s) as i32, right: x1, bottom: g.cy + (2.0 * s) as i32 };
        if !theme.is_invalid() {
            let _ = DrawThemeBackground(theme, dc, TKP_TRACK.0, TRS_NORMAL.0, &track, None);
        } else {
            let _ = DrawEdge(dc, &track as *const _ as *mut _, EDGE_SUNKEN, BF_RECT);
        }
        // Ticks every 5 %
        let pen = CreatePen(PS_SOLID, 1, COLORREF(GetSysColor(COLOR_GRAYTEXT)));
        let oldp = SelectObject(dc, HGDIOBJ(pen.0));
        for i in 0..=20 {
            let x = x0 + ((x1 - x0) as f32 * i as f32 / 20.0).round() as i32;
            let len = if i % 5 == 0 { 6.0 } else { 3.0 };
            let _ = MoveToEx(dc, x, g.cy + (11.0 * s) as i32, None);
            let _ = LineTo(dc, x, g.cy + ((11.0 + len) * s) as i32);
        }
        SelectObject(dc, oldp);
        let _ = DeleteObject(HGDIOBJ(pen.0));

        // Handles: the system trackbar thumb.
        if a.info.is_some() {
            let (tw, th) = ((11.0 * s) as i32, (20.0 * s) as i32);
            for (h, t) in [(Handle::Start, a.start), (Handle::End, a.end)] {
                let x = t_to_x(&g, a, t);
                let r = RECT { left: x - tw / 2, top: g.cy - th / 2 - (2.0 * s) as i32, right: x - tw / 2 + tw, bottom: g.cy + th / 2 };
                let state = if !enabled {
                    TUBS_DISABLED
                } else if a.drag == Some(h) {
                    TUBS_PRESSED
                } else if a.hot == Some(h) {
                    TUBS_HOT
                } else if a.focus == h && GetFocus() == hwnd {
                    TUBS_FOCUSED
                } else {
                    TUBS_NORMAL
                };
                if !theme.is_invalid() {
                    let _ = DrawThemeBackground(theme, dc, TKP_THUMBBOTTOM.0, state.0, &r, None);
                } else {
                    let _ = DrawFrameControl(dc, &r as *const _ as *mut _, DFC_BUTTON, DFCS_BUTTONPUSH);
                }
            }
        }
        if !theme.is_invalid() {
            let _ = CloseThemeData(theme);
        }

        // Labels: 00:00:00 … clip length … duration
        let old = SelectObject(dc, HGDIOBJ(a.font.0));
        SetBkMode(dc, TRANSPARENT);
        SetTextColor(dc, COLORREF(GetSysColor(COLOR_GRAYTEXT)));
        let ly = g.cy + (20.0 * s) as i32;
        let mut r = RECT { left: (2.0 * s) as i32, top: ly, right: rc.right - (14.0 * s) as i32, bottom: rc.bottom };
        let dur = a.info.as_ref().map(|i| i.duration).unwrap_or(0.0);
        draw_text(dc, "00:00:00", &mut r.clone(), DT_LEFT | DT_SINGLELINE);
        draw_text(dc, &splitter::fmt_time(dur), &mut r.clone(), DT_RIGHT | DT_SINGLELINE);
        if a.info.is_some() {
            SetTextColor(dc, COLORREF(GetSysColor(COLOR_WINDOWTEXT)));
            draw_text(dc, &splitter::fmt_time(a.end - a.start), &mut r, DT_CENTER | DT_SINGLELINE);
        }
        SelectObject(dc, old);
        if GetFocus() == hwnd && (SendMessageW(hwnd, WM_QUERYUISTATE, None, None).0 as u32 & UISF_HIDEFOCUS) == 0 {
            let _ = DrawFocusRect(dc, &rc);
        }
    }
}

extern "system" fn timeline_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    unsafe {
        match msg {
            WM_PAINT => {
                let mut drawn = false;
                buffered_paint(hwnd, |dc, rc| {
                    drawn = with_app(|a| paint_timeline(hwnd, dc, rc, a)).is_some();
                    if !drawn {
                        let _ = FillRect(dc, &rc, GetSysColorBrush(COLOR_WINDOW));
                    }
                });
                if !drawn {
                    let _ = InvalidateRect(Some(hwnd), None, false);
                }
                return LRESULT(0);
            }
            WM_ERASEBKGND => return LRESULT(1),
            WM_GETDLGCODE => return LRESULT(DLGC_WANTARROWS as isize),
            WM_SETFOCUS | WM_KILLFOCUS => {
                let _ = InvalidateRect(Some(hwnd), None, false);
                return LRESULT(0);
            }
            WM_LBUTTONDOWN => {
                let _ = SetFocus(Some(hwnd));
                let (x, _) = lparam_xy(lp);
                let grabbed = with_app(|a| {
                    if a.info.is_none() || a.running {
                        return false;
                    }
                    let g = track_geom(hwnd, a);
                    let h = handle_at(hwnd, a, x);
                    a.drag = Some(h);
                    let t = x_to_t(&g, a, x);
                    a.move_handle(h, t);
                    true
                })
                .unwrap_or(false);
                if grabbed {
                    SetCapture(hwnd);
                }
                return LRESULT(0);
            }
            WM_MOUSEMOVE => {
                let (x, _) = lparam_xy(lp);
                with_app(|a| {
                    if a.info.is_none() {
                        return;
                    }
                    let g = track_geom(hwnd, a);
                    if let Some(h) = a.drag {
                        let t = x_to_t(&g, a, x);
                        a.move_handle(h, t);
                    } else {
                        let near = |t: f64| (x - t_to_x(&g, a, t)).abs() < (8.0 * a.scale) as i32;
                        let hot = if near(a.start) || near(a.end) { Some(handle_at(hwnd, a, x)) } else { None };
                        if hot != a.hot {
                            a.hot = hot;
                            let mut tme = TRACKMOUSEEVENT {
                                cbSize: std::mem::size_of::<TRACKMOUSEEVENT>() as u32,
                                dwFlags: TME_LEAVE,
                                hwndTrack: hwnd,
                                dwHoverTime: 0,
                            };
                            let _ = TrackMouseEvent(&mut tme);
                            redraw(hwnd);
                        }
                    }
                });
                return LRESULT(0);
            }
            WM_MOUSELEAVE => {
                with_app(|a| {
                    a.hot = None;
                    redraw(hwnd);
                });
                return LRESULT(0);
            }
            WM_LBUTTONUP => {
                let _ = ReleaseCapture();
                with_app(|a| {
                    a.drag = None;
                    redraw(hwnd);
                });
                return LRESULT(0);
            }
            WM_SETCURSOR => {
                let over = with_app(|a| a.hot.is_some() || a.drag.is_some()).unwrap_or(false);
                if over {
                    SetCursor(LoadCursorW(None, IDC_SIZEWE).ok());
                    return LRESULT(1);
                }
            }
            WM_KEYDOWN => {
                // ←/→ nudge the selected handle by one frame (Shift: one second).
                let key = wp.0 as u16;
                if key == VK_LEFT.0 || key == VK_RIGHT.0 {
                    let shift = GetKeyState(VK_SHIFT.0 as i32) < 0;
                    with_app(|a| {
                        if a.info.is_none() || a.running {
                            return;
                        }
                        let fps = a.info.as_ref().map(|i| i.fps).unwrap_or(0.0);
                        let step = if shift { 1.0 } else if fps > 0.0 { 1.0 / fps } else { 0.1 };
                        let d = if key == VK_LEFT.0 { -step } else { step };
                        let h = a.focus;
                        let t = if h == Handle::Start { a.start } else { a.end } + d;
                        a.move_handle(h, t);
                    });
                    return LRESULT(0);
                }
            }
            _ => {}
        }
        DefWindowProcW(hwnd, msg, wp, lp)
    }
}
