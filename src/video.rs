//! Find Bink 2 videos stored as-is inside a large game archive (.rpf), save them as .bk2 files, and
//! optionally convert them to MP4. Converting needs two free tools: RAD Video Tools (from RAD Game Tools) to decode
//! the Bink 2 video, since the open-source Bink 2 decoders don't decode this revision correctly, and FFmpeg to
//! encode the MP4 (H.264 + AAC).
//!
//! The archive's file table is encrypted, so the videos are found by their header: "KB2" + a version
//! letter, then the file size, frame count, width, height and frame rate, which must all be plausible.
//! The size in the header is exact, so each video is copied byte for byte.

use memchr::memmem;
use memmap2::Mmap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone)]
pub struct Video {
    pub offset: usize,
    pub size: usize,
    pub width: u32,
    pub height: u32,
    pub frames: u32,
    pub seconds: f32,
}

impl Video {
    pub fn name(&self) -> String {
        format!("{:010x}_{}x{}_{:.0}s", self.offset, self.width, self.height, self.seconds)
    }
}

fn u32_at(d: &[u8], o: usize) -> u32 {
    u32::from_le_bytes(d[o..o + 4].try_into().unwrap())
}

fn map(path: &Path) -> Result<Mmap, String> {
    let file = std::fs::File::open(path).map_err(|e| format!("Can't open {}: {e}", path.display()))?;
    // SAFETY: read-only mapping; the file is not modified while open.
    unsafe { Mmap::map(&file) }.map_err(|e| format!("Can't read {}: {e}", path.display()))
}

/// Find the videos in an archive. `scanned`/`total` report progress in bytes.
pub fn scan(path: &Path, scanned: &AtomicUsize, total: &AtomicUsize) -> Result<Vec<Video>, String> {
    if !path.extension().is_some_and(|e| e.eq_ignore_ascii_case("rpf")) {
        return Err(format!("{} is not an .rpf file.", path.file_name().unwrap_or_default().to_string_lossy()));
    }
    let d = map(path)?;
    total.store(d.len(), Ordering::Relaxed);
    let mut out = Vec::new();
    let mut pos = 0;
    let finder = memmem::Finder::new(b"KB2");
    while let Some(rel) = finder.find(&d[pos..]) {
        let i = pos + rel;
        pos = i + 1;
        scanned.store(i, Ordering::Relaxed);
        if i + 44 > d.len() || !d[i + 3].is_ascii_lowercase() {
            continue;
        }
        let size = u32_at(&d, i + 4) as usize + 8;
        let (frames, largest) = (u32_at(&d, i + 8), u32_at(&d, i + 12) as usize);
        let (width, height) = (u32_at(&d, i + 20), u32_at(&d, i + 24));
        let (num, den) = (u32_at(&d, i + 28), u32_at(&d, i + 32));
        let ok = (1..=1_000_000).contains(&frames)
            && (16..=8192).contains(&width)
            && (16..=8192).contains(&height)
            && num > 0
            && den > 0
            && (1.0..=240.0).contains(&(num as f32 / den as f32))
            && size >= 1024
            && i + size <= d.len()
            && largest <= size;
        if !ok {
            continue;
        }
        out.push(Video { offset: i, size, width, height, frames, seconds: frames as f32 * den as f32 / num as f32 });
        pos = i + size; // a video can't contain another one
    }
    scanned.store(d.len(), Ordering::Relaxed);
    Ok(out)
}

/// RAD Video Tools, if installed in a usual place.
pub fn find_rad() -> Option<PathBuf> {
    let mut dirs = vec![PathBuf::from(r"C:\Program Files (x86)\RADVideo"), PathBuf::from(r"C:\Program Files\RADVideo"), PathBuf::from(r"C:\RADVideo")];
    if let Some(l) = std::env::var_os("LOCALAPPDATA") {
        dirs.push(PathBuf::from(l).join("RADVideo"));
    }
    dirs.into_iter().flat_map(|d| [d.join("radvideo64.exe"), d.join("radvideo.exe")]).find(|p| p.is_file())
}

/// FFmpeg, if it's on the PATH, next to this app, or in a usual place.
pub fn find_ffmpeg() -> Option<PathBuf> {
    let mut dirs: Vec<PathBuf> = std::env::var_os("PATH").map(|p| std::env::split_paths(&p).collect()).unwrap_or_default();
    if let Some(d) = std::env::current_exe().ok().and_then(|e| e.parent().map(|p| p.to_path_buf())) {
        dirs.push(d);
    }
    dirs.push(PathBuf::from(r"C:\ffmpeg\bin"));
    dirs.push(PathBuf::from(r"C:\Program Files\ffmpeg\bin"));
    if let Some(l) = std::env::var_os("LOCALAPPDATA") {
        dirs.push(PathBuf::from(l).join(r"Microsoft\WinGet\Links"));
    }
    dirs.into_iter().map(|d| d.join("ffmpeg.exe")).find(|p| p.is_file())
}

/// The tools needed for MP4 conversion.
#[derive(Clone)]
pub struct Converter {
    pub rad: PathBuf,
    pub ffmpeg: PathBuf,
}

fn run(cmd: &mut std::process::Command) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        cmd.creation_flags(0x0800_0000); // CREATE_NO_WINDOW: no console window for ffmpeg
    }
    let name = Path::new(cmd.get_program()).file_name().unwrap_or_default().to_string_lossy().into_owned();
    let out = cmd.stdin(std::process::Stdio::null()).output().map_err(|e| format!("can't run {name}: {e}"))?;
    if out.status.success() {
        return Ok(());
    }
    let err = String::from_utf8_lossy(&out.stderr);
    Err(format!("{name} failed ({}){}", out.status, err.lines().last().map(|l| format!(": {l}")).unwrap_or_default()))
}

