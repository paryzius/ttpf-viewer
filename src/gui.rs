//! The app window: texture gallery, large preview with mip levels, and PNG export.

use crate::archive::{self, Archive, Progress, Status};
use crate::bcn::Rgba;
use eframe::egui::{
    self, Align2, Color32, ColorImage, FontId, Key, Rect, RichText, Sense, Stroke, StrokeKind, TextureFilter, TextureHandle,
    TextureOptions, TextureWrapMode, Vec2, pos2, vec2,
};
use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

const THUMB: usize = 256;
const MIP_THUMB: usize = 96;
const MAX_THUMBS: usize = 900;

pub fn run(file: Option<PathBuf>) -> Result<(), String> {
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1400.0, 900.0])
            .with_min_inner_size([800.0, 500.0])
            .with_title("TTPF Viewer")
            .with_drag_and_drop(true),
        ..Default::default()
    };
    eframe::run_native("TTPF Viewer", options, Box::new(move |_cc| Ok(Box::new(App::new(file)))))
        .map_err(|e| e.to_string())
}

// ------------------------------------------------------------------ background work

type Job = Box<dyn FnOnce() + Send>;

/// Worker threads that run the most recently requested job first (what's on screen now).
struct Pool {
    jobs: Mutex<Vec<Job>>,
    cv: Condvar,
}

impl Pool {
    fn new(threads: usize) -> Arc<Pool> {
        let p = Arc::new(Pool { jobs: Mutex::new(Vec::new()), cv: Condvar::new() });
        for _ in 0..threads {
            let p = p.clone();
            std::thread::spawn(move || loop {
                let job = {
                    let mut j = p.jobs.lock().unwrap();
                    while j.is_empty() {
                        j = p.cv.wait(j).unwrap();
                    }
                    j.pop().unwrap()
                };
                job();
            });
        }
        p
    }
    fn push(&self, job: Job) {
        self.jobs.lock().unwrap().push(job);
        self.cv.notify_one();
    }
    fn clear(&self) {
        self.jobs.lock().unwrap().clear();
    }
}

/// (texture, mip level)
/// (texture, face / slice, mip level)
type Key2 = (usize, usize, usize);

/// Gallery order.
#[derive(Clone, Copy, PartialEq)]
enum Sort {
    Number,
    Size,
    Format,
}

/// Decoded images; the last field is the HDR setting generation they were made with.
enum Done {
    Thumb(Key2, Result<ColorImage, String>, u32),
    Full(Key2, Result<ColorImage, String>, u32),
}

fn to_color_image(img: &Rgba) -> ColorImage {
    ColorImage::from_rgba_unmultiplied([img.w, img.h], &img.px)
}

struct Export {
    total: usize,
    done: Arc<AtomicUsize>,
    result: Arc<Mutex<Option<Result<(usize, usize), String>>>>,
    dir: PathBuf,
}

/// An entry of the folder sidebar.
struct Entry {
    path: PathBuf,
    dir: bool,
    size: u64,
}

struct Loading {
    progress: Arc<Progress>,
    result: Arc<Mutex<Option<Result<Arc<Archive>, String>>>>,
}

pub struct App {
    pool: Arc<Pool>,
    done: Arc<Mutex<Vec<Done>>>,
    arc: Option<Arc<Archive>>,
    loading: Option<Loading>,
    error: Option<String>,
    to_open: Option<PathBuf>,
    // textures on the GPU
    thumbs: HashMap<Key2, (TextureHandle, u64)>,
    pending: HashSet<Key2>,
    failed: HashMap<Key2, String>,
    full: Option<(Key2, TextureHandle)>,
    full_pending: Option<Key2>,
    checker: Option<TextureHandle>,
    frame: u64,
    // view
    rows: Vec<usize>,
    unavailable: usize,
    selected: BTreeSet<usize>,
    focus: Option<usize>,
    /// mip level shown in the preview (None = the best available)
    level: Option<usize>,
    anchor: Option<usize>,
    scroll_to_focus: bool,
    show_unavailable: bool,
    search: String,
    tsize: f32,
    /// extract as .dds (original data) instead of .png
    as_dds: bool,
    /// face / slice shown for a cube map or texture array
    layer: usize,
    sort: Sort,
    descending: bool,
    /// bumped when the HDR display setting changes, so images decoded with the old one are dropped
    hdr_gen: u32,
    cols: usize,
    scroll_y: f32,
    // preview zoom/pan
    zoom: f32,
    pan: Vec2,
    refit: bool,
    export: Option<Export>,
    /// folder shown in the sidebar, and the listing of every folder opened in it
    folder: Option<PathBuf>,
    listing: HashMap<PathBuf, Vec<Entry>>,
    open_path: Option<PathBuf>,
    message: Option<(String, Option<PathBuf>)>,
    /// Developer test hook (env TTPF_VIEWER_TEST="out.png;select=12,level=1,unavailable,light,size=200"): once
    /// everything has loaded, save the app's own rendered frame to out.png and quit.
    test: Option<(PathBuf, Vec<String>, u64, bool)>,
}

