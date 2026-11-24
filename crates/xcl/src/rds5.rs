//! Runtime metadata for GTA VI RDS5 / ETPF texture stores.
//!
//! The RDS5 chunk table is the authoritative codec and boundary map. DirectStorage requests
//! expose the file offset, source length, codec mode, destination and decoded length. When a
//! request exactly covers an RDS5 chunk, their relationship gives the runtime virtual base:
//! runtime_base = destination - chunk.virtual_offset. ETPF descriptors can then be matched
//! to the title's CreatePlacedResourceX virtual address without guessing from DXGI format.

use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom};
use std::sync::{Arc, Mutex, OnceLock, Weak};

#[derive(Clone, Debug)]
pub struct Chunk {
    pub file_offset: u64, pub file_end: u64, pub stored_len: u64,
    pub virtual_offset: u64, pub virtual_end: u64, pub virtual_len: u64,
    pub codec: u8, pub padding: u64,
}

#[derive(Clone, Debug)]
pub struct Texture {
    pub hash: u64, pub width: u32, pub height: u32, pub dxgi_format: u32,
    pub virtual_offset: u64, pub d0: u32, pub d1: u32,
}
impl Texture {
    pub fn mips_field(&self) -> u32 { (self.d0 >> 28) + 1 }
    pub fn array_size(&self) -> u32 { ((self.d1 >> 10) & 0x3ff) + 1 }
    pub fn kind(&self) -> u32 { (self.d1 >> 20) & 0xf }
    pub fn tile_mode(&self) -> u32 { (self.d1 >> 24) & 0xf }
}

#[derive(Debug)]
pub struct Store {
    pub path: String, pub file_len: u64, pub virtual_size: u64,
    pub chunks: Vec<Chunk>, pub textures: Vec<Texture>,
}

impl Store {
    fn parse<R: Read + Seek>(file: &mut R, path: String, file_len: u64) -> io::Result<Self> {
        let mut header = [0u8; 24];
        file.seek(SeekFrom::Start(0))?;
        file.read_exact(&mut header)?;
        if &header[..4] != b"RDS5" { return Err(invalid("not an RDS5 store")); }
        let count = u32le(&header, 4) as usize;
        if !(2..=1_000_000).contains(&count) { return Err(invalid("invalid RDS5 chunk count")); }
        let virtual_size = u32le(&header, 8) as u64;
        let table_len = count.checked_mul(8).ok_or_else(|| invalid("chunk table overflow"))?;
        if 24u64.checked_add(table_len as u64).map_or(true, |end| end > file_len) {
            return Err(invalid("truncated RDS5 chunk table"));
        }
        let mut raw = vec![0u8; table_len];
        file.read_exact(&mut raw)?;
        let mut rows = Vec::with_capacity(count);
        for i in 0..count {
            let a = u32le(&raw, i * 8);
            let b = u32le(&raw, i * 8 + 4);
            if a == 0 && b == 0 { break; }
            let virtual_offset = 0x1000u64
                .checked_add(((b & 0x000f_ffff) as u64) * 0x4000)
                .and_then(|v| v.checked_add(((a >> 30) as u64) * 0x1000))
                .ok_or_else(|| invalid("virtual chunk offset overflow"))?;
            rows.push((((a & 0x00ff_ffff) as u64) << 8, virtual_offset, ((a >> 24) & 0x3f) as u8, (b >> 20) as u64));
        }
        if rows.len() < 2 { return Err(invalid("RDS5 chunk table has no terminal range")); }
        let mut chunks = Vec::with_capacity(rows.len() - 1);
        for pair in rows.windows(2) {
            let (file_offset, virtual_offset, codec, padding) = pair[0];
            let (file_end, virtual_end, _, _) = pair[1];
            let stored_len = file_end.checked_sub(file_offset).and_then(|v| v.checked_sub(padding))
                .ok_or_else(|| invalid("invalid RDS5 stored chunk length"))?;
            let virtual_len = virtual_end.checked_sub(virtual_offset)
                .ok_or_else(|| invalid("non-monotonic RDS5 virtual chunk table"))?;
            if file_end > file_len || stored_len == 0 || virtual_len == 0 {
                return Err(invalid("RDS5 chunk range outside file or empty"));
            }
            chunks.push(Chunk { file_offset, file_end, stored_len, virtual_offset, virtual_end, virtual_len, codec, padding });
        }

        // ETPF follows the chunk table on a 256-byte boundary.
        let etpf_offset = (0x10u64 + 8u64 * count as u64 + 0xff) & !0xff;
        if etpf_offset.checked_add(16).map_or(true, |end| end > file_len) {
            return Err(invalid("ETPF header outside RDS5 file"));
        }
        file.seek(SeekFrom::Start(etpf_offset))?;
        let mut etpf = [0u8; 16];
        file.read_exact(&mut etpf)?;
        if &etpf[..4] != b"ETPF" { return Err(invalid("RDS5 store has no ETPF index")); }
        let texture_count = u32le(&etpf, 8) as usize;
        if texture_count > 1_000_000 { return Err(invalid("unreasonable ETPF texture count")); }
        let body_len = texture_count.checked_mul(24).ok_or_else(|| invalid("ETPF table overflow"))?;
        let body_start = etpf_offset + 16;
        if body_start.checked_add(body_len as u64).map_or(true, |end| end > file_len) {
            return Err(invalid("truncated ETPF descriptors / hashes"));
        }
        let mut body = vec![0u8; body_len];
        file.read_exact(&mut body)?;
        let mut textures = Vec::with_capacity(texture_count);
        for i in 0..texture_count {
            let d0 = u32le(&body, i * 16);
            let d1 = u32le(&body, i * 16 + 4);
            let loc = u64le(&body, i * 16 + 8);
            textures.push(Texture {
                hash: u64le(&body, texture_count * 16 + i * 8),
                width: (d0 & 0x3fff) + 1,
                height: ((d0 >> 14) & 0x3fff) + 1,
                dxgi_format: (d1 >> 3) & 0x7f,
                virtual_offset: (loc & 0x1f_ffff) * 0x1000 + 0x1000,
                d0, d1,
            });
        }
        Ok(Self { path, file_len, virtual_size, chunks, textures })
    }

