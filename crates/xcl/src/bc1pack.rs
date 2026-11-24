// SPDX-License-Identifier: MIT OR Apache-2.0
// Adapted 2026-11-23 from bcpack-rs (https://github.com/Ahmet-Dedeler/bcpack-rs).
// The fitted BC1 preset-window and quadrant tables are included under the same dual license.
// bcpack-rs by Ahmet Dedeler (https://github.com/Ahmet-Dedeler, https://ahmetdedeler.com)
// Source: https://github.com/Ahmet-Dedeler/bcpack-rs
// SPDX-License-Identifier: MIT OR Apache-2.0
// If you copy, vendor or port this file, keep this header and credit bcpack-rs in your README.
//! BC1 semantics: lane symbols -> BC1 blocks.
//!
//! Lane layout of a BC1 chunk (12 lanes, each with one symbol per block):
//!
//! | lane | width | meaning |
//! |---|---|---|
//! | 0 | 8 | back-reference distance `d = unzigzag(v) mod 256` (0 = none) |
//! | 1 | 8 | unused (always 0 in retail data) |
//! | 2-7 | 5,6,5,5,6,5 | zigzag residuals of R0 G0 B0 R1 G1 B1 |
//! | 8-11 | 8 | 2x2 index quadrants TL, TR, BL, BR (codebook symbols) |
//!
//! Endpoint fields of block i = fields of block `i - d` (or zero when d = 0)
//! plus the residual, modulo 2^width. A reference before block 0 reads slot
//! `(i - d) mod 256` of a fixed preset window ([`Window`]). The index word is
//! assembled from the four quadrant patterns of a fixed codebook
//! ([`QuadTable`]). Blocks come out in stream order, which is the stored Gfx10
//! tile order of the texture.

use crate::bcpack_rans::rans::{self, DecodeError, LaneOutput};
use serde_json::Value;

/// Verilog `alphabet_bits` (symbol width - 1) of the 12 BC1 lanes.
pub const BC1_ALPHABET_BITS: [u8; 12] = [7, 7, 4, 5, 4, 4, 5, 4, 7, 7, 7, 7];
/// Bit widths of the endpoint fields R0 G0 B0 R1 G1 B1 (lanes 2-7).
pub const ENDPOINT_WIDTHS: [u32; 6] = [5, 6, 5, 5, 6, 5];

/// Default preset window, `data/bc1_window.json`.
pub const DEFAULT_WINDOW_JSON: &str = include_str!("bc1pack_window.json");
/// Default index codebook, `data/bc1_quad_table.json`.
pub const DEFAULT_QUAD_JSON: &str = include_str!("bc1pack_quad_table.json");

/// The 256-slot preset endpoint window read by references before block 0.
#[derive(Clone, Debug)]
pub struct Window {
    pub slots: [[u8; 6]; 256],
    pub fitted: [bool; 256],
}

impl Window {
    /// Parse `{"slots": [null | {"value": [r0,g0,b0,r1,g1,b1], ...}, x256]}`.
    pub fn from_json(text: &str) -> Result<Window, String> {
        let v: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let arr = v["slots"]
            .as_array()
            .ok_or("window: missing \"slots\" array")?;
        if arr.len() != 256 {
            return Err(format!("window: {} slots, need 256", arr.len()));
        }
        let mut w = Window {
            slots: [[0; 6]; 256],
            fitted: [false; 256],
        };
        for (s, e) in arr.iter().enumerate() {
            if e.is_null() {
                continue;
            }
            let val = e["value"]
                .as_array()
                .ok_or("window: slot without \"value\"")?;
            if val.len() != 6 {
                return Err(format!("window: slot {s} has {} fields", val.len()));
            }
            for k in 0..6 {
                let x = val[k].as_u64().ok_or("window: non-integer field")?;
                if x >> ENDPOINT_WIDTHS[k] != 0 {
                    return Err(format!("window: slot {s} field {k} = {x} too wide"));
                }
                w.slots[s][k] = x as u8;
            }
            w.fitted[s] = true;
        }
        Ok(w)
    }

