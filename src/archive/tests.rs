use super::*;
use std::collections::BTreeSet;
use std::io::Write;

fn fixture(data: &[u8]) -> (tempfile::TempDir, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("sample.TTPF");
    std::fs::File::create(&path).unwrap().write_all(data).unwrap();
    (dir, path)
}

fn catalog(bytes: &mut [u8], version: u32, width: u32, height: u32, mode_format: u32) {
    bytes[..4].copy_from_slice(b"ETPF");
    for (dst, word) in
        bytes[4..32].chunks_exact_mut(4).zip([version, 1, 16, (width - 1) | ((height - 1) << 14), mode_format, 1, 0])
    {
        dst.copy_from_slice(&word.to_le_bytes());
    }
}

#[test]
fn ps5_reads_standard_tiles_before_trailing_padding() {
    let mut file = vec![0; 4096 + 65536 + 8192];
    catalog(&mut file, 0x270, 256, 256, (1 << 21) | (61 << 3) | 1);
    // R8 in PS5 64 KiB standard mode: element 8 is (8,0), 16 is (0,1).
    file[4096 + 8] = 79;
    file[4096 + 16] = 131;
    file[4096 + 65536..].fill(253);
    let (_dir, path) = fixture(&file);
    let arc = Archive::open(&path, &Progress::default()).unwrap();
    let img = arc.best_image(0).unwrap();
    assert_eq!(arc.platform, Platform::Ps5);
    assert_eq!(img.px[4 * 8], 79);
    assert_eq!(img.px[4 * 256], 131);
    let dds = arc.dds(0).unwrap();
    assert_eq!(dds.len(), 148 + 65536);
    assert_eq!(dds[148 + 8], 79);
    assert_eq!(dds[148 + 256], 131);
    assert!(!dds[148..].contains(&253));
}

// Encode one block with equal endpoint and index references, including references
// before the start of the stream. The ordinary encoder does no texture reconstruction.
fn dictionary_stream(mode: bcpack::format::Mode, distance: u8, selector: u8) -> Vec<u8> {
    use bcpack::encode::{Fragment, encode_lanes};
    use bcpack::format::{Mode, deinterleave};
    let widths = mode.alphabet_bits();
    let per = mode.symbols_per_block();
    let mut lanes = vec![vec![Fragment { symbols: vec![0; per], q: 0, z: 0 }]; widths.len()];
    if per == 1 {
        lanes[1][0].symbols[0] = u16::from(distance);
    } else {
        let (even, odd) = deinterleave(distance);
        lanes[1][0].symbols.copy_from_slice(&[even, odd]);
    }
    match mode {
        Mode::Bc1 => {
            for lane in &mut lanes[8..12] {
                lane[0].symbols[0] = u16::from(selector);
            }
        }
        Mode::Bc6h => lanes[15][0].symbols.copy_from_slice(&[u16::from(selector), u16::from(distance % 32)]),
        _ => {}
    }
    encode_lanes(widths, &lanes).unwrap()
}

#[test]
fn xbox_initial_dictionaries() {
    use bcpack::format::Mode;
    // BC1 golden: pack the fixed RGB565 endpoints and apply the quadrant
    // pattern indexed by distance to each of the four dictionary quadrants.
    let mut bc1 = flate2::Crc::new();
    for distance in 0..=255 {
        let packed = dictionary_stream(Mode::Bc1, distance, distance);
        bc1.update(&xbox::decode(1, &packed, 8).unwrap());

        for mode in [Mode::Bc3, Mode::Bc4, Mode::Bc5] {
            let packed = dictionary_stream(mode, distance, 0);
            let size = mode.block_bytes();
            let tables = bcpack::bcn::Tables::default_for(mode).unwrap();
            let reference = bcpack::bcn::decode(&packed, size, tables).unwrap();
            assert_eq!(reference.unfitted_blocks, 0);
            assert_eq!(xbox::decode(mode.codec() >> 2, &packed, size).unwrap(), reference.blocks, "{mode:?} distance {distance}");
        }

        let packed = dictionary_stream(Mode::Bc7, distance, 0);
        let reference = bcpack::bc7::decode(&packed, 16, bcpack::bc7::Tables::default_table()).unwrap();
        assert_eq!(reference.unfitted_blocks, 0);
        assert_eq!(reference.unknown_controls, 0);
        assert_eq!(xbox::decode(7, &packed, 16).unwrap(), reference.blocks, "BC7 distance {distance}");
    }
    assert_eq!(bc1.sum(), 0x0e5c_0398);
}

