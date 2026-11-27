//! Texture formats and decoding to RGBA8.

use crate::tile::{self, Tile};

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Fmt {
    BC1,
    BC3,
    BC4,
    BC5,
    /// HDR (half-float) colour; shown clipped to 0..1
    BC6H,
    BC7,
    /// One channel, shown as grey.
    R8,
    /// Alpha only, shown as grey.
    A8,
    BC2,
    /// BC6H with signed values
    BC6HS,
    /// Uncompressed colour (blue first).
    BGRA8,
    /// Uncompressed colour (red first).
    RGBA8,
    /// 16-bit float, one channel, shown as grey scaled to its own range.
    R16F,
    /// 16-bit float RGBA, shown clipped to 0..1.
    RGBA16F,
    /// 16-bit RGBA.
    RGBA16,
}

impl Fmt {
    /// Bytes per element (a 4x4 block for BCn, a pixel otherwise).
    pub fn bytes(self) -> usize {
        match self {
            Fmt::BC1 | Fmt::BC4 => 8,
            Fmt::BC2 | Fmt::BC3 | Fmt::BC5 | Fmt::BC6H | Fmt::BC6HS | Fmt::BC7 => 16,
            Fmt::R8 | Fmt::A8 => 1,
            Fmt::R16F => 2,
            Fmt::BGRA8 | Fmt::RGBA8 => 4,
            Fmt::RGBA16F | Fmt::RGBA16 => 8,
        }
    }
    /// Pixels per element side: 4 for block-compressed formats, 1 for uncompressed.
    pub fn block(self) -> usize {
        match self {
            Fmt::R8 | Fmt::A8 | Fmt::BGRA8 | Fmt::RGBA8 | Fmt::R16F | Fmt::RGBA16F | Fmt::RGBA16 => 1,
            _ => 4,
        }
    }
    /// Size of the tile in pixels.
    pub fn tile_px(self, tile: Tile) -> (usize, usize) {
        let p = tile.pattern(self.bytes());
        if p.is_empty() {
            return (self.block(), self.block()); // tile pattern unknown
        }
        let (tw, th) = tile::tile_elems(p);
        (tw * self.block(), th * self.block())
    }
}

/// A decoded image, RGBA8, row-major.
#[derive(Clone)]
pub struct Rgba {
    pub w: usize,
    pub h: usize,
    pub px: Vec<u8>,
}

/// Decode `data` as `fmt` at `w` x `h` pixels (a multiple of the tile size) using the given tile layout.
/// Untile stored data of a `w` x `h` (padded) level into rows of elements (pixels, or 4x4 blocks).
pub fn linear(data: &[u8], fmt: Fmt, w: usize, h: usize, tile: Tile) -> Option<Vec<u8>> {
    let (b, bytes) = (fmt.block(), fmt.bytes());
    let (we, he) = (w / b, h / b);
    let n = data.len() / bytes;
    if we == 0 || he == 0 || we * he != n {
        return None;
    }
    let pat = tile.pattern(bytes);
    if pat.is_empty() {
        // tile pattern not known: only a texture whose elements are all equal can be decoded exactly
        let first = data.get(..bytes)?;
        if !data.chunks_exact(bytes).all(|p| p == first) {
            return None;
        }
        return Some(first.repeat(n));
    }
    Some(tile::untile(&data[..n * bytes], bytes, we, he, tile))
}

