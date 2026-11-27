//! Shared ETPF texture descriptors, mip layouts, previews and export.
//! PS5 stores the logical byte stream directly. Xbox wraps it in RDS5 chunks.

#[cfg(test)]
mod tests;
mod xbox;

use crate::bcn::{self, Fmt, Rgba};
use crate::layout;
use crate::tile::Tile;
use memmap2::Mmap;
use rayon::prelude::*;
use sixng_core::gfx10::{Layout as VolumeLayout, Swizzle};
use std::borrow::Cow;
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
            Problem::Compression => "invalid or unsupported compression",
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
    pub bcpack: bool,
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
    volume: Option<Box<VolumeLayout>>,
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
    BcPack(u8),
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

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Platform {
    Xbox,
    Ps5,
}

impl Platform {
    pub fn name(self) -> &'static str {
        match self {
            Self::Xbox => "Xbox",
            Self::Ps5 => "PS5",
        }
    }
}

pub struct Archive {
    pub platform: Platform,
    pub path: PathBuf,
    /// File name without extension; used to name the export folder.
    pub stem: String,
    map: Mmap,
    blocks: Vec<Block>,
    pub textures: Vec<Texture>,
    cache: Mutex<Cache>,
}

fn texture_start(record: &[u8]) -> usize {
    ((u32_at(record, 8).unwrap() & 0x1f_ffff) as usize) << 12
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
        let (platform, at, total, blocks) = match d.get(..4) {
            Some(b"RDS5") => {
                let (at, total, blocks) = xbox::index(d).ok_or_else(bad)?;
                (Platform::Xbox, at, total, blocks)
            }
            Some(b"ETPF") => (Platform::Ps5, 0, d.len(), Vec::new()),
            _ => return Err(bad()),
        };
        let count = u32_at(d, at + 8).ok_or_else(bad)? as usize;
        let area = u32_at(d, at + 12).ok_or_else(bad)? as usize;
        let table_len = count.checked_mul(16).ok_or_else(bad)?;
        let catalog_limit = blocks.first().map_or(d.len(), |header| header.file_len);
        if table_len > area || 16 + area > catalog_limit {
            return Err(bad());
        }
        progress.stage("Reading texture table…");
        let records = d.get(at + 16..at + 16 + table_len).ok_or_else(bad)?;
        let mut starts: Vec<usize> = records.chunks_exact(16).map(texture_start).collect();
        starts.sort_unstable();
        starts.dedup();
        let stem = path.file_stem().unwrap_or_default().to_string_lossy().into_owned();
        let mut arc = Archive {
            path: path.to_path_buf(),
            stem,
            map,
            blocks,
            platform,
            textures: Vec::new(),
            cache: Mutex::new(Cache { blocks: HashMap::new(), order: VecDeque::new(), bytes: 0 }),
        };
        let records = &arc.map[at + 16..at + 16 + table_len];
        let data_min = if platform == Platform::Xbox { arc.blocks[0].len } else { 16 + area };
        arc.textures = records
            .chunks_exact(16)
            .enumerate()
            .map(|(index, record)| {
                let start = texture_start(record);
                let end = starts.get(starts.partition_point(|&other| other <= start)).copied();
                arc.texture(index, record, start, end, data_min, total)
            })
            .collect();
        progress.stage("Done");
        progress.finished.store(true, Ordering::Relaxed);
        Ok(arc)
    }

    fn texture(&self, index: usize, record: &[u8], start: usize, next: Option<usize>, data_min: usize, total: usize) -> Texture {
        let a = u32_at(record, 0).unwrap();
        let b = u32_at(record, 4).unwrap();
        let w = (a & 0x3fff) as usize + 1;
        let h = ((a >> 14) & 0x3fff) as usize + 1;
        let dxgi = (b >> 3) & 127;
        let fmt = format_of(dxgi);
        let mips = ((a >> 28) + (b & 7)) as usize;
        let depth = ((b >> 10) & 0x7ff) as usize + 1;
        let dimension = (b >> 21) & 3;
        let mode = (b >> 24) & 15;
        let tile = match (mode, self.platform) {
            (0, Platform::Ps5) => Tile::Ps5T64K,
            (0, Platform::Xbox) => Tile::T64K,
            _ => Tile::T4K,
        };
        let cube = dimension == 2 && depth == 6 && (9..=12).contains(&mode);
        let mut tex = Texture {
            index,
            w,
            h,
            dxgi,
            fmt,
            tile,
            levels: vec![],
            layers: vec![],
            cube,
            volume: None,
            status: Status::Unavailable(Problem::Layout),
        };
        let Some(fmt) = fmt else {
            tex.status = Status::Unavailable(Problem::Format);
            return tex;
        };
        if start >= total {
            tex.status = Status::Unavailable(Problem::Missing);
            return tex;
        }
        if start < data_min || mips == 0 || mips > 16 {
            return tex;
        }
        // PS5's gap to the following record can include trailing alignment padding.
        // Derive addresses from this record. Each level is checked against its neighbour
        // and against the actual available bytes independently (including a cut-off last record).
        let level = |offset: usize, p: &layout::Place| {
            let a = start + offset;
            let b = a + p.len;
            let pieces = if next.is_some_and(|end| b > end) { Err(Problem::Layout) } else { self.locate(a, b) };
            let bcpack =
                pieces.as_ref().is_ok_and(|parts| parts.iter().any(|&(i, _, _)| matches!(self.blocks[i].kind, Kind::BcPack(_))));
            Level { w: p.w, h: p.h, pw: p.pw, ph: p.ph, ox: p.ox, oy: p.oy, a, b, state: pieces.map(|_| ()), bcpack }
        };
        if dimension == 3 && mode == 1 && mips == 1 && fmt.block() == 1 {
            let Some(v) = VolumeLayout::new_3d(Swizzle::S4K, w as u32, h as u32, depth as u32, fmt.bytes() as u32) else {
                return tex;
            };
            let place = layout::Place { w, h, pw: w, ph: h, ox: 0, oy: 0, off: 0, len: v.total as usize };
            tex.layers = vec![vec![level(0, &place)]; depth];
            tex.volume = Some(Box::new(v));
        } else {
            if !(dimension == 1 && mode <= 3 || cube) {
                return tex;
            }
            let Some(chain) = layout::chain(fmt, tile, w, h, mips) else { return tex };
            if cube {
                let Some((base, _)) = layout::cube_copies(&chain, mode as usize - 8) else { return tex };
                tex.layers =
                    (0..6).map(|s| chain.levels.iter().map(|p| level(base + s * chain.size + p.off, p)).collect()).collect();
            } else if depth > 1 {
                tex.layers =
                    (0..depth).map(|s| chain.levels.iter().map(|p| level(p.off * depth + s * p.len, p)).collect()).collect();
            } else {
                tex.levels = chain.levels.iter().map(|p| level(p.off, p)).collect();
            }
        }
        if let Some(first) = tex.layers.first() {
            tex.levels = first.clone();
        }
        tex.status = match tex.levels.iter().position(|l| l.state.is_ok()) {
            Some(0) => Status::Full,
            Some(k) => Status::Reduced(k),
            None => Status::Unavailable(tex.levels.first().and_then(|l| l.state.err()).unwrap_or(Problem::Layout)),
        };
        tex
    }

    /// Which blocks supply bytes [a, b) of the unpacked stream: (block, from, to) pieces.
    fn locate(&self, a: usize, b: usize) -> Result<Vec<(usize, usize, usize)>, Problem> {
        if self.platform == Platform::Ps5 {
            return self.map.get(a..b).map(|_| Vec::new()).ok_or(Problem::Missing);
        }
        let mut out = Vec::new();
        let mut pos = a;
        while pos < b {
            let i = self.blocks.partition_point(|bl| bl.start + bl.len <= pos);
            let bl = self.blocks.get(i).filter(|bl| bl.start <= pos).ok_or(Problem::Missing)?;
            match bl.kind {
                Kind::Zlib | Kind::Stored | Kind::BcPack(_) => {}
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
        let packed = self.map.get(bl.file_off..bl.file_off + bl.file_len).ok_or(Problem::Missing)?;
        let data = Arc::new(match bl.kind {
            Kind::Stored => packed.to_vec(),
            Kind::BcPack(codec) => xbox::decode(codec, packed, bl.len)?,
            Kind::Zlib => xbox::inflate(packed, bl.len)?,
            Kind::Missing => return Err(Problem::Missing),
            Kind::Other => return Err(Problem::Compression),
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

    fn read(&self, a: usize, b: usize) -> Result<Cow<'_, [u8]>, Problem> {
        if self.platform == Platform::Ps5 {
            return self.map.get(a..b).map(Cow::Borrowed).ok_or(Problem::Missing);
        }
        let mut out = Vec::with_capacity(b - a);
        for (i, from, to) in self.locate(a, b)? {
            let data = self.block(i)?;
            out.extend_from_slice(data.get(from..to).ok_or(Problem::Missing)?);
        }
        Ok(Cow::Owned(out))
    }

    /// Decode mip level `level` of face / slice `layer` of texture `t`.
    pub fn image_in(&self, t: usize, layer: usize, level: usize) -> Result<Rgba, Problem> {
        let tex = &self.textures[t];
        let fmt = tex.fmt.ok_or(Problem::Format)?;
        let l = tex.layer_levels(layer).get(level).ok_or(Problem::Layout)?;
        l.state?;
        let lin = self.linear_level(tex, fmt, layer, l)?;
        let img = bcn::decode_linear(&lin, fmt, l.pw, l.ph).ok_or(Problem::Format)?;
        Ok(img.crop_at(l.ox, l.oy, l.w, l.h))
    }

    fn linear_level(&self, tex: &Texture, fmt: Fmt, layer: usize, l: &Level) -> Result<Vec<u8>, Problem> {
        let data = self.read(l.a, l.b)?;
        if let Some(v) = &tex.volume {
            Ok(v.untile_slice(&data, 0, layer as u32))
        } else {
            bcn::linear(&data, fmt, l.pw, l.ph, tex.tile).ok_or(Problem::Format)
        }
    }

    /// Raw stored bytes of a level and its padded size (development aid).
    pub fn raw(&self, t: usize, level: usize) -> Result<(Vec<u8>, usize, usize), Problem> {
        let l = self.textures[t].levels.get(level).ok_or(Problem::Layout)?;
        l.state?;
        Ok((self.read(l.a, l.b)?.into_owned(), l.pw, l.ph))
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
    /// DDS with the reconstructed BC blocks or original texels for each readable mip and layer.
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
        let level_data = |layer: usize, l: &Level| -> Option<Vec<u8>> {
            let lin = self.linear_level(tex, fmt, layer, l).ok()?;
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
                match level_data(layer, l) {
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
        let depth = if tex.volume.is_some() { layers as u32 } else { 0 };
        let flags = if depth > 0 { flags | 0x800000 } else { flags };
        for v in [124, flags, top.h as u32, top.w as u32, pitch as u32, depth, mips] {
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
        let caps2 = if tex.cube { 0xfe00 } else if depth > 0 { 0x200000 } else { 0 };
        for v in [caps, caps2, 0, 0, 0] {
            out.extend_from_slice(&u32::to_le_bytes(v));
        }
        // DX10 header: format, 2D texture, cube flag, array size (one cube counts as 1), alpha mode unknown
        let (misc, array) = if tex.cube { (0x4, 1) } else if depth > 0 { (0, 1) } else { (0, layers as u32) };
        for v in [tex.dxgi, if depth > 0 { 4 } else { 3 }, misc, array, 0] {
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
/// `normal_maps`: textures to save with their blue channel rebuilt as a normal map's Z (PNG only).
pub fn export(arc: &Archive, list: &[usize], dir: &Path, dds: bool, normal_maps: &std::collections::HashSet<usize>, done: &AtomicUsize) -> Result<(usize, usize), String> {
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
                    if let Ok(mut img) = arc.best_image_in(t, layer) {
                        if normal_maps.contains(&t) {
                            img.rebuild_normal_z();
                        }
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
