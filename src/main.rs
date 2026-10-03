//! Chop Chop Splitter — cut a clip from a start time to an end time, or batch-split a whole
//! video into parts, with a live thumbnail preview and multicore FFmpeg encoding.
//! GTK 3 + Ubuntu's Yaru icons.

mod i18n;
mod splitter;

use gtk::prelude::*;
use gtk::{cairo, gdk, gdk_pixbuf, gio, glib};
use i18n::{bind, tr, trf, Lang};
use splitter::{AudioFormat, CutJob, Msg, TaskKind, VideoInfo};
use std::cell::RefCell;
use std::f64::consts::PI;
use std::path::{Path, PathBuf};
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc, Arc};
use std::time::{Duration, Instant};

const APP_ID: &str = "io.github.ChopChop";
const RES: &str = "/io/github/chopchop";
const PREVIEW_W: i32 = 560;
const PREVIEW_H: i32 = 315;
const MIN_CLIP: f64 = 0.1;
const TL_PAD: f64 = 14.0;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Handle {
    Start,
    End,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Mode {
    /// One clip between a start and an end time.
    Custom,
    /// The whole video split into many part files.
    Batch,
}

struct Ui {
    window: gtk::ApplicationWindow,
    open_dir_btn: gtk::Button,
    drop_btn: gtk::Button,
    drop_thumb: gtk::Image,
    drop_title: gtk::Label,
    drop_sub: gtk::Label,
    chips: gtk::Box,
    mode_stack: gtk::Stack,
    preview: gtk::Image,
    preview_badge: gtk::Label,
    preview_spinner: gtk::Spinner,
    timeline: gtk::DrawingArea,
    // custom selection
    start_entry: gtk::Entry,
    end_entry: gtk::Entry,
    // batch split
    split_stack: gtk::Stack,
    min_spin: gtk::SpinButton,
    sec_spin: gtk::SpinButton,
    parts_spin: gtk::SpinButton,
    // export
    workers_spin: gtk::SpinButton,
    out_btn: gtk::Button,
    out_label: gtk::Label,
    name_entry: gtk::Entry,
    ext_label: gtk::Label,
    open_when_done: gtk::CheckButton,
    audio_btn: gtk::Button,
    go_btn: gtk::Button,
    go_icon: gtk::Image,
    go_label: gtk::Label,
    overall: gtk::ProgressBar,
    percent: gtk::Label,
    status: gtk::Label,
    list: gtk::ListBox,
}

struct Row {
    bar: gtk::ProgressBar,
    icon: gtk::Image,
    state: gtk::Label,
    frac: f64,
    weight: f64,
}

struct State {
    info: Option<VideoInfo>,
    mode: Mode,
    start: f64,
    end: f64,
    focus: Handle,
    drag: Option<Handle>,
    batch: Vec<(f64, f64)>,
    preview_gen: u64,
    name_auto: bool,
    setting_name: bool,
    updating_entries: bool,
    running: bool,
    cancel: Option<Arc<AtomicBool>>,
    rows: Vec<Row>,
    started: Option<Instant>,
    out_dir_user_set: bool,
    out_dir: Option<PathBuf>,
    last_audio: AudioFormat,
}

impl Default for State {
    fn default() -> Self {
        State {
            info: None,
            mode: Mode::Custom,
            start: 0.0,
            end: 0.0,
            focus: Handle::Start,
            drag: None,
            batch: Vec::new(),
            preview_gen: 0,
            name_auto: true,
            setting_name: false,
            updating_entries: false,
            running: false,
            cancel: None,
            rows: Vec::new(),
            started: None,
            out_dir_user_set: false,
            out_dir: None,
            last_audio: AudioFormat::Mp3,
        }
    }
}

type St = Rc<RefCell<State>>;

fn main() -> glib::ExitCode {
    // Use the desktop's file dialogs (xdg-desktop-portal, same as the Files app) instead of
    // GTK 3's built-in chooser. GTK falls back to its own dialog if no portal is running.
    if std::env::var_os("GTK_USE_PORTAL").is_none() {
        std::env::set_var("GTK_USE_PORTAL", "1");
    }
    gio::resources_register_include!("compiled.gresource").expect("failed to register resources");
    i18n::init();
    let app = gtk::Application::builder()
        .application_id(APP_ID)
        .flags(gio::ApplicationFlags::NON_UNIQUE)
        .build();
    let initial: Option<PathBuf> = std::env::args().nth(1).map(PathBuf::from);
    app.connect_activate(move |app| build(app, initial.clone()));
    let argv0: Vec<String> = std::env::args().take(1).collect();
    app.run_with_args(&argv0)
}

// ───────────────────────── widget helpers ─────────────────────────

fn icon(name: &str, px: i32) -> gtk::Image {
    let i = gtk::Image::from_icon_name(Some(name), gtk::IconSize::Button);
    i.set_pixel_size(px);
    i
}

fn label(text: &str, class: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.set_xalign(0.0);
    if !class.is_empty() {
        l.style_context().add_class(class);
    }
    l
}

/// A label whose text follows the current language.
fn tlabel(key: &'static str, class: &str) -> gtk::Label {
    let l = label("", class);
    let l2 = l.clone();
    bind(move || l2.set_text(&tr(key)));
    l
}

/// A tooltip that follows the current language.
fn ttip(w: &impl IsA<gtk::Widget>, key: &'static str) {
    let w = w.clone().upcast::<gtk::Widget>();
    bind(move || w.set_tooltip_text(Some(&tr(key))));
}

/// Translated title of a GtkStack page (used by the StackSwitchers).
fn tstack_title(stack: &gtk::Stack, child: &impl IsA<gtk::Widget>, key: &'static str) {
    let (stack, child) = (stack.clone(), child.clone().upcast::<gtk::Widget>());
    bind(move || stack.child_set_property(&child, "title", &tr(key)));
}

fn chip(text: &str) -> gtk::Label {
    let l = gtk::Label::new(Some(text));
    l.style_context().add_class("vs-chip");
    l
}

fn accent_icon(name: &str, px: i32) -> gtk::Image {
    let i = icon(name, px);
    i.style_context().add_class("vs-icon");
    i
}

fn card(title: &'static str, title_icon: &str) -> (gtk::Box, gtk::Box, gtk::Box) {
    let outer = gtk::Box::new(gtk::Orientation::Vertical, 14);
    outer.style_context().add_class("vs-card");
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    head.pack_start(&accent_icon(title_icon, 16), false, false, 0);
    let t = label("", "vs-card-title");
    let t2 = t.clone();
    bind(move || t2.set_text(&i18n::upper(&tr(title))));
    t.set_valign(gtk::Align::Center);
    head.pack_start(&t, false, false, 0);
    outer.pack_start(&head, false, false, 0);
    let content = gtk::Box::new(gtk::Orientation::Vertical, 14);
    outer.pack_start(&content, true, true, 0);
    (outer, content, head)
}

/// A labelled form field: icon + title (+ hint) above a full-width widget.
fn field(parent: &gtk::Box, icon_name: &str, title: &'static str, hint: &str, widget: &impl IsA<gtk::Widget>) -> gtk::Label {
    let wrap = gtk::Box::new(gtk::Orientation::Vertical, 6);
    let head = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    head.pack_start(&accent_icon(icon_name, 16), false, false, 0);
    head.pack_start(&tlabel(title, "vs-field-label"), false, false, 0);
    let h = label(hint, "vs-hint");
    h.set_halign(gtk::Align::End);
    h.set_ellipsize(gtk::pango::EllipsizeMode::End);
    head.pack_end(&h, true, true, 0);
    wrap.pack_start(&head, false, false, 0);
    wrap.pack_start(widget, false, false, 0);
    parent.pack_start(&wrap, false, false, 0);
    h
}

/// Small "icon + title" header used above inline controls.
fn mini_head(title: &'static str, ic: &str) -> gtk::Box {
    let h = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    h.pack_start(&accent_icon(ic, 14), false, false, 0);
    h.pack_start(&tlabel(title, "vs-field-label"), false, false, 0);
    h
}

fn pixbuf_from_png(bytes: &[u8]) -> Option<gdk_pixbuf::Pixbuf> {
    let loader = gdk_pixbuf::PixbufLoader::new();
    loader.write(bytes).ok()?;
    loader.close().ok()?;
    loader.pixbuf()
}

fn time_entry() -> gtk::Entry {
    let e = gtk::Entry::new();
    e.set_width_chars(13);
    e.set_max_width_chars(13);
    e.style_context().add_class("vs-time");
    e.set_alignment(0.5);
    e.set_sensitive(false);
    e.set_text("00:00:00.000");
    e
}

// ───────────────────────── UI construction ─────────────────────────

fn build(app: &gtk::Application, initial: Option<PathBuf>) {
    if let Some(theme) = gtk::IconTheme::default() {
        theme.add_resource_path(&format!("{RES}/icons"));
    }
    gtk::Window::set_default_icon_name(APP_ID);
    let css = gtk::CssProvider::new();
    css.load_from_resource(&format!("{RES}/style.css"));
    if let Some(screen) = gdk::Screen::default() {
        gtk::StyleContext::add_provider_for_screen(&screen, &css, gtk::STYLE_PROVIDER_PRIORITY_APPLICATION);
    }

    let window = gtk::ApplicationWindow::builder()
        .application(app)
        .title("Chop Chop Splitter")
        .default_width(1200)
        .default_height(840)
        .icon_name(APP_ID)
        .build();
    window.style_context().add_class("vs-window");

    // Header bar
    let header = gtk::HeaderBar::builder()
        .show_close_button(true)
        .title("Chop Chop Splitter")
        .build();
    header.style_context().add_class("vs-header");
    let open_btn = gtk::Button::new();
    let ob = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    ob.pack_start(&icon("vs-document-open-symbolic", 16), false, false, 0);
    ob.pack_start(&tlabel("Open", ""), false, false, 0);
    open_btn.add(&ob);
    ttip(&open_btn, "Open a video (Ctrl+O)");
    header.pack_start(&open_btn);
    let open_dir_btn = gtk::Button::new();
    open_dir_btn.set_image(Some(&icon("vs-folder-open-symbolic", 16)));
    ttip(&open_dir_btn, "Show output folder");
    header.pack_end(&open_dir_btn);

    // Language picker: flag button with a popover listing the four languages.
    let lang_btn = gtk::MenuButton::new();
    lang_btn.style_context().add_class("vs-lang-btn");
    let lang_flag = icon(i18n::lang().flag_icon(), 22);
    lang_btn.add(&lang_flag);
    ttip(&lang_btn, "Language");
    let pop = gtk::Popover::new(Some(&lang_btn));
    pop.style_context().add_class("vs-lang-pop");
    let pbox = gtk::Box::new(gtk::Orientation::Vertical, 2);
    pbox.set_border_width(8);
    let ptitle = tlabel("Language", "vs-card-title");
    ptitle.set_margin_start(8);
    ptitle.set_margin_top(2);
    ptitle.set_margin_bottom(6);
    pbox.pack_start(&ptitle, false, false, 0);
    let mut lang_rows: Vec<(Lang, gtk::Button, gtk::Image)> = Vec::new();
    for l in Lang::ALL {
        let row = gtk::Button::new();
        row.set_relief(gtk::ReliefStyle::None);
        row.style_context().add_class("vs-lang-row");
        let rb = gtk::Box::new(gtk::Orientation::Horizontal, 12);
        rb.pack_start(&icon(l.flag_icon(), 24), false, false, 0);
        let name = label(l.native_name(), "vs-lang-name");
        name.set_hexpand(true);
        rb.pack_start(&name, true, true, 0);
        let check = accent_icon("vs-emblem-ok-symbolic", 16);
        check.set_no_show_all(true);
        check.set_visible(l == i18n::lang());
        rb.pack_end(&check, false, false, 0);
        row.add(&rb);
        pbox.pack_start(&row, false, false, 0);
        lang_rows.push((l, row, check));
    }
    pbox.show_all();
    pop.add(&pbox);
    lang_btn.set_popover(Some(&pop));
    header.pack_end(&lang_btn);
    window.set_titlebar(Some(&header));

    // Two columns: video on the left, export + progress on the right.
    let shell = gtk::Box::new(gtk::Orientation::Horizontal, 18);
    shell.style_context().add_class("vs-root");
    shell.set_border_width(20);
    let left_scroll = gtk::ScrolledWindow::builder().hscrollbar_policy(gtk::PolicyType::Never).build();
    let left = gtk::Box::new(gtk::Orientation::Vertical, 16);
    left_scroll.add(&left);
    let right = gtk::Box::new(gtk::Orientation::Vertical, 16);
    right.set_size_request(440, -1);
    shell.pack_start(&left_scroll, true, true, 0);
    shell.pack_start(&right, false, true, 0);
    window.add(&shell);

    // ── Drop zone / file info ──
    let drop_btn = gtk::Button::new();
    drop_btn.style_context().add_class("vs-drop");
    let drop_box = gtk::Box::new(gtk::Orientation::Horizontal, 16);
    let drop_thumb = icon("vs-video-hero", 64);
    drop_thumb.style_context().add_class("vs-thumb");
    drop_box.pack_start(&drop_thumb, false, false, 0);
    let drop_text = gtk::Box::new(gtk::Orientation::Vertical, 6);
    drop_text.set_valign(gtk::Align::Center);
    let drop_title = label(&tr("Drop a video here…"), "vs-drop-title");
    drop_title.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    let drop_sub = label("", "vs-drop-sub");
    drop_sub.set_no_show_all(true); // shown once a video is loaded (folder path)
    drop_sub.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    let chips = gtk::Box::new(gtk::Orientation::Horizontal, 6);
    drop_text.pack_start(&drop_title, false, false, 0);
    drop_text.pack_start(&drop_sub, false, false, 0);
    drop_text.pack_start(&chips, false, false, 2);
    drop_box.pack_start(&drop_text, true, true, 0);
    drop_btn.add(&drop_box);
    left.pack_start(&drop_btn, false, false, 0);

    // ── Preview card (mode switcher in the header) ──
    let (pcard, pcontent, phead) = card("Preview", "vs-camera-video-symbolic");
    let mode_stack = gtk::Stack::new();
    mode_stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    mode_stack.set_homogeneous(false);
    mode_stack.set_vhomogeneous(false);
    let mode_switch = gtk::StackSwitcher::new();
    mode_switch.set_stack(Some(&mode_stack));
    mode_switch.style_context().add_class("vs-seg");
    phead.pack_end(&mode_switch, false, false, 0);

    let overlay = gtk::Overlay::new();
    let frame_box = gtk::Box::new(gtk::Orientation::Vertical, 0);
    frame_box.style_context().add_class("vs-preview");
    frame_box.set_size_request(PREVIEW_W, PREVIEW_H);
    let preview = icon("vs-video-hero", 96);
    preview.set_opacity(0.85);
    preview.set_valign(gtk::Align::Center);
    preview.set_halign(gtk::Align::Center);
    frame_box.pack_start(&preview, true, true, 0);
    overlay.add(&frame_box);
    let preview_badge = gtk::Label::new(Some(&tr("NO VIDEO")));
    preview_badge.style_context().add_class("vs-badge");
    preview_badge.set_halign(gtk::Align::Start);
    preview_badge.set_valign(gtk::Align::Start);
    preview_badge.set_margin_start(12);
    preview_badge.set_margin_top(12);
    overlay.add_overlay(&preview_badge);
    let preview_spinner = gtk::Spinner::new();
    preview_spinner.style_context().add_class("vs-spinner");
    preview_spinner.set_halign(gtk::Align::End);
    preview_spinner.set_valign(gtk::Align::Start);
    preview_spinner.set_margin_end(14);
    preview_spinner.set_margin_top(14);
    preview_spinner.set_size_request(20, 20);
    overlay.add_overlay(&preview_spinner);
    pcontent.pack_start(&overlay, false, false, 0);

    let timeline = gtk::DrawingArea::new();
    timeline.set_size_request(-1, 70);
    timeline.add_events(
        gdk::EventMask::BUTTON_PRESS_MASK | gdk::EventMask::BUTTON_RELEASE_MASK | gdk::EventMask::POINTER_MOTION_MASK,
    );

    // Custom selection panel
    let custom = gtk::Box::new(gtk::Orientation::Vertical, 8);
    ttip(&timeline, "Drag the handles to choose the start and end of the clip");
    custom.pack_start(&timeline, false, false, 0);
    let times = gtk::Grid::new();
    times.set_column_spacing(12);
    let start_entry = time_entry();
    let end_entry = time_entry();
    let col = |title: &'static str, ic: &str, w: &gtk::Entry| {
        let b = gtk::Box::new(gtk::Orientation::Vertical, 6);
        b.pack_start(&mini_head(title, ic), false, false, 0);
        b.pack_start(w, false, false, 0);
        b
    };
    times.attach(&col("Start time", "vs-media-playback-start-symbolic", &start_entry), 0, 0, 1, 1);
    times.attach(&col("End time", "vs-process-stop-symbolic", &end_entry), 1, 0, 1, 1);
    times.set_margin_top(12); // a little breathing room below the bar
    custom.pack_start(&times, false, false, 0);
    mode_stack.add_titled(&custom, "custom", "Custom selection");
    tstack_title(&mode_stack, &custom, "Custom selection");

    // Batch split panel
    let batch = gtk::Box::new(gtk::Orientation::Vertical, 8);
    let bgrid = gtk::Grid::new();
    bgrid.set_column_spacing(12);
    let split_stack = gtk::Stack::new();
    split_stack.set_transition_type(gtk::StackTransitionType::Crossfade);
    split_stack.set_homogeneous(true);
    let every_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let min_spin = gtk::SpinButton::with_range(0.0, 1440.0, 1.0);
    min_spin.set_value(5.0);
    let sec_spin = gtk::SpinButton::with_range(0.0, 59.0, 1.0);
    every_box.pack_start(&min_spin, false, false, 0);
    every_box.pack_start(&tlabel("min", "vs-hint"), false, false, 0);
    every_box.pack_start(&sec_spin, false, false, 0);
    every_box.pack_start(&tlabel("sec", "vs-hint"), false, false, 0);
    let parts_box = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    let parts_spin = gtk::SpinButton::with_range(2.0, 999.0, 1.0);
    parts_spin.set_value(4.0);
    parts_box.pack_start(&parts_spin, false, false, 0);
    parts_box.pack_start(&tlabel("equal parts", "vs-hint"), false, false, 0);
    split_stack.add_titled(&every_box, "every", "Every…");
    split_stack.add_titled(&parts_box, "parts", "Equal parts");
    tstack_title(&split_stack, &every_box, "Every…");
    tstack_title(&split_stack, &parts_box, "Equal parts");
    let split_switch = gtk::StackSwitcher::new();
    split_switch.set_stack(Some(&split_stack));
    split_switch.style_context().add_class("vs-seg");
    split_switch.style_context().add_class("vs-seg-small");
    // Layout (as agreed in the mockup): the Every…/Equal parts switch sits on its own
    // row just under the preview, right-aligned; the heading and boxes sit lower.
    split_switch.set_halign(gtk::Align::End);
    split_switch.set_margin_top(28);
    batch.pack_start(&split_switch, false, false, 0);
    let bleft = gtk::Box::new(gtk::Orientation::Vertical, 10);
    bleft.set_margin_top(22);
    let bhead = mini_head("Split the whole video", "vs-view-grid-symbolic");
    bleft.pack_start(&bhead, false, false, 0);
    bleft.pack_start(&split_stack, false, false, 0);
    bleft.set_hexpand(true);
    bgrid.attach(&bleft, 0, 0, 1, 1);
    batch.pack_start(&bgrid, false, false, 0);
    mode_stack.add_titled(&batch, "batch", "Batch split");
    tstack_title(&mode_stack, &batch, "Batch split");
    // The mode panel fills the rest of the card: Custom selection sits at the top,
    // Batch split also starts at the top.
    custom.set_valign(gtk::Align::Start);
    batch.set_valign(gtk::Align::Start);
    pcontent.pack_start(&mode_stack, true, true, 0);
    // Expands so its bottom edge lines up with the Progress card on the right.
    left.pack_start(&pcard, true, true, 0);

    // ── Export card ──
    let (ecard, econtent, ehead) = card("Export", "vs-edit-cut-symbolic");
    let cores = splitter::cores();
    let wbox = gtk::Box::new(gtk::Orientation::Horizontal, 18);
    let workers_spin = gtk::SpinButton::with_range(1.0, (cores * 2).max(2) as f64, 1.0);
    workers_spin.set_value(cores as f64);
    wbox.pack_start(&workers_spin, false, false, 0);
    let cores_chip = chip("");
    {
        let c = cores_chip.clone();
        bind(move || c.set_text(&trf("{n} cores detected", &[("n", cores.to_string())])));
    }
    wbox.pack_start(&cores_chip, false, false, 0);
    field(&econtent, "vs-computer-chip-symbolic", "Parallel workers", "", &wbox);

    // Output folder: a button that opens the system folder picker (portal), showing the chosen folder.
    let out_btn = gtk::Button::new();
    out_btn.style_context().add_class("vs-folder-btn");
    let obox = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    obox.pack_start(&icon("vs-folder-open-symbolic", 16), false, false, 0);
    let out_label = gtk::Label::new(None);
    out_label.set_xalign(0.0);
    out_label.set_ellipsize(gtk::pango::EllipsizeMode::Middle);
    obox.pack_start(&out_label, true, true, 0);
    obox.pack_end(&tlabel("Change…", "vs-hint"), false, false, 0);
    out_btn.add(&obox);
    field(&econtent, "vs-folder-videos-symbolic", "Output folder", "", &out_btn);

    let nbox = gtk::Box::new(gtk::Orientation::Horizontal, 0);
    nbox.style_context().add_class("linked");
    let name_entry = gtk::Entry::new();
    name_entry.set_placeholder_text(Some("clip"));
    name_entry.set_hexpand(true);
    let ext_label = gtk::Label::new(Some(".mp4"));
    ext_label.style_context().add_class("vs-ext");
    nbox.pack_start(&name_entry, true, true, 0);
    nbox.pack_start(&ext_label, false, false, 0);
    field(&econtent, "vs-view-grid-symbolic", "File name", "", &nbox);

    let open_when_done = gtk::CheckButton::with_label("");
    {
        let c = open_when_done.clone();
        bind(move || c.set_label(&tr("Open the output folder when finished")));
    }
    open_when_done.set_active(true);
    econtent.pack_start(&open_when_done, false, false, 0);
    right.pack_start(&ecard, false, false, 0);

    // Export audio lives in the Export card header; the format is chosen in the save dialog.
    let audio_btn = gtk::Button::new();
    audio_btn.style_context().add_class("vs-audio-btn");
    let ab = gtk::Box::new(gtk::Orientation::Horizontal, 8);
    ab.pack_start(&icon("vs-audio-x-generic-symbolic", 16), false, false, 0);
    ab.pack_start(&tlabel("Export audio…", ""), false, false, 0);
    audio_btn.add(&ab);
    audio_btn.set_sensitive(false);
    ehead.pack_end(&audio_btn, false, false, 0);

    // ── Main action, then Progress right below it (next to the list of output videos) ──
    let go_btn = gtk::Button::new();
    go_btn.style_context().add_class("vs-go");
    let go_inner = gtk::Box::new(gtk::Orientation::Horizontal, 10);
    let go_icon = icon("vs-edit-cut-symbolic", 18);
    let go_label = gtk::Label::new(Some(&tr("Cut clip")));
    go_inner.pack_start(&go_icon, false, false, 0);
    go_inner.pack_start(&go_label, false, false, 0);
    go_inner.set_halign(gtk::Align::Center);
    go_btn.add(&go_inner);
    go_btn.set_sensitive(false);
    right.pack_start(&go_btn, false, false, 0);
    let (prcard, prcontent, _) = card("Progress", "vs-preferences-system-time-symbolic");
    let prog_row = gtk::Box::new(gtk::Orientation::Horizontal, 12);
    let overall = gtk::ProgressBar::new();
    overall.style_context().add_class("vs-big");
    overall.set_valign(gtk::Align::Center);
    let percent = gtk::Label::new(Some("0%"));
    percent.style_context().add_class("vs-percent");
    percent.set_width_chars(5);
    percent.set_xalign(1.0);
    prog_row.pack_start(&overall, true, true, 0);
    prog_row.pack_start(&percent, false, false, 0);
    prcontent.pack_start(&prog_row, false, false, 0);
    let status = label("", "vs-status");
    status.set_line_wrap(true);
    status.set_max_width_chars(1); // wrap within the column instead of widening it
    let list = gtk::ListBox::new();
    list.style_context().add_class("vs-list");
    list.set_selection_mode(gtk::SelectionMode::None);
    let ph = tlabel("Encoding jobs will appear here.", "");
    ph.set_xalign(0.5);
    ph.style_context().add_class("vs-empty");
    ph.set_margin_top(14);
    ph.set_margin_bottom(14);
    ph.show();
    list.set_placeholder(Some(&ph));
    // Always-visible scrollbar so it's obvious when more jobs are below.
    let list_scroll = gtk::ScrolledWindow::builder()
        .hscrollbar_policy(gtk::PolicyType::Never)
        .vscrollbar_policy(gtk::PolicyType::Automatic)
        .min_content_height(100)
        .vexpand(true)
        .build();
    list_scroll.set_overlay_scrolling(false);
    list_scroll.style_context().add_class("vs-jobs");
    list_scroll.add(&list);
    prcontent.pack_start(&list_scroll, true, true, 0);
    // Status / result line sits under the list of output videos.
    prcontent.pack_start(&status, false, false, 0);
    right.pack_start(&prcard, true, true, 0);

    let ui = Rc::new(Ui {
        window: window.clone(),
        open_dir_btn,
        drop_btn,
        drop_thumb,
        drop_title,
        drop_sub,
        chips,
        mode_stack,
        preview,
        preview_badge,
        preview_spinner,
        timeline,
        start_entry,
        end_entry,
        split_stack,
        min_spin,
        sec_spin,
        parts_spin,
        workers_spin,
        out_btn,
        out_label,
        name_entry,
        ext_label,
        open_when_done,
        audio_btn,
        go_btn,
        go_icon,
        go_label,
        overall,
        percent,
        status,
        list,
    });
    let st: St = Rc::new(RefCell::new(State::default()));
    let videos = glib::user_special_dir(glib::UserDirectory::Videos).unwrap_or_else(glib::home_dir);
    set_out_dir(&ui, &st, &videos);
    connect_signals(&ui, &st, &open_btn);
    {
        let rows = Rc::new(lang_rows);
        for (l, row, _) in rows.iter() {
            let (ui2, st2, rows2, flag, pop2, l) = (ui.clone(), st.clone(), rows.clone(), lang_flag.clone(), pop.clone(), *l);
            row.connect_clicked(move |_| {
                pop2.popdown();
                if l == i18n::lang() {
                    return;
                }
                i18n::set_lang(l);
                flag.set_from_icon_name(Some(l.flag_icon()), gtk::IconSize::Button);
                flag.set_pixel_size(22);
                for (ll, _, check) in rows2.iter() {
                    check.set_visible(*ll == l);
                }
                language_changed(&ui2, &st2);
            });
        }
    }
    window.show_all();
    mode_changed(&ui, &st);

    if splitter::ffmpeg_available() {
        // Find a working GPU encoder in the background, so the first export doesn't wait.
        std::thread::spawn(|| {
            splitter::video_encoder();
        });
    } else {
        error_dialog(
            &ui.window,
            &tr("FFmpeg was not found"),
            &tr("Chop Chop Splitter uses FFmpeg to cut videos. Install it with:\n\nsudo apt install ffmpeg"),
        );
    }
    if let Some(p) = initial {
        load_file(&ui, &st, &p);
    }
}