    pub fn chunk_at_file_offset(&self, offset: u64) -> Option<&Chunk> {
        self.chunks.binary_search_by_key(&offset, |c| c.file_offset).ok().and_then(|i| self.chunks.get(i))
    }
}

pub struct RuntimeStore { pub store: Store, bases: Mutex<Vec<usize>> }

impl RuntimeStore {
    /// Match a full DirectStorage request to an RDS5 chunk. All four metadata dimensions must agree.
    pub fn observe_request(&self, file_offset: u64, source_len: usize, destination: usize,
        destination_len: usize, bcpack_mode: u8, zlib: bool) -> bool {
        let Some(chunk) = self.store.chunk_at_file_offset(file_offset) else { return false };
        let Some(expected_mode) = codec_mode(chunk.codec) else { return false };
        if expected_mode != bcpack_mode || (chunk.codec == 0x20) != zlib
            || chunk.stored_len != source_len as u64 || chunk.virtual_len != destination_len as u64 {
            return false;
        }
        let Some(base) = destination.checked_sub(chunk.virtual_offset as usize) else { return false };
        if base == 0 { return false; }
        let mut bases = self.bases.lock().unwrap();
        if !bases.contains(&base) {
            bases.push(base);
            crate::xlog_info!("RDS5: mapped '{}' runtime base {base:#x} from file chunk {file_offset:#x} -> virtual {:#x} (codec {:#04x})",
                self.store.path, chunk.virtual_offset, chunk.codec);
        }
        true
    }

    fn texture_at(&self, address: usize) -> Option<Texture> {
        let bases = self.bases.lock().ok()?;
        self.store.textures.iter().find_map(|t| {
            bases.iter().any(|&base| base.checked_add(t.virtual_offset as usize) == Some(address)).then(|| t.clone())
        })
    }
}

fn codec_mode(codec: u8) -> Option<u8> {
    match codec {
        0x00 | 0x20 => Some(0), 0x04 => Some(1), 0x08 => Some(2), 0x0c => Some(3),
        0x10 => Some(4), 0x14 => Some(5), 0x18 => Some(6), 0x1c => Some(7), _ => None,
    }
}

static STORES: OnceLock<Mutex<Vec<Weak<RuntimeStore>>>> = OnceLock::new();

/// Open and index RDS5/ETPF metadata. The texture payload is not read here.
pub fn open(path: &str) -> Option<Arc<RuntimeStore>> {
    if !path.to_ascii_lowercase().ends_with(".ttpf") { return None; }
    let mut file = File::open(path).ok()?;
    let file_len = file.metadata().ok()?.len();
    let store = match Store::parse(&mut file, path.to_string(), file_len) {
        Ok(store) => store,
        Err(error) => {
            crate::xlog_warn!("RDS5: could not index '{}': {error}", path);
            return None;
        }
    };
    crate::xlog_info!("RDS5: indexed '{}': {} chunks, {} ETPF textures, file {} bytes, virtual space {:#x}",
        store.path, store.chunks.len(), store.textures.len(), store.file_len, store.virtual_size);
    let runtime = Arc::new(RuntimeStore { store, bases: Mutex::new(Vec::new()) });
    let mut stores = STORES.get_or_init(|| Mutex::new(Vec::new())).lock().ok()?;
    stores.retain(|w| w.strong_count() > 0);
    stores.push(Arc::downgrade(&runtime));
    Some(runtime)
}

