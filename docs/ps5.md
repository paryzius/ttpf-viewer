# PS5

The tested PS5 archives begin with ETPF at byte zero (version 0x270). Each descriptor's page address points directly into the file. Preview and export read only the required mip or slice from the mapped archive.

Alignment padding can follow a texture, including the final texture. The viewer measures the mip chain from its first page. Using the following texture or the end of the file as the end of the image can shift the picture into padding.

Tile mode zero uses 64 KiB standard tiles. Its element order differs from the Xbox display swizzle. The 4 KiB 2D layout is shared. The viewer uses the PS5 layout automatically, including when both platforms have archives with the same name.

The tested PS5 installation contained 56 archives and 357,111 textures. All catalog entries had a full-size layout. The installation tests also decoded a preview and a DDS from each archive, and compared selected complete mip chains, cube faces and volume slices against shared Xbox assets.