// ───────────────────────── signals ─────────────────────────

fn connect_signals(ui: &Rc<Ui>, st: &St, open_btn: &gtk::Button) {
    // Open dialog (button, drop zone, Ctrl+O)
    let pick: Rc<dyn Fn()> = {
        let (ui, st) = (ui.clone(), st.clone());
        Rc::new(move || {
            if st.borrow().running {
                return;
            }
            let dlg = gtk::FileChooserNative::new(
                Some(&tr("Open a video")),
                Some(&ui.window),
                gtk::FileChooserAction::Open,
                Some(&tr("_Open")),
                Some(&tr("_Cancel")),
            );
            let f = gtk::FileFilter::new();
            f.set_name(Some(&tr("Videos")));
            f.add_mime_type("video/*");
            dlg.add_filter(f);
            let all = gtk::FileFilter::new();
            all.set_name(Some(&tr("All files")));
            all.add_pattern("*");
            dlg.add_filter(all);
            if dlg.run() == gtk::ResponseType::Accept {
                if let Some(p) = dlg.filename() {
                    load_file(&ui, &st, &p);
                }
            }
        })
    };
    {
        let p = pick.clone();
        ui.drop_btn.connect_clicked(move |_| p());
    }
    {
        let p = pick.clone();
        open_btn.connect_clicked(move |_| p());
    }
    {
        let p = pick.clone();
        ui.window.connect_key_press_event(move |_, ev| {
            if ev.state().contains(gdk::ModifierType::CONTROL_MASK) && ev.keyval() == gdk::keys::constants::o {
                p();
                return glib::Propagation::Stop;
            }
            glib::Propagation::Proceed
        });
    }

    // Drag & drop a file anywhere on the window
    ui.window.drag_dest_set(
        gtk::DestDefaults::ALL,
        &[gtk::TargetEntry::new("text/uri-list", gtk::TargetFlags::OTHER_APP, 0)],
        gdk::DragAction::COPY,
    );
    {
        let (ui2, st) = (ui.clone(), st.clone());
        ui.window.connect_drag_data_received(move |_, _, _, _, data, _, _| {
            if st.borrow().running {
                return;
            }
            if let Some(uri) = data.uris().first() {
                if let Some(p) = gio::File::for_uri(uri).path() {
                    load_file(&ui2, &st, &p);
                }
            }
        });
    }

    // Mode switch (Custom selection / Batch split)
    {
        let (ui2, st2) = (ui.clone(), st.clone());
        ui.mode_stack.connect_visible_child_name_notify(move |_| mode_changed(&ui2, &st2));
    }

    // Batch settings
    {
        let refresh: Rc<dyn Fn()> = {
            let (ui2, st2) = (ui.clone(), st.clone());
            Rc::new(move || batch_changed(&ui2, &st2))
        };
        for spin in [&ui.min_spin, &ui.sec_spin, &ui.parts_spin] {
            let r = refresh.clone();
            spin.connect_value_changed(move |_| r());
        }
        let r = refresh.clone();
        ui.split_stack.connect_visible_child_name_notify(move |_| r());
    }

    // Timeline drawing + interaction
    {
        let st = st.clone();
        ui.timeline.connect_draw(move |w, cr| {
            draw_timeline(w, cr, &st.borrow());
            glib::Propagation::Proceed
        });
    }
    {
        let (ui2, st) = (ui.clone(), st.clone());
        ui.timeline.connect_button_press_event(move |w, ev| {
            if ev.button() != 1 || st.borrow().info.is_none() || st.borrow().running {
                return glib::Propagation::Proceed;
            }
            let x = ev.position().0;
            let t = x_to_time(w, &st.borrow(), x);
            let h = pick_handle(w, &st.borrow(), x);
            st.borrow_mut().drag = Some(h);
            move_handle(&ui2, &st, h, t);
            glib::Propagation::Stop
        });
    }
    {
        let (ui2, st) = (ui.clone(), st.clone());
        ui.timeline.connect_motion_notify_event(move |w, ev| {
            let x = ev.position().0;
            let drag = st.borrow().drag;
            if let Some(h) = drag {
                let t = x_to_time(w, &st.borrow(), x);
                move_handle(&ui2, &st, h, t);
            } else if let Some(win) = w.window() {
                let s = st.borrow();
                let cursor_name = s.info.as_ref().and_then(|_| {
                    let xs = time_to_x(w, &s, s.start);
                    let xe = time_to_x(w, &s, s.end);
                    ((x - xs).abs() < 10.0 || (x - xe).abs() < 10.0).then_some("ew-resize")
                });
                let cursor = cursor_name.and_then(|n| gdk::Cursor::from_name(&w.display(), n));
                win.set_cursor(cursor.as_ref());
            }
            glib::Propagation::Proceed
        });
    }
    {
        let st = st.clone();
        ui.timeline.connect_button_release_event(move |_, _| {
            st.borrow_mut().drag = None;
            glib::Propagation::Proceed
        });
    }

    // Time entries: live update while typing, normalise on Enter / focus-out
    for (entry, h) in [(&ui.start_entry, Handle::Start), (&ui.end_entry, Handle::End)] {
        let (ui2, st2) = (ui.clone(), st.clone());
        entry.connect_changed(move |e| entry_typed(&ui2, &st2, e, h));
        let (ui2, st2) = (ui.clone(), st.clone());
        entry.connect_activate(move |e| apply_entry(&ui2, &st2, e, h));
        let (ui2, st2) = (ui.clone(), st.clone());
        entry.connect_focus_out_event(move |e, _| {
            apply_entry(&ui2, &st2, e, h);
            glib::Propagation::Proceed
        });
    }

    // Export audio → save dialog → audio-only job
    {
        let (ui2, st2) = (ui.clone(), st.clone());
        ui.audio_btn.connect_clicked(move |_| {
            if st2.borrow().running {
                return;
            }
            if let Some(target) = pick_audio_target(&ui2, &st2) {
                start(&ui2, &st2, Some(target));
            }
        });
    }

    // File name
    {
        let st = st.clone();
        ui.name_entry.connect_changed(move |_| {
            let mut s = st.borrow_mut();
            if !s.setting_name {
                s.name_auto = false;
            }
        });
    }
    {
        let (ui2, st2) = (ui.clone(), st.clone());
        ui.out_btn.connect_clicked(move |_| {
            let dlg = gtk::FileChooserNative::new(
                Some(&tr("Choose the output folder")),
                Some(&ui2.window),
                gtk::FileChooserAction::SelectFolder,
                Some(&tr("_Select")),
                Some(&tr("_Cancel")),
            );
            if let Some(d) = st2.borrow().out_dir.clone() {
                dlg.set_current_folder(&d);
            }
            if dlg.run() == gtk::ResponseType::Accept {
                if let Some(d) = dlg.filename() {
                    set_out_dir(&ui2, &st2, &d);
                    st2.borrow_mut().out_dir_user_set = true;
                }
            }
        });
    }
    {
        let st2 = st.clone();
        ui.open_dir_btn.connect_clicked(move |_| {
            if let Some(d) = st2.borrow().out_dir.clone() {
                open_folder(&d);
            }
        });
    }
    {
        let (ui2, st) = (ui.clone(), st.clone());
        ui.go_btn.connect_clicked(move |_| {
            let running = st.borrow().running;
            if running {
                if let Some(c) = &st.borrow().cancel {
                    c.store(true, Ordering::Relaxed);
                }
                ui2.go_btn.set_sensitive(false);
                ui2.status.set_text(&tr("Cancelling…"));
            } else {
                start(&ui2, &st, None);
            }
        });
    }
}

