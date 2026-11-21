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
use crate::layout;
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
    /// where the level sits in the decoded `pw` x `ph` region (levels in a shared mip-tail tile)
    ox: usize,
    oy: usize,
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
    /// Readable mip levels, top first (empty if the format is unsupported). For a cube map or texture
    /// array: those of its first face or slice.
    pub levels: Vec<Level>,
    /// Cube maps and texture arrays: the levels of every face / slice (empty for a plain texture).
    pub layers: Vec<Vec<Level>>,
    pub cube: bool,
    pub status: Status,
}

const FACES: [&str; 6] = ["+X", "-X", "+Y", "-Y", "+Z", "-Z"];

impl Texture {
    /// Number of faces / slices (1 for a plain texture).
    pub fn layer_count(&self) -> usize {
        self.layers.len().max(1)
    }

    pub fn layer_levels(&self, layer: usize) -> &[Level] {
        if self.layers.is_empty() { &self.levels } else { &self.layers[layer] }
    }

    /// "face +X", "slice 3"
    pub fn layer_name(&self, layer: usize) -> String {
        if self.cube { format!("face {}", FACES[layer]) } else { format!("slice {layer}") }
    }

    /// File-name suffix of a face / slice: "_px", "_nz", "_3" (empty for a plain texture)
    pub fn layer_suffix(&self, layer: usize) -> String {
        if self.layers.is_empty() {
            String::new()
        } else if self.cube {
            format!("_{}", ["px", "nx", "py", "ny", "pz", "nz"][layer])
        } else {
            format!("_{layer}")
        }
    }

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
        let mut raw: Vec<(usize, usize, usize, u32, Tile, usize, (usize, usize, usize))> = (0..count)
            .map(|i| {
                let o = at + 16 + 16 * i;
                let (a, b, c) = (u32_at(d, o).unwrap(), u32_at(d, o + 4).unwrap(), u32_at(d, o + 8).unwrap());
                // tile mode 0 = 64 KB tiles; 1-3 (and 9-12 on cube maps) = 4 KB tiles
                let tile = if (b >> 24) & 0xf == 0 { Tile::T64K } else { Tile::T4K };
                // mip field, tile mode, array size
                let shape = ((a >> 28) as usize, ((b >> 24) & 0xf) as usize, ((b >> 10) & 0x3ff) as usize + 1);
                (i, (a & 0x3fff) as usize + 1, ((a >> 14) & 0x3fff) as usize + 1, (b & 0x3ff) >> 3, tile, (c & 0x1f_ffff) as usize * 4096, shape)
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
            .map(|(index, w, h, dxgi, tile, start, (mip_field, mode, slices))| {
                let fmt = format_of(dxgi);
                let layers = fmt.and_then(|f| arc.layers(f, tile, w, h, (mip_field, mode, slices), start, ends[index])).unwrap_or_default();
                let levels = match layers.first() {
                    Some(l) => l.clone(),
                    None => fmt.map(|f| arc.levels(f, tile, w, h, start, ends[index])).unwrap_or_default(),
                };
                let cube = mode >= 9 && !layers.is_empty();
                let status = match (fmt, levels.iter().position(|l| l.state.is_ok())) {
                    (None, _) => Status::Unavailable(Problem::Format),
                    (_, Some(0)) => Status::Full,
                    (_, Some(k)) => Status::Reduced(k),
                    (_, None) => Status::Unavailable(levels.first().map(|l| l.state.unwrap_err()).unwrap_or(Problem::Layout)),
                };
                Texture { index, w, h, dxgi, fmt, tile, levels, layers, cube, status }
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
                    out.push(Level { w: lw, h: lh, pw, ph, ox: 0, oy: 0, a: pos, b: pos, state: Err(Problem::Layout) });
                }
                break;
            }
            let (a, b) = (pos - bytes, pos);
            out.push(Level { w: lw, h: lh, pw, ph, ox: 0, oy: 0, a, b, state: self.locate(a, b).map(|_| ()) });
            pos = a;
            if lw == 1 && lh == 1 {
                break;
            }
        }
        out
    }