#[test]
fn xbox_bc6h_all_modes_and_dictionary_entries() {
    // One CRC per BC6H mode. Packed independently from the fixed dictionary
    // using SixNG's BC6H field layouts. Each series starts at the zero reference
    // and reads entries 0..254 in order, with partition = distance modulo 32.
    let expected = [
        0x4c8afb1c, 0x53405412, 0x0941fd72, 0x8375ce41, 0x45da84e7, 0x46024b07, 0x333e6387,
        0x1525d28a, 0x7c808cd0, 0x86332c5f, 0x81ec6836, 0x4a25370d, 0xf4fe938e, 0x982d81e7,
    ];
    for (mode, expected_crc) in expected.into_iter().enumerate() {
        let mut crc = flate2::Crc::new();
        for distance in 0..=255 {
            let packed = dictionary_stream(bcpack::format::Mode::Bc6h, distance, mode as u8);
            crc.update(&xbox::decode(6, &packed, 16).unwrap());
        }
        assert_eq!(crc.sum(), expected_crc, "BC6H mode {}", mode + 1);
    }
}

#[test]
fn xbox_invalid_bcpack() {
    assert_eq!(xbox::decode(2, &[0; 64], 16), Err(Problem::Compression));
    assert_eq!(xbox::decode(1, &[], 8), Err(Problem::Compression));
    assert_eq!(xbox::decode(6, &[0; 64], 15), Err(Problem::Compression));
    let reserved = dictionary_stream(bcpack::format::Mode::Bc6h, 1, 14);
    assert_eq!(xbox::decode(6, &reserved, 16), Err(Problem::Compression));
}

#[test]
fn xbox_bcpack_with_trailing_padding_then_cut_off_payload() {
    let (packed, _) = bcpack::encode::synthetic_bc1(7, 512, 200);
    let at = 256;
    let base = at + 4096;
    let end = (base + packed.len()).next_multiple_of(256);
    let mut file = vec![0; end];
    file[..4].copy_from_slice(b"RDS5");
    for (dst, v) in file[4..24].chunks_exact_mut(4).zip([3u32, 8192, 0, 1, 0]) {
        dst.copy_from_slice(&v.to_le_bytes());
    }
    let pad = end - base - packed.len();
    let entry = ((base / 256) as u64) | (1 << 26) | (1 << 30) | ((pad as u64) << 52);
    file[24..32].copy_from_slice(&entry.to_le_bytes());
    file[32..40].copy_from_slice(&((end / 256) as u64 | (2 << 30)).to_le_bytes());
    catalog(&mut file[at..], 0x27a, 128, 64, (1 << 24) | (1 << 21) | (71 << 3) | 1);
    file[base..base + packed.len()].copy_from_slice(&packed);
    file[base + packed.len()..].fill(0xcc);
    let (_dir, path) = fixture(&file);
    let arc = Archive::open(&path, &Progress::default()).unwrap();
    assert!(arc.textures[0].levels[0].bcpack);
    assert_eq!(arc.best_image(0).unwrap().px.len(), 128 * 64 * 4);
    assert_eq!(arc.dds(0).unwrap().len(), 148 + 4096);
    let (_dir, path) = fixture(&file[..base + packed.len() - 1]);
    let partial = Archive::open(&path, &Progress::default()).unwrap();
    assert_eq!(partial.textures[0].status, Status::Unavailable(Problem::Missing));
}

#[test]
fn truncated_ps5_keeps_the_smaller_mip_and_rejects_a_short_catalog() {
    let mut file = vec![0; 4096 + 4096];
    catalog(&mut file, 0x270, 128, 128, (2 << 24) | (1 << 21) | (98 << 3) | 2);
    let (_dir, path) = fixture(&file);
    let partial = Archive::open(&path, &Progress::default()).unwrap();
    assert_eq!(partial.textures[0].status, Status::Reduced(1));
    assert_eq!(partial.best_image(0).unwrap().w, 64);
    let (_dir, path) = fixture(&file[..30]);
    assert!(Archive::open(&path, &Progress::default()).is_err());
}

fn install(platform: Platform) -> PathBuf {
    let name = match platform {
        Platform::Xbox => "TTPF_XBOX_ROOT",
        Platform::Ps5 => "TTPF_PS5_ROOT",
    };
    std::env::var_os(name).unwrap_or_else(|| panic!("Set {name} to the install folder")).into()
}

