//! A .ttpf archive: its header, its blocks and its textures.
//!
//! Layout (little-endian):
//! ```text
//! 0x00  "RDS5"
//! 0x04  u32  number of blocks
//! 0x08  u32  total size of the unpacked data, low 32 bits
//! 0x0C  u32  bits 0-7: total size, bits 32-39; rest unknown
//! 0x10  u32  position of the ETPF section / 256
//! 0x14  u32  unknown
//! 0x18  u64 per block:  bits  0-25  where the block ends in the file / 256
//!                       bits 26-29  how the NEXT block is stored: 0 = as-is, 1-7 = BCPack, 8 = zlib
//!                       bits 30-51  where the block ends in the unpacked data / 4096
//!                       bits 52-63  unknown (checksum?)
//!       The first block starts at the ETPF position in the file and at 0 in the unpacked data; it holds
//!       the ETPF section itself and is stored as-is.
//! then, at the ETPF position:
//!       "ETPF", u32 unknown, u32 number of textures, u32 section size
//!       16 bytes per texture:
//!         dword0  bits  0-13 width-1, 14-27 height-1, 28-31 unknown (mip related)
//!         dword1  bits  0-9  DXGI format << 3 | flags, bits 24-25 tile mode (0 = 64 KB tiles, else 4 KB)
//!         dword2  bits  0-20 start of the texture in the unpacked data / 4096
//!         dword3  unknown
//!       then a sorted list of 8-byte name hashes
//! ```
//! The blocks are consecutive chunks of one unpacked stream; most are zlib, a few are stored as-is, and
//! some use BCPack, a console texture compression with no public decoder. Each texture is a contiguous run of that stream holding its mip levels,
//! smallest first: the top level ends where the next texture starts, the next level ends where the top
//! level starts, and so on. Levels smaller than a tile are stored padded to a whole tile; below the top
//! level such levels may be packed together, so they are not used.
//!
//! Blocks are only unpacked when a texture needs them (with a cache), so huge archives open instantly.

use crate::bcn::{self, Fmt, Rgba};
use crate::tile::Tile;
use flate2::{Decompress, FlushDecompress, Status as ZStatus};
use memmap2::Mmap;
use rayon::prelude::*;
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

/// Why a texture (level) can't be shown.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Problem {
    /// Not in the file (it was cut off, e.g. a partial download).
    Missing,
    /// Stored with a compression this app can't read.
    Compression,
    /// A pixel format this app can't decode.
    Format,
    /// Doesn't fit the space the header gives it.
    Layout,
}

