//! The Videos part of the app: find the Bink 2 videos in an .rpf archive, save them, optionally as MP4.

use crate::video::{self, Converter, Saved, Video};
use eframe::egui::{self, RichText};
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

struct Job<T> {
    done: Arc<AtomicUsize>,
    total: Arc<AtomicUsize>,
    result: Arc<Mutex<Option<Result<T, String>>>>,
}

impl<T> Job<T> {
    fn new() -> Job<T> {
        Job { done: Arc::new(AtomicUsize::new(0)), total: Arc::new(AtomicUsize::new(0)), result: Arc::new(Mutex::new(None)) }
    }
    fn fraction(&self) -> f32 {
        self.done.load(Ordering::Relaxed) as f32 / self.total.load(Ordering::Relaxed).max(1) as f32
    }
    fn take(&self) -> Option<Result<T, String>> {
        self.result.lock().unwrap().take()
    }
}

pub struct VideosTab {
    file: Option<PathBuf>,
    scan: Option<Job<Vec<Video>>>,
    videos: Vec<Video>,
    selected: BTreeSet<usize>,
    rad: Option<PathBuf>,
    ffmpeg: Option<PathBuf>,
    convert: bool,
    save: Option<(Job<Saved>, PathBuf)>,
    message: Option<(String, Option<PathBuf>)>,
}

impl VideosTab {
    pub fn new() -> VideosTab {
        let (rad, ffmpeg) = (video::find_rad(), video::find_ffmpeg());
        VideosTab {
            file: None,
            scan: None,
            videos: Vec::new(),
            selected: BTreeSet::new(),
            convert: rad.is_some() && ffmpeg.is_some(),
            rad,
            ffmpeg,
            save: None,
            message: None,
        }
    }

    pub fn busy(&self) -> bool {
        self.scan.is_some() || self.save.is_some()
    }

    /// Scan an .rpf for videos.
    pub fn open(&mut self, path: PathBuf) {
        if self.busy() {
            return;
        }
        self.videos.clear();
        self.selected.clear();
        self.message = None;
        let job = Job::new();
        let (done, total, result) = (job.done.clone(), job.total.clone(), job.result.clone());
        let p = path.clone();
        std::thread::spawn(move || {
            let r = video::scan(&p, &done, &total);
            *result.lock().unwrap() = Some(r);
        });
        self.file = Some(path);
        self.scan = Some(job);
    }

    fn start_save(&mut self, list: Vec<usize>) {
        let (Some(file), false) = (self.file.clone(), list.is_empty()) else { return };
        let Some(dir) = rfd::FileDialog::new().set_title("Choose a folder for the videos").pick_folder() else { return };
        let dir = dir.join(file.file_stem().unwrap_or_default());
        let vids: Vec<Video> = list.iter().map(|&i| self.videos[i].clone()).collect();
        let converter = self.converter().filter(|_| self.convert);
        let job = Job::new();
        job.total.store(vids.len(), Ordering::Relaxed);
        let (done, result, d) = (job.done.clone(), job.result.clone(), dir.clone());
        std::thread::spawn(move || {
            let r = video::save(&file, &vids, &d, converter.as_ref(), &done);
            *result.lock().unwrap() = Some(r);
        });
        self.message = None;
        self.save = Some((job, dir));
    }

    fn converter(&self) -> Option<Converter> {
        Some(Converter { rad: self.rad.clone()?, ffmpeg: self.ffmpeg.clone()? })
    }

    fn poll(&mut self) {
        if let Some(r) = self.scan.as_ref().and_then(|j| j.take()) {
            match r {
                Ok(v) => {
                    if v.is_empty() {
                        self.message = Some(("No videos found in this file.".into(), None));
                    }
                    self.videos = v;
                }
                Err(e) => self.message = Some((e, None)),
            }
            self.scan = None;
        }
        if let Some(r) = self.save.as_ref().and_then(|(j, _)| j.take()) {
            let dir = self.save.take().map(|(_, d)| d);
            self.message = Some(match r {
                Ok(s) => {
                    let mut m = format!("Saved {} video{}", s.saved, if s.saved == 1 { "" } else { "s" });
                    if s.converted + s.failed > 0 {
                        m += &format!(", {} converted to MP4", s.converted);
                        if s.failed > 0 {
                            m += &format!(" ({} failed to convert: {})", s.failed, s.error.unwrap_or_default());
                        }
                    }
                    (m, dir)
                }
                Err(e) => (format!("Saving failed: {e}"), None),
            });
        }
    }