    /// Cube maps and texture arrays: the levels of each face / slice, if the layout accounts exactly for the
    /// texture's space (otherwise it is treated as a plain texture).
    ///
    /// - Texture arrays store level by level, smallest first, each level's slices back to back: level m of
    ///   slice s is at offset(m) * slices + s * size(m), with offset/size those of a single image.
    /// - Cube maps (tile modes 9-12; here the mip field is the level count itself) store tile mode - 8
    ///   complete cubes, smallest first (see [`layout::cube_copies`]); the last is the full-size one, its 6
    ///   faces one after another.
    #[allow(clippy::too_many_arguments)]
    fn layers(&self, fmt: Fmt, tile: Tile, w: usize, h: usize, shape: (usize, usize, usize), start: usize, end: usize) -> Option<Vec<Vec<Level>>> {
        let (mip_field, mode, slices) = shape;
        let level = |a: usize, p: &layout::Place| Level {
            w: p.w,
            h: p.h,
            pw: p.pw,
            ph: p.ph,
            ox: p.ox,
            oy: p.oy,
            a,
            b: a + p.len,
            state: self.locate(a, a + p.len).map(|_| ()),
        };
        if mode >= 9 {
            let c = layout::chain(fmt, tile, w, h, mip_field.max(1))?;
            let (full, total) = layout::cube_copies(&c, mode - 8)?;
            if total != end.checked_sub(start)? {
                return None;
            }
            Some((0..6).map(|f| c.levels.iter().map(|p| level(start + full + f * c.size + p.off, p)).collect()).collect())
        } else if slices > 1 {
            let c = layout::chain(fmt, tile, w, h, mip_field + 1)?;
            if slices * c.size != end.checked_sub(start)? {
                return None;
            }
            Some((0..slices).map(|s| c.levels.iter().map(|p| level(start + p.off * slices + s * p.len, p)).collect()).collect())
        } else {
            None
        }
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
        self.image_in(t, 0, level)
    }

    /// Decode mip level `level` of face / slice `layer` of texture `t`.
    pub fn image_in(&self, t: usize, layer: usize, level: usize) -> Result<Rgba, Problem> {
        let tex = &self.textures[t];
        let fmt = tex.fmt.ok_or(Problem::Format)?;
        let l = tex.layer_levels(layer).get(level).ok_or(Problem::Layout)?;
        l.state?;
        let data = self.read(l.a, l.b)?;
        let img = bcn::decode(&data, fmt, l.pw, l.ph, tex.tile).ok_or(Problem::Format)?;
        Ok(img.crop_at(l.ox, l.oy, l.w, l.h))
    }

    /// Raw stored bytes of a level and its padded size (development aid).
    pub fn raw(&self, t: usize, level: usize) -> Result<(Vec<u8>, usize, usize), Problem> {
        let l = self.textures[t].levels.get(level).ok_or(Problem::Layout)?;
        l.state?;
        Ok((self.read(l.a, l.b)?, l.pw, l.ph))
    }

    /// The image to show/export for a texture: full size, or its largest readable mip level.
    pub fn best_image(&self, t: usize) -> Result<Rgba, Problem> {
        self.best_image_in(t, 0)
    }

    /// The 6 faces of a cube map at the largest size all of them can supply.
    pub fn cube_faces(&self, t: usize) -> Result<[Rgba; 6], Problem> {
        let tex = &self.textures[t];
        if !tex.cube {
            return Err(Problem::Layout);
        }
        let level = (0..6)
            .map(|f| tex.layer_levels(f).iter().position(|l| l.state.is_ok()))
            .try_fold(0, |m, f| f.map(|f| m.max(f)))
            .ok_or(Problem::Compression)?;
        let faces: Vec<Rgba> = (0..6).map(|f| self.image_in(t, f, level)).collect::<Result<_, _>>()?;
        faces.try_into().map_err(|_| Problem::Layout)
    }

