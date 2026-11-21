//! The app window: texture gallery, large preview with mip levels, and PNG export.

use crate::archive::{Texture, self, Archive, Progress, Status};
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

/// Which textures the gallery shows.
#[derive(Clone, Copy, PartialEq)]
pub enum Show {
    All,
    Cube,
    Array,
    Hdr,
    Reduced,
    Unavailable,
}

impl Show {
    pub const ALL: [Show; 6] = [Show::All, Show::Cube, Show::Array, Show::Hdr, Show::Reduced, Show::Unavailable];

    pub fn name(self) -> &'static str {
        match self {
            Show::All => "All textures",
            Show::Cube => "Cube maps",
            Show::Array => "Texture arrays",
            Show::Hdr => "HDR (BC6H, half-float)",
            Show::Reduced => "Only at a smaller size",
            Show::Unavailable => "Unavailable",
        }
    }

    pub fn key(self) -> &'static str {
        match self {
            Show::All => "all",
            Show::Cube => "cube",
            Show::Array => "array",
            Show::Hdr => "hdr",
            Show::Reduced => "reduced",
            Show::Unavailable => "unavailable",
        }
    }

    pub fn from_key(k: &str) -> Show {
        Show::ALL.into_iter().find(|s| s.key() == k).unwrap_or(Show::All)
    }

    pub fn wants(self, t: &Texture) -> bool {
        match self {
            Show::All => true,
            Show::Cube => t.cube,
            Show::Array => !t.cube && t.layer_count() > 1,
            Show::Hdr => t.fmt.is_some_and(|f| f.is_hdr()),
            Show::Reduced => matches!(t.status, Status::Reduced(_)),
            Show::Unavailable => matches!(t.status, Status::Unavailable(_)),
        }
    }
}

/// Does texture `t` pass the search text `q` (lower case, no '#': a number, "512x256", a format name) and the filter?
/// Unavailable textures only pass when asked for.
pub fn matches(t: &Texture, q: &str, show: Show, unavailable: bool) -> bool {
    show.wants(t)
        && (unavailable || show == Show::Unavailable || !matches!(t.status, Status::Unavailable(_)))
        && (q.is_empty() || t.index.to_string() == q || format!("{}x{} {}", t.w, t.h, t.format_name()).to_lowercase().contains(q))
}

/// Decoded images; the last field is the HDR setting generation they were made with.
enum Done {
    Thumb(Key2, Result<ColorImage, String>, u32),
    Full(Key2, Result<ColorImage, String>, u32),
    Cube(usize, Result<Arc<[Rgba; 6]>, String>, u32),
    /// a status-bar message from background work, and a folder to offer opening
    Message(String, Option<PathBuf>),
    /// thumbnail of folder-search result n, made for search number `u32`
    FThumb(usize, Result<ColorImage, String>, u32),
}

/// Put an image on the clipboard.
pub fn copy_image(img: &Rgba) -> Result<(), String> {
    let mut cb = arboard::Clipboard::new().map_err(|e| e.to_string())?;
    cb.set_image(arboard::ImageData { width: img.w, height: img.h, bytes: std::borrow::Cow::Borrowed(&img.px) }).map_err(|e| e.to_string())
}

/// The 3D view of a cube map: its faces, the camera, and the last rendered picture.
struct Cube3d {
    t: usize,
    faces: Option<Arc<[Rgba; 6]>>,
    error: Option<String>,
    view: crate::cube3d::View,
    shown: Option<(TextureHandle, crate::cube3d::View, [usize; 2])>,
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
    show: Show,
    search: String,
    tsize: f32,
    /// extract as .dds (original data) instead of .png
    as_dds: bool,
    /// face / slice shown for a cube map or texture array
    layer: usize,
    /// textures of the open archive shown as normal maps (blue rebuilt from red and green), by the user's choice
    normal_maps: HashSet<usize>,
    /// cube maps: show the 3D view instead of a face
    view3d: bool,
    cube3d: Option<Cube3d>,
    sort: Sort,
    descending: bool,
    /// 3D view: Z is up (kept for new cube maps and between runs)
    z_up: bool,
    /// settings as last saved
    saved: crate::settings::Settings,
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
    /// search across the sidebar folder: window open, query, filter, running search, results
    fwin: bool,
    fquery: String,
    fshow: Show,
    funavailable: bool,
    fsearch: Option<crate::folder::Task<crate::folder::Found>>,
    found: Option<crate::folder::Found>,
    /// folder-search results as pictures (else a list), their thumbnails, and which search they belong to
    fimages: bool,
    fthumbs: HashMap<usize, TextureHandle>,
    fpending: HashSet<usize>,
    ffailed: HashMap<usize, String>,
    fgen: u32,
    /// extracting the whole sidebar folder, and where to
    fexport: Option<(crate::folder::Task<Result<crate::folder::Extracted, String>>, PathBuf)>,
    /// texture to select once the archive being opened has loaded
    pending_focus: Option<usize>,
    message: Option<(String, Option<PathBuf>)>,
    /// Developer test hook (env TTPF_VIEWER_TEST="out.png;select=12,level=1,unavailable,light,size=200"): once
    /// everything has loaded, save the app's own rendered frame to out.png and quit.
    test: Option<(PathBuf, Vec<String>, u64, bool)>,
}

