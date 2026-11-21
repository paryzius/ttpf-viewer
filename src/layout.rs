//! Where each mip level of one image (a plain texture, one array slice or one cube face) sits, in the
//! AMD Gfx10 (RDNA2) layout the stores use:
//!
//! - levels are stored smallest first; every level gets whole tiles of its own,
//! - except the "mip tail": once a level is at most half a tile wide (and a tile high) and among the last
//!   8 levels (4 KB tiles) or 12 levels (64 KB tiles), it and all smaller levels share one tile, stored
//!   first, each at its own spot inside that tile. A texture with a single level has no tail.
//!
//! Spot of the k-th level in the tail (k = 0 is the largest): slot m = max_in_tail - 1 - k sits at byte
//! (m > 6 ? 16 << m : m << 8) of the tile; read in 256-byte micro blocks, that number is a Morton code with
//! y in the even bits and x in the odd bits.

use crate::bcn::Fmt;
use crate::tile::Tile;

/// One level of one image.
#[derive(Clone, Copy, Debug)]
pub struct Place {
    /// size in pixels
    pub w: usize,
    pub h: usize,
    /// bytes from the image's start, and how many to read
    pub off: usize,
    pub len: usize,
    /// the region those bytes decode to (whole tiles), and where the level sits in it, in pixels
    pub pw: usize,
    pub ph: usize,
    pub ox: usize,
    pub oy: usize,
}

pub struct Chain {
    /// levels, largest first
    pub levels: Vec<Place>,
    /// bytes of the whole chain
    pub size: usize,
    /// sizes of its parts in storage order (the tail tile if any, then each other level, smallest first)
    pub units: Vec<usize>,
}

pub fn chain(fmt: Fmt, tile: Tile, w: usize, h: usize, mips: usize) -> Option<Chain> {
    let bytes = fmt.bytes();
    if tile.pattern(bytes).is_empty() || mips == 0 {
        return None;
    }
    let b = fmt.block();
    let (tw, th) = fmt.tile_px(tile);
    let (bw, bh) = (tw / b, th / b); // tile size in elements
    let tile_bytes = bw * bh * bytes;
    let max_in_tail = if tile == Tile::T64K { 12 } else { 8 };
    let px = |i: usize| ((w >> i).max(1), (h >> i).max(1));
    let el = |i: usize| (px(i).0.div_ceil(b), px(i).1.div_ceil(b));
    let tail = if mips > 1 {
        (0..mips).find(|&i| el(i).0 <= bw / 2 && el(i).1 <= bh && mips - i <= max_in_tail).unwrap_or(mips)
    } else {
        mips
    };
    // micro block (256 bytes) in elements, the step of tail spots
    let (mw, mh) = match bytes {
        16 => (4, 4),
        8 => (8, 4),
        4 => (8, 8),
        2 => (16, 8),
        _ => (16, 16),
    };
    let mut levels = vec![Place { w: 0, h: 0, off: 0, len: 0, pw: 0, ph: 0, ox: 0, oy: 0 }; mips];
    let mut units = Vec::new();
    let mut pos = 0;
    if tail < mips {
        units.push(tile_bytes);
        pos = tile_bytes;
        for i in tail..mips {
            let slot = (max_in_tail - 1 - (i - tail)) as u32;
            let at: u32 = if slot > 6 { 16 << slot } else { slot << 8 };
            let morton = at >> 8;
            let (mut mx, mut my) = (0, 0);
            for bit in 0..8 {
                my |= (((morton >> (2 * bit)) & 1) as usize) << bit;
                mx |= (((morton >> (2 * bit + 1)) & 1) as usize) << bit;
            }
            let (ox, oy) = (mx * mw, my * mh);
            if ox + el(i).0 > bw || oy + el(i).1 > bh {
                return None;
            }
            let (lw, lh) = px(i);
            levels[i] = Place { w: lw, h: lh, off: 0, len: tile_bytes, pw: tw, ph: th, ox: ox * b, oy: oy * b };
        }
    }
    for i in (0..tail).rev() {
        let (ew, eh) = el(i);
        let (nx, ny) = (ew.div_ceil(bw), eh.div_ceil(bh));
        let len = nx * ny * tile_bytes;
        let (lw, lh) = px(i);
        levels[i] = Place { w: lw, h: lh, off: pos, len, pw: nx * tw, ph: ny * th, ox: 0, oy: 0 };
        units.push(len);
        pos += len;
    }
    Some(Chain { levels, size: pos, units })
}

/// Cube map stored as `copies` complete cubes, smallest first: copy k holds the smallest
/// (units - copies + 1 + k) parts of each face's chain, the 6 faces one after another; a copy over 128 KB
/// is padded to a multiple of 64 KB. Returns where the full-size copy starts and the whole size.
pub fn cube_copies(c: &Chain, copies: usize) -> Option<(usize, usize)> {
    let u = c.units.len();
    if copies == 0 || copies > u {
        return None;
    }
    let mut total = 0;
    let mut full_start = 0;
    for k in 0..copies {
        let n = u - copies + 1 + k;
        let mut size = 6 * c.units[..n].iter().sum::<usize>();
        if size > 128 << 10 {
            size = size.div_ceil(64 << 10) * (64 << 10);
        }
        if k == copies - 1 {
            full_start = total;
        }
        total += size;
    }
    Some((full_start, total))
}