pub fn decode_linear(lin: &[u8], fmt: Fmt, w: usize, h: usize) -> Option<Rgba> {
    let (b, bytes) = (fmt.block(), fmt.bytes());
    let (we, he) = (w / b, h / b);
    if lin.len() != we * he * bytes { return None; }
    if b == 1 {
        return Some(Rgba { w, h, px: pixels(lin, fmt) });
    }
    let mut px = vec![0u8; w * h * 4];
    let mut blk = [0u32; 16];
    let mut hdr = [[0u16; 3]; 16];
    for by in 0..he {
        for bx in 0..we {
            let s = &lin[(by * we + bx) * bytes..(by * we + bx + 1) * bytes];
            if matches!(fmt, Fmt::BC6H | Fmt::BC6HS) {
                // HDR: decode the real values and tone-map them instead of clipping
                crate::bc6h::decode_block(s, &mut hdr, fmt == Fmt::BC6HS);
                for (k, c) in hdr.iter().enumerate() {
                    let o = ((by * 4 + k / 4) * w + bx * 4 + k % 4) * 4;
                    px[o..o + 4].copy_from_slice(&[tonemap(half(c[0])), tonemap(half(c[1])), tonemap(half(c[2])), 255]);
                }
                continue;
            }
            blk.fill(u32::from_le_bytes([0, 0, 0, 255]));
            match fmt {
                Fmt::BC1 => texture2ddecoder::decode_bc1a_block(s, &mut blk),
                Fmt::BC3 => texture2ddecoder::decode_bc3_block(s, &mut blk),
                Fmt::BC4 => texture2ddecoder::decode_bc4_block(s, &mut blk),
                Fmt::BC5 => texture2ddecoder::decode_bc5_block(s, &mut blk),
                Fmt::BC2 => texture2ddecoder::decode_bc2_block(s, &mut blk),
                Fmt::BC7 => texture2ddecoder::decode_bc7_block(s, &mut blk),
                _ => unreachable!(),
            }
            for (k, v) in blk.iter().enumerate() {
                let [bb, g, r, a] = v.to_le_bytes(); // the decoder packs BGRA
                let (r, g, bb, a) = match fmt {
                    Fmt::BC4 => (r, r, r, 255), // grey
                    Fmt::BC5 => (r, g, normal_z(r, g), 255), // two-channel normal map: rebuild blue (z)
                    _ => (r, g, bb, a),
                };
                let o = ((by * 4 + k / 4) * w + bx * 4 + k % 4) * 4;
                px[o..o + 4].copy_from_slice(&[r, g, bb, a]);
            }
        }
    }
    Some(Rgba { w, h, px })
}

/// How HDR textures (BC6H, half-float RGBA) are shown and exported as PNG: tone-mapped (true) or with
/// values above 1 clipped to white (false).
pub static TONEMAP: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// HDR value -> display byte. Tone-mapped: Reinhard (x / (1 + x)) then gamma 2.2, so bright values roll
/// off instead of turning white. Clipped: 0..1 as is.
fn tonemap(v: f32) -> u8 {
    let x = if v.is_finite() { v.max(0.0) } else { 0.0 };
    if TONEMAP.load(std::sync::atomic::Ordering::Relaxed) {
        ((x / (1.0 + x)).powf(1.0 / 2.2) * 255.0 + 0.5) as u8
    } else {
        (x.min(1.0) * 255.0 + 0.5) as u8
    }
}

impl Fmt {
    pub fn is_hdr(self) -> bool {
        matches!(self, Fmt::BC6H | Fmt::BC6HS | Fmt::RGBA16F)
    }
}

/// Blue channel of a two-channel normal map: z = sqrt(1 - x² - y²), with x and y stored as 0..255 for -1..1.
fn normal_z(r: u8, g: u8) -> u8 {
    let (x, y) = (r as f32 / 127.5 - 1.0, g as f32 / 127.5 - 1.0);
    let z = (1.0 - x * x - y * y).max(0.0).sqrt();
    ((z + 1.0) * 127.5 + 0.5).min(255.0) as u8
}

impl Rgba {
    /// Treat as a normal map whose blue channel isn't its Z: rebuild blue from red and green.
    pub fn rebuild_normal_z(&mut self) {
        for p in self.px.chunks_exact_mut(4) {
            p[2] = normal_z(p[0], p[1]);
        }
    }
}

impl Fmt {
    /// Colour formats where "rebuild blue as a normal map" makes sense (BC5 always does it already).
    pub fn may_be_normal_map(self) -> bool {
        matches!(self, Fmt::BC1 | Fmt::BC2 | Fmt::BC3 | Fmt::BC7 | Fmt::RGBA8 | Fmt::BGRA8)
    }
}

#[cfg(test)]
pub fn half_to_f32(h: u16) -> f32 {
    half(h)
}

