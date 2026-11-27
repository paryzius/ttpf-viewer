# Both platforms

Open a game installation with Open folder, then use Search all to search its data and pack directories. The viewer identifies each archive by its header. Files can also be opened individually.

The ETPF header holds a version at 0x04, a texture count at 0x08 and a table size at 0x0C. Records start at 0x10 and occupy 16 bytes each. The first word packs width minus one, height minus one and a mip count field. Add the low three bits of the second word to that field for the total mip count. Bits 3–9 of the second word identify the DXGI format. Bits 10–20 encode slice count or depth minus one. Bits 21–22 distinguish 2D, cube and volume resources. The low 21 bits of the third word give the texture's start in 4 KiB pages.

Levels run from smallest to largest in storage. Small levels can share one tile. Arrays store slices together per mip. Streamed cubes store successive copies, with all six full-resolution faces in the last copy. PNG exports the selected view. DDS preserves the decoded blocks or texels, mip levels, and face or slice order.