#[test]
#[ignore = "requires the two local game installations"]
fn installed_archives() {
    for platform in [Platform::Xbox, Platform::Ps5] {
        let files = crate::folder::ttpf_files(&install(platform));
        assert!(!files.is_empty());
        let mut textures = 0;
        let mut sampled = BTreeSet::new();
        for path in &files {
            let arc = Archive::open(path, &Progress::default()).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
            assert_eq!(arc.platform, platform);
            assert!(arc.blocks.iter().all(|b| !matches!(b.kind, Kind::Other | Kind::Missing)), "{} has unsupported or missing chunks", path.display());
            assert!(!arc.textures.is_empty());
            textures += arc.textures.len();
            for t in &arc.textures {
                assert_eq!(t.status, Status::Full, "{} #{}", path.display(), t.index);
                let level = &t.levels[0];
                let mut check_pixels = t.index == 0;
                if level.bcpack {
                    for (i, _, _) in arc.locate(level.a, level.b).unwrap() {
                        if let Kind::BcPack(codec) = arc.blocks[i].kind {
                            check_pixels |= sampled.insert(codec);
                        }
                    }
                }
                if check_pixels {
                    let img = arc.best_image(t.index).unwrap();
                    assert_eq!((img.w, img.h, img.px.len()), (t.w, t.h, t.w * t.h * 4));
                    assert_eq!(&arc.dds(t.index).unwrap()[..4], b"DDS ");
                }
            }
        }
        eprintln!("{}: {} archives, {textures} textures, sampled BCPack codecs {sampled:?}", platform.name(), files.len());
        if platform == Platform::Xbox {
            assert_eq!(sampled, BTreeSet::from([1, 3, 4, 5, 6, 7]));
        }
    }
}