impl App {
    fn new(file: Option<PathBuf>) -> App {
        let mut app = App::blank(file);
        // the test hook always starts from the defaults
        if app.test.is_none() {
            let s = crate::settings::Settings::load();
            app.sort = match s.sort.as_str() {
                "size" => Sort::Size,
                "format" => Sort::Format,
                _ => Sort::Number,
            };
            app.descending = s.descending;
            app.tsize = s.thumb_size;
            app.show = Show::from_key(&s.show);
            app.show_unavailable = s.show_unavailable;
            crate::bcn::TONEMAP.store(s.tonemap, Ordering::Relaxed);
            app.as_dds = s.as_dds;
            app.z_up = s.z_up;
            app.folder = s.folder.clone();
            app.saved = s;
        }
        app
    }

    /// Settings as they are now.
    fn settings(&self) -> crate::settings::Settings {
        crate::settings::Settings {
            sort: match self.sort {
                Sort::Number => "number",
                Sort::Size => "size",
                Sort::Format => "format",
            }
            .into(),
            descending: self.descending,
            thumb_size: self.tsize,
            show: self.show.key().into(),
            show_unavailable: self.show_unavailable,
            tonemap: crate::bcn::TONEMAP.load(Ordering::Relaxed),
            as_dds: self.as_dds,
            z_up: self.z_up,
            folder: self.folder.clone(),
        }
    }

    fn new_cube3d(&self, t: usize) -> Cube3d {
        Cube3d { t, faces: None, error: None, view: crate::cube3d::View { z_up: self.z_up, ..Default::default() }, shown: None }
    }