// ───────────────────────── modes ─────────────────────────

fn mode_changed(ui: &Rc<Ui>, st: &St) {
    let mode = if ui.mode_stack.visible_child_name().as_deref() == Some("batch") { Mode::Batch } else { Mode::Custom };
    st.borrow_mut().mode = mode;
    if st.borrow().info.as_ref().map(|i| !i.acodec.is_empty()).unwrap_or(false) {
        ui.audio_btn.set_tooltip_text(Some(&tr(match mode {
            Mode::Custom => "Save the selected range as WAV, MP3, OGG, FLAC, M4A or OPUS",
            Mode::Batch => "Save one audio file per part as WAV, MP3, OGG, FLAC, M4A or OPUS",
        })));
    }
    if mode == Mode::Batch {
        batch_changed(ui, st);
    } else {
        ui.timeline.queue_draw();
        update_auto_name(ui, st);
        refresh_go(ui, st);
    }
    request_preview(ui, st, 0);
}

fn batch_changed(ui: &Rc<Ui>, st: &St) {
    let (dur, fps) = match st.borrow().info.as_ref() {
        Some(i) => (i.duration, i.fps),
        None => (0.0, 0.0),
    };
    let every = ui.split_stack.visible_child_name().as_deref() == Some("every");
    let every_secs = ui.min_spin.value() * 60.0 + ui.sec_spin.value();
    let bounds = if every {
        splitter::batch_bounds(dur, fps, Some(every_secs), None)
    } else {
        splitter::batch_bounds(dur, fps, None, Some(ui.parts_spin.value_as_int().max(1) as usize))
    };
    st.borrow_mut().batch = bounds;
    update_auto_name(ui, st);
    refresh_go(ui, st);
}

