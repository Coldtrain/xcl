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