#[test]
#[ignore = "compares shared assets in the two local game builds"]
fn matching_surfaces() {
    // These assets share encoded elements across the two builds. DDS equality
    // checks all mips and faces, including HDR values and the 3D slice layout.
    for (file, indices) in [
        ("ui/ttdb_ui", &[1, 11, 16][..]),
        ("vehicles/ttdb_vehicles", &[0, 1]),
        ("lighting/lighting", &[1, 2]),
        ("graphics/ttdb_graphics", &[75, 200, 575, 901]),
        ("terrain/ttdb_terrain_0_7", &[1205]),
        ("maps/ttdb_maps_0_7", &[605]),
        ("interiors/ttdb_interiors_1_3", &[2952]),
    ] {
        let [x, p] = [Platform::Xbox, Platform::Ps5].map(|platform| {
            Archive::open(&install(platform).join("data/ttdb").join(file).with_extension("ttpf"), &Progress::default()).unwrap()
        });
        for &i in indices {
            assert!(x.dds(i).unwrap() == p.dds(i).unwrap(), "{file} #{i} DDS mismatch");
            assert!(x.best_image(i).unwrap().px == p.best_image(i).unwrap().px, "{file} #{i} preview mismatch");
        }
    }
}
/// Cross-check the decoded chunks against stored PS5 blocks whenever these builds
/// have the same texture key and descriptor. This compares original BC payloads
/// before tone mapping, normal reconstruction or PNG quantization.
#[test]
#[ignore = "reads every Xbox BCPack chunk and its matching PS5 data"]
fn installed_bcpack_chunks() {
    let xroot = install(Platform::Xbox);
    let proot = install(Platform::Ps5);
    let mut counts = [[0usize; 3]; 8]; // decoded chunks, compared chunks, chunks with padding differences
    let mut dds_checks = 0;
    let mut differing_blocks = 0;
    for path in crate::folder::ttpf_files(&xroot) {
        let x = Archive::open(&path, &Progress::default()).unwrap();
        if !x.blocks.iter().any(|b| matches!(b.kind, Kind::BcPack(_))) {
            continue;
        }
        let rel = path.strip_prefix(&xroot).unwrap();
        let mut peer = proot.join(rel);
        if !peer.exists()
            && let Ok(tail) = rel.strip_prefix("pack/core_restofworld001")
        {
            peer = proot.join("pack/core_restofworld").join(tail);
        }
        let p = peer.exists().then(|| Archive::open(&peer, &Progress::default()).unwrap());
        let peer_records: HashMap<_, _> = p
            .as_ref()
            .map(records)
            .unwrap_or_default()
            .into_iter()
            .enumerate()
            .map(|(i, (key, start))| (key, (i, start)))
            .collect();
        let mut starts: Vec<_> = records(&x)
            .into_iter()
            .enumerate()
            .map(|(index, (key, start))| {
                let mode = (key.1[1] >> 24) & 15;
                (start, index, peer_records.get(&key).copied(), mode)
            })
            .collect();
        starts.sort_unstable();
        let results: Vec<_> = x
            .blocks
            .par_iter()
            .enumerate()
            .filter_map(|(i, b)| {
                let Kind::BcPack(codec) = b.kind else { return None };
                let bytes = x.block(i).unwrap_or_else(|e| panic!("{} chunk {i} codec {codec}: {e:?}", path.display()));
                assert_eq!(bytes.len(), b.len);
                let k = starts.partition_point(|r| r.0 <= b.start) - 1;
                let (start, index, peer_texture, mode) = starts[k];
                assert!(starts.get(k + 1).is_none_or(|r| r.0 >= b.start + b.len));
                let compared = peer_texture.zip(p.as_ref()).map(|((pi, base), p)| {
                    let a = base + (b.start - start);
                    let expected = &p.map[a..a + b.len];
                    let size = if matches!(codec, 1 | 4) { 8 } else { 16 };
                    let mut padding = 0;
                    for (j, block) in bytes.chunks_exact(size).enumerate() {
                        let ps = match (mode, size) {
                            (0, 8) => (j & !31) | (j & 3) | ((j >> 2) & 4) | ((j << 1) & 24),
                            (0, 16) => (j & !15) | ((j >> 1) & 1) | ((j >> 2) & 2) | ((j << 2) & 4) | ((j << 1) & 8),
                            _ => j,
                        };
                        let gold = &expected[ps * size..(ps + 1) * size];
                        if block != gold {
                            // BC7 zero-filled unused elements are encoded as mode 0 on Xbox.
                            // Check this exact representation AND the entire visible DDS below.
                            assert!(
                                codec == 7 && block[0] == 1 && block[1..] == [0; 15] && gold == [0; 16],
                                "{} chunk {i} codec {codec}, BC block {j}",
                                path.display()
                            );
                            padding += 1;
                        }
                    }
                    (index, pi, padding)
                });
                Some((codec, compared))
            })
            .collect();
        let mut check_dds = BTreeSet::new();
        for (codec, compared) in results {
            let c = &mut counts[codec as usize];
            c[0] += 1;
            if let Some((i, pi, padding)) = compared {
                c[1] += 1;
                if padding > 0 {
                    c[2] += 1;
                    differing_blocks += padding;
                    check_dds.insert((i, pi));
                }
            }
        }
        for (i, pi) in check_dds {
            assert!(
                x.dds(i).unwrap() == p.as_ref().unwrap().dds(pi).unwrap(),
                "{} #{i}: BC7 differences reached an exported subresource",
                path.display()
            );
            dds_checks += 1;
        }
    }
    eprintln!(
        "BCPack [decoded, compared, padding differences] by codec: {counts:?}; {differing_blocks} padding blocks, {dds_checks} complete DDS comparisons"
    );
    for codec in [1, 3, 4, 5, 6, 7] {
        assert!(counts[codec][0] > 0);
        assert_eq!(counts[codec][0], counts[codec][1], "codec {codec} missing a PS5 reference");
    }
}

// A texture's sorted 64-bit name hash and storage-independent descriptor words.
fn records(arc: &Archive) -> Vec<((u64, [u32; 4]), usize)> {
    let at = if arc.platform == Platform::Xbox { arc.blocks[0].file_off } else { 0 };
    let n = arc.textures.len();
    let descriptors = arc.map[at + 16..at + 16 + 16 * n].chunks_exact(16);
    let hashes = arc.map[at + 16 + 16 * n..at + 16 + 24 * n].chunks_exact(8);
    descriptors
        .zip(hashes)
        .map(|(r, h)| {
            let words = [u32_at(r, 0).unwrap(), u32_at(r, 4).unwrap(), u32_at(r, 8).unwrap() >> 21, u32_at(r, 12).unwrap()];
            ((u64::from_le_bytes(h.try_into().unwrap()), words), texture_start(r))
        })
        .collect()
}