    pub fn default_table() -> Window {
        Window::from_json(DEFAULT_WINDOW_JSON).expect("embedded window table")
    }

    pub fn fitted_count(&self) -> usize {
        self.fitted.iter().filter(|&&f| f).count()
    }
}

/// Index codebook: for each quadrant (TL, TR, BL, BR) and symbol, the bits that
/// symbol contributes to the 32-bit BC1 index word (pixel (x, y) at bit 2*(4y+x)).
#[derive(Clone, Debug)]
pub struct QuadTable {
    pub codes: [[u8; 4]; 256],
    pub masks: [[u32; 256]; 4],
}

/// (x, y) of the top-left pixel of quadrants TL, TR, BL, BR.
const QUAD_ORIGIN: [(u32, u32); 4] = [(0, 0), (2, 0), (0, 2), (2, 2)];
/// Pixel offsets of a quadrant's four codes, in table order TL TR BL BR.
const GROUP_PIXEL: [(u32, u32); 4] = [(0, 0), (1, 0), (0, 1), (1, 1)];

impl QuadTable {
    pub fn from_codes(codes: [[u8; 4]; 256]) -> QuadTable {
        let mut masks = [[0u32; 256]; 4];
        for (q, &(ox, oy)) in QUAD_ORIGIN.iter().enumerate() {
            for s in 0..256 {
                let mut w = 0u32;
                for (k, &(dx, dy)) in GROUP_PIXEL.iter().enumerate() {
                    let (x, y) = (ox + dx, oy + dy);
                    w |= (codes[s][k] as u32 & 3) << (2 * (4 * y + x));
                }
                masks[q][s] = w;
            }
        }
        QuadTable { codes, masks }
    }

    /// Parse `{"entries": [{"sym": s, "codes": [tl, tr, bl, br]}, ...]}`.
    pub fn from_json(text: &str) -> Result<QuadTable, String> {
        let v: Value = serde_json::from_str(text).map_err(|e| e.to_string())?;
        let arr = v["entries"]
            .as_array()
            .ok_or("quad table: missing \"entries\"")?;
        let mut codes = [[0u8; 4]; 256];
        let mut seen = [false; 256];
        for e in arr {
            let s_u64 = e["sym"].as_u64().ok_or("quad table: bad sym")?;
            let c = e["codes"].as_array().ok_or("quad table: bad codes")?;
            if s_u64 > 255 || c.len() != 4 {
                return Err(format!("quad table: bad entry for symbol {s_u64}"));
            }
            let s = s_u64 as usize;
            if seen[s] {
                return Err(format!("quad table: bad entry for symbol {s}"));
            }
            for k in 0..4 {
                let x = c[k].as_u64().ok_or("quad table: bad code")?;
                if x > 3 {
                    return Err(format!("quad table: code {x} > 3"));
                }
                codes[s][k] = x as u8;
            }
            seen[s] = true;
        }
        if !seen.iter().all(|&b| b) {
            return Err("quad table: needs all 256 symbols".into());
        }
        Ok(QuadTable::from_codes(codes))
    }

    pub fn default_table() -> QuadTable {
        QuadTable::from_json(DEFAULT_QUAD_JSON).expect("embedded quad table")
    }
}

/// Lane 0 symbol -> back-reference distance (`unzigzag(v) mod 256`).
#[inline]
pub fn back_distance(v: u16) -> usize {
    let v = v as usize & 0xFF;
    if v & 1 == 0 {
        v >> 1
    } else {
        255 - (v >> 1)
    }
}

/// Zigzag: 0, 1, 2, 3, 4 -> 0, -1, +1, -2, +2.
#[inline]
pub fn unzigzag(v: u16) -> i32 {
    let v = v as i32;
    if v & 1 != 0 {
        -((v + 1) >> 1)
    } else {
        v >> 1
    }
}

/// Endpoints and index words assembled from decoded lanes.
#[derive(Clone, Debug)]
pub struct Bc1Output {
    /// BC1 blocks, 8 bytes each (c0, c1 little endian, then the index word).
    pub blocks: Vec<u8>,
    /// Blocks whose reference chain reaches an unfitted window slot (their
    /// endpoints are probably wrong until the window table is complete).
    pub unfitted_blocks: usize,
    /// Number of references that pointed before block 0.
    pub prestart_refs: usize,
}

