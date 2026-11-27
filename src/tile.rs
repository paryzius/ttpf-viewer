//! Console texture untiling (AMD standard swizzle, as used by the .ttpf textures).
//!
//! A tile pattern lists, from the lowest bit up, whether each bit of an element's index inside a tile
//! moves it along x or y. Elements are 4x4 blocks for BCn formats and single pixels for uncompressed
//! formats. Tiles themselves are laid out row-major.

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tile {
    /// 64 KB tiles
    T64K,
    /// PS5 64 KB standard swizzle (Xbox mode zero uses display swizzle).
    Ps5T64K,
    /// 4 KB tiles
    T4K,
}

impl Tile {
    pub fn name(self) -> &'static str {
        match self {
            Tile::T64K | Tile::Ps5T64K => "64 KB",
            Tile::T4K => "4 KB",
        }
    }

    /// Bit pattern for elements of `bytes` bytes: 16 (BC2/3/5/6H/7 blocks), 8 (BC1/BC4 blocks, RGBA16 pixels),
    /// 4 (RGBA8/BGRA8 pixels), 2 (R16F pixels), 1 (R8/A8 pixels). Each was checked against decoded textures.
    pub fn pattern(self, bytes: usize) -> &'static str {
        match (self, bytes) {
            (Tile::Ps5T64K, 16) => "yyxxyxyxyxyx",
            (Tile::Ps5T64K, 8) => "xyyxxyxyxyxyx",
            (Tile::Ps5T64K, 4) => "xxyyyxyxyxyxyx",
            (Tile::Ps5T64K, 2) => "xxxyyyxyxyxyxyx",
            (Tile::Ps5T64K, 1) => "xxxxyyyyyxyxyxyx",
            (Tile::T64K, 16) => "xyxyyxyxyxyx",
            (Tile::T4K, 16) => "yyxxyxyx",
            (Tile::T64K, 8) => "xyxxyyxyxyxyx",
            (Tile::T4K, 8) => "xyyxxyxyx",
            (Tile::T64K, 4) => "xxyyxyyxyxyxyx",
            (Tile::T4K, 4) => "xxyyyxyxyx",
            (Tile::T64K, 2) => "xxxyyyxyxyxyxyx",
            (Tile::T4K, 2) => "xxxyyyxyxyx",
            (Tile::T64K, 1) => "xxxyyyxyyxyxyxyx",
            (Tile::T4K, 1) => "xxxxyyyyyxyx",
            _ => "",
        }
    }
}

fn log2(n: usize) -> usize {
    (usize::BITS - 1 - n.max(1).leading_zeros()) as usize
}

/// The pattern actually used for `n` elements: surfaces smaller than a tile use only the low bits.
pub fn truncated(pattern: &str, n: usize) -> &str {
    &pattern[..pattern.len().min(log2(n))]
}

/// Tile size in elements (width, height) for a full pattern.
pub fn tile_elems(pattern: &str) -> (usize, usize) {
    let xs = pattern.bytes().filter(|&c| c == b'x').count();
    (1 << xs, 1 << (pattern.len() - xs))
}

/// Rearrange tiled elements into a linear `w` x `h` grid. Elements that land outside are dropped,
/// cells nobody fills stay zero.
pub fn untile(data: &[u8], bytes: usize, w: usize, h: usize, tile: Tile) -> Vec<u8> {
    let n = data.len() / bytes;
    let t = truncated(tile.pattern(bytes), n).as_bytes();
    let tile_len = 1usize << t.len();
    let mut lx = vec![0usize; tile_len];
    let mut ly = vec![0usize; tile_len];
    for j in 0..tile_len {
        let (mut x, mut y, mut xi, mut yi) = (0, 0, 0, 0);
        for (b, &c) in t.iter().enumerate() {
            let bit = (j >> b) & 1;
            if c == b'x' {
                x |= bit << xi;
                xi += 1;
            } else {
                y |= bit << yi;
                yi += 1;
            }
        }
        lx[j] = x;
        // The 8-bit Xbox display equation orders Y1 before Y0.
        ly[j] = if tile == Tile::T64K && bytes == 1 {
            (y & !3) | ((y & 1) << 1) | ((y & 2) >> 1)
        } else { y };
    }
    let xs = t.iter().filter(|&&c| c == b'x').count();
    let (tw, th) = (1usize << xs, 1usize << (t.len() - xs));
    let tpr = w.div_ceil(tw).max(1);
    let mask = tile_len - 1;
    let mut out = vec![0u8; w * h * bytes];
    for i in 0..n {
        let tl = i >> t.len();
        let x = (tl % tpr) * tw + lx[i & mask];
        let y = (tl / tpr) * th + ly[i & mask];
        if x < w && y < h {
            let d = (y * w + x) * bytes;
            out[d..d + bytes].copy_from_slice(&data[i * bytes..(i + 1) * bytes]);
        }
    }
    out
}