/// Main button text + sensitivity for the current mode.
fn refresh_go(ui: &Ui, st: &St) {
    let s = st.borrow();
    if s.running {
        return;
    }
    ui.go_icon.set_from_icon_name(Some("vs-edit-cut-symbolic"), gtk::IconSize::Button);
    ui.go_icon.set_pixel_size(18);
    let (text, ok) = match s.mode {
        Mode::Custom => (tr("Cut clip"), s.end - s.start >= MIN_CLIP),
        Mode::Batch => {
            let n = s.batch.len();
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
    ui.go_label.set_text(&text);
    ui.go_btn.set_sensitive(s.info.is_some() && ok);
    ui.ext_label.set_text(match s.mode {
        Mode::Custom => ".mp4",
        Mode::Batch => "_partNN.mp4",
    });
}

// ───────────────────────── timeline ─────────────────────────

fn track_width(w: &gtk::DrawingArea) -> f64 {
    (w.allocated_width() as f64 - 2.0 * TL_PAD).max(1.0)
}

fn time_to_x(w: &gtk::DrawingArea, s: &State, t: f64) -> f64 {
    let d = s.info.as_ref().map(|i| i.duration).unwrap_or(1.0).max(0.001);
    TL_PAD + (t / d).clamp(0.0, 1.0) * track_width(w)
}

fn x_to_time(w: &gtk::DrawingArea, s: &State, x: f64) -> f64 {
    let d = s.info.as_ref().map(|i| i.duration).unwrap_or(0.0);
    ((x - TL_PAD) / track_width(w)).clamp(0.0, 1.0) * d
}

fn pick_handle(w: &gtk::DrawingArea, s: &State, x: f64) -> Handle {
    let xs = time_to_x(w, s, s.start);
    let xe = time_to_x(w, s, s.end);
    if (x - xs).abs() < (x - xe).abs() || ((x - xs).abs() == (x - xe).abs() && x < xs) {
        Handle::Start
    } else {
        Handle::End
    }
}

fn rounded(cr: &cairo::Context, x: f64, y: f64, w: f64, h: f64, r: f64) {
    let r = r.min(w / 2.0).min(h / 2.0);
    cr.new_sub_path();
    cr.arc(x + w - r, y + r, r, -PI / 2.0, 0.0);
    cr.arc(x + w - r, y + h - r, r, 0.0, PI / 2.0);
    cr.arc(x + r, y + h - r, r, PI / 2.0, PI);
    cr.arc(x + r, y + r, r, PI, 1.5 * PI);
    cr.close_path();
}

fn draw_timeline(w: &gtk::DrawingArea, cr: &cairo::Context, s: &State) {
    let fg = w.style_context().color(gtk::StateFlags::NORMAL);
    let (r, g, b) = (fg.red(), fg.green(), fg.blue());
    let (ar, ag, ab) = (0.914, 0.329, 0.125); // Ubuntu orange #E95420
    let width = w.allocated_width() as f64;
    let (ty, th) = (12.0, 34.0);
    let tw = track_width(w);

    // Track
    rounded(cr, TL_PAD, ty, tw, th, 8.0);
    cr.set_source_rgba(r, g, b, 0.08);
    let _ = cr.fill();
    cr.set_source_rgba(r, g, b, 0.15);
    cr.set_line_width(1.0);
    for i in 1..20 {
        let x = (TL_PAD + tw * i as f64 / 20.0).round() + 0.5;
        let tick = if i % 5 == 0 { 10.0 } else { 5.0 };
        cr.move_to(x, ty + th - tick);
        cr.line_to(x, ty + th);
    }
    let _ = cr.stroke();

    // Labels
    let dur = s.info.as_ref().map(|i| i.duration).unwrap_or(0.0);
    cr.set_font_size(11.0);
    cr.set_source_rgba(r, g, b, 0.55);
    cr.move_to(TL_PAD, ty + th + 18.0);
    let _ = cr.show_text("00:00:00");
    let end_txt = splitter::fmt_time(dur);
    if let Ok(ext) = cr.text_extents(&end_txt) {
        cr.move_to(width - TL_PAD - ext.width(), ty + th + 18.0);
        let _ = cr.show_text(&end_txt);
    }
    if s.info.is_none() {
        return;
    }

    let xs = time_to_x(w, s, s.start);
    let xe = time_to_x(w, s, s.end);
    rounded(cr, xs, ty, (xe - xs).max(1.0), th, 6.0);
    cr.set_source_rgba(ar, ag, ab, 0.28);
    let _ = cr.fill_preserve();
    cr.set_source_rgba(ar, ag, ab, 0.9);
    cr.set_line_width(2.0);
    let _ = cr.stroke();
    let mid = splitter::fmt_time(s.end - s.start);
    if let Ok(ext) = cr.text_extents(&mid) {
        if xe - xs > ext.width() + 24.0 {
            cr.set_source_rgba(ar, ag, ab, 1.0);
            cr.move_to((xs + xe) / 2.0 - ext.width() / 2.0, ty + th / 2.0 + ext.height() / 2.0);
            let _ = cr.show_text(&mid);
        }
    }
    for (x, active) in [(xs, s.focus == Handle::Start), (xe, s.focus == Handle::End)] {
        let hw = 12.0;
        rounded(cr, x - hw / 2.0, ty - 6.0, hw, th + 12.0, 4.0);
        if active {
            cr.set_source_rgba(ar, ag, ab, 1.0);
        } else {
            cr.set_source_rgba(0.85 * ar, 0.85 * ag, 0.85 * ab, 1.0);
        }
        let _ = cr.fill();
        cr.set_source_rgba(1.0, 1.0, 1.0, 0.9);
        cr.set_line_width(1.5);
        for dx in [-2.0, 2.0] {
            cr.move_to(x + dx, ty + th / 2.0 - 7.0);
            cr.line_to(x + dx, ty + th / 2.0 + 7.0);
        }
        let _ = cr.stroke();
    }
}

/// Clamp and store a new handle time. Returns the value actually stored.
fn set_handle(st: &St, h: Handle, t: f64) -> Option<f64> {
    let mut s = st.borrow_mut();
    let d = s.info.as_ref().map(|i| i.duration)?;
    let v = match h {
        Handle::Start => t.clamp(0.0, (s.end - MIN_CLIP).max(0.0)),
        Handle::End => t.clamp((s.start + MIN_CLIP).min(d), d),
    };
    match h {
        Handle::Start => s.start = v,
        Handle::End => s.end = v,
    }
    s.focus = h;
    Some(v)
}

fn move_handle(ui: &Rc<Ui>, st: &St, h: Handle, t: f64) {
    if set_handle(st, h, t).is_none() {
        return;
    }
    range_changed(ui, st);
    request_preview(ui, st, 140);
}

/// Live update while the user types a time: move the bar, but don't rewrite the text.
fn entry_typed(ui: &Rc<Ui>, st: &St, e: &gtk::Entry, h: Handle) {
    if st.borrow().updating_entries || st.borrow().info.is_none() {
        return;
    }
    let Some(t) = splitter::parse_ts(&e.text()) else {
        e.style_context().add_class("error");
        return;
    };
    let dur = st.borrow().info.as_ref().map(|i| i.duration).unwrap_or(0.0);
    let (start, end) = {
        let s = st.borrow();
        (s.start, s.end)
    };
    // Flag values that would be clamped (end before start, past the end, …).
    let valid = match h {
        Handle::Start => t <= end - MIN_CLIP,
        Handle::End => t >= start + MIN_CLIP && t <= dur + 0.0005,
    };
    if valid {
        e.style_context().remove_class("error");
    } else {
        e.style_context().add_class("error");
    }
    set_handle(st, h, t);
    ui.timeline.queue_draw();
    update_auto_name(ui, st);
    refresh_go(ui, st);
    request_preview(ui, st, 250);
}

fn apply_entry(ui: &Rc<Ui>, st: &St, e: &gtk::Entry, h: Handle) {
    if st.borrow().info.is_none() {
        return;
    }
    match splitter::parse_ts(&e.text()) {
        Some(t) => {
            let changed = {
                let s = st.borrow();
                let cur = if h == Handle::Start { s.start } else { s.end };
                (cur - t).abs() > 0.0005
            };
            if changed {
                move_handle(ui, st, h, t);
            } else {
                range_changed(ui, st); // normalise the text
            }
        }
        None => e.style_context().add_class("error"),
    }
}

fn range_changed(ui: &Ui, st: &St) {
    let (start, end) = {
        let s = st.borrow();
        (s.start, s.end)
    };
    st.borrow_mut().updating_entries = true;
    for (e, v) in [(&ui.start_entry, start), (&ui.end_entry, end)] {
        let txt = splitter::fmt_ts(v);
        if e.text() != txt {
            e.set_text(&txt);
        }
        e.style_context().remove_class("error");
    }
    st.borrow_mut().updating_entries = false;
    ui.timeline.queue_draw();
    update_auto_name(ui, st);
    refresh_go(ui, st);
}

// ───────────────────────── thumbnails ─────────────────────────

/// Debounced frame grab for the big preview (runs FFmpeg off the UI thread).
fn request_preview(ui: &Rc<Ui>, st: &St, delay_ms: u64) {
    let (gen, path, t, badge) = {
        let mut s = st.borrow_mut();
        let Some(info) = &s.info else { return };
        let path = info.path.clone();
        let dur = info.duration;
        s.preview_gen += 1;
        let (t, badge) = match s.mode {
            Mode::Batch => (0.0, format!("{} · {}", tr("START"), splitter::fmt_ts(0.0))),
            Mode::Custom => match s.focus {
                Handle::Start => (s.start, format!("{} · {}", tr("START"), splitter::fmt_ts(s.start))),
                // The very last frame often can't be decoded; back off a little.
                Handle::End => (
                    (s.end - 0.05).max(0.0).min((dur - 0.1).max(0.0)),
                    format!("{} · {}", tr("END"), splitter::fmt_ts(s.end)),
                ),
            },
        };
        (s.preview_gen, path, t, badge)
    };
    ui.preview_badge.set_text(&badge);
    let (ui, st) = (ui.clone(), st.clone());
    glib::timeout_add_local_once(Duration::from_millis(delay_ms), move || {
        if st.borrow().preview_gen != gen {
            return;
        }
        ui.preview_spinner.start();
        ui.preview_spinner.show();
        glib::spawn_future_local(async move {
            let res = gio::spawn_blocking(move || splitter::thumbnail(&path, t, PREVIEW_W as u32, PREVIEW_H as u32)).await;
            if st.borrow().preview_gen != gen {
                return;
            }
            ui.preview_spinner.stop();
            ui.preview_spinner.hide();
            if let Ok(Ok(bytes)) = res {
                if let Some(pb) = pixbuf_from_png(&bytes) {
                    ui.preview.set_from_pixbuf(Some(&pb));
                    ui.preview.set_opacity(1.0);
                }
            }
        });
    });
}

/// Small thumbnail for the file card.
fn load_card_thumb(ui: &Rc<Ui>, path: PathBuf, t: f64) {
    let ui = ui.clone();
    glib::spawn_future_local(async move {
        if let Ok(Ok(bytes)) = gio::spawn_blocking(move || splitter::thumbnail(&path, t, 128, 72)).await {
            if let Some(pb) = pixbuf_from_png(&bytes) {
                ui.drop_thumb.set_from_pixbuf(Some(&pb));
            }
        }
    });
}

// ───────────────────────── dialogs ─────────────────────────

fn error_dialog(parent: &gtk::ApplicationWindow, title: &str, body: &str) {
    let d = gtk::MessageDialog::new(
        Some(parent),
        gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
        gtk::MessageType::Error,
        gtk::ButtonsType::Close,
        title,
    );
    d.set_secondary_text(Some(body));
    d.run();
    d.close();
}

fn confirm(parent: &gtk::ApplicationWindow, title: &str, body: &str) -> bool {
    let d = gtk::MessageDialog::new(
        Some(parent),
        gtk::DialogFlags::MODAL | gtk::DialogFlags::DESTROY_WITH_PARENT,
        gtk::MessageType::Question,
        gtk::ButtonsType::YesNo,
        title,
    );
    d.set_secondary_text(Some(body));
    let r = d.run();
    d.close();
    r == gtk::ResponseType::Yes
}

/// Ask before replacing any of `files` that already exist.
fn confirm_overwrite(ui: &Ui, files: &[PathBuf]) -> bool {
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
    confirm(
        &ui.window,
        &tr(if existing.len() == 1 { "Replace existing file?" } else { "Replace existing files?" }),
        &format!("{}\n{shown}", tr("Already in this folder:")),
    )
}

fn set_out_dir(ui: &Ui, st: &St, dir: &Path) {
    let name = dir
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| dir.display().to_string());
    ui.out_label.set_text(&name);
    ui.out_btn.set_tooltip_text(Some(&dir.display().to_string()));
    st.borrow_mut().out_dir = Some(dir.to_path_buf());
}

fn open_folder(dir: &Path) {
    let uri = gio::File::for_path(dir).uri();
    let _ = gio::AppInfo::launch_default_for_uri(&uri, None::<&gio::AppLaunchContext>);
}

fn clean_name(ui: &Ui) -> String {
    let n: String = ui.name_entry.text().trim().replace(['/', '\\'], "_");
    if n.is_empty() { "clip".into() } else { n }
}

fn part_name(stem: &str, i: usize, n: usize, ext: &str) -> String {
    let digits = n.to_string().len().max(2);
    format!("{stem}_part{:0digits$}.{ext}", i + 1)
}

/// Save dialog for the audio export; the format comes from the dropdown or the typed extension.
fn pick_audio_target(ui: &Ui, st: &St) -> Option<(AudioFormat, PathBuf)> {
    let batch = st.borrow().mode == Mode::Batch;
    let dlg = gtk::FileChooserNative::new(
        Some(&tr(if batch { "Export audio — one file per part" } else { "Export audio" })),
        Some(&ui.window),
        gtk::FileChooserAction::Save,
        Some(&tr("_Export")),
        Some(&tr("_Cancel")),
    );
    // In batch mode the chosen name is a prefix (name_part01.ext …), checked separately.
    dlg.set_do_overwrite_confirmation(!batch);
    if let Some(dir) = st.borrow().out_dir.clone() {
        dlg.set_current_folder(&dir);
    }
    let name = clean_name(ui);
    let last = st.borrow().last_audio;
    dlg.set_current_name(&format!("{name}.{}", last.ext()));
    let filters: Vec<(AudioFormat, gtk::FileFilter)> = AudioFormat::ALL
        .iter()
        .map(|f| {
            let ff = gtk::FileFilter::new();
            ff.set_name(Some(&format!("{} — {}", f.label(), tr(f.describe()))));
            ff.add_pattern(&format!("*.{}", f.ext()));
            ff.add_pattern(&format!("*.{}", f.ext().to_uppercase()));
            dlg.add_filter(ff.clone());
            (*f, ff)
        })
        .collect();
    if let Some((_, ff)) = filters.iter().find(|(f, _)| *f == last) {
        dlg.set_filter(ff);
    }
    if dlg.run() != gtk::ResponseType::Accept {
        return None;
    }
    let chosen = dlg.filename()?;
    let by_ext = chosen
        .extension()
        .map(|e| e.to_string_lossy().to_lowercase())
        .and_then(|e| AudioFormat::ALL.iter().copied().find(|f| f.ext() == e));
    let by_filter = dlg.filter().and_then(|sel| filters.iter().find(|(_, ff)| *ff == sel).map(|(f, _)| *f));
    // GTK 3 doesn't rename the file when another format is picked in the dropdown, so a
    // changed dropdown wins; otherwise the typed extension decides.
    let fmt = match (by_filter, by_ext) {
        (Some(f), _) if f != last => f,
        (_, Some(e)) => e,
        (Some(f), None) => f,
        (None, None) => last,
    };
    let path = if by_ext.is_some() {
        chosen.with_extension(fmt.ext())
    } else {
        let mut os = chosen.clone().into_os_string();
        os.push(format!(".{}", fmt.ext()));
        PathBuf::from(os)
    };
    if !batch && path != chosen && !confirm_overwrite(ui, &[path.clone()]) {
        return None;
    }
    st.borrow_mut().last_audio = fmt;
    Some((fmt, path))
}

// ───────────────────────── behaviour ─────────────────────────

/// Translate the engine's job titles ("Parallel chunk 1/2", "Part 03 · MP3", "MP3 audio", …).
fn loc_title(t: &str) -> String {
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

/// Re-apply texts that are computed at runtime after the language changes.
fn language_changed(ui: &Rc<Ui>, st: &St) {
    let (loaded, has_audio) = {
        let s = st.borrow();
        (s.info.is_some(), s.info.as_ref().map(|i| !i.acodec.is_empty()).unwrap_or(false))
    };
    if !loaded {
        ui.drop_title.set_text(&tr("Drop a video here…"));
        ui.preview_badge.set_text(&tr("NO VIDEO"));
    } else if !has_audio {
        ui.audio_btn.set_tooltip_text(Some(&tr("This video has no audio track")));
    }
    if st.borrow().running {
        ui.go_label.set_text(&tr("Cancel"));
    }
    mode_changed(ui, st); // main button, tooltips and the preview badge
}

fn update_auto_name(ui: &Ui, st: &St) {
    let name = {
        let s = st.borrow();
        if !s.name_auto {
            return;
        }
        let Some(info) = &s.info else { return };
        let stem = info.path.file_stem().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "clip".into());
        match s.mode {
            Mode::Batch => stem,
            Mode::Custom => format!(
                "{stem}_{}-{}",
                splitter::fmt_ts(s.start)[..8].replace(':', "."),
                splitter::fmt_ts(s.end)[..8].replace(':', ".")
            ),
        }
    };
    if ui.name_entry.text() == name {
        return;
    }
    st.borrow_mut().setting_name = true;
    ui.name_entry.set_text(&name);
    st.borrow_mut().setting_name = false;
}

fn load_file(ui: &Rc<Ui>, st: &St, path: &Path) {
    ui.status.set_text(&tr("Reading video information…"));
    let info = match splitter::probe(path) {
        Ok(i) => i,
        Err(e) => {
            ui.status.set_text("");
            error_dialog(&ui.window, &tr("Can't open this video"), &e);
            return;
        }
    };
    let name = path.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    ui.drop_title.set_text(&name);
    ui.drop_sub.set_text(&path.parent().map(|p| p.display().to_string()).unwrap_or_default());
    ui.drop_sub.show();
    ui.drop_btn.style_context().add_class("loaded");
    for c in ui.chips.children() {
        ui.chips.remove(&c);
    }
    let mut tags = vec![splitter::fmt_time(info.duration)];
    if info.width > 0 {
        tags.push(format!("{}×{}", info.width, info.height));
    }
    tags.push(info.vcodec.to_uppercase());
    if !info.acodec.is_empty() {
        tags.push(info.acodec.to_uppercase());
    }
    tags.push(splitter::fmt_size(info.size_bytes));
    for t in tags {
        ui.chips.pack_start(&chip(&t), false, false, 0);
    }
    ui.chips.show_all();

    if !st.borrow().out_dir_user_set {
        if let Some(parent) = path.parent() {
            set_out_dir(ui, st, parent);
        }
    }
    for c in ui.list.children() {
        ui.list.remove(&c);
    }
    ui.overall.set_fraction(0.0);
    ui.overall.style_context().remove_class("done");
    ui.overall.style_context().remove_class("failed");
    ui.percent.set_text("0%");
    ui.status.set_text("");

    let dur = info.duration;
    let has_audio = !info.acodec.is_empty();
    load_card_thumb(ui, info.path.clone(), (dur * 0.1).min(30.0));
    {
        let mut s = st.borrow_mut();
        s.info = Some(info);
        s.start = 0.0;
        s.end = dur;
        s.focus = Handle::Start;
        s.name_auto = true;
    }
    ui.start_entry.set_sensitive(true);
    ui.end_entry.set_sensitive(true);
    ui.audio_btn.set_sensitive(has_audio);
    if !has_audio {
        ui.audio_btn.set_tooltip_text(Some(&tr("This video has no audio track")));
    }
    range_changed(ui, st);
    batch_changed(ui, st);
    mode_changed(ui, st);
}

fn set_running_ui(ui: &Ui, st: &St, running: bool) {
    let ctx = ui.go_btn.style_context();
    ui.go_btn.set_sensitive(true);
    if running {
        ctx.add_class("cancel");
        ui.go_icon.set_from_icon_name(Some("vs-process-stop-symbolic"), gtk::IconSize::Button);
        ui.go_icon.set_pixel_size(18);
        ui.go_label.set_text(&tr("Cancel"));
    } else {
        ctx.remove_class("cancel");
        refresh_go(ui, st);
    }
    let has_audio = st.borrow().info.as_ref().map(|i| !i.acodec.is_empty()).unwrap_or(false);
    ui.audio_btn.set_sensitive(!running && has_audio);
    for w in [
        ui.drop_btn.upcast_ref::<gtk::Widget>(),
        ui.mode_stack.upcast_ref(),
        ui.start_entry.upcast_ref(),
        ui.end_entry.upcast_ref(),
        ui.out_btn.upcast_ref(),
        ui.name_entry.upcast_ref(),
        ui.workers_spin.upcast_ref(),
    ] {
        w.set_sensitive(!running);
    }
}

/// `audio == None` exports video (one clip, or batch parts); `Some` exports audio only.
fn start(ui: &Rc<Ui>, st: &St, audio: Option<(AudioFormat, PathBuf)>) {
    let (info, mode, start_t, end_t, bounds) = {
        let s = st.borrow();
        let Some(info) = s.info.clone() else { return };
        (info, s.mode, s.start, s.end, s.batch.clone())
    };
    if mode == Mode::Custom && end_t - start_t < MIN_CLIP {
        error_dialog(&ui.window, &tr("The clip is too short"), &tr("Choose an end time after the start time."));
        return;
    }
    if mode == Mode::Batch && bounds.is_empty() {
        error_dialog(&ui.window, &tr("Nothing to split"), &tr("Choose a part length or a number of parts."));
        return;
    }
    let Some(out_dir) = st.borrow().out_dir.clone() else {
        error_dialog(&ui.window, &tr("No output folder"), &tr("Please choose where the files should be saved."));
        return;
    };
    if let Err(e) = std::fs::create_dir_all(&out_dir) {
        error_dialog(&ui.window, &tr("Output folder is not writable"), &e.to_string());
        return;
    }
    let name = clean_name(ui);
    let n = bounds.len();

    let mut output = None;
    let mut parts = Vec::new();
    let mut audio_exports = Vec::new();
    let mut ask_overwrite = true;
    match (&audio, mode) {
        (None, Mode::Custom) => output = Some(out_dir.join(format!("{name}.mp4"))),
        (None, Mode::Batch) => {
            parts = bounds.iter().enumerate().map(|(i, (s, l))| (*s, *l, out_dir.join(part_name(&name, i, n, "mp4")))).collect()
        }
        (Some((fmt, path)), Mode::Custom) => {
            audio_exports.push((*fmt, start_t, end_t - start_t, path.clone()));
            ask_overwrite = false; // the save dialog already asked
        }
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
        output: output.clone(),
        parts: parts.clone(),
        audio_exports: audio_exports.clone(),
        has_audio: false,
        fps: 0.0,
        tmp_dir: PathBuf::new(),
    };
    let files = splitter::outputs(&job_preview);
    if files.iter().any(|p| *p == info.path) {
        error_dialog(&ui.window, &tr("Choose another file name"), &tr("The export would overwrite the original video."));
        return;
    }
    if ask_overwrite && !confirm_overwrite(ui, &files) {
        return;
    }
    let work_dir = files.first().and_then(|p| p.parent()).map(Path::to_path_buf).unwrap_or_else(|| out_dir.clone());

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    let job = CutJob {
        workers: ui.workers_spin.value_as_int().max(1) as usize,
        has_audio: !info.acodec.is_empty(),
        fps: info.fps,
        tmp_dir: work_dir.join(format!(".chop-chop-{}-{stamp}", std::process::id())),
        ..job_preview
    };
    let tasks = splitter::plan_tasks(&job);

    // Job list
    for c in ui.list.children() {
        ui.list.remove(&c);
    }
    let mut rows = Vec::new();
    for t in &tasks {
        let row = gtk::Box::new(gtk::Orientation::Horizontal, 10);
        let ic_name = match t.kind {
            TaskKind::Audio { .. } | TaskKind::AudioExport { .. } => "vs-audio-x-generic-symbolic",
            TaskKind::Join => "vs-view-grid-symbolic",
            _ => "vs-video-x-generic-symbolic",
        };
        row.pack_start(&accent_icon(ic_name, 16), false, false, 0);
        let text = gtk::Box::new(gtk::Orientation::Vertical, 2);
        text.pack_start(&label(&loc_title(&t.title), "vs-part-name"), false, false, 0);
        text.pack_start(&label(&tr(&t.detail), "vs-mono"), false, false, 0);
        text.set_size_request(150, -1);
        row.pack_start(&text, false, false, 0);
        let bar = gtk::ProgressBar::new();
        bar.set_valign(gtk::Align::Center);
        row.pack_start(&bar, true, true, 0);
        let state = label(&tr("Queued"), "vs-hint");
        state.set_width_chars(7);
        state.set_xalign(1.0);
        row.pack_start(&state, false, false, 0);
        let status_icon = icon("vs-timer-short-symbolic", 14);
        status_icon.set_opacity(0.4);
        row.pack_start(&status_icon, false, false, 0);
        ui.list.insert(&row, -1);
        rows.push(Row { bar, icon: status_icon, state, frac: 0.0, weight: t.weight });
    }
    ui.list.show_all();

    let cancel = Arc::new(AtomicBool::new(false));
    {
        let mut s = st.borrow_mut();
        s.rows = rows;
        s.running = true;
        s.cancel = Some(cancel.clone());
        s.started = Some(Instant::now());
    }
    ui.overall.set_fraction(0.0);
    let oc = ui.overall.style_context();
    oc.remove_class("done");
    oc.remove_class("failed");
    ui.percent.set_text("0%");
    let workers = job.workers;
    let chunks = tasks.iter().filter(|t| matches!(t.kind, TaskKind::VideoChunk { .. })).count();
    let w = workers.min(n).to_string();
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
    let encoder_note = if audio.is_none() { format!(" · {}", splitter::video_encoder().label()) } else { String::new() };
    status.push_str(&encoder_note);
    ui.status.set_text(&status);
    set_running_ui(ui, st, true);

    let (tx, rx) = mpsc::channel::<Msg>();
    splitter::run(job, tasks, tx, cancel);

    let (ui, st) = (ui.clone(), st.clone());
    glib::timeout_add_local(Duration::from_millis(80), move || {
        let mut finished = None;
        while let Ok(msg) = rx.try_recv() {
            let mut s = st.borrow_mut();
            match msg {
                Msg::Started(i) => {
                    let r = &s.rows[i];
                    r.state.set_text(&tr("Working"));
                    r.icon.set_from_icon_name(Some("vs-media-playback-start-symbolic"), gtk::IconSize::Button);
                    r.icon.set_pixel_size(14);
                    r.icon.set_opacity(1.0);
                    r.icon.style_context().add_class("vs-icon");
                }
                Msg::Progress(i, f) => {
                    let r = &mut s.rows[i];
                    r.frac = f;
                    r.bar.set_fraction(f);
                    r.state.set_text(&format!("{:.0}%", f * 100.0));
                }
                Msg::Finished(i, res) => {
                    let r = &mut s.rows[i];
                    let ic = r.icon.style_context();
                    ic.remove_class("vs-icon");
                    match &res {
                        Ok(()) => {
                            r.frac = 1.0;
                            r.bar.set_fraction(1.0);
                            r.bar.style_context().add_class("done");
                            r.state.set_text(&tr("Done"));
                            r.icon.set_from_icon_name(Some("vs-emblem-ok-symbolic"), gtk::IconSize::Button);
                            ic.add_class("vs-ok");
                        }
                        Err(e) => {
                            r.bar.style_context().add_class("failed");
                            r.state.set_text(&tr(if e == "Cancelled" { "Stopped" } else { "Failed" }));
                            r.state.set_tooltip_text(Some(e));
                            r.icon.set_from_icon_name(Some("vs-dialog-error-symbolic"), gtk::IconSize::Button);
                            r.icon.set_tooltip_text(Some(e));
                            ic.add_class("vs-err");
                        }
                    }
                    r.icon.set_pixel_size(14);
                }
                Msg::AllDone { cancelled, result } => finished = Some((cancelled, result)),
            }
        }

        let s = st.borrow();
        let total: f64 = s.rows.iter().map(|r| r.weight).sum();
        let done: f64 = s.rows.iter().map(|r| r.frac * r.weight).sum();
        let frac = if total > 0.0 { (done / total).clamp(0.0, 1.0) } else { 0.0 };
        ui.overall.set_fraction(frac);
        ui.percent.set_text(&format!("{:.0}%", frac * 100.0));
        let elapsed = s.started.map(|t| t.elapsed().as_secs_f64()).unwrap_or(0.0);
        drop(s);

        if let Some((cancelled, result)) = finished {
            {
                let mut s = st.borrow_mut();
                s.running = false;
                s.cancel = None;
            }
            set_running_ui(&ui, &st, false);
            match result {
                Ok(paths) => {
                    ui.overall.style_context().add_class("done");
                    ui.overall.set_fraction(1.0);
                    ui.percent.set_text("100%");
                    let size: u64 = paths.iter().map(|p| std::fs::metadata(p).map(|m| m.len()).unwrap_or(0)).sum();
                    let what = if paths.len() == 1 {
                        format!("“{}”", paths[0].file_name().unwrap_or_default().to_string_lossy())
                    } else {
                        trf("{n} files", &[("n", paths.len().to_string())])
                    };
                    ui.status.set_text(&trf(
                        "Saved {what} · {size} · done in {secs} s",
                        &[("what", what), ("size", splitter::fmt_size(size)), ("secs", format!("{elapsed:.1}"))],
                    ));
                    if ui.open_when_done.is_active() {
                        if let Some(d) = paths.first().and_then(|p| p.parent()) {
                            open_folder(d);
                        }
                    }
                }
                Err(_) if cancelled => ui.status.set_text(&tr("Cancelled — no files were kept.")),
                Err(e) => {
                    ui.overall.style_context().add_class("failed");
                    ui.status.set_text(&trf("Failed: {e}", &[("e", e)]));
                }
            }
            return glib::ControlFlow::Break;
        }
        if frac > 0.02 && elapsed > 1.0 {
            let eta = elapsed / frac * (1.0 - frac);
            let eta_text = trf(
                "{t} elapsed · about {left} left",
                &[("t", splitter::fmt_time(elapsed)), ("left", splitter::fmt_time(eta))],
            );
            ui.status.set_text(&format!("{eta_text}{encoder_note}"));
        }
        glib::ControlFlow::Continue
    });
}
