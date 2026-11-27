//! Xbox RDS5 storage. Each entry describes the start, codec and trailing
//! padding of a chunk. The next entry supplies its two end offsets.

use super::{Block, Kind, Problem, u32_at};
use flate2::{Decompress, FlushDecompress, Status};
use std::sync::OnceLock;
use victory_bcpack as bcpack;

pub(super) fn index(d: &[u8]) -> Option<(usize, usize, Vec<Block>)> {
    let n = u32_at(d, 4)? as usize;
    let total = u32_at(d, 8)? as usize | ((usize::from(*d.get(12)?)) << 32);
    let at = (u32_at(d, 16)? as usize) << 8;
    let table = d.get(16..16 + n.checked_mul(8)?)?;
    if n < 2 || 16 + table.len() > at || d.get(at..at + 4)? != b"ETPF" {
        return None;
    }
    let entry = |p: &[u8]| u64::from_le_bytes(p.try_into().unwrap());
    let physical = |v: u64| ((v & 0x3ff_ffff) << 8) as usize;
    let logical = |v: u64| (((v >> 30) & 0x3f_ffff) << 12) as usize;
    let mut blocks = Vec::with_capacity(n - 1);
    let mut curr = entry(&table[..8]);
    for next in table[8..].chunks_exact(8).map(entry) {
        let file_off = physical(curr);
        let file_end = physical(next);
        let start = logical(curr);
        let len = logical(next).checked_sub(start)?;
        let file_len = file_end.checked_sub(file_off)?.checked_sub((curr >> 52) as usize)?;
        if len == 0 || file_len == 0 || start + len > total {
            return None;
        }
        let codec = ((curr >> 26) & 15) as u8;
        let kind = if file_off + file_len > d.len() {
            Kind::Missing
        } else {
            match codec {
                0 if file_len == len => Kind::Stored,
                1 | 3..=7 => Kind::BcPack(codec),
                8 => Kind::Zlib,
                _ => Kind::Other,
            }
        };
        blocks.push(Block { file_off, file_len, start, len, kind });
        curr = next;
    }
    (logical(curr) == total).then_some((at, total, blocks))
}

/// Grow the output for large chunks only as the stream supplies bytes.
pub(super) fn inflate(packed: &[u8], expect: usize) -> Result<Vec<u8>, Problem> {
    let mut z = Decompress::new(true);
    let mut out = Vec::with_capacity(expect.min(4 << 20) + 1);
    loop {
        let before = (z.total_in(), z.total_out());
        let status = z
            .decompress_vec(&packed[z.total_in() as usize..], &mut out, FlushDecompress::None)
            .map_err(|_| Problem::Compression)?;
        if status == Status::StreamEnd {
            return if out.len() == expect && z.total_in() == packed.len() as u64 { Ok(out) } else { Err(Problem::Compression) };
        }
        if out.len() > expect || before == (z.total_in(), z.total_out()) {
            return Err(Problem::Compression);
        }
        if out.len() == out.capacity() {
            out.try_reserve_exact((expect + 1 - out.len()).min(4 << 20)).map_err(|_| Problem::Compression)?;
        }
    }
}

pub(super) fn decode(codec: u8, packed: &[u8], len: usize) -> Result<Vec<u8>, Problem> {
    let block_bytes = match codec {
        1 | 4 => 8,
        3 | 5..=7 => 16,
        _ => return Err(Problem::Compression),
    };
    // Bound the entropy decoder's lane allocations before passing it a chunk.
    if len == 0 || !len.is_multiple_of(block_bytes) || len > 16 << 20 {
        return Err(Problem::Compression);
    }
    static TABLES: OnceLock<Result<bcpack::Tables, bcpack::Error>> = OnceLock::new();
    let tables = TABLES.get_or_init(bcpack::Tables::builtin).as_ref().map_err(|_| Problem::Compression)?;
    let mut bytes = vec![0; len];
    bcpack::decode(codec, packed, &mut bytes, tables).map_err(|_| Problem::Compression)?;
    Ok(bytes)
}