impl Problem {
    pub fn text(self) -> &'static str {
        match self {
            Problem::Missing => "not in the file (cut off)",
            Problem::Compression => "unsupported compression",
            Problem::Format => "unsupported format",
            Problem::Layout => "doesn't fit its space",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Status {
    /// Full size available.
    Full,
    /// Only a smaller mip level can be read; this is the largest one.
    Reduced(usize),
    Unavailable(Problem),
}

impl Status {
    pub fn shown_level(self) -> Option<usize> {
        match self {
            Status::Full => Some(0),
            Status::Reduced(l) => Some(l),
            Status::Unavailable(_) => None,
        }
    }
}

/// One mip level: its size, the padded size it is stored at, and where it is.
#[derive(Clone, Debug)]
pub struct Level {
    pub w: usize,
    pub h: usize,
    pw: usize,
    ph: usize,
    a: usize,
    b: usize,
    pub state: Result<(), Problem>,
}

pub struct Texture {
    pub index: usize,
    pub w: usize,
    pub h: usize,
    pub dxgi: u32,
    pub fmt: Option<Fmt>,
    pub tile: Tile,
    /// Readable mip levels, top first (empty if the format is unsupported).
    pub levels: Vec<Level>,
    pub status: Status,
}

impl Texture {
    pub fn format_name(&self) -> String {
        dxgi_name(self.dxgi)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Zlib,
    Stored,
    /// BCPack or anything else this app can't unpack
    Other,
    Missing,
}

struct Block {
    file_off: usize,
    file_len: usize,
    /// where it starts in the unpacked stream, and how many bytes it supplies
    start: usize,
    len: usize,
    kind: Kind,
}

/// Progress of opening a file, shared with the UI thread.
#[derive(Default)]
pub struct Progress {
    pub stage: Mutex<String>,
    pub done: AtomicUsize,
    pub total: AtomicUsize,
    pub finished: AtomicBool,
}

impl Progress {
    pub fn fraction(&self) -> f32 {
        let t = self.total.load(Ordering::Relaxed);
        if t == 0 { 0.0 } else { self.done.load(Ordering::Relaxed) as f32 / t as f32 }
    }
    fn stage(&self, s: &str) {
        *self.stage.lock().unwrap() = s.to_string();
    }
}

struct Cache {
    blocks: HashMap<usize, Arc<Vec<u8>>>,
    order: VecDeque<usize>,
    bytes: usize,
}

const CACHE_BYTES: usize = 768 << 20;

pub struct Archive {
    pub path: PathBuf,
    /// File name without extension; used to name the export folder.
    pub stem: String,
    map: Mmap,
    blocks: Vec<Block>,
    pub textures: Vec<Texture>,
    cache: Mutex<Cache>,
}

fn u32_at(d: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(o..o + 4)?.try_into().ok()?))
}

/// DXGI format number -> decodable format.
fn format_of(dxgi: u32) -> Option<Fmt> {
    Some(match dxgi {
        70..=72 => Fmt::BC1,
        76..=78 => Fmt::BC3,
        79 | 80 => Fmt::BC4,
        82 | 83 => Fmt::BC5,
        94 | 95 => Fmt::BC6H,
        97..=99 => Fmt::BC7,
        73..=75 => Fmt::BC2,
        96 => Fmt::BC6HS,
        10 => Fmt::RGBA16F,
        11 => Fmt::RGBA16,
        27..=29 => Fmt::RGBA8,
        54 => Fmt::R16F,
        61 => Fmt::R8,
        65 => Fmt::A8,
        87 | 90 | 91 => Fmt::BGRA8,
        _ => return None,
    })
}

pub fn dxgi_name(dxgi: u32) -> String {
    let s = match dxgi {
        10 => "R16G16B16A16_FLOAT",
        11 => "R16G16B16A16_UNORM",
        28 => "R8G8B8A8_UNORM",
        29 => "R8G8B8A8_UNORM_SRGB",
        54 => "R16_FLOAT",
        74 => "BC2_UNORM",
        61 => "R8_UNORM",
        65 => "A8_UNORM",
        71 => "BC1_UNORM",
        72 => "BC1_UNORM_SRGB",
        77 => "BC3_UNORM",
        78 => "BC3_UNORM_SRGB",
        80 => "BC4_UNORM",
        81 => "BC4_SNORM",
        83 => "BC5_UNORM",
        84 => "BC5_SNORM",
        87 => "B8G8R8A8_UNORM",
        91 => "B8G8R8A8_UNORM_SRGB",
        95 => "BC6H_UF16",
        96 => "BC6H_SF16",
        98 => "BC7_UNORM",
        99 => "BC7_UNORM_SRGB",
        _ => "",
    };
    if s.is_empty() { format!("DXGI format {dxgi}") } else { s.to_string() }
}

/// Inflate a zlib stream. Err(true) = cut off by the end of the file, Err(false) = corrupt.
fn inflate(d: &[u8], expect: usize) -> Result<Vec<u8>, bool> {
    let mut z = Decompress::new(true);
    let mut out = Vec::with_capacity(expect.min(64 << 20));
    loop {
        if out.len() == out.capacity() {
            out.reserve(1 << 16);
        }
        let before = (z.total_in(), z.total_out());
        let st = z.decompress_vec(&d[z.total_in() as usize..], &mut out, FlushDecompress::None).map_err(|_| false)?;
        if st == ZStatus::StreamEnd {
            return Ok(out);
        }
        if (z.total_in(), z.total_out()) == before {
            return Err(true);
        }
    }
}

pub fn is_ttpf_name(path: &Path) -> bool {
    path.extension().is_some_and(|e| e.eq_ignore_ascii_case("ttpf"))
}

impl Archive {
    pub fn open(path: &Path, progress: &Progress) -> Result<Archive, String> {
        if !is_ttpf_name(path) {
            return Err(format!("{} is not a .ttpf file.", path.file_name().unwrap_or_default().to_string_lossy()));
        }
        progress.stage("Reading header…");
        let file = std::fs::File::open(path).map_err(|e| format!("Can't open {}: {e}", path.display()))?;
        // SAFETY: read-only mapping; the file is not modified while open.
        let map = unsafe { Mmap::map(&file) }.map_err(|e| format!("Can't read {}: {e}", path.display()))?;
        let d: &[u8] = &map;
        let bad = || "This file has no valid .ttpf header.".to_string();
        if d.get(0..4) != Some(b"RDS5") {
            return Err(bad());
        }
        let n = u32_at(d, 4).ok_or_else(bad)? as usize;
        let total = u32_at(d, 8).ok_or_else(bad)? as usize | ((u32_at(d, 0x0c).ok_or_else(bad)? as usize & 0xff) << 32);
        let at = (u32_at(d, 0x10).ok_or_else(bad)? as usize) << 8;
        if n == 0 || at < 0x18 || d.get(at..at + 4) != Some(b"ETPF") {
            return Err(bad());
        }
        let n = n.min((at - 0x18) / 8);
        let mut blocks = Vec::with_capacity(n);
        let (mut file_pos, mut pos, mut codec) = (at, 0usize, None);
        for i in 0..n {
            let v = u64::from_le_bytes(d[0x18 + 8 * i..0x20 + 8 * i].try_into().unwrap());
            let file_end = ((v & 0x3ff_ffff) << 8) as usize;
            let end = (((v >> 30) & 0x3f_ffff) << 12) as usize;
            if file_end > file_pos && end > pos {
                let (file_off, file_len, len) = (file_pos, file_end - file_pos, end - pos);
                let head = d.get(file_off..file_off + 2);
                let zlib = head.is_some_and(|h| h[0] & 0x0f == 8 && (h[0] as u32 * 256 + h[1] as u32) % 31 == 0);
                let kind = if file_end > d.len() {
                    Kind::Missing
                } else {
                    match codec {
                        Some(8) | None if zlib => Kind::Zlib,
                        Some(0) | None if file_len == len => Kind::Stored,
                        _ => Kind::Other,
                    }
                };
                blocks.push(Block { file_off, file_len, start: pos, len, kind });
                pos = end;
            }
            file_pos = file_pos.max(file_end);
            codec = Some(((v >> 26) & 0xf) as u8);
        }
        let count = u32_at(d, at + 8).ok_or_else(bad)? as usize;
        if at + 16 + 16 * count > d.len() {
            return Err(bad());
        }

        // textures
        progress.stage("Reading texture table…");
        let mut raw: Vec<(usize, usize, usize, u32, Tile, usize, usize)> = (0..count)
            .map(|i| {
                let o = at + 16 + 16 * i;
                let (a, b, c) = (u32_at(d, o).unwrap(), u32_at(d, o + 4).unwrap(), u32_at(d, o + 8).unwrap());
                // tile mode 0 = 64 KB tiles; 1-3 (and 9-12 on cube maps) = 4 KB tiles
                let tile = if (b >> 24) & 0xf == 0 { Tile::T64K } else { Tile::T4K };
                let mips = (a >> 28) as usize + 1;
                (i, (a & 0x3fff) as usize + 1, ((a >> 14) & 0x3fff) as usize + 1, (b & 0x3ff) >> 3, tile, (c & 0x1f_ffff) as usize * 4096, mips)
            })
            .collect();
        let mut by_start: Vec<usize> = (0..raw.len()).collect();
        by_start.sort_by_key(|&i| raw[i].5);
        let mut ends = vec![total; raw.len()];
        for k in 0..by_start.len().saturating_sub(1) {
            ends[by_start[k]] = raw[by_start[k + 1]].5;
        }
        let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "textures".into());
        let mut arc = Archive {
            path: path.to_path_buf(),
            stem,
            map,
            blocks,
            textures: Vec::new(),
            cache: Mutex::new(Cache { blocks: HashMap::new(), order: VecDeque::new(), bytes: 0 }),
        };
        let textures: Vec<Texture> = raw
            .drain(..)
            .map(|(index, w, h, dxgi, tile, start, _mips)| {
                let fmt = format_of(dxgi);
                let levels = fmt.map(|f| arc.levels(f, tile, w, h, start, ends[index])).unwrap_or_default();
                let status = match (fmt, levels.iter().position(|l| l.state.is_ok())) {
                    (None, _) => Status::Unavailable(Problem::Format),
                    (_, Some(0)) => Status::Full,
                    (_, Some(k)) => Status::Reduced(k),
                    (_, None) => Status::Unavailable(levels.first().map(|l| l.state.unwrap_err()).unwrap_or(Problem::Layout)),
                };
                Texture { index, w, h, dxgi, fmt, tile, levels, status }
            })
            .collect();
        arc.textures = textures;
        // formats whose tile pattern isn't known are only decoded when uniform: check now so the status is right
        for i in 0..arc.textures.len() {
            let unknown = arc.textures[i].fmt.is_some_and(|f| arc.textures[i].tile.pattern(f.bytes()).is_empty());
            let lvl = arc.textures[i].status.shown_level();
            if unknown && lvl.is_some() && arc.image(i, lvl.unwrap_or(0)).is_err() {
                arc.textures[i].status = Status::Unavailable(Problem::Format);
            }
        }
        progress.stage("Done");
        progress.finished.store(true, Ordering::Relaxed);
        Ok(arc)
    }