    pub fn ui(&mut self, ui: &mut egui::Ui) {
        self.poll();
        if self.busy() {
            ui.ctx().request_repaint_after(Duration::from_millis(150));
        }
        let vis = ui.visuals().clone();
        ui.horizontal_wrapped(|ui| {
            if ui.add_enabled(!self.busy(), egui::Button::new("Open .rpf…")).clicked() {
                if let Some(p) = rfd::FileDialog::new().set_title("Choose a game archive").add_filter("RPF archive", &["rpf"]).pick_file() {
                    self.open(p);
                }
            }
            if let Some(f) = &self.file {
                ui.label(RichText::new(f.file_name().unwrap_or_default().to_string_lossy()).strong());
            }
            if self.videos.is_empty() {
                return;
            }
            ui.separator();
            if ui.button("Select all").clicked() {
                self.selected = (0..self.videos.len()).collect();
            }
            if ui.button("Select none").clicked() {
                self.selected.clear();
            }
            ui.separator();
            let busy = self.busy();
            let n = self.selected.len();
            if ui.add_enabled(n > 0 && !busy, egui::Button::new(format!("Save selected ({n})"))).clicked() {
                let l = self.selected.iter().copied().collect();
                self.start_save(l);
            }
            if ui.add_enabled(!busy, egui::Button::new(format!("Save all ({})", self.videos.len()))).clicked() {
                let l = (0..self.videos.len()).collect();
                self.start_save(l);
            }
            ui.separator();
            match self.converter() {
                Some(c) => {
                    ui.checkbox(&mut self.convert, "Also convert to MP4")
                        .on_hover_text(format!("Uses {} and {}", c.rad.display(), c.ffmpeg.display()));
                }
                None => {
                    ui.label(RichText::new("MP4 conversion needs RAD Video Tools and FFmpeg").color(vis.weak_text_color()));
                    for (slot, name, exe) in [(&mut self.rad, "RAD Video Tools", "radvideo64.exe"), (&mut self.ffmpeg, "FFmpeg", "ffmpeg.exe")] {
                        if slot.is_none() && ui.button(format!("Locate {exe}…")).clicked() {
                            if let Some(p) = rfd::FileDialog::new().add_filter(name, &["exe"]).pick_file() {
                                *slot = Some(p);
                            }
                        }
                    }
                    self.convert = self.rad.is_some() && self.ffmpeg.is_some();
                }
            }
        });
        ui.horizontal(|ui| {
            if let Some(j) = &self.scan {
                ui.add(egui::ProgressBar::new(j.fraction()).desired_width(260.0).text("Finding videos…"));
            }
            if let Some((j, _)) = &self.save {
                let t = j.total.load(Ordering::Relaxed);
                ui.add(egui::ProgressBar::new(j.fraction()).desired_width(260.0).text(format!("Saving {}/{}", j.done.load(Ordering::Relaxed), t)));
            }
            if let Some((m, dir)) = self.message.clone() {
                ui.label(m);
                if let Some(d) = dir {
                    if ui.small_button("Open folder").clicked() {
                        crate::gui::open_folder(&d);
                    }
                }
            }
        });
        ui.separator();
        if self.file.is_none() {
            ui.vertical_centered(|ui| {
                ui.add_space(ui.available_height() * 0.3);
                ui.label(RichText::new("Open or drop a game archive (.rpf) to find its videos").size(22.0));
                ui.add_space(6.0);
                ui.label(RichText::new("Videos are Bink 2 (.bk2). They're saved as-is, and converted to MP4 if RAD Video Tools and FFmpeg are installed.").color(vis.weak_text_color()));
            });
            return;
        }
        if self.videos.is_empty() {
            return;
        }
        let total: usize = self.videos.iter().map(|v| v.size).sum();
        ui.label(format!("{} videos, {:.0} MB", self.videos.len(), total as f64 / 1048576.0));
        egui::ScrollArea::vertical().auto_shrink(false).show(ui, |ui| {
            egui::Grid::new("videos").striped(true).num_columns(5).spacing([24.0, 4.0]).show(ui, |ui| {
                ui.label("");
                ui.label(RichText::new("Name").strong());
                ui.label(RichText::new("Resolution").strong());
                ui.label(RichText::new("Length").strong());
                ui.label(RichText::new("Size").strong());
                ui.end_row();
                for (i, v) in self.videos.iter().enumerate() {
                    let mut on = self.selected.contains(&i);
                    if ui.checkbox(&mut on, "").changed() {
                        if on {
                            self.selected.insert(i);
                        } else {
                            self.selected.remove(&i);
                        }
                    }
                    ui.label(v.name());
                    ui.label(format!("{}x{}", v.width, v.height));
                    ui.label(format!("{:.1} s", v.seconds));
                    ui.label(format!("{:.1} MB", v.size as f64 / 1048576.0));
                    ui.end_row();
                }
            });
        });
    }
}