impl App {
    fn new(file: Option<PathBuf>) -> App {
        let threads = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4).clamp(2, 16);
        App {
            pool: Pool::new(threads),
            done: Arc::new(Mutex::new(Vec::new())),
            arc: None,
            loading: None,
            error: None,
            to_open: file,
            thumbs: HashMap::new(),
            pending: HashSet::new(),
            failed: HashMap::new(),
            full: None,
            full_pending: None,
            checker: None,
            frame: 0,
            rows: Vec::new(),
            unavailable: 0,
            selected: BTreeSet::new(),
            focus: None,
            level: None,
            anchor: None,
            scroll_to_focus: false,
            show_unavailable: false,
            search: String::new(),
            tsize: 160.0,
            as_dds: false,
            layer: 0,
            sort: Sort::Number,
            descending: false,
            hdr_gen: 0,
            cols: 1,
            scroll_y: 0.0,
            zoom: 1.0,
            pan: Vec2::ZERO,
            refit: true,
            export: None,
            folder: None,
            listing: HashMap::new(),
            open_path: None,
            message: None,
            test: std::env::var("TTPF_VIEWER_TEST").ok().map(|v| {
                let (out, acts) = v.split_once(';').unwrap_or((&v, ""));
                (PathBuf::from(out), acts.split(',').filter(|a| !a.is_empty()).map(String::from).collect(), 0, false)
            }),
        }
    }

    fn open(&mut self, path: PathBuf, ctx: &egui::Context) {
        if !archive::is_ttpf_name(&path) {
            self.message = Some((format!("{} is not a .ttpf file.", path.file_name().unwrap_or_default().to_string_lossy()), None));
            return;
        }
        self.pool.clear();
        self.arc = None;
        self.error = None;
        self.thumbs.clear();
        self.pending.clear();
        self.failed.clear();
        self.full = None;
        self.full_pending = None;
        self.rows.clear();
        self.selected.clear();
        self.focus = None;
        self.level = None;
        self.layer = 0;
        self.message = None;
        self.open_path = Some(path.clone());
        let progress = Arc::new(Progress::default());
        let result = Arc::new(Mutex::new(None));
        self.loading = Some(Loading { progress: progress.clone(), result: result.clone() });
        let ctx = ctx.clone();
        std::thread::spawn(move || {
            let r = Archive::open(&path, &progress).map(Arc::new);
            *result.lock().unwrap() = Some(r);
            ctx.request_repaint();
        });
    }

    fn poll(&mut self, ctx: &egui::Context) {
        self.frame += 1;
        if let Some(p) = self.to_open.take() {
            self.open(p, ctx);
        }
        let dropped: Vec<PathBuf> = ctx.input(|i| i.raw.dropped_files.iter().map(|f| f.path().to_path_buf()).collect());
        if let Some(p) = dropped.into_iter().find(|p| !p.as_os_str().is_empty()) {
            if p.is_dir() {
                self.set_folder(p);
            } else {
                self.open(p, ctx);
            }
        }
        let loaded = self.loading.as_ref().and_then(|l| l.result.lock().unwrap().take());
        if let Some(r) = loaded {
            match r {
                Ok(a) => self.arc = Some(a),
                Err(e) => self.error = Some(e),
            }
            self.loading = None;
        }
        for d in std::mem::take(&mut *self.done.lock().unwrap()) {
            match d {
                Done::Thumb(_, _, g) | Done::Full(_, _, g) if g != self.hdr_gen => {}
                Done::Thumb(k, r, _) => {
                    self.pending.remove(&k);
                    match r {
                        Ok(img) => {
                            let t = ctx.load_texture(format!("t{}_{}_{}", k.0, k.1, k.2), img, texture_options());
                            self.thumbs.insert(k, (t, self.frame));
                        }
                        Err(e) => {
                            self.failed.insert(k, e);
                        }
                    }
                }
                Done::Full(k, r, _) => {
                    if self.full_pending == Some(k) {
                        self.full_pending = None;
                        match r {
                            Ok(img) => {
                                self.full = Some((k, ctx.load_texture("full", img, texture_options())));
                                self.refit = true;
                            }
                            Err(e) => {
                                self.failed.insert(k, e);
                            }
                        }
                    }
                }
            }
        }
        if self.thumbs.len() > MAX_THUMBS {
            let mut by_age: Vec<(u64, Key2)> = self.thumbs.iter().map(|(k, (_, f))| (*f, *k)).collect();
            by_age.sort_by_key(|x| x.0);
            for (_, k) in by_age.into_iter().take(self.thumbs.len() - MAX_THUMBS * 3 / 4) {
                self.thumbs.remove(&k);
            }
        }
        let finished = self.export.as_ref().and_then(|e| e.result.lock().unwrap().take().map(|r| (r, e.dir.clone())));
        if let Some((r, dir)) = finished {
            self.message = Some(match r {
                Ok((n, skipped)) => {
                    let extra = if skipped > 0 { format!(" ({skipped} unreadable skipped)") } else { String::new() };
                    (format!("Saved {n} texture{} to {}{extra}", if n == 1 { "" } else { "s" }, dir.display()), Some(dir))
                }
                Err(err) => (format!("Export failed: {err}"), None),
            });
            self.export = None;
        }
        self.refresh_rows();
        self.test_hook(ctx);
        if self.loading.is_some() || self.export.is_some() || !self.pending.is_empty() || self.full_pending.is_some() {
            ctx.request_repaint_after(Duration::from_millis(150));
        }
    }

    fn refresh_rows(&mut self) {
        let Some(arc) = &self.arc else { return };
        let q = self.search.trim().trim_start_matches('#').to_lowercase();
        self.unavailable = arc.textures.iter().filter(|t| matches!(t.status, Status::Unavailable(_))).count();
        self.rows = arc
            .textures
            .iter()
            .filter(|t| self.show_unavailable || !matches!(t.status, Status::Unavailable(_)))
            .filter(|t| {
                q.is_empty() || t.index.to_string() == q || format!("{}x{} {}", t.w, t.h, t.format_name()).to_lowercase().contains(&q)
            })
            .map(|t| t.index)
            .collect();
        let t = &arc.textures;
        match self.sort {
            Sort::Number => {}
            Sort::Size => self.rows.sort_by_key(|&i| (t[i].w * t[i].h, t[i].w, i)),
            Sort::Format => self.rows.sort_by_cached_key(|&i| (t[i].format_name(), i)),
        }
        if self.descending {
            self.rows.reverse();
        }
    }

    fn request_thumb(&mut self, k: Key2, size: usize) {
        if self.thumbs.contains_key(&k) || self.pending.contains(&k) || self.failed.contains_key(&k) {
            return;
        }
        let Some(arc) = self.arc.clone() else { return };
        self.pending.insert(k);
        let (done, g) = (self.done.clone(), self.hdr_gen);
        self.pool.push(Box::new(move || {
            let r = arc.image_in(k.0, k.1, k.2).map(|img| to_color_image(&img.thumbnail(size))).map_err(|p| p.text().to_string());
            done.lock().unwrap().push(Done::Thumb(k, r, g));
        }));
    }

    fn request_full(&mut self, k: Key2) {
        if self.full.as_ref().is_some_and(|f| f.0 == k) || self.full_pending == Some(k) || self.failed.contains_key(&k) {
            return;
        }
        let Some(arc) = self.arc.clone() else { return };
        self.full_pending = Some(k);
        let (done, g) = (self.done.clone(), self.hdr_gen);
        self.pool.push(Box::new(move || {
            let r = arc
                .image_in(k.0, k.1, k.2)
                .map(|img| to_color_image(&if img.w.max(img.h) > 8192 { img.thumbnail(8192) } else { img }))
                .map_err(|p| p.text().to_string());
            done.lock().unwrap().push(Done::Full(k, r, g));
        }));
    }

    fn select_click(&mut self, t: usize, ctrl: bool, shift: bool) {
        if shift && self.anchor.is_some() {
            let a = self.rows.iter().position(|&r| Some(r) == self.anchor);
            let b = self.rows.iter().position(|&r| r == t);
            if let (Some(a), Some(b)) = (a, b) {
                for &r in &self.rows[a.min(b)..=a.max(b)] {
                    self.selected.insert(r);
                }
            }
        } else if ctrl {
            if !self.selected.remove(&t) {
                self.selected.insert(t);
            }
            self.anchor = Some(t);
        } else {
            self.selected.clear();
            self.selected.insert(t);
            self.anchor = Some(t);
        }
        if self.focus != Some(t) {
            self.focus = Some(t);
            self.level = None;
            self.layer = 0;
            self.refit = true;
        }
    }

    fn start_export(&mut self, list: Vec<usize>) {
        let Some(arc) = self.arc.clone() else { return };
        if list.is_empty() {
            return;
        }
        let dds = self.as_dds;
        let Some(dir) = rfd::FileDialog::new().set_title(if dds { "Choose a folder for the DDS files" } else { "Choose a folder for the PNGs" }).pick_folder() else { return };
        let dir = dir.join(&arc.stem);
        let done = Arc::new(AtomicUsize::new(0));
        let result = Arc::new(Mutex::new(None));
        self.export = Some(Export { total: list.len(), done: done.clone(), result: result.clone(), dir: dir.clone() });
        self.message = None;
        std::thread::spawn(move || {
            let r = archive::export(&arc, &list, &dir, dds, &done);
            *result.lock().unwrap() = Some(r);
        });
    }

    fn save_one(&mut self, t: usize, layer: usize, level: usize) {
        let Some(arc) = self.arc.clone() else { return };
        let sfx = arc.textures[t].layer_suffix(layer);
        let name = if level == 0 { format!("{t}{sfx}.png") } else { format!("{t}{sfx}_mip{level}.png") };
        let Some(path) = rfd::FileDialog::new().set_file_name(&name).add_filter("PNG image", &["png"]).save_file() else { return };
        self.message = Some(match arc.image_in(t, layer, level).map_err(|p| p.text().to_string()).and_then(|img| archive::save_png(&img, &path)) {
            Ok(()) => (format!("Saved {}", path.display()), path.parent().map(|p| p.to_path_buf())),
            Err(e) => (format!("Couldn't save: {e}"), None),
        });
    }

    fn save_dds(&mut self, t: usize) {
        let Some(arc) = self.arc.clone() else { return };
        let Some(path) = rfd::FileDialog::new().set_file_name(format!("{t}.dds")).add_filter("DDS texture", &["dds"]).save_file() else { return };
        self.message = Some(match arc.dds(t).map_err(|p| p.text().to_string()).and_then(|d| std::fs::write(&path, d).map_err(|e| e.to_string())) {
            Ok(()) => (format!("Saved {}", path.display()), path.parent().map(|p| p.to_path_buf())),
            Err(e) => (format!("Couldn't save: {e}"), None),
        });
    }

    fn set_folder(&mut self, dir: PathBuf) {
        self.listing.clear();
        self.folder = Some(dir);
    }

    /// Subfolders and .ttpf files of `dir` (read once, then cached).
    fn list_dir(&mut self, dir: &PathBuf) -> &Vec<Entry> {
        self.listing.entry(dir.clone()).or_insert_with(|| {
            let mut v: Vec<Entry> = std::fs::read_dir(dir)
                .map(|rd| {
                    rd.filter_map(|e| e.ok())
                        .filter_map(|e| {
                            let path = e.path();
                            let md = e.metadata().ok()?;
                            (md.is_dir() || archive::is_ttpf_name(&path)).then(|| Entry { path, dir: md.is_dir(), size: md.len() })
                        })
                        .collect()
                })
                .unwrap_or_default();
            v.sort_by(|a, b| b.dir.cmp(&a.dir).then_with(|| a.path.cmp(&b.path)));
            v
        })
    }

    fn tree(&mut self, ui: &mut egui::Ui, dir: PathBuf, ctx: &egui::Context) {
        let entries: Vec<(PathBuf, bool, u64)> = self.list_dir(&dir).iter().map(|e| (e.path.clone(), e.dir, e.size)).collect();
        if entries.is_empty() {
            ui.label(RichText::new("no .ttpf files").color(ui.visuals().weak_text_color()));
        }
        for (path, is_dir, size) in entries {
            let name = path.file_name().unwrap_or_default().to_string_lossy().into_owned();
            if is_dir {
                egui::CollapsingHeader::new(name).id_salt(&path).show(ui, |ui| self.tree(ui, path.clone(), ctx));
            } else {
                let open = self.open_path.as_ref() == Some(&path);
                let label = format!("{name}   {:.0} MB", size as f64 / 1048576.0);
                if ui.selectable_label(open, label).clicked() && !open {
                    self.open(path, ctx);
                }
            }
        }
    }

    fn pick_file(&mut self) {
        if let Some(p) = rfd::FileDialog::new().add_filter("TTPF archive", &["ttpf"]).pick_file() {
            self.to_open = Some(p);
        }
    }

    fn test_hook(&mut self, ctx: &egui::Context) {
        let Some((out, acts, settled, sent)) = self.test.clone() else { return };
        if sent || self.loading.is_some() {
            ctx.request_repaint_after(Duration::from_millis(100));
            return;
        }
        if settled == 0 {
            for a in &acts {
                match a.split_once('=') {
                    Some(("select", n)) => {
                        if let Ok(t) = n.parse() {
                            self.select_click(t, false, false);
                            self.scroll_to_focus = true;
                        }
                    }
                    Some(("level", n)) => self.level = n.parse().ok(),
                    Some(("layer", n)) => self.layer = n.parse().unwrap_or(0),
                    Some(("folder", d)) => self.set_folder(PathBuf::from(d)),
                    Some(("size", n)) => self.tsize = n.parse().unwrap_or(self.tsize),
                    _ if a == "unavailable" => self.show_unavailable = true,
                    _ if a == "light" => ctx.set_theme(egui::Theme::Light),
                    _ if a == "desc" => self.descending = true,
                    _ if a == "clip" => crate::bcn::TONEMAP.store(false, Ordering::Relaxed),
                    Some(("sort", v)) => {
                        self.sort = match v {
                            "size" => Sort::Size,
                            "format" => Sort::Format,
                            _ => Sort::Number,
                        }
                    }
                    _ => {}
                }
            }
        }
        let busy = !self.pending.is_empty() || self.full_pending.is_some();
        let settled = if busy { 1 } else { settled + 1 };
        self.test.as_mut().unwrap().2 = settled;
        if settled > 20 {
            self.test.as_mut().unwrap().3 = true;
            let c = ctx.clone();
            ctx.send_viewport_cmd(egui::ViewportCommand::Screenshot(egui::ScreenshotCallback::new(move |img| {
                let px: Vec<u8> = img.pixels.iter().flat_map(|p| p.to_array()).collect();
                let _ = image::save_buffer(&out, &px, img.size[0] as u32, img.size[1] as u32, image::ColorType::Rgba8);
                c.send_viewport_cmd(egui::ViewportCommand::Close);
                c.request_repaint();
            })));
        }
        ctx.request_repaint_after(Duration::from_millis(50));
    }
}