    /// Mip levels of a texture, top first, as far as their layout is known.
    fn levels(&self, fmt: Fmt, tile: Tile, w: usize, h: usize, start: usize, end: usize) -> Vec<Level> {
        let (tw, th) = fmt.tile_px(tile);
        let mut out = Vec::new();
        let mut pos = end;
        for k in 0..16 {
            let (lw, lh) = ((w >> k).max(1), (h >> k).max(1));
            let (pw, ph) = (lw.div_ceil(tw) * tw, lh.div_ceil(th) * th);
            if k > 0 && (pw, ph) != (lw, lh) {
                break; // smaller than a tile: may be packed with other levels
            }
            let bytes = pw / fmt.block() * (ph / fmt.block()) * fmt.bytes();
            if bytes > pos.saturating_sub(start) {
                if k == 0 {
                    out.push(Level { w: lw, h: lh, pw, ph, a: pos, b: pos, state: Err(Problem::Layout) });
                }
                break;
            }
            let (a, b) = (pos - bytes, pos);
            out.push(Level { w: lw, h: lh, pw, ph, a, b, state: self.locate(a, b).map(|_| ()) });
            pos = a;
            if lw == 1 && lh == 1 {
                break;
            }
        }
        out
    }

    /// Which blocks supply bytes [a, b) of the unpacked stream: (block, from, to) pieces.
    fn locate(&self, a: usize, b: usize) -> Result<Vec<(usize, usize, usize)>, Problem> {
        let mut out = Vec::new();
        let mut pos = a;
        while pos < b {
            let i = self.blocks.partition_point(|bl| bl.start + bl.len <= pos);
            let bl = self.blocks.get(i).filter(|bl| bl.start <= pos).ok_or(Problem::Missing)?;
            match bl.kind {
                Kind::Zlib | Kind::Stored => {}
                Kind::Other => return Err(Problem::Compression),
                Kind::Missing => return Err(Problem::Missing),
            }
            let end = b.min(bl.start + bl.len);
            out.push((i, pos - bl.start, end - bl.start));
            pos = end;
        }
        Ok(out)
    }