pub fn texture_at_runtime_address(address: usize) -> Option<Texture> {
    let stores = STORES.get()?.lock().ok()?;
    stores.iter().filter_map(Weak::upgrade).find_map(|s| s.texture_at(address))
}

fn invalid(message: &str) -> io::Error { io::Error::new(io::ErrorKind::InvalidData, message) }
fn u32le(b: &[u8], o: usize) -> u32 { u32::from_le_bytes(b[o..o + 4].try_into().unwrap()) }
fn u64le(b: &[u8], o: usize) -> u64 { u64::from_le_bytes(b[o..o + 8].try_into().unwrap()) }

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;
    #[test]
    fn maps_rds5_codec_flags_to_directstorage_modes() {
        assert_eq!(codec_mode(0x00), Some(0)); assert_eq!(codec_mode(0x04), Some(1));
        assert_eq!(codec_mode(0x0c), Some(3)); assert_eq!(codec_mode(0x18), Some(6));
        assert_eq!(codec_mode(0x1c), Some(7)); assert_eq!(codec_mode(0x20), Some(0));
        assert_eq!(codec_mode(0x24), None);
    }
    #[test]
    fn parses_rds5_chunk_ranges_and_etpf_texture_metadata() {
        let mut bytes = vec![0u8; 0x400];
        bytes[..4].copy_from_slice(b"RDS5");
        bytes[4..8].copy_from_slice(&2u32.to_le_bytes());
        bytes[8..12].copy_from_slice(&0x5000u32.to_le_bytes());
        bytes[24..28].copy_from_slice(&0x1c00_0002u32.to_le_bytes());
        bytes[28..32].copy_from_slice(&0u32.to_le_bytes());
        bytes[32..36].copy_from_slice(&3u32.to_le_bytes());
        bytes[36..40].copy_from_slice(&1u32.to_le_bytes());
        bytes[0x100..0x104].copy_from_slice(b"ETPF");
        bytes[0x108..0x10c].copy_from_slice(&1u32.to_le_bytes());
        let d0 = 255u32 | (127u32 << 14) | (6u32 << 28);
        let d1 = 98u32 << 3;
        bytes[0x110..0x114].copy_from_slice(&d0.to_le_bytes());
        bytes[0x114..0x118].copy_from_slice(&d1.to_le_bytes());
        bytes[0x118..0x120].copy_from_slice(&0u64.to_le_bytes());
        bytes[0x120..0x128].copy_from_slice(&0x1122_3344_5566_7788u64.to_le_bytes());

        let len = bytes.len() as u64;
        let mut cursor = Cursor::new(bytes);
        let store = Store::parse(&mut cursor, "synthetic.ttpf".into(), len).unwrap();
        assert_eq!(store.chunks.len(), 1);
        assert_eq!(store.chunks[0].file_offset, 0x200);
        assert_eq!(store.chunks[0].stored_len, 0x100);
        assert_eq!(store.chunks[0].virtual_offset, 0x1000);
        assert_eq!(store.chunks[0].virtual_len, 0x4000);
        assert_eq!(store.chunks[0].codec, 0x1c);
        assert_eq!(store.textures.len(), 1);
        assert_eq!(store.textures[0].hash, 0x1122_3344_5566_7788);
        assert_eq!((store.textures[0].width, store.textures[0].height), (256, 128));
        assert_eq!(store.textures[0].dxgi_format, 98);
        assert_eq!(store.textures[0].virtual_offset, 0x1000);
        assert_eq!(store.textures[0].mips_field(), 7);
    }

    #[test]
    fn runtime_base_requires_an_exact_chunk_request() {
        let runtime = RuntimeStore {
            store: Store { path: "test.ttpf".into(), file_len: 0x2000, virtual_size: 0x5000,
                chunks: vec![Chunk { file_offset: 0x100, file_end: 0x180, stored_len: 0x80,
                    virtual_offset: 0x2000, virtual_end: 0x3000, virtual_len: 0x1000, codec: 0x1c, padding: 0 }],
                textures: vec![Texture { hash: 0x1234, width: 256, height: 256, dxgi_format: 98,
                    virtual_offset: 0x2000, d0: 0, d1: 0 }] },
            bases: Mutex::new(Vec::new()),
        };
        assert!(!runtime.observe_request(0x100, 0x80, 0x902000, 0x1000, 6, false));
        assert!(runtime.observe_request(0x100, 0x80, 0x902000, 0x1000, 7, false));
        assert_eq!(runtime.texture_at(0x902000).unwrap().hash, 0x1234);
        assert!(runtime.texture_at(0x902001).is_none());
    }
}