/// Turn the 12 decoded BC1 lanes into BC1 blocks.
pub fn assemble(lanes: &LaneOutput, window: &Window, quads: &QuadTable) -> Bc1Output {
    let n = lanes.target;
    let l0 = lanes.lane(0);
    let res: [&[u16]; 6] = [
        lanes.lane(2),
        lanes.lane(3),
        lanes.lane(4),
        lanes.lane(5),
        lanes.lane(6),
        lanes.lane(7),
    ];
    let idx: [&[u16]; 4] = [lanes.lane(8), lanes.lane(9), lanes.lane(10), lanes.lane(11)];
    let mut fields: Vec<[u8; 6]> = vec![[0; 6]; n];
    let mut tainted: Vec<bool> = vec![false; n];
    let mut out = vec![0u8; n * 8];
    let mut unfitted = 0usize;
    let mut prestart = 0usize;
    for i in 0..n {
        let d = back_distance(l0[i]);
        let (pred, t) = if d == 0 {
            ([0u8; 6], false)
        } else if d <= i {
            (fields[i - d], tainted[i - d])
        } else {
            prestart += 1;
            let slot = (i as isize - d as isize).rem_euclid(256) as usize;
            (window.slots[slot], !window.fitted[slot])
        };
        let mut e = [0u8; 6];
        for k in 0..6 {
            let m = (1i32 << ENDPOINT_WIDTHS[k]) - 1;
            e[k] = ((pred[k] as i32 + unzigzag(res[k][i])) & m) as u8;
        }
        fields[i] = e;
        tainted[i] = t;
        unfitted += t as usize;
        let c0 = (e[0] as u16) << 11 | (e[1] as u16) << 5 | e[2] as u16;
        let c1 = (e[3] as u16) << 11 | (e[4] as u16) << 5 | e[5] as u16;
        let w = quads.masks[0][idx[0][i] as usize & 0xFF]
            | quads.masks[1][idx[1][i] as usize & 0xFF]
            | quads.masks[2][idx[2][i] as usize & 0xFF]
            | quads.masks[3][idx[3][i] as usize & 0xFF];
        let o = &mut out[i * 8..i * 8 + 8];
        o[0..2].copy_from_slice(&c0.to_le_bytes());
        o[2..4].copy_from_slice(&c1.to_le_bytes());
        o[4..8].copy_from_slice(&w.to_le_bytes());
    }
    Bc1Output {
        blocks: out,
        unfitted_blocks: unfitted,
        prestart_refs: prestart,
    }
}

/// Decode the 12 lane symbol streams of a BCPack BC1 chunk.
pub fn decode_symbols(chunk: &[u8], blocks: usize) -> Result<LaneOutput, DecodeError> {
    rans::decode_slice(chunk, &BC1_ALPHABET_BITS, blocks)
}

/// BCPack BC1 chunk bytes + block count -> BC1 blocks (stream / Gfx10 order).
pub fn decode(
    chunk: &[u8],
    blocks: usize,
    window: &Window,
    quads: &QuadTable,
) -> Result<Bc1Output, DecodeError> {
    let lanes = decode_symbols(chunk, blocks)?;
    Ok(assemble(&lanes, window, quads))
}

/// Decode a BCPack BC1 chunk using the embedded fitted history tables.
pub fn decode_bc1(chunk: &[u8], blocks: usize) -> Option<Bc1Output> {
    static WINDOW: std::sync::OnceLock<Window> = std::sync::OnceLock::new();
    static QUADS: std::sync::OnceLock<QuadTable> = std::sync::OnceLock::new();
    let window = WINDOW.get_or_init(Window::default_table);
    let quads = QUADS.get_or_init(QuadTable::default_table);
    decode(chunk, blocks, window, quads).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_tables_load() {
        let window = Window::default_table();
        let quads = QuadTable::default_table();
        assert!(window.fitted_count() >= 250);
        assert_eq!(quads.codes.len(), 256);
    }
}