    fn blank(file: Option<PathBuf>) -> App {
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
            show: Show::All,
            search: String::new(),
            tsize: 160.0,
            as_dds: false,
            layer: 0,
            normal_maps: HashSet::new(),
            view3d: false,
            cube3d: None,
            sort: Sort::Number,
            descending: false,
            z_up: true,
            saved: Default::default(),
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
            fwin: false,
            fquery: String::new(),
            fshow: Show::All,
            funavailable: false,
            fsearch: None,
            found: None,
            fimages: true,
            fthumbs: HashMap::new(),
            fpending: HashSet::new(),
            ffailed: HashMap::new(),
            fgen: 0,
            fexport: None,
            pending_focus: None,
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
        self.normal_maps.clear();
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
                Ok(a) => {
                    self.arc = Some(a);
                    if let Some(i) = self.pending_focus.take() {
                        self.focus_texture(i);
                    }
                }
                Err(e) => self.error = Some(e),
            }
            self.loading = None;
        }
        for d in std::mem::take(&mut *self.done.lock().unwrap()) {
            match d {
                Done::Thumb(_, _, g) | Done::Full(_, _, g) | Done::Cube(_, _, g) if g != self.hdr_gen => {}
                Done::Message(m, dir) => self.message = Some((m, dir)),
                Done::FThumb(n, r, g) => {
                    if g == self.fgen {
                        self.fpending.remove(&n);
                        match r {
                            Ok(img) => {
                                let t = ctx.load_texture(format!("f{g}_{n}"), img, texture_options());
                                self.fthumbs.insert(n, t);
                            }
                            Err(e) => {
                                self.ffailed.insert(n, e);
                            }
                        }
                    }
                }
                Done::Cube(t, r, _) => {
                    if let Some(c) = self.cube3d.as_mut().filter(|c| c.t == t) {
                        match r {
                            Ok(f) => c.faces = Some(f),
                            Err(e) => c.error = Some(e),
                        }
                        c.shown = None;
                    }
                }
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
        if let Some(f) = self.fsearch.as_ref().and_then(|t| t.take()) {
            self.found = Some(f);
            self.fsearch = None;
        }
        let fin = self.fexport.as_ref().and_then(|(t, d)| t.take().map(|r| (r, d.clone())));
        if let Some((r, dir)) = fin {
            self.fexport = None;
            self.message = Some(match r {
                Ok(e) => {
                    let mut m = format!("Saved {} textures from {} files to {}", e.written, e.files - e.failed, dir.display());
                    if e.skipped > 0 {
                        m += &format!(" ({} unreadable)", e.skipped);
                    }
                    if e.failed > 0 {
                        m += &format!(", {} files couldn't be opened", e.failed);
                    }
                    (m, Some(dir))
                }
                Err(e) => (format!("Extracting failed: {e}"), None),
            });
        }
        if self.loading.is_some() || self.export.is_some() || self.fsearch.is_some() || self.fexport.is_some() || !self.pending.is_empty() || self.full_pending.is_some() {
            ctx.request_repaint_after(Duration::from_millis(150));
        }
    }

    fn refresh_rows(&mut self) {
        let Some(arc) = &self.arc else { return };
        let q = self.search.trim().trim_start_matches('#').to_lowercase();
        self.unavailable = arc.textures.iter().filter(|t| matches!(t.status, Status::Unavailable(_))).count();
        self.rows = arc.textures.iter().filter(|t| matches(t, &q, self.show, self.show_unavailable)).map(|t| t.index).collect();
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
        let (done, g, nz) = (self.done.clone(), self.hdr_gen, self.normal_maps.contains(&k.0));
        self.pool.push(Box::new(move || {
            let r = arc
                .image_in(k.0, k.1, k.2)
                .map(|mut img| {
                    if nz {
                        img.rebuild_normal_z();
                    }
                    to_color_image(&img.thumbnail(size))
                })
                .map_err(|p| p.text().to_string());
            done.lock().unwrap().push(Done::Thumb(k, r, g));
        }));
    }

    fn request_full(&mut self, k: Key2) {
        if self.full.as_ref().is_some_and(|f| f.0 == k) || self.full_pending == Some(k) || self.failed.contains_key(&k) {
            return;
        }
        let Some(arc) = self.arc.clone() else { return };
        self.full_pending = Some(k);
        let (done, g, nz) = (self.done.clone(), self.hdr_gen, self.normal_maps.contains(&k.0));
        self.pool.push(Box::new(move || {
            let r = arc
                .image_in(k.0, k.1, k.2)
                .map(|mut img| {
                    if nz {
                        img.rebuild_normal_z();
                    }
                    to_color_image(&if img.w.max(img.h) > 8192 { img.thumbnail(8192) } else { img })
                })
                .map_err(|p| p.text().to_string());
            done.lock().unwrap().push(Done::Full(k, r, g));
        }));
    }

    /// Draw the 3D view of cube map `t` into `area`, decoding its faces first if needed.
    fn cube_view(&mut self, ui: &egui::Ui, p: &egui::Painter, area: Rect, resp: &egui::Response, t: usize) {
        let vis = ui.visuals().clone();
        if self.cube3d.as_ref().is_none_or(|c| c.t != t) {
            let sphere = self.cube3d.as_ref().is_some_and(|c| c.view.sphere);
            let mut c = self.new_cube3d(t);
            c.view.sphere = sphere;
            self.cube3d = Some(c);
        }
        let Some(arc) = self.arc.clone() else { return };
        let c = self.cube3d.as_mut().unwrap();
        if let Some(e) = &c.error {
            p.text(area.center(), Align2::CENTER_CENTER, format!("Can't show in 3D: {e}"), FontId::proportional(15.0), vis.error_fg_color);
            return;
        }
        let Some(faces) = c.faces.clone() else {
            if c.shown.is_none() {
                // first request: decode the 6 faces in the background (marker: a dummy shown entry)
                c.shown = Some((ui.ctx().load_texture("cube", ColorImage::new([1, 1], vec![Color32::TRANSPARENT]), texture_options()), c.view, [0, 0]));
                let (done, g) = (self.done.clone(), self.hdr_gen);
                self.pool.push(Box::new(move || {
                    let r = arc.cube_faces(t).map(Arc::new).map_err(|p| p.text().to_string());
                    done.lock().unwrap().push(Done::Cube(t, r, g));
                }));
            }
            p.text(area.center(), Align2::CENTER_CENTER, "Decoding faces…", FontId::proportional(16.0), vis.weak_text_color());
            return;
        };
        // drag turns, wheel zooms
        if resp.dragged() {
            let d = resp.drag_delta();
            let k = if c.view.sphere { -0.008 } else { 0.004 * c.view.fov };
            c.view.yaw -= d.x * k;
            c.view.pitch = (c.view.pitch + d.y * k).clamp(-1.55, 1.55);
        }
        if resp.hovered() {
            let scroll = ui.input(|i| i.smooth_scroll_delta.y);
            if scroll != 0.0 {
                c.view.fov = (c.view.fov * (-scroll * 0.0015).exp()).clamp(15f32.to_radians(), 150f32.to_radians());
            }
        }
        // render at most ~800 px wide, scaled up to the area
        let s = (800.0 / area.width()).min(1.0);
        let size = [((area.width() * s) as usize).max(16), ((area.height() * s) as usize).max(16)];
        let stale = c.shown.as_ref().is_none_or(|(_, v, sz)| *v != c.view || *sz != size);
        if stale {
            let px = crate::cube3d::render(&faces, c.view, size[0], size[1]);
            let img = ColorImage::from_rgba_unmultiplied(size, &px);
            let opts = TextureOptions { magnification: TextureFilter::Linear, ..texture_options() };
            match &mut c.shown {
                Some((th, v, sz)) if *sz != [0, 0] => {
                    th.set(img, opts);
                    *v = c.view;
                    *sz = size;
                }
                _ => c.shown = Some((ui.ctx().load_texture("cube", img, opts), c.view, size)),
            }
        }
        let (th, _, _) = c.shown.as_ref().unwrap();
        p.image(th.id(), area, FULL_UV, Color32::WHITE);
        let hint = if c.view.sphere { "drag: turn the ball   wheel: zoom" } else { "drag: look around   wheel: zoom" };
        p.text(area.left_top() + vec2(6.0, 6.0), Align2::LEFT_TOP, hint, FontId::proportional(12.0), vis.text_color());
    }

    /// Select texture `i` of the open archive and make sure the gallery shows it.
    fn focus_texture(&mut self, i: usize) {
        let Some(arc) = &self.arc else { return };
        if i >= arc.textures.len() {
            return;
        }
        let unavailable = matches!(arc.textures[i].status, Status::Unavailable(_));
        if !matches(&arc.textures[i], &self.search.trim().trim_start_matches('#').to_lowercase(), self.show, self.show_unavailable) {
            self.search.clear();
            self.show = Show::All;
            self.show_unavailable |= unavailable;
        }
        self.refresh_rows();
        self.select_click(i, false, false);
        self.scroll_to_focus = true;
    }

    /// The "Search all files" window.
    fn search_window(&mut self, ui: &mut egui::Ui, ctx: &egui::Context) {
        let Some(dir) = self.folder.clone() else { return };
        ui.label(format!("Every .ttpf in {}", dir.display()));
        let running = self.fsearch.is_some();
        let mut go = false;
        ui.horizontal(|ui| {
            let r = ui.add(egui::TextEdit::singleline(&mut self.fquery).hint_text("number, 512x256, BC7 (empty: everything)").desired_width(220.0));
            go |= r.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
            egui::ComboBox::from_id_salt("fshow").selected_text(self.fshow.name()).show_ui(ui, |ui| {
                for s in Show::ALL {
                    ui.selectable_value(&mut self.fshow, s, s.name());
                }
            });
        });
        ui.horizontal(|ui| {
            ui.checkbox(&mut self.funavailable, "Include unavailable");
            go |= ui.add_enabled(!running, egui::Button::new("Search")).clicked();
            if running && ui.button("Stop").clicked() {
                if let Some(t) = &self.fsearch {
                    t.cancel.store(true, Ordering::Relaxed);
                }
            }
        });
        if go && !running {
            self.found = None;
            self.clear_fthumbs();
            self.fsearch = Some(crate::folder::search(dir.clone(), self.fquery.clone(), self.fshow, self.funavailable));
        }
        if let Some(t) = &self.fsearch {
            ui.add(egui::ProgressBar::new(t.fraction()).text(format!("Searching file {}/{}", t.done.load(Ordering::Relaxed), t.total.load(Ordering::Relaxed))));
        }
        let Some(found) = &self.found else { return };
        let mut info = format!("{} found in {} files", found.hits.len(), found.files);
        if found.truncated {
            info += &format!(" (only the first {} are listed)", crate::folder::MAX_HITS);
        }
        if found.failed > 0 {
            info += &format!(", {} files couldn't be opened", found.failed);
        }
        ui.horizontal(|ui| {
            ui.label(info);
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                ui.selectable_value(&mut self.fimages, false, "List");
                ui.selectable_value(&mut self.fimages, true, "Images");
            });
        });
        ui.separator();
        if self.fimages {
            let found = self.found.take();
            if let Some(f) = &found {
                if let Some(pick) = self.search_grid(ui, f) {
                    let (path, i) = (f.hits[pick].path.clone(), f.hits[pick].index);
                    self.found = found;
                    self.go_to(path, i, ctx);
                    return;
                }
            }
            self.found = found;
            return;
        }
        let Some(found) = &self.found else { return };
        let mut pick = None;
        let row_h = ui.text_style_height(&egui::TextStyle::Body) + 4.0;
        egui::ScrollArea::vertical().auto_shrink(false).show_rows(ui, row_h, found.hits.len(), |ui, range| {
            for h in &found.hits[range] {
                let file = h.path.file_name().unwrap_or_default().to_string_lossy();
                let mut text = format!("{file}   #{}   {}x{}   {}", h.index, h.w, h.h, h.format);
                if !h.note.is_empty() {
                    text += &format!("   ({})", h.note);
                }
                let current = self.open_path.as_ref() == Some(&h.path) && self.focus == Some(h.index);
                if ui.selectable_label(current, text).on_hover_text(h.path.display().to_string()).clicked() {
                    pick = Some((h.path.clone(), h.index));
                }
            }
        });
        if let Some((path, i)) = pick {
            self.go_to(path, i, ctx);
        }
    }

    /// Open `path` (if it isn't already) and select texture `i`.
    fn go_to(&mut self, path: PathBuf, i: usize, ctx: &egui::Context) {
        if self.open_path.as_ref() == Some(&path) && self.arc.is_some() {
            self.focus_texture(i);
        } else {
            self.open(path, ctx);
            self.pending_focus = Some(i);
        }
    }

    fn clear_fthumbs(&mut self) {
        self.fgen += 1;
        self.fthumbs.clear();
        self.fpending.clear();
        self.ffailed.clear();
    }

    /// Folder-search results as a grid of pictures. Returns the result clicked.
    fn search_grid(&mut self, ui: &mut egui::Ui, found: &crate::folder::Found) -> Option<usize> {
        const CELL: f32 = 128.0;
        const CAP: f32 = 46.0;
        let vis = ui.visuals().clone();
        let gap = ui.spacing().item_spacing;
        let cols = (((ui.available_width() + gap.x) / (CELL + gap.x)).floor() as usize).max(1);
        let mut pick = None;
        egui::ScrollArea::vertical().auto_shrink(false).show_rows(ui, CELL + CAP, found.hits.len().div_ceil(cols), |ui, range| {
            for r in range {
                ui.horizontal(|ui| {
                    for n in r * cols..((r + 1) * cols).min(found.hits.len()) {
                        let h = &found.hits[n];
                        let (rect, resp) = ui.allocate_exact_size(vec2(CELL, CELL + CAP), Sense::click());
                        let p = ui.painter_at(rect);
                        let img = Rect::from_min_size(rect.min, vec2(CELL, CELL));
                        p.rect_filled(img, 4.0, vis.extreme_bg_color);
                        if h.note == "unavailable" {
                            p.text(img.center(), Align2::CENTER_CENTER, "unavailable", FontId::proportional(11.0), vis.weak_text_color());
                        } else if let Some(th) = self.fthumbs.get(&n) {
                            let rr = fit_rect(img.shrink(4.0), th.size_vec2());
                            let id = th.id();
                            self.checker(&p, rr);
                            p.image(id, rr, FULL_UV, Color32::WHITE);
                        } else if let Some(e) = self.ffailed.get(&n) {
                            p.text(img.center(), Align2::CENTER_CENTER, e, FontId::proportional(11.0), vis.error_fg_color);
                        } else {
                            if !self.fpending.contains(&n) {
                                if let Some(arc) = found.archives.get(&h.path).cloned() {
                                    self.fpending.insert(n);
                                    let (done, g, t) = (self.done.clone(), self.fgen, h.index);
                                    self.pool.push(Box::new(move || {
                                        let r = arc.best_image(t).map(|i| to_color_image(&i.thumbnail(CELL as usize))).map_err(|p| p.text().to_string());
                                        done.lock().unwrap().push(Done::FThumb(n, r, g));
                                    }));
                                }
                            }
                            p.text(img.center(), Align2::CENTER_CENTER, "…", FontId::proportional(16.0), vis.weak_text_color());
                        }
                        let current = self.open_path.as_ref() == Some(&h.path) && self.focus == Some(h.index);
                        let stroke = if current { Stroke::new(3.0, vis.strong_text_color()) } else { vis.widgets.noninteractive.bg_stroke };
                        p.rect_stroke(img, 4.0, stroke, StrokeKind::Inside);
                        let x = rect.min.x + 2.0;
                        p.text(pos2(x, img.max.y + 2.0), Align2::LEFT_TOP, format!("#{}  {}x{}", h.index, h.w, h.h), FontId::proportional(12.0), vis.strong_text_color());
                        let file = h.path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
                        p.text(pos2(x, img.max.y + 17.0), Align2::LEFT_TOP, file, FontId::proportional(10.5), vis.text_color());
                        p.text(pos2(x, img.max.y + 30.0), Align2::LEFT_TOP, &h.format, FontId::proportional(10.5), vis.weak_text_color());
                        let resp = resp.on_hover_text(h.path.display().to_string());
                        if resp.clicked() {
                            pick = Some(n);
                        }
                    }
                });
            }
        });
        pick
    }

    /// Extract every .ttpf of the sidebar folder.
    fn extract_folder(&mut self) {
        let Some(dir) = self.folder.clone() else { return };
        let what = if self.as_dds { "DDS files" } else { "PNGs" };
        let Some(out) = rfd::FileDialog::new().set_title(format!("Choose a folder for the {what}")).pick_folder() else { return };
        let out = out.join(dir.file_name().unwrap_or_default());
        self.message = None;
        self.fexport = Some((crate::folder::extract_all(dir, out.clone(), self.as_dds), out));
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
        let normal_maps = self.normal_maps.clone();
        let Some(dir) = rfd::FileDialog::new().set_title(if dds { "Choose a folder for the DDS files" } else { "Choose a folder for the PNGs" }).pick_folder() else { return };
        let dir = dir.join(&arc.stem);
        let done = Arc::new(AtomicUsize::new(0));
        let result = Arc::new(Mutex::new(None));
        self.export = Some(Export { total: list.len(), done: done.clone(), result: result.clone(), dir: dir.clone() });
        self.message = None;
        std::thread::spawn(move || {
            let r = archive::export(&arc, &list, &dir, dds, &normal_maps, &done);
            *result.lock().unwrap() = Some(r);
        });
    }

    fn save_one(&mut self, t: usize, layer: usize, level: usize) {
        let Some(arc) = self.arc.clone() else { return };
        let sfx = arc.textures[t].layer_suffix(layer);
        let name = if level == 0 { format!("{t}{sfx}.png") } else { format!("{t}{sfx}_mip{level}.png") };
        let Some(path) = rfd::FileDialog::new().set_file_name(&name).add_filter("PNG image", &["png"]).save_file() else { return };
        let nz = self.normal_maps.contains(&t);
        self.message = Some(match arc
            .image_in(t, layer, level)
            .map(|mut i| {
                if nz {
                    i.rebuild_normal_z();
                }
                i
            }).map_err(|p| p.text().to_string()).and_then(|img| archive::save_png(&img, &path)) {
            Ok(()) => (format!("Saved {}", path.display()), path.parent().map(|p| p.to_path_buf())),
            Err(e) => (format!("Couldn't save: {e}"), None),
        });
    }

    /// Copy a face / slice / mip level to the clipboard (decoded in the background).
    fn copy(&mut self, t: usize, layer: usize, level: Option<usize>) {
        let Some(arc) = self.arc.clone() else { return };
        let (done, nz) = (self.done.clone(), self.normal_maps.contains(&t));
        self.message = Some(("Copying…".into(), None));
        self.pool.push(Box::new(move || {
            let img = match level {
                Some(l) => arc.image_in(t, layer, l),
                None => arc.best_image_in(t, layer),
            }
            .map(|mut i| {
                if nz {
                    i.rebuild_normal_z();
                }
                i
            });
            let m = match img.map_err(|p| p.text().to_string()).and_then(|i| copy_image(&i).map(|_| (i.w, i.h))) {
                Ok((w, h)) => format!("Copied #{t} ({w}x{h}) to the clipboard"),
                Err(e) => format!("Couldn't copy: {e}"),
            };
            done.lock().unwrap().push(Done::Message(m, None));
        }));
    }

    /// Save a cube map as a 360-degree panorama PNG.
    fn save_panorama(&mut self, t: usize) {
        let Some(arc) = self.arc.clone() else { return };
        let Some(path) = rfd::FileDialog::new().set_file_name(format!("{t}_panorama.png")).add_filter("PNG image", &["png"]).save_file() else { return };
        let (done, z_up) = (self.done.clone(), self.z_up);
        self.message = Some(("Making panorama…".into(), None));
        self.pool.push(Box::new(move || {
            let r = arc.cube_faces(t).map_err(|p| p.text().to_string()).and_then(|f| archive::save_png(&crate::cube3d::panorama(&f, z_up), &path));
            let m = match r {
                Ok(()) => (format!("Saved {}", path.display()), path.parent().map(|p| p.to_path_buf())),
                Err(e) => (format!("Couldn't save: {e}"), None),
            };
            done.lock().unwrap().push(Done::Message(m.0, m.1));
        }));
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
                    Some(("show", v)) => self.show = Show::from_key(v),
                    Some(("normal", n)) => {
                        if let Ok(n) = n.parse() {
                            self.normal_maps.insert(n);
                        }
                    }
                    Some(("fsearch", q)) => {
                        self.fwin = true;
                        self.fquery = q.replace('+', " ");
                        if let Some(d) = self.folder.clone() {
                            self.fsearch = Some(crate::folder::search(d, self.fquery.clone(), self.fshow, false));
                        }
                    }
                    _ if a == "3d" => self.view3d = true,
                    _ if a == "yup" => {
                        let t = self.focus.unwrap_or(0);
                        let c = self.new_cube3d(t);
                        self.cube3d.get_or_insert(c).view.z_up = false;
                        self.z_up = false;
                    }
                    _ if a == "sphere" || a.starts_with("yaw=") => {
                        let t = self.focus.unwrap_or(0);
                        let c = self.new_cube3d(t);
                        let c = self.cube3d.get_or_insert(c);
                        if a == "sphere" {
                            c.view.sphere = true;
                        } else {
                            c.view.yaw = a[4..].parse::<f32>().unwrap_or(0.0).to_radians();
                        }
                    }
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
        let busy = !self.pending.is_empty()
            || self.full_pending.is_some()
            || self.fsearch.is_some()
            || !self.fpending.is_empty()
            || (self.view3d && self.cube3d.as_ref().is_some_and(|c| c.faces.is_none() && c.error.is_none()));
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
        if let Some(c) = &self.cube3d {
            self.z_up = c.view.z_up;
        }
        if self.test.is_none() {
            let now = self.settings();
            if now != self.saved {
                now.save();
                self.saved = now;
            }
        }
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
                ui.horizontal(|ui| {
                    if ui.button("Search all…").on_hover_text("Find textures in every .ttpf of this folder").clicked() {
                        self.fwin = true;
                    }
                    let what = if self.as_dds { "DDS" } else { "PNG" };
                    if ui
                        .add_enabled(self.fexport.is_none(), egui::Button::new("Extract all…"))
                        .on_hover_text(format!("Save every readable texture of every .ttpf in this folder as {what} (PNG/DDS: the choice in the toolbar), one folder per file"))
                        .clicked()
                    {
                        self.extract_folder();
                    }
                });
                if let Some((t, _)) = &self.fexport {
                    ui.horizontal(|ui| {
                        ui.add(egui::ProgressBar::new(t.fraction()).desired_width(150.0).text(format!("file {}/{}", t.done.load(Ordering::Relaxed), t.total.load(Ordering::Relaxed))));
                        if ui.small_button("Stop").clicked() {
                            t.cancel.store(true, Ordering::Relaxed);
                        }
                    });
                }
                ui.separator();
                egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| self.tree(ui, root.clone(), &ctx));
            });
        }
        if self.fwin && self.folder.is_some() {
            let mut open = true;
            egui::Window::new("Search all files").open(&mut open).default_size([760.0, 620.0]).show(&ctx, |ui| self.search_window(ui, &ctx));
            self.fwin = open;
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
            let counts: Vec<usize> = Show::ALL.iter().map(|s| self.arc.as_ref().map_or(0, |a| a.textures.iter().filter(|t| s.wants(t)).count())).collect();
            egui::ComboBox::from_id_salt("show")
                .width(150.0)
                .selected_text(format!("Show: {}", self.show.name().to_lowercase()))
                .show_ui(ui, |ui| {
                    for (s, n) in Show::ALL.into_iter().zip(&counts) {
                        if *n > 0 || s == Show::All {
                            ui.selectable_value(&mut self.show, s, format!("{}  ({n})", s.name()));
                        }
                    }
                });
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
                    self.cube3d = None;
                    self.clear_fthumbs();
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
        if !matches!(tex.status, Status::Unavailable(_)) {
            let cube = tex.cube;
            resp.context_menu(|ui| {
                if ui.button("Copy image").clicked() {
                    self.copy(t, 0, None);
                    ui.close();
                }
                if ui.button("Save PNG…").clicked() {
                    let l = arc.textures[t].status.shown_level().unwrap_or(0);
                    self.save_one(t, 0, l);
                    ui.close();
                }
                if ui.button("Save DDS…").clicked() {
                    self.save_dds(t);
                    ui.close();
                }
                if cube && ui.button("Save panorama…").clicked() {
                    self.save_panorama(t);
                    ui.close();
                }
            });
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
        if tex.cube {
            ui.horizontal(|ui| {
                ui.selectable_value(&mut self.view3d, false, "Faces");
                ui.selectable_value(&mut self.view3d, true, "3D").on_hover_text("See the cube map in 3D");
                if self.view3d {
                    let c = self.new_cube3d(t);
                    let view = &mut self.cube3d.get_or_insert(c).view;
                    ui.separator();
                    ui.selectable_value(&mut view.sphere, false, "Look around").on_hover_text("From inside, like a sky");
                    ui.selectable_value(&mut view.sphere, true, "Sphere").on_hover_text("Wrapped on a ball");
                    ui.separator();
                    ui.label("Up:");
                    ui.selectable_value(&mut view.z_up, true, "Z").on_hover_text("Z is up, as in RAGE games");
                    ui.selectable_value(&mut view.z_up, false, "Y").on_hover_text("Y is up, the Direct3D convention");
                    if ui.small_button("Reset").clicked() {
                        *view = crate::cube3d::View { sphere: view.sphere, z_up: view.z_up, ..Default::default() };
                    }
                }
            });
        }
        // image with zoom / pan
        let below = 110.0 + 120.0 + if tex.layer_count() > 1 { MIP_THUMB as f32 + 42.0 } else { 0.0 } + if tex.cube { 24.0 } else { 0.0 };
        let area_size = vec2(ui.available_width(), (ui.available_height() - below).max(200.0));
        let (area, resp) = ui.allocate_exact_size(area_size, Sense::click_and_drag());
        let p = ui.painter_at(area);
        p.rect_filled(area, 4.0, vis.extreme_bg_color);
        if tex.cube && self.view3d {
            self.cube_view(ui, &p, area, &resp, t);
        } else if let Some(l) = level {
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
        if tex.fmt.is_some_and(|f| f.may_be_normal_map()) {
            let mut on = self.normal_maps.contains(&t);
            if ui
                .checkbox(&mut on, "Normal map: rebuild blue")
                .on_hover_text("For normal maps that keep only X and Y (red and green): show blue as the Z they imply. Untick to see the stored colours again. Affects the preview, Copy, Save PNG and Extract; DDS keeps the original data.")
                .changed()
            {
                if on {
                    self.normal_maps.insert(t);
                } else {
                    self.normal_maps.remove(&t);
                }
                // decode this texture's pictures again; drop what is being decoded with the old setting
                self.hdr_gen += 1;
                self.pool.clear();
                self.pending.clear();
                self.full_pending = None;
                self.thumbs.retain(|k, _| k.0 != t);
                self.failed.retain(|k, _| k.0 != t);
                if self.full.as_ref().is_some_and(|f| f.0.0 == t) {
                    self.full = None;
                }
            }
        }
        ui.add_space(4.0);
        if let Some(l) = level {
            ui.horizontal(|ui| {
                if ui.button("Copy").on_hover_text("Copy this image to the clipboard").clicked() {
                    self.copy(t, self.layer, Some(l));
                }
                if ui.button("Save PNG…").clicked() {
                    self.save_one(t, self.layer, l);
                }
                if tex.cube && ui.button("Save panorama…").on_hover_text("The whole cube map as one 360° picture (2:1), for sky viewers and 3D programs").clicked() {
                    self.save_panorama(t);
                }
                if ui.button("Save DDS…").on_hover_text("The original data: lossless, keeps the compression, HDR values and mip levels").clicked() {
                    self.save_dds(t);
                }
            });
        }
    }
}