    /// A face / slice at its largest readable size.
    pub fn best_image_in(&self, t: usize, layer: usize) -> Result<Rgba, Problem> {
        let tex = &self.textures[t];
        if let Status::Unavailable(p) = tex.status {
            return Err(p);
        }
        let levels = tex.layer_levels(layer);
        let level = levels.iter().position(|l| l.state.is_ok()).ok_or_else(|| levels.first().map(|l| l.state.unwrap_err()).unwrap_or(Problem::Layout))?;
        self.image_in(t, layer, level)
    }
}

impl Archive {
    /// The texture as a .dds file holding its original stored data (lossless, keeps BCn compression and
    /// HDR values): the shown level and every readable smaller level below it.
    pub fn dds(&self, t: usize) -> Result<Vec<u8>, Problem> {
        let tex = &self.textures[t];
        let fmt = tex.fmt.ok_or(Problem::Format)?;
        if let Status::Unavailable(p) = tex.status {
            return Err(p);
        }
        // the largest level every face / slice can supply
        let first = (0..tex.layer_count())
            .map(|l| tex.layer_levels(l).iter().position(|lv| lv.state.is_ok()))
            .try_fold(0, |m, f| f.map(|f| m.max(f)))
            .ok_or(Problem::Compression)?;
        let (b, bytes) = (fmt.block(), fmt.bytes());
        // one level's elements, rows of blocks (or pixels) without the tile padding
        let level_data = |l: &Level| -> Option<Vec<u8>> {
            let lin = self.read(l.a, l.b).ok().and_then(|d| bcn::linear(&d, fmt, l.pw, l.ph, tex.tile))?;
            let (we, ew, eh, ox, oy) = (l.pw / b, l.w.div_ceil(b), l.h.div_ceil(b), l.ox / b, l.oy / b);
            let mut out = Vec::with_capacity(ew * eh * bytes);
            for y in oy..oy + eh {
                out.extend_from_slice(&lin[(y * we + ox) * bytes..(y * we + ox + ew) * bytes]);
            }
            Some(out)
        };
        // every face / slice gets the same levels: as many as all of them can supply
        let layers = tex.layer_count();
        let mut per_layer: Vec<Vec<Vec<u8>>> = Vec::with_capacity(layers);
        for layer in 0..layers {
            let mut v = Vec::new();
            for l in tex.layer_levels(layer)[first..].iter().take_while(|l| l.state.is_ok()) {
                match level_data(l) {
                    Some(d) => v.push(d),
                    None => break,
                }
            }
            per_layer.push(v);
        }
        let mips = per_layer.iter().map(|v| v.len()).min().unwrap_or(0) as u32;
        if mips == 0 {
            return Err(Problem::Missing);
        }
        let data: Vec<u8> = per_layer.iter().flat_map(|v| v[..mips as usize].concat()).collect();
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
        // caps: texture, plus mipmap|complex when there are several levels, complex for cubes/arrays;
        // caps2: cube map with all six faces
        let caps = 0x1000 | if mips > 1 { 0x400008 } else { 0 } | if layers > 1 { 0x8 } else { 0 };
        let caps2 = if tex.cube { 0xfe00 } else { 0 };
        for v in [caps, caps2, 0, 0, 0] {
            out.extend_from_slice(&u32::to_le_bytes(v));
        }
        // DX10 header: format, 2D texture, cube flag, array size (one cube counts as 1), alpha mode unknown
        let (misc, array) = if tex.cube { (0x4, 1) } else { (0, layers as u32) };
        for v in [tex.dxgi, 3, misc, array, 0] {
            out.extend_from_slice(&v.to_le_bytes());
        }
        out.extend_from_slice(&data);
        Ok(out)
    }
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
                // cube maps and arrays: one PNG per face / slice
                let tex = &arc.textures[t];
                let mut r = Ok(false);
                for layer in 0..tex.layer_count() {
                    if let Ok(img) = arc.best_image_in(t, layer) {
                        match save_png(&img, &dir.join(format!("{index}{}.png", tex.layer_suffix(layer)))) {
                            Ok(()) => r = Ok(true),
                            Err(e) => {
                                r = Err(e);
                                break;
                            }
                        }
                    }
                }
                r
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
