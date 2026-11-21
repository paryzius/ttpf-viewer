# TTPF Viewer

A fast viewer and extractor for the textures inside `.ttpf` texture archives. Drop a file in and browse
thousands of textures at once, look at any of them up close, and save them as PNG or DDS.

![TTPF Viewer: a gallery of light textures, with a cube map shown on a 3D sphere](docs/screenshot.png)

- **Exact decoding**: format, size, GPU tile layout and mip levels come straight from the archive's header,
  with no guessing
- **All common formats**: BC1 to BC7, HDR (BC6H, half-float) and uncompressed
- **Cube maps and texture arrays**, with a 3D view and 360° panorama export
- **Export** as PNG, or as DDS with the original data (lossless, mip levels included)
- **Whole folders**: browse them in a sidebar, search every archive at once, extract everything in one go
- **Fast**: multi-gigabyte archives open instantly; textures are only unpacked when shown

## Download

Get `ttpf-viewer-windows-x64.exe` from the [Releases](../../releases) page. It's a single file with nothing to
install. On Linux or macOS, [build it from source](#building-from-source).

## Quick start

1. Drag a `.ttpf` onto the window (or click **Open .ttpf…**). To browse a whole folder, drag the folder in
   instead, or use **Open folder…**.
2. Click a texture to see it large, with its mip levels underneath.
3. Select textures and click **Extract selected**, or **Extract all shown**. They're saved as `<number>.png`
   in a folder named after the archive.

## Finding textures

- **Search** by texture number, size (`512x256`) or format (`BC7`).
- **Show** only cube maps, texture arrays, HDR textures, ones readable only at a smaller size, or unavailable ones.
- **Sort** by number, size or format, ascending or descending.
- With a folder open, **Search all…** searches every `.ttpf` in it and shows the results as pictures or a list.
  Click one to open it.

## Looking at a texture

- Mouse wheel zooms, drag pans, double-click fits. Click a mip level under the image to view it.
- **Cube maps** show their six faces. Switch to **3D** to look around inside the cube map like a sky, or see it
  wrapped on a **Sphere**: drag to turn, wheel to zoom. **Up: Z / Y** sets which way is up (Z for RAGE games).
- **Texture arrays** show their slices. Click one to view it.
- **HDR textures** (BC6H, half-float) are tone-mapped so bright parts keep their detail. Choose **HDR: clipped**
  in the toolbar to see the stored values instead.
- **Normal maps** that store only X and Y (in red and green) use blue for other data, so they look olive.
  Tick **Normal map: rebuild blue** to see them as normal maps; untick it to go back. The archives don't mark
  which textures are normal maps, so this is never done automatically. (BC5 normal maps always get blue rebuilt.)

## Saving

| | |
|---|---|
| **PNG** | what you see, as `<number>.png` |
| **DDS** | the original data: lossless, with its compression, HDR values and mip levels. Choose **DDS** next to the Extract buttons, or **Save DDS…** for one texture |
| **Panorama** | a cube map as one 360° picture (2:1), the format sky viewers and 3D programs use |
| **Copy** | right-click a texture, or **Copy** under the preview, to put it on the clipboard |
| **Whole folder** | **Extract all…** in the folder sidebar: every archive into its own folder |

Cube maps are saved as one PNG per face (`<number>_px.png`, `_nx`, `_py`, `_ny`, `_pz`, `_nz`) and texture
arrays as one per slice (`<number>_0.png`, `_1`, …). In DDS, all faces or slices go in one file.

Settings (sort, filter, thumbnail size, HDR, PNG/DDS, 3D up axis and the open folder) are remembered between runs.

## Keys and mouse

| | |
|---|---|
| Click | select and preview |
| Ctrl+click / Shift+click | add to the selection / select a range |
| Ctrl+A | select everything shown |
| Arrow keys | move through the gallery |
| Esc | clear the selection and close the preview |
| Right-click | copy, save PNG, DDS or panorama |

## What it can't show

- **BCPack**: some parts of these archives use BCPack, the Xbox hardware texture compression, which has no
  public decoder. When only the full-size level is stored that way, the largest readable mip level is shown
  instead, marked "(only WxH)". Otherwise the texture is unavailable; tick **Show unavailable** to list it anyway.
- **Cut-off files**: parts missing from a partly downloaded archive.
- **Unsupported formats**: any not in the list below. None of the archives seen so far use one.

Supported formats: BC1, BC2, BC3, BC4, BC5, BC6H (unsigned and signed), BC7, R8, A8, R16_FLOAT (shown as grey,
scaled to its own range, e.g. heightmaps), R8G8B8A8, B8G8R8A8, R16G16B16A16 (UNORM and FLOAT).

## Command line

```
ttpf-viewer FILE.ttpf                      open a file in the app
ttpf-viewer extract FILE.ttpf OUTDIR       save every readable texture as OUTDIR/<number>.png
ttpf-viewer extract-folder DIR OUTDIR      the same for every .ttpf under DIR, one folder per file
ttpf-viewer panorama FILE.ttpf N OUT.png   save cube map #N as a 360° panorama
ttpf-viewer list FILE.ttpf                 print the textures in the file
```

Options: `--dds` saves DDS instead of PNG, `--clip` clips HDR textures instead of tone-mapping them, and `--y-up`
(for `panorama`) treats Y as up.

## The format

```text
0x00  "RDS5"
0x04  u32  number of blocks
0x08  u32  total size of the unpacked data (low 32 bits; bits 32-39 are in the low byte of 0x0C)
0x10  u32  position of the ETPF section / 256
0x18  u64 per block:  bits  0-25  where the block ends in the file / 256
                      bits 26-29  how the NEXT block is stored: 0 = as-is, 1-7 = BCPack, 8 = zlib
                      bits 30-51  where the block ends in the unpacked data / 4096
      (the first block starts at the ETPF position and holds the ETPF section, stored as-is)
then, at the ETPF position:
      "ETPF", u32 ?, u32 number of textures, u32 section size
      16 bytes per texture:
        dword0  bits  0-13 width-1, 14-27 height-1, 28-31 mip levels - 1 (cube maps: mip levels)
        dword1  bits  0-9  DXGI format << 3 | flags, bits 10-19 array size - 1,
                bits 24-27 tile mode (0 = 64 KB tiles, 1-3 = 4 KB, 9-12 = 4 KB cube maps)
        dword2  bits  0-20 start of the texture in the unpacked data / 4096
      then a sorted list of 8-byte name hashes
```

The blocks are consecutive chunks of one unpacked stream. Each texture is a contiguous run of that stream
holding its mip levels smallest first; the full-size level ends where the next texture starts. Pixel data is
stored in AMD's standard tile order; the tile patterns are in [`src/tile.rs`](src/tile.rs).

Mip levels follow AMD's Gfx10 layout: smallest first, each in whole tiles, except that the smallest levels share
one "mip tail" tile at fixed spots ([`src/layout.rs`](src/layout.rs)). Texture arrays store level by level, each
level's slices back to back. Cube maps (tile modes 9-12) store tile mode - 8 complete cubes of growing size, the
full-size one last.

## Building from source

Install Rust from [rustup.rs](https://rustup.rs), then:

```
cargo build --release
```

The program is `target/release/ttpf-viewer` (`.exe` on Windows). On Linux you also need the GTK3 and
xkbcommon development packages (`sudo apt install libgtk-3-dev libxkbcommon-dev`).

For development, `TTPF_VIEWER_TEST="out.png;select=12,level=1,light"` makes the app open the given file, apply
the actions, save a screenshot of its own window to `out.png` and quit.

## Credits

- [egui / eframe](https://github.com/emilk/egui) for the interface
- [texture2ddecoder](https://github.com/UniversalGameExtraction/texture2ddecoder) for BCn decoding;
  `src/bc6h.rs` is its BC6H decoder adapted to keep HDR values (MIT, notice in the file)
- [arboard](https://github.com/1Password/arboard), [rfd](https://github.com/PolyMeilex/rfd),
  [image](https://github.com/image-rs/image), [flate2](https://github.com/rust-lang/flate2-rs),
  [rayon](https://github.com/rayon-rs/rayon) and [memmap2](https://github.com/RazrFalcon/memmap2-rs)
- [Percy6Stars](https://github.com/Ahmet-Dedeler/Percy6Stars) and [PercyStar](https://github.com/rpatty-dev/PercyStar)
  (by rpatty-dev), whose research showed how the block table encodes block ends and storage types, and
  [GTA6-Asset-Info](https://github.com/TrinitronClient/GTA6-Asset-Info) for the descriptor fields and the cube map
  and array layouts. This app's code is its own.

## License

MIT, see [LICENSE](LICENSE).
