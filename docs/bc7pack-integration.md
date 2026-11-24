# BC7 BCPack texture upload

XCL exposes an explicit BC7 BCPack upload path in `gpu::texture`:

- `bc7pack_subresources(image, chunks, pipes)` decodes chunks and returns ordinary row-major BC7 subresources.
- `record_upload_bc7pack(...)` records those subresources into an existing D3D12 resource on a caller-owned command list.
- `upload_bc7pack(...)` uploads to an existing resource and waits for completion.
- `create_bc7pack(...)` creates a BC7 D3D12 resource and uploads the decoded chunks.

## Input contract

The caller supplies one BCPack codec `0x1C` chunk for every array-slice/mip pair, ordered array slice first and mip level second. The decoder derives each chunk's block count from the image dimensions. The BC7 decoder emits blocks in physical GFX10 tile order; XCL reorders them using the mip's actual tile offsets, including mip-tail origins, array-slice XORs and the base-address swizzle, before producing the row-major data D3D12 expects.

The GFX10/DXGI format value alone cannot identify BCPack: ordinary BC7 and BCPack BC7 both use BC7 format descriptors. Therefore these functions are deliberately explicit. Do not pass ordinary BC7 block bytes to them, and do not infer codec `0x1C` merely from format 181/182. The existing `subresources` and `create` paths remain the ordinary, uncompressed BC7 path.

## Decoder statistics

The returned `Bc7PackStats` reports total blocks, blocks that depended on unknown fitted history entries, invalid anchor blocks, and blocks normalized by swapping endpoints. Unknown history entries are represented by the decoder's documented fallback and are counted; callers should inspect these counts when validating a texture set.

## Scope

This API completes the decoder-to-D3D12 upload path when the asset/streaming layer has identified the BCPack chunk boundaries. The current title-texture registration callback receives a live resource descriptor and a pointer to its console memory, but no BCPack codec tag or per-mip compressed-chunk boundaries. It must not auto-run the BCPack decoder against that memory: doing so would risk treating valid ordinary BC7 data as a compressed stream. Connect the explicit API at the layer that owns the BCPack metadata.


## Runtime RDS5 / ETPF discovery

XCL now indexes RDS5/ETPF metadata when DirectStorage opens a .ttpf file. It reads only the RDS5 header, chunk table, ETPF descriptors and texture hashes; pixel payloads are still read by the normal DirectStorage request path.

Each DirectStorage request already carries the codec mode, source file offset/length, destination address and decoded length. XCL matches those values against an RDS5 chunk. Only an exact match establishes a runtime store base, calculated as destination minus the chunk virtual offset. This avoids identifying BCPack from a DXGI format value.

The RDS5 registry can then match an ETPF texture descriptor to the title-memory address passed to CreatePlacedResourceX. Matched texture hashes and descriptors are logged for runtime validation. Physical-page destinations and partial/nonmatching requests do not establish a base; they continue through the existing DirectStorage handling without guessed metadata.

DirectStorage BC7 mode 7 is now decoded by the SixNG-derived BC7 decoder, optionally after zlib, into ordinary 16-byte BC7 blocks. The title's existing texture upload path subsequently handles the GFX10 tiled layout.