fn texture_options() -> TextureOptions {
    TextureOptions {
        magnification: TextureFilter::Nearest,
        minification: TextureFilter::Linear,
        wrap_mode: TextureWrapMode::ClampToEdge,
        mipmap_mode: Some(TextureFilter::Linear),
    }
}

fn open_folder(dir: &std::path::Path) {
    #[cfg(windows)]
    let cmd = "explorer";
    #[cfg(target_os = "macos")]
    let cmd = "open";
    #[cfg(all(unix, not(target_os = "macos")))]
    let cmd = "xdg-open";
    let _ = std::process::Command::new(cmd).arg(dir).spawn();
}

/// Rectangle of size `img` scaled to fit inside `area`, centred.
fn fit_rect(area: Rect, img: Vec2) -> Rect {
    let s = (area.width() / img.x).min(area.height() / img.y);
    Rect::from_center_size(area.center(), img * s)
}

const FULL_UV: Rect = Rect { min: pos2(0.0, 0.0), max: pos2(1.0, 1.0) };

// ------------------------------------------------------------------ drawing

impl eframe::App for App {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        if self.checker.is_none() {
            let mut img = ColorImage::new([16, 16], vec![Color32::from_gray(150); 256]);
            for y in 0..16 {
                for x in 0..16 {
                    if (x / 8 + y / 8) % 2 == 0 {
                        img[(x, y)] = Color32::from_gray(185);
                    }
                }
            }
            let opts = TextureOptions { wrap_mode: TextureWrapMode::Repeat, ..TextureOptions::NEAREST };
            self.checker = Some(ctx.load_texture("checker", img, opts));
        }
        self.poll(&ctx);
        self.keys(&ctx);
        egui::Panel::top("toolbar").show(ui, |ui| self.toolbar(ui));
        egui::Panel::bottom("status").show(ui, |ui| self.status_bar(ui));
        if let Some(root) = self.folder.clone() {
            egui::Panel::left("folder").resizable(true).default_size(260.0).min_size(160.0).show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(root.file_name().unwrap_or_default().to_string_lossy()).strong());
                    ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                        if ui.small_button("Close").clicked() {
                            self.folder = None;
                        }
                        if ui.small_button("Refresh").clicked() {
                            self.listing.clear();
                        }
                    });
                });
                ui.separator();
                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| self.tree(ui, root.clone(), &ctx));
            });
        }
        if self.focus.is_some() && self.arc.is_some() {
            egui::Panel::right("preview").resizable(true).default_size(540.0).min_size(340.0).show(ui, |ui| self.preview(ui));
        }
        egui::CentralPanel::default_margins().show(ui, |ui| self.gallery(ui));
    }
}

