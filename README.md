# TTPF Viewer

Browse and extract the textures inside `.ttpf` texture archives.

Drop a `.ttpf` onto the window and every texture appears in a gallery, decoded exactly as the archive's header
describes it: format, size, GPU tile layout and mip levels. Click one to see it large with all its mip levels,
save it as PNG, or extract a whole selection at once.

## Download

Grab `ttpf-viewer-windows-x64.exe` from the [Releases](../../releases) page. It's a single file, nothing to install.
(Linux and macOS: build from source, see below.)

## Using it

| | |
|---|---|
| Open a file | drag it onto the window, click **Open .ttpf…**, or `ttpf-viewer FILE.ttpf` |
| Browse a folder | **Open folder…** (or drop a folder): its `.ttpf` files appear in a sidebar; click one to open it |
| Select | click; **Ctrl**+click to add; **Shift**+click for a range; **Ctrl+A** all; arrow keys to move |
| Preview | mouse wheel zooms, drag pans, double-click fits; click a mip level below the image to view it |
| Extract | **Extract selected** or **Extract all shown**: PNGs named `<number>.png` go into a folder named after the archive |
| Find | search by texture number, size (`512x256`) or format (`BC7`) |

Archives open instantly, even multi-gigabyte ones: only the header is read up front, and texture data is
unpacked when it is shown or exported.

### What it can't show

- **Unsupported compression**: some blocks in these archives are not zlib but a console texture compression
  (probably Xbox BCPack) that has no public decoder. If only a texture's full-size level is stored that way,
  the largest readable mip level is shown instead, marked "only WxH". Otherwise the texture is listed under
  **Show unavailable**.
- **Cut off**: parts missing from a partially downloaded archive.
- **Unsupported format**: any DXGI format not listed below.

Supported formats: BC1, BC2, BC3, BC4, BC5, BC6H (unsigned and signed, shown clipped to 0..1), BC7, R8, A8,
R16_FLOAT (shown as grey scaled to its own range, e.g. heightmaps), R8G8B8A8, B8G8R8A8, R16G16B16A16.

### Videos (.rpf)

The **Videos** tab finds the Bink 2 videos inside a game archive (`.rpf`): open or drop one, tick the videos you
want (or **Save all**). They're saved as `.bk2`, named `<offset>_<width>x<height>_<seconds>s.bk2`. The archive's file
table is encrypted, so videos are found by their own header.

**Also convert to MP4** needs two free tools: RAD Video Tools (from RAD Game Tools) to decode the Bink 2 video, and
FFmpeg to encode the MP4 (H.264 + AAC sound). The app finds them in their usual places (FFmpeg also on the PATH or next
to the app), or use **Locate radvideo64.exe…** / **Locate ffmpeg.exe…**. Open-source Bink 2 decoders don't handle
these videos correctly yet, so without RAD Video Tools you get the `.bk2` files only.

### Command line

```
ttpf-viewer extract FILE.ttpf OUTDIR   save every readable texture as OUTDIR/<number>.png
ttpf-viewer list FILE.ttpf             print the textures in the file
ttpf-viewer videos FILE.rpf OUTDIR     save the Bink 2 videos found in a game archive (--mp4: also convert)
```

## The format

```text
0x00  "RDS5"
0x04  u32  number of blocks
0x08  u32  total size of the unpacked data (low 32 bits; bits 32-39 are in the low byte of 0x0C)
0x10  u32  position of the ETPF section / 256
0x18  u64 per block:  bits  0-23  file offset / 256
                      bits 24-47  position in the unpacked data / 64 (low 24 bits)
                      bits 48-51  position in the unpacked data / 64 (high 4 bits)
then, at the ETPF position:
      "ETPF", u32 ?, u32 number of textures, u32 section size
      16 bytes per texture:
        dword0  bits  0-13 width-1, 14-27 height-1
        dword1  bits  0-9  DXGI format << 3 | flags, bits 24-25 tile mode (0 = 64 KB tiles, else 4 KB)
        dword2  bits  0-20 start of the texture in the unpacked data, in 4 KB pages (+0x800)
      then a sorted list of 8-byte name hashes
```

The blocks are consecutive chunks of one unpacked stream. Each texture is a contiguous run of that stream
holding its mip levels smallest first; the full-size level ends where the next texture starts. Pixel data is
stored in AMD's standard tile order; the tile patterns are in [`src/tile.rs`](src/tile.rs).

## Building from source

Install Rust from [rustup.rs](https://rustup.rs), then:

```
cargo build --release
```

The program is `target/release/ttpf-viewer` (`.exe` on Windows). On Linux you also need the GTK3 and
xkbcommon development packages (`sudo apt install libgtk-3-dev libxkbcommon-dev`).

For development, `TTPF_VIEWER_TEST="out.png;select=12,level=1,light"` makes the app load the given file,
apply the actions, save its own rendered window to `out.png` and quit.

## Credits

- [egui / eframe](https://github.com/emilk/egui) for the interface
- [texture2ddecoder](https://github.com/UniversalGameExtraction/texture2ddecoder) for BCn decoding
- [rfd](https://github.com/PolyMeilex/rfd), [image](https://github.com/image-rs/image), [flate2](https://github.com/rust-lang/flate2-rs), [rayon](https://github.com/rayon-rs/rayon), [memmap2](https://github.com/RazrFalcon/memmap2-rs), [memchr](https://github.com/BurntSushi/memchr)
- MP4 conversion runs [RAD Video Tools](https://www.radgametools.com/bnkdown.htm) and [FFmpeg](https://ffmpeg.org) if you have them installed; neither is included.

## License

MIT, see [LICENSE](LICENSE).
