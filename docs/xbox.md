# Xbox

The Xbox archives start with RDS5. The word at 0x10 gives the ETPF offset in 256-byte units. The count at 0x04 includes the entry at 0x10 and the final boundary. The decoded stream length uses the word at 0x08 and the low byte at 0x0C. Each pair of adjacent entries gives one chunk’s extent. Bits 0–25 of each entry give the file offset in 256-byte units and bits 30–51 give the logical offset in 4 KiB pages. Bits 52–63 of the starting entry give its trailing padding in bytes.

The codec is in bits 26–29 of the starting entry. Stored chunks use 0 and zlib uses 8. The tested archives use six BCPack modes, numbered 1 for BC1, 3 for BC3, 4 for BC4, 5 for BC5, 6 for BC6H and 7 for BC7. Microsoft also assigns 2 to packed BC2. The viewer reports that codec as unsupported.

The gallery, PNG and DDS use the same VIctory decoder. A packed block identifies where to get its endpoints and indices and stores the differences needed to reconstruct them. A reference can select the zero state, an earlier block in the same chunk or one of 255 fixed dictionary entries. The decoder reads the reference and applies the encoded differences. The complete dictionaries ship inside the viewer, so an Xbox archive opens on its own.

Tile mode zero uses the 64 KiB display swizzle. Modes 1–3 and the supported cube modes use 4 KiB standard tiles. A cut-off archive can still supply levels contained entirely in its available chunks.

The dictionary tests exercise all 255 initial entries and the zero reference in each supported format, including every BC1 quadrant code and all 14 BC6H block modes. The full install test compares Xbox blocks with the independently stored PS5 blocks for the same texture key, descriptor and tile position. Graphics texture 75 also checks the BC7 index dictionary through a full DDS export. Shifting the index table by one word changes that export.

The tested Xbox installation contained 57 archives and 357,115 textures. All 108,605 BCPack chunks decoded, comprising 102,725 BC1, 35 BC3, 77 BC4, 333 BC5, 268 BC6H and 5,167 BC7. The only cross-platform differences were unused BC7 padding in 164 textures. Each of their complete DDS exports matched its PS5 counterpart byte for byte.
