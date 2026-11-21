//! Work on every .ttpf in a folder (and its subfolders): search them all, or extract them all.

use crate::archive::{self, Archive, Progress, Status};
use crate::gui::{Show, matches};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Most results a search keeps.
pub const MAX_HITS: usize = 50_000;

/// A background job over the folder's files.
pub struct Task<T> {
    /// files finished, out of `total`
    pub done: Arc<AtomicUsize>,
    pub total: Arc<AtomicUsize>,
    pub cancel: Arc<AtomicBool>,
    pub result: Arc<Mutex<Option<T>>>,
}

impl<T: Send + 'static> Task<T> {
    fn start(work: impl FnOnce(&AtomicUsize, &AtomicUsize, &AtomicBool) -> T + Send + 'static) -> Task<T> {
        let t = Task {
            done: Arc::new(AtomicUsize::new(0)),
            total: Arc::new(AtomicUsize::new(0)),
            cancel: Arc::new(AtomicBool::new(false)),
            result: Arc::new(Mutex::new(None)),
        };
        let (d, n, c, r) = (t.done.clone(), t.total.clone(), t.cancel.clone(), t.result.clone());
        std::thread::spawn(move || {
            let v = work(&d, &n, &c);
            *r.lock().unwrap() = Some(v);
        });
        t
    }

    pub fn fraction(&self) -> f32 {
        self.done.load(Ordering::Relaxed) as f32 / self.total.load(Ordering::Relaxed).max(1) as f32
    }

    pub fn take(&self) -> Option<T> {
        self.result.lock().unwrap().take()
    }
}

/// Every .ttpf under `dir`, sorted.
pub fn ttpf_files(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(d) = stack.pop() {
        let Ok(rd) = std::fs::read_dir(&d) else { continue };
        for e in rd.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if archive::is_ttpf_name(&p) {
                out.push(p);
            }
        }
    }
    out.sort();
    out
}

/// One search result.
pub struct Hit {
    pub path: PathBuf,
    pub index: usize,
    pub w: usize,
    pub h: usize,
    pub format: String,
    pub note: &'static str,
}

pub struct Found {
    pub hits: Vec<Hit>,
    /// more matched than MAX_HITS
    pub truncated: bool,
    pub files: usize,
    /// files that couldn't be opened
    pub failed: usize,
    /// the files with results, kept open to show their textures
    pub archives: HashMap<PathBuf, Arc<Archive>>,
}

/// Find the textures matching `query` (as typed in the search box) and `show` in every .ttpf under `dir`.
pub fn search(dir: PathBuf, query: String, show: Show, unavailable: bool) -> Task<Found> {
    Task::start(move |done, total, cancel| {
        let files = ttpf_files(&dir);
        total.store(files.len(), Ordering::Relaxed);
        let q = query.trim().trim_start_matches('#').to_lowercase();
        let mut f = Found { hits: Vec::new(), truncated: false, files: files.len(), failed: 0, archives: HashMap::new() };
        for p in files {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            match Archive::open(&p, &Progress::default()) {
                Ok(a) => {
                    let before = f.hits.len();
                    for t in a.textures.iter().filter(|t| matches(t, &q, show, unavailable)) {
                        if f.hits.len() == MAX_HITS {
                            f.truncated = true;
                            break;
                        }
                        let note = match t.status {
                            Status::Full => "",
                            Status::Reduced(_) => "smaller size only",
                            Status::Unavailable(_) => "unavailable",
                        };
                        f.hits.push(Hit { path: p.clone(), index: t.index, w: t.w, h: t.h, format: t.format_name(), note });
                    }
                    if f.hits.len() > before {
                        f.archives.insert(p.clone(), Arc::new(a));
                    }
                }
                Err(_) => f.failed += 1,
            }
            done.fetch_add(1, Ordering::Relaxed);
        }
        f
    })
}

pub struct Extracted {
    pub written: usize,
    pub skipped: usize,
    pub files: usize,
    pub failed: usize,
}

/// Extract every readable texture of every .ttpf under `dir` into `out`, one subfolder per archive (keeping
/// the folder structure), as PNG or DDS.
pub fn extract_all(dir: PathBuf, out: PathBuf, dds: bool) -> Task<Result<Extracted, String>> {
    Task::start(move |done, total, cancel| {
        let files = ttpf_files(&dir);
        total.store(files.len(), Ordering::Relaxed);
        let mut r = Extracted { written: 0, skipped: 0, files: files.len(), failed: 0 };
        for p in files {
            if cancel.load(Ordering::Relaxed) {
                break;
            }
            let rel = p.strip_prefix(&dir).unwrap_or(&p).with_extension("");
            match Archive::open(&p, &Progress::default()) {
                Ok(a) => {
                    let all: Vec<usize> = (0..a.textures.len()).collect();
                    let (w, s) = archive::export(&a, &all, &out.join(rel), dds, &AtomicUsize::new(0))?;
                    r.written += w;
                    r.skipped += s;
                }
                Err(_) => r.failed += 1,
            }
            done.fetch_add(1, Ordering::Relaxed);
        }
        Ok(r)
    })
}