impl App {
    fn checker(&self, p: &egui::Painter, r: Rect) {
        if let Some(ch) = &self.checker {
            p.image(ch.id(), r, Rect::from_min_max(pos2(0.0, 0.0), pos2(r.width() / 16.0, r.height() / 16.0)), Color32::WHITE);
        }
    }

    fn keys(&mut self, ctx: &egui::Context) {
        if ctx.egui_wants_keyboard_input() {
            return;
        }
        let (left, right, up, down, all, esc) = ctx.input(|i| {
            (
                i.key_pressed(Key::ArrowLeft),
                i.key_pressed(Key::ArrowRight),
                i.key_pressed(Key::ArrowUp),
                i.key_pressed(Key::ArrowDown),
                i.modifiers.command && i.key_pressed(Key::A),
                i.key_pressed(Key::Escape),
            )
        });
        if all {
            self.selected = self.rows.iter().copied().collect();
        }
        if esc {
            self.selected.clear();
            self.focus = None;
        }
        let cols = self.cols.max(1) as isize;
        let step = if left { -1 } else if right { 1 } else if up { -cols } else if down { cols } else { 0 };
        if step != 0 && !self.rows.is_empty() {
            let i = self.focus.and_then(|t| self.rows.iter().position(|&r| r == t)).unwrap_or(0) as isize;
            let j = (i + step).clamp(0, self.rows.len() as isize - 1) as usize;
            let t = self.rows[j];
            self.select_click(t, false, false);
            self.scroll_to_focus = true;
        }
    }