fn half(h: u16) -> f32 {
    let (s, e, m) = ((h >> 15) as u32, ((h >> 10) & 0x1f) as i32, (h & 0x3ff) as f32);
    let v = if e == 0 {
        m / 1024.0 * 2f32.powi(-14)
    } else if e == 31 {
        f32::INFINITY
    } else {
        (1.0 + m / 1024.0) * 2f32.powi(e - 15)
    };
    if s == 1 { -v } else { v }
}

/// Uncompressed elements (one per pixel) to RGBA8.
fn pixels(lin: &[u8], fmt: Fmt) -> Vec<u8> {
    let to8 = |f: f32| (f.clamp(0.0, 1.0) * 255.0 + 0.5) as u8;
    match fmt {
        Fmt::R8 | Fmt::A8 => lin.iter().flat_map(|&v| [v, v, v, 255]).collect(),
        Fmt::BGRA8 => lin.chunks_exact(4).flat_map(|p| [p[2], p[1], p[0], p[3]]).collect(),
        Fmt::RGBA8 => lin.to_vec(),
        Fmt::RGBA16 => lin.chunks_exact(8).flat_map(|p| [p[1], p[3], p[5], p[7]]).collect(),
        Fmt::RGBA16F => lin
            .chunks_exact(8)
            .flat_map(|p| {
                let c = |i: usize| half(u16::from_le_bytes([p[i], p[i + 1]]));
                [tonemap(c(0)), tonemap(c(2)), tonemap(c(4)), to8(c(6))]
            })
            .collect(),
        Fmt::R16F => {
            // heights and the like: scale the texture's own range to black..white
            let v: Vec<f32> = lin.chunks_exact(2).map(|p| half(u16::from_le_bytes([p[0], p[1]]))).collect();
            let (lo, hi) = v.iter().filter(|x| x.is_finite()).fold((f32::MAX, f32::MIN), |(a, b), &x| (a.min(x), b.max(x)));
            let span = if hi > lo { hi - lo } else { 1.0 };
            v.iter()
                .flat_map(|&x| {
                    let g = to8((x - lo) / span);
                    [g, g, g, 255]
                })
                .collect()
        }
        _ => unreachable!(),
    }
}

impl Rgba {
    /// Box-filter downscale so the longest side is at most `max` (never upscales).
    pub fn thumbnail(&self, max: usize) -> Rgba {
        let s = (self.w.max(self.h) as f32 / max as f32).max(1.0);
        let (tw, th) = (((self.w as f32 / s).round() as usize).max(1), ((self.h as f32 / s).round() as usize).max(1));
        if tw == self.w && th == self.h {
            return self.clone();
        }
        let mut px = vec![0u8; tw * th * 4];
        for ty in 0..th {
            let (y0, y1) = (ty * self.h / th, ((ty + 1) * self.h / th).max(ty * self.h / th + 1));
            for tx in 0..tw {
                let (x0, x1) = (tx * self.w / tw, ((tx + 1) * self.w / tw).max(tx * self.w / tw + 1));
                let mut acc = [0u32; 4];
                for y in y0..y1 {
                    for x in x0..x1 {
                        let o = (y * self.w + x) * 4;
                        for c in 0..4 {
                            acc[c] += self.px[o + c] as u32;
                        }
                    }
                }
                let n = ((y1 - y0) * (x1 - x0)) as u32;
                let o = (ty * tw + tx) * 4;
                for c in 0..4 {
                    px[o + c] = (acc[c] / n) as u8;
                }
            }
        }
        Rgba { w: tw, h: th, px }
    }

    /// The top-left `w` x `h` part (removes tile padding).
    /// The `w` x `h` part starting at (`x`, `y`).
    pub fn crop_at(&self, x: usize, y: usize, w: usize, h: usize) -> Rgba {
        if (x, y, self.w, self.h) == (0, 0, w, h) {
            return self.clone();
        }
        let mut px = Vec::with_capacity(w * h * 4);
        for row in y..y + h {
            px.extend_from_slice(&self.px[(row * self.w + x) * 4..(row * self.w + x + w) * 4]);
        }
        Rgba { w, h, px }
    }
}