/// RAD Video Tools reads anything starting with '/' as a switch, so give it backslash paths only.
fn win(p: &Path) -> String {
    p.to_string_lossy().replace('/', "\\")
}

fn rad(c: &Converter, bk2: &Path, out: &Path, switches: &[String]) -> Result<(), String> {
    run(std::process::Command::new(&c.rad).arg("BinkConv").arg(win(bk2)).arg(win(out)).args(switches).args(["/O", "/#"]))
}

fn ffmpeg(c: &Converter, args: &[&std::ffi::OsStr]) -> Result<(), String> {
    run(std::process::Command::new(&c.ffmpeg).args(["-v", "error", "-y"]).args(args))
}

/// Convert one .bk2 video to .mp4: RAD Video Tools decodes it to uncompressed AVI (in pieces, as its AVIs must stay
/// under 2 GB) plus a WAV of the sound, and FFmpeg encodes each piece to H.264 and joins them with the sound as AAC.
pub fn convert(c: &Converter, v: &Video, bk2: &Path, mp4: &Path) -> Result<(), String> {
    let tmp = mp4.with_extension("tmp");
    std::fs::create_dir_all(&tmp).map_err(|e| format!("Can't create {}: {e}", tmp.display()))?;
    let r = convert_in(c, v, bk2, mp4, &tmp);
    let _ = std::fs::remove_dir_all(&tmp);
    if r.is_err() {
        let _ = std::fs::remove_file(mp4);
    }
    r
}

fn convert_in(c: &Converter, v: &Video, bk2: &Path, mp4: &Path, tmp: &Path) -> Result<(), String> {
    let wav = tmp.join("sound.wav");
    let sound = rad(c, bk2, &wav, &["/NoVideo".into()]).is_ok() && wav.metadata().is_ok_and(|m| m.len() > 44);
    let frame = v.width as u64 * v.height as u64 * 3;
    let per = (1_800_000_000 / frame).max(1) as u32;
    let mut list = String::new();
    let mut first = 1;
    let mut k = 0;
    while first <= v.frames {
        let last = (first + per - 1).min(v.frames);
        let end = if last == v.frames { "e".to_string() } else { last.to_string() };
        let avi = tmp.join(format!("{k}.avi"));
        let part = tmp.join(format!("{k}.mp4"));
        rad(c, bk2, &avi, &["/NoAudio".into(), format!("/Include({first}-{end})")])?;
        ffmpeg(c, &["-i".as_ref(), avi.as_os_str(), "-c:v".as_ref(), "libx264".as_ref(), "-preset".as_ref(), "fast".as_ref(), "-crf".as_ref(), "18".as_ref(), "-pix_fmt".as_ref(), "yuv420p".as_ref(), part.as_os_str()])?;
        let _ = std::fs::remove_file(&avi);
        list += &format!("file '{k}.mp4'\n");
        first = last + 1;
        k += 1;
    }
    let lst = tmp.join("parts.txt");
    std::fs::write(&lst, list).map_err(|e| e.to_string())?;
    let mut args: Vec<&std::ffi::OsStr> = vec!["-f".as_ref(), "concat".as_ref(), "-safe".as_ref(), "0".as_ref(), "-i".as_ref(), lst.as_os_str()];
    if sound {
        args.extend::<[&std::ffi::OsStr; 8]>(["-i".as_ref(), wav.as_os_str(), "-map".as_ref(), "0:v".as_ref(), "-map".as_ref(), "1:a".as_ref(), "-c:a".as_ref(), "aac".as_ref()]);
        args.extend::<[&std::ffi::OsStr; 2]>(["-b:a".as_ref(), "192k".as_ref()]);
    }
    args.extend::<[&std::ffi::OsStr; 5]>(["-c:v".as_ref(), "copy".as_ref(), "-movflags".as_ref(), "+faststart".as_ref(), mp4.as_os_str()]);
    ffmpeg(c, &args)
}

pub struct Saved {
    pub saved: usize,
    pub converted: usize,
    pub failed: usize,
    /// the first conversion error, if any
    pub error: Option<String>,
}

/// Save `videos` from `archive` into `dir` as .bk2, and convert each to .mp4 if `converter` is given.
pub fn save(archive: &Path, videos: &[Video], dir: &Path, converter: Option<&Converter>, done: &AtomicUsize) -> Result<Saved, String> {
    let d = map(archive)?;
    std::fs::create_dir_all(dir).map_err(|e| format!("Can't create {}: {e}", dir.display()))?;
    let mut r = Saved { saved: 0, converted: 0, failed: 0, error: None };
    for v in videos {
        let bk2 = dir.join(format!("{}.bk2", v.name()));
        std::fs::write(&bk2, &d[v.offset..v.offset + v.size]).map_err(|e| e.to_string())?;
        r.saved += 1;
        if let Some(c) = converter {
            match convert(c, v, &bk2, &dir.join(format!("{}.mp4", v.name()))) {
                Ok(()) => r.converted += 1,
                Err(e) => {
                    r.failed += 1;
                    r.error.get_or_insert(e);
                }
            }
        }
        done.fetch_add(1, Ordering::Relaxed);
    }
    Ok(r)
}