    fn toolbar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal_wrapped(|ui| {
            if ui.button("Open .ttpf…").clicked() {
                self.pick_file();
            }
            if ui.button("Open folder…").on_hover_text("Shows a folder's .ttpf files in a sidebar").clicked() {
                if let Some(d) = rfd::FileDialog::new().set_title("Choose a folder with .ttpf files").pick_folder() {
                    self.set_folder(d);
                }
            }
            if let Some(a) = &self.arc {
                ui.label(RichText::new(a.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()).strong());
            }
            if self.arc.is_none() {
                return;
            }
            ui.separator();
            ui.checkbox(&mut self.show_unavailable, format!("Show unavailable ({})", self.unavailable)).on_hover_text(
                "Textures that can't be shown: cut off from the file, stored with a compression this app can't read, or in an unsupported pixel format.",
            );
            ui.separator();
            ui.add(egui::TextEdit::singleline(&mut self.search).hint_text("Search: number, 512x256, BC7").desired_width(190.0));
            ui.separator();
            egui::ComboBox::from_id_salt("sort")
                .width(90.0)
                .selected_text(match self.sort {
                    Sort::Number => "Sort: number",
                    Sort::Size => "Sort: size",
                    Sort::Format => "Sort: format",
                })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.sort, Sort::Number, "Number");
                    ui.selectable_value(&mut self.sort, Sort::Size, "Size (pixels)");
                    ui.selectable_value(&mut self.sort, Sort::Format, "Format");
                });
            if ui
                .button(if self.descending { "Descending" } else { "Ascending" })
                .on_hover_text("Reverse the order")
                .clicked()
            {
                self.descending = !self.descending;
            }
            ui.separator();
            ui.label("Size");
            ui.add(egui::Slider::new(&mut self.tsize, 64.0..=384.0).show_value(false));
            ui.separator();
            let busy = self.export.is_some();
            if self.arc.as_ref().is_some_and(|a| a.textures.iter().any(|t| t.fmt.is_some_and(|f| f.is_hdr()))) {
                let mut tm = crate::bcn::TONEMAP.load(Ordering::Relaxed);
                let before = tm;
                egui::ComboBox::from_id_salt("hdr")
                    .width(110.0)
                    .selected_text(if tm { "HDR: tone-mapped" } else { "HDR: clipped" })
                    .show_ui(ui, |ui| {
                        ui.selectable_value(&mut tm, true, "Tone-mapped").on_hover_text("Bright values roll off smoothly, so detail above white stays visible");
                        ui.selectable_value(&mut tm, false, "Clipped").on_hover_text("Values shown as stored; anything brighter than white is white");
                    })
                    .response
                    .on_hover_text("How BC6H and half-float textures are shown and saved as PNG (DDS always keeps the original values)");
                if tm != before && !busy {
                    crate::bcn::TONEMAP.store(tm, Ordering::Relaxed);
                    self.hdr_gen += 1;
                    self.pool.clear();
                    self.thumbs.clear();
                    self.pending.clear();
                    self.failed.clear();
                    self.full = None;
                    self.full_pending = None;
                }
                ui.separator();
            }
            egui::ComboBox::from_id_salt("as")
                .width(60.0)
                .selected_text(if self.as_dds { "DDS" } else { "PNG" })
                .show_ui(ui, |ui| {
                    ui.selectable_value(&mut self.as_dds, false, "PNG");
                    ui.selectable_value(&mut self.as_dds, true, "DDS").on_hover_text("The original data: lossless, keeps the compression, HDR values and mip levels");
                });
            let n_sel = self.selected.len();
            if ui.add_enabled(n_sel > 0 && !busy, egui::Button::new(format!("Extract selected ({n_sel})"))).clicked() {
                let l: Vec<usize> = self.selected.iter().copied().collect();
                self.start_export(l);
            }
            let shown = self.rows.len();
            if ui.add_enabled(shown > 0 && !busy, egui::Button::new(format!("Extract all shown ({shown})"))).clicked() {
                let l = self.rows.clone();
                self.start_export(l);
            }
        });
    }

    fn status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            if let Some(l) = &self.loading {
                ui.add(egui::ProgressBar::new(l.progress.fraction()).desired_width(260.0));
                ui.label(l.progress.stage.lock().unwrap().clone());
            } else if let Some(e) = &self.error {
                ui.colored_label(ui.visuals().error_fg_color, e);
            } else if let Some(a) = &self.arc {
                let reduced = a.textures.iter().filter(|t| matches!(t.status, Status::Reduced(_))).count();
                let mut s = format!("{} textures", a.textures.len());
                if reduced > 0 {
                    s += &format!(", {reduced} only at reduced size");
                }
                if self.unavailable > 0 {
                    s += &format!(", {} unavailable", self.unavailable);
                }
                ui.label(s);
            }
            if let Some(e) = &self.export {
                ui.separator();
                let d = e.done.load(Ordering::Relaxed);
                ui.add(egui::ProgressBar::new(d as f32 / e.total.max(1) as f32).desired_width(200.0).text(format!("Saving {d}/{}", e.total)));
            }
            if let Some((msg, dir)) = self.message.clone() {
                ui.separator();
                ui.label(msg);
                if let Some(d) = dir {
                    if ui.small_button("Open folder").clicked() {
                        open_folder(&d);
                    }
                }
            }
        });
    }

    fn gallery(&mut self, ui: &mut egui::Ui) {
        let Some(arc) = self.arc.clone() else {
            self.empty_state(ui);
            return;
        };
        if self.rows.is_empty() {
            ui.centered_and_justified(|ui| {
                ui.label("Nothing matches.");
            });
            return;
        }
        let cell = self.tsize;
        let cap = 34.0;
        let gap = ui.spacing().item_spacing;
        let cols = (((ui.available_width() + gap.x) / (cell + gap.x)).floor() as usize).max(1);
        self.cols = cols;
        let nrows = self.rows.len().div_ceil(cols);
        let row_h = cell + cap;
        let mut scroll = egui::ScrollArea::vertical().auto_shrink(false);
        if self.scroll_to_focus {
            self.scroll_to_focus = false;
            if let Some(i) = self.focus.and_then(|t| self.rows.iter().position(|&r| r == t)) {
                let y = (i / cols) as f32 * (row_h + gap.y);
                let view_h = ui.available_height();
                if y < self.scroll_y || y + row_h > self.scroll_y + view_h {
                    scroll = scroll.vertical_scroll_offset((y - view_h / 2.0 + row_h / 2.0).max(0.0));
                }
            }
        }
        let rows = self.rows.clone();
        let out = scroll.show_rows(ui, row_h, nrows, |ui, range| {
            for r in range {
                ui.horizontal(|ui| {
                    for c in 0..cols {
                        if let Some(&t) = rows.get(r * cols + c) {
                            self.cell(ui, &arc, t, cell, cap);
                        }
                    }
                });
            }
        });
        self.scroll_y = out.state.clamped_offset().y;
    }

    fn cell(&mut self, ui: &mut egui::Ui, arc: &Archive, t: usize, cell: f32, cap: f32) {
        let (rect, resp) = ui.allocate_exact_size(vec2(cell, cell + cap), Sense::click());
        if !ui.is_rect_visible(rect) {
            return;
        }
        let vis = ui.visuals().clone();
        let tex = &arc.textures[t];
        let p = ui.painter_at(rect);
        let img_area = Rect::from_min_size(rect.min, vec2(cell, cell));
        p.rect_filled(img_area, 4.0, vis.extreme_bg_color);
        match tex.status {
            Status::Unavailable(prob) => {
                p.text(img_area.center(), Align2::CENTER_CENTER, prob.text(), FontId::proportional(12.0), vis.weak_text_color());
            }
            s => {
                let k = (t, 0, s.shown_level().unwrap_or(0));
                if let Some(e) = self.failed.get(&k) {
                    p.text(img_area.center(), Align2::CENTER_CENTER, e, FontId::proportional(12.0), vis.error_fg_color);
                } else if let Some((th, f)) = self.thumbs.get_mut(&k) {
                    *f = self.frame;
                    let r = fit_rect(img_area.shrink(4.0), th.size_vec2());
                    let id = th.id();
                    self.checker(&p, r);
                    p.image(id, r, FULL_UV, Color32::WHITE);
                } else {
                    self.request_thumb(k, THUMB);
                    p.text(img_area.center(), Align2::CENTER_CENTER, "…", FontId::proportional(18.0), vis.weak_text_color());
                }
            }
        }
        let stroke = if self.focus == Some(t) {
            Stroke::new(3.0, vis.strong_text_color())
        } else if self.selected.contains(&t) {
            Stroke::new(3.0, vis.selection.stroke.color)
        } else {
            vis.widgets.noninteractive.bg_stroke
        };
        p.rect_stroke(img_area, 4.0, stroke, StrokeKind::Inside);
        let size = match tex.status {
            Status::Reduced(l) => format!("{}x{}  (only {}x{})", tex.w, tex.h, tex.levels[l].w, tex.levels[l].h),
            _ => format!("{}x{}", tex.w, tex.h),
        };
        let x = rect.min.x + 2.0;
        p.text(pos2(x, img_area.max.y + 3.0), Align2::LEFT_TOP, format!("#{t}"), FontId::proportional(13.0), vis.strong_text_color());
        p.text(pos2(x, img_area.max.y + 19.0), Align2::LEFT_TOP, size, FontId::proportional(11.5), vis.text_color());
        if resp.clicked() {
            let m = ui.input(|i| i.modifiers);
            self.select_click(t, m.command, m.shift);
        }
    }

    fn empty_state(&mut self, ui: &mut egui::Ui) {
        let hovering = ui.ctx().input(|i| !i.raw.hovered_files.is_empty());
        ui.vertical_centered(|ui| {
            ui.add_space(ui.available_height() * 0.3);
            ui.label(RichText::new(if hovering { "Drop it!" } else { "Drop a .ttpf file or a folder here" }).size(28.0));
            ui.add_space(8.0);
            ui.label("or");
            ui.add_space(8.0);
            if ui.button(RichText::new("Open .ttpf…").size(18.0)).clicked() {
                self.pick_file();
            }
            if self.loading.is_some() {
                ui.add_space(16.0);
                ui.spinner();
            }
            if let Some(e) = &self.error {
                ui.add_space(16.0);
                ui.colored_label(ui.visuals().error_fg_color, e);
            }
        });
    }

    fn preview(&mut self, ui: &mut egui::Ui) {
        let (Some(arc), Some(t)) = (self.arc.clone(), self.focus) else { return };
        let tex = &arc.textures[t];
        let vis = ui.visuals().clone();
        let layer = self.layer.min(tex.layer_count() - 1);
        let levels = tex.layer_levels(layer);
        // a face / slice can be readable only at a smaller size than the first one: then show its largest
        let level = self.level.or(if layer == 0 { tex.status.shown_level() } else { levels.iter().position(|l| l.state.is_ok()) });
        ui.horizontal(|ui| {
            ui.heading(format!("#{t}"));
            ui.label(format!("{}x{}", tex.w, tex.h));
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui.button("Close").on_hover_text("Esc").clicked() {
                    self.focus = None;
                }
            });
        });
        match tex.status {
            Status::Full => {}
            Status::Reduced(_) => {
                ui.colored_label(vis.warn_fg_color, "Full size is stored with a compression this app can't read; showing the largest readable mip level.");
            }
            Status::Unavailable(prob) => {
                ui.colored_label(vis.warn_fg_color, format!("Can't show this texture: {}.", prob.text()));
            }
        }
        // image with zoom / pan
        let below = 110.0 + 120.0 + if tex.layer_count() > 1 { MIP_THUMB as f32 + 42.0 } else { 0.0 };
        let area_size = vec2(ui.available_width(), (ui.available_height() - below).max(200.0));
        let (area, resp) = ui.allocate_exact_size(area_size, Sense::click_and_drag());
        let p = ui.painter_at(area);
        p.rect_filled(area, 4.0, vis.extreme_bg_color);
        if let Some(l) = level {
            let k = (t, layer, l);
            self.request_full(k);
            let shown = self.full.as_ref().filter(|f| f.0 == k).map(|f| f.1.clone());
            if let Some(th) = shown {
                let img = th.size_vec2();
                if self.refit || resp.double_clicked() {
                    self.refit = false;
                    let z = (area.width() / img.x).min(area.height() / img.y);
                    self.zoom = if z >= 1.0 { z.floor().min(8.0) } else { z };
                    self.pan = Vec2::ZERO;
                }
                if resp.hovered() {
                    let (scroll, pointer) = ui.input(|i| (i.smooth_scroll_delta.y, i.pointer.hover_pos()));
                    if scroll != 0.0 {
                        let old = self.zoom;
                        self.zoom = (self.zoom * (scroll * 0.0025).exp()).clamp(0.02, 64.0);
                        if let Some(pp) = pointer {
                            let c = area.center() + self.pan;
                            self.pan += (pp - c) * (1.0 - self.zoom / old);
                        }
                    }
                }
                if resp.dragged() {
                    self.pan += resp.drag_delta();
                }
                let r = Rect::from_center_size(area.center() + self.pan, img * self.zoom);
                self.checker(&p, r.intersect(area));
                p.image(th.id(), r, FULL_UV, Color32::WHITE);
                p.text(area.left_top() + vec2(6.0, 6.0), Align2::LEFT_TOP, format!("{:.0}%", self.zoom * 100.0), FontId::proportional(12.0), vis.text_color());
            } else if let Some(e) = self.failed.get(&k) {
                p.text(area.center(), Align2::CENTER_CENTER, format!("Can't decode: {e}"), FontId::proportional(15.0), vis.error_fg_color);
            } else {
                p.text(area.center(), Align2::CENTER_CENTER, "Decoding…", FontId::proportional(16.0), vis.weak_text_color());
            }
        }
        ui.horizontal(|ui| {
            if ui.small_button("Fit").clicked() {
                self.refit = true;
            }
            if ui.small_button("100%").clicked() {
                self.zoom = 1.0;
                self.pan = Vec2::ZERO;
            }
            ui.label(RichText::new("wheel: zoom   drag: pan   double-click: fit").color(vis.weak_text_color()));
        });
        // faces / slices
        if tex.layer_count() > 1 {
            let shown = tex.status.shown_level();
            ui.label(RichText::new(if tex.cube { "Faces" } else { "Slices" }).strong());
            egui::ScrollArea::horizontal().id_salt("layers").max_height(MIP_THUMB as f32 + 24.0).show(ui, |ui| {
                ui.horizontal(|ui| {
                    for li in 0..tex.layer_count() {
                        let s = MIP_THUMB as f32;
                        let (rect, r) = ui.allocate_exact_size(vec2(s, s + 18.0), Sense::click());
                        let p = ui.painter_at(rect);
                        let box_ = Rect::from_min_size(rect.min, vec2(s, s));
                        p.rect_filled(box_, 3.0, vis.extreme_bg_color);
                        let best = if shown.is_some() { tex.layer_levels(li).iter().position(|l| l.state.is_ok()) } else { None };
                        if best.is_none() {
                            p.text(box_.center(), Align2::CENTER_CENTER, "unreadable", FontId::proportional(11.0), vis.weak_text_color());
                        }
                        if let Some(sl) = best {
                            let k = (t, li, sl);
                            if let Some((th, f)) = self.thumbs.get_mut(&k) {
                                *f = self.frame;
                                let rr = fit_rect(box_.shrink(3.0), th.size_vec2());
                                let id = th.id();
                                self.checker(&p, rr);
                                p.image(id, rr, FULL_UV, Color32::WHITE);
                            } else if self.failed.contains_key(&k) {
                                p.text(box_.center(), Align2::CENTER_CENTER, "error", FontId::proportional(11.0), vis.error_fg_color);
                            } else {
                                self.request_thumb(k, MIP_THUMB);
                            }
                        }
                        let st = if li == layer { Stroke::new(2.0, vis.strong_text_color()) } else { vis.widgets.noninteractive.bg_stroke };
                        p.rect_stroke(box_, 3.0, st, StrokeKind::Inside);
                        let name = tex.layer_name(li);
                        p.text(pos2(rect.min.x + 2.0, box_.max.y + 2.0), Align2::LEFT_TOP, name, FontId::proportional(11.0), vis.text_color());
                        if r.clicked() && li != layer {
                            self.layer = li;
                            self.level = None;
                            self.refit = true;
                        }
                    }
                });
            });
        }
        // mip levels
        if !levels.is_empty() {
            ui.label(RichText::new("Mip levels").strong());
            egui::ScrollArea::horizontal().id_salt("mips").max_height(MIP_THUMB as f32 + 24.0).show(ui, |ui| {
                ui.horizontal(|ui| {
                    for (li, lv) in levels.iter().enumerate() {
                        let s = MIP_THUMB as f32;
                        let (rect, r) = ui.allocate_exact_size(vec2(s, s + 18.0), Sense::click());
                        let p = ui.painter_at(rect);
                        let box_ = Rect::from_min_size(rect.min, vec2(s, s));
                        p.rect_filled(box_, 3.0, vis.extreme_bg_color);
                        let k = (t, layer, li);
                        if lv.state.is_err() {
                            p.text(box_.center(), Align2::CENTER_CENTER, "unreadable", FontId::proportional(11.0), vis.weak_text_color());
                        } else if let Some((th, f)) = self.thumbs.get_mut(&k) {
                            *f = self.frame;
                            let rr = fit_rect(box_.shrink(3.0), th.size_vec2());
                            let id = th.id();
                            self.checker(&p, rr);
                            p.image(id, rr, FULL_UV, Color32::WHITE);
                        } else if self.failed.contains_key(&k) {
                            p.text(box_.center(), Align2::CENTER_CENTER, "error", FontId::proportional(11.0), vis.error_fg_color);
                        } else {
                            self.request_thumb(k, MIP_THUMB);
                        }
                        let st = if level == Some(li) { Stroke::new(2.0, vis.strong_text_color()) } else { vis.widgets.noninteractive.bg_stroke };
                        p.rect_stroke(box_, 3.0, st, StrokeKind::Inside);
                        p.text(pos2(rect.min.x + 2.0, box_.max.y + 2.0), Align2::LEFT_TOP, format!("{}x{}", lv.w, lv.h), FontId::proportional(11.0), vis.text_color());
                        if r.clicked() && lv.state.is_ok() {
                            self.level = Some(li);
                            self.refit = true;
                        }
                    }
                });
            });
        }
        egui::Grid::new("info").num_columns(2).spacing([16.0, 2.0]).show(ui, |ui| {
            ui.label("Format");
            ui.label(format!("{}  ({} tiles)", tex.format_name(), tex.tile.name()));
            ui.end_row();
            if let Some(l) = level {
                ui.label("Showing");
                let what = if tex.layer_count() > 1 { format!("{}, ", tex.layer_name(layer)) } else { String::new() };
                ui.label(format!("{what}mip {l}: {}x{}", levels[l].w, levels[l].h));
                ui.end_row();
            }
        });
        ui.add_space(4.0);
        if let Some(l) = level {
            ui.horizontal(|ui| {
                if ui.button("Save PNG…").clicked() {
                    self.save_one(t, self.layer, l);
                }
                if ui.button("Save DDS…").on_hover_text("The original data: lossless, keeps the compression, HDR values and mip levels").clicked() {
                    self.save_dds(t);
                }
            });
        }
    }
}