    /// A block's unpacked bytes (cached).
    fn block(&self, i: usize) -> Result<Arc<Vec<u8>>, Problem> {
        if let Some(b) = self.cache.lock().unwrap().blocks.get(&i) {
            return Ok(b.clone());
        }
        let bl = &self.blocks[i];
        let data = Arc::new(match bl.kind {
            Kind::Stored => self.map[bl.file_off..bl.file_off + bl.file_len].to_vec(),
            _ => inflate(&self.map[bl.file_off..bl.file_off + bl.file_len], bl.len).map_err(|_| Problem::Missing)?,
        });
        let mut c = self.cache.lock().unwrap();
        if c.blocks.insert(i, data.clone()).is_none() {
            c.bytes += data.len();
            c.order.push_back(i);
        }
        while c.bytes > CACHE_BYTES {
            let Some(old) = c.order.pop_front() else { break };
            if let Some(b) = c.blocks.remove(&old) {
                c.bytes -= b.len();
            }
        }
        Ok(data)
    }

    fn read(&self, a: usize, b: usize) -> Result<Vec<u8>, Problem> {
        let mut out = Vec::with_capacity(b - a);
        for (i, from, to) in self.locate(a, b)? {
            let data = self.block(i)?;
            out.extend_from_slice(data.get(from..to).ok_or(Problem::Missing)?);
        }
        Ok(out)
    }

    /// Decode mip level `level` of texture `t`.
    pub fn image(&self, t: usize, level: usize) -> Result<Rgba, Problem> {
        let tex = &self.textures[t];
        let fmt = tex.fmt.ok_or(Problem::Format)?;
        let l = tex.levels.get(level).ok_or(Problem::Layout)?;
        l.state?;
        let data = self.read(l.a, l.b)?;
        let img = bcn::decode(&data, fmt, l.pw, l.ph, tex.tile).ok_or(Problem::Format)?;
        Ok(img.crop(l.w, l.h))
    }

    /// Raw stored bytes of a level and its padded size (development aid).
    pub fn raw(&self, t: usize, level: usize) -> Result<(Vec<u8>, usize, usize), Problem> {
        let l = self.textures[t].levels.get(level).ok_or(Problem::Layout)?;
        l.state?;
        Ok((self.read(l.a, l.b)?, l.pw, l.ph))
    }

    /// The image to show/export for a texture: full size, or its largest readable mip level.
    pub fn best_image(&self, t: usize) -> Result<Rgba, Problem> {
        match self.textures[t].status {
            Status::Unavailable(p) => Err(p),
            s => self.image(t, s.shown_level().unwrap_or(0)),
        }
    }
}

impl Archive {
    /// The texture as a .dds file holding its original stored data (lossless, keeps BCn compression and
    /// HDR values): the shown level and every readable smaller level below it.
    pub fn dds(&self, t: usize) -> Result<Vec<u8>, Problem> {
        let tex = &self.textures[t];
        let fmt = tex.fmt.ok_or(Problem::Format)?;
        let first = match tex.status {
            Status::Unavailable(p) => return Err(p),
            s => s.shown_level().unwrap_or(0),
        };
        let (b, bytes) = (fmt.block(), fmt.bytes());
        let mut data = Vec::new();
        let mut mips = 0u32;
        for l in tex.levels[first..].iter().take_while(|l| l.state.is_ok()) {
            let Some(lin) = self.read(l.a, l.b).ok().and_then(|d| bcn::linear(&d, fmt, l.pw, l.ph, tex.tile)) else { break };
            let (we, ew, eh) = (l.pw / b, l.w.div_ceil(b), l.h.div_ceil(b));
            for y in 0..eh {
                data.extend_from_slice(&lin[y * we * bytes..(y * we + ew) * bytes]);
            }
            mips += 1;
        }
        if mips == 0 {
            return Err(Problem::Missing);
        }
        let top = &tex.levels[first];
        let pitch = if b == 4 { top.w.div_ceil(4) * top.h.div_ceil(4) * bytes } else { top.w * bytes };
        let mut out = Vec::with_capacity(148 + data.len());
        out.extend_from_slice(b"DDS ");
        // size, flags (caps|height|width|pixelformat|mipmapcount|linearsize or pitch), height, width, pitch, depth, mips
        let flags = 0x1 | 0x2 | 0x4 | 0x1000 | 0x20000 | if b == 4 { 0x80000 } else { 0x8 };
        for v in [124, flags, top.h as u32, top.w as u32, pitch as u32, 0, mips] {
            out.extend_from_slice(&u32::to_le_bytes(v));
        }
        out.extend_from_slice(&[0; 44]);
        // pixel format: "DX10" four-cc, the real format follows in the extra header
        for v in [32u32, 0x4] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(b"DX10");
        out.extend_from_slice(&[0; 20]);
        // caps: texture, plus mipmap|complex when there are several levels
        let caps = 0x1000 | if mips > 1 { 0x400008 } else { 0 };
        for v in [caps, 0, 0, 0, 0] {
            out.extend_from_slice(&u32::to_le_bytes(v));
        }
        // DX10 header: format, 2D texture, no flags, array size 1, alpha mode unknown
        for v in [tex.dxgi, 3, 0, 1, 0] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&data);
        Ok(out)
    }
}

/// File name an exported texture gets.
pub fn png_name(index: usize) -> String {
    format!("{index}.png")
}

pub fn save_png(img: &Rgba, path: &Path) -> Result<(), String> {
    image::save_buffer(path, &img.px, img.w as u32, img.h as u32, image::ColorType::Rgba8).map_err(|e| e.to_string())
}

/// Export textures to `dir` as PNGs, in parallel. Unavailable ones are skipped.
/// Returns (written, skipped).
/// With `dds`, each is saved as .dds holding its original data instead (see [`Archive::dds`]).
pub fn export(arc: &Archive, list: &[usize], dir: &Path, dds: bool, done: &AtomicUsize) -> Result<(usize, usize), String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("Can't create {}: {e}", dir.display()))?;
    let results: Vec<Result<bool, String>> = list
        .par_iter()
        .map(|&t| {
            let index = arc.textures[t].index;
            let r = if dds {
                match arc.dds(t) {
                    Ok(d) => std::fs::write(dir.join(format!("{index}.dds")), d).map(|_| true).map_err(|e| e.to_string()),
                    Err(_) => Ok(false),
                }
            } else {
                match arc.best_image(t) {
                    Ok(img) => save_png(&img, &dir.join(png_name(index))).map(|_| true),
                    Err(_) => Ok(false),
                }
            };
            done.fetch_add(1, Ordering::Relaxed);
            r
        })
        .collect();
    if let Some(Err(e)) = results.iter().find(|r| r.is_err()) {
        return Err(e.clone());
    }
    let written = results.iter().filter(|r| matches!(r, Ok(true))).count();
    Ok((written, list.len() - written))
}
