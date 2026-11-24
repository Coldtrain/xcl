// SPDX-License-Identifier: GPL-3.0-only
// Adapted 2026-11-23 from the clean-room BC5 block rebuilder in Dom-in/Leonida's tools/bcpack.py
// and its codec-level tables in tools/bcpack_tables.py (GPL-3.0-only).
// The rANS lane decoder is shared with the bcpack-rs/Microsoft silicon-rans port.

use crate::bcpack_rans::rans;
use std::collections::BTreeSet;
use std::sync::OnceLock;

const BC5_ALPHABET_BITS: [u8; 12] = [4, 4, 8, 8, 6, 6, 6, 6, 6, 6, 6, 6];

const BC5_PAIR: [[u8; 2]; 64] = [
    [0, 0],
    [1, 1],
    [4, 4],
    [3, 3],
    [6, 6],
    [7, 7],
    [4, 0],
    [0, 4],
    [0, 1],
    [2, 2],
    [1, 0],
    [3, 0],
    [0, 3],
    [5, 5],
    [2, 0],
    [0, 2],
    [1, 6],
    [6, 1],
    [4, 3],
    [1, 7],
    [3, 4],
    [1, 4],
    [7, 1],
    [6, 0],
    [0, 6],
    [6, 3],
    [6, 4],
    [3, 6],
    [3, 2],
    [4, 6],
    [1, 5],
    [1, 3],
    [2, 3],
    [5, 4],
    [5, 3],
    [5, 0],
    [4, 1],
    [7, 6],
    [4, 5],
    [4, 2],
    [3, 1],
    [3, 5],
    [5, 1],
    [6, 7],
    [2, 4],
    [7, 5],
    [7, 4],
    [0, 5],
    [6, 5],
    [4, 7],
    [5, 7],
    [7, 0],
    [5, 6],
    [0, 7],
    [7, 3],
    [3, 7],
    [5, 2],
    [2, 5],
    [1, 2],
    [6, 2],
    [2, 1],
    [2, 6],
    [7, 2],
    [2, 7],
];

struct History {
    confirmed: [Option<[u8; 4]>; 256],
    provisional: [Option<[u8; 4]>; 256],
}

fn parse_entries(value: &serde_json::Value) -> [Option<[u8; 4]>; 256] {
    let mut out = [None; 256];
    if let Some(entries) = value.as_object() {
        for (key, value) in entries {
            let Ok(index) = key.parse::<usize>() else { continue };
            if index >= out.len() { continue; }
            let Some(values) = value.as_array() else { continue };
            if values.len() != 4 { continue; }
            let mut row = [0u8; 4];
            let mut valid = true;
            for (i, value) in values.iter().enumerate() {
                if let Some(n) = value.as_u64() {
                    row[i] = n as u8;
                } else {
                    valid = false;
                    break;
                }
            }
            if valid {
                out[index] = Some(row);
            }
        }
    }
    out
}

fn history() -> &'static History {
    static HISTORY: OnceLock<History> = OnceLock::new();
    HISTORY.get_or_init(|| {
        let json: serde_json::Value =
            serde_json::from_str(include_str!("bc5pack_history.json")).expect("valid BC5 history table");
        History {
            confirmed: parse_entries(&json["confirmed"]),
            provisional: parse_entries(&json["provisional"]),
        }
    })
}

#[derive(Debug)]
pub struct Bc5Output {
    pub blocks: Vec<u8>,
    pub provisional_entries: Vec<usize>,
    pub unknown_entries: Vec<usize>,
    pub unknown_blocks: usize,
}

#[inline]
fn unzig(v: u16) -> i32 {
    let v = v as i32;
    if v & 1 != 0 { -((v + 1) >> 1) } else { v >> 1 }
}

#[inline]
fn inter8(a: u16, b: u16) -> u16 {
    let mut d = 0u16;
    for k in 0..4 {
        d |= ((a >> k) & 1) << (2 * k);
        d |= ((b >> k) & 1) << (2 * k + 1);
    }
    d
}

/// Decode one BCPack BC5 chunk to ordinary 16-byte BC5 blocks.
/// Fitted history entries are accepted provisionally; unknown roots use the neutral
/// endpoint fallback only while they remain below the source decoder's loose-block limit.
pub fn decode_bc5(chunk: &[u8], blocks: usize) -> Option<Bc5Output> {
    let symbols = blocks.checked_mul(2)?;
    let lanes = rans::decode_slice(chunk, &BC5_ALPHABET_BITS, symbols).ok()?;
    let l0 = lanes.lane(0);
    let lane1 = lanes.lane(1);
    let red = lanes.lane(2);
    let green = lanes.lane(3);
    let rows: Vec<&[u16]> = (4..12).map(|i| lanes.lane(i)).collect();
    if l0.len() != symbols || lane1.len() != symbols || red.len() != symbols || green.len() != symbols
        || rows.iter().any(|lane| lane.len() != symbols) || lane1.iter().any(|&v| v != 0)
    {
        return None;
    }

    let h = history();
    let mut out = vec![0u8; blocks.checked_mul(16)?];
    let mut hist = vec![[128u8; 4]; blocks];
    let mut lost = vec![false; blocks];
    let mut provisional = BTreeSet::new();
    let mut unknown = BTreeSet::new();

    for i in 0..blocks {
        let a = 2 * i;
        let b = a + 1;
        let d = (unzig(inter8(l0[a], l0[b])) & 255) as usize;
        let base = if d == 0 {
            [128u8; 4]
        } else if i >= d {
            lost[i] = lost[i - d];
            hist[i - d]
        } else {
            let entry = 256 + i - d;
            if let Some(base) = h.confirmed.get(entry).and_then(|v| *v) {
                base
            } else if let Some(base) = h.provisional.get(entry).and_then(|v| *v) {
                provisional.insert(entry);
                base
            } else {
                unknown.insert(entry);
                lost[i] = true;
                [128u8; 4]
            }
        };
        let e = [
            (base[0] as i32 + unzig(red[a])).rem_euclid(256) as u8,
            (base[1] as i32 + unzig(red[b])).rem_euclid(256) as u8,
            (base[2] as i32 + unzig(green[a])).rem_euclid(256) as u8,
            (base[3] as i32 + unzig(green[b])).rem_euclid(256) as u8,
        ];
        hist[i] = e;

        let at = i * 16;
        for ch in 0..2 {
            let mut bits = 0u64;
            for r in 0..4 {
                let left_index = rows[4 * ch + r][a] as usize;
                let right_index = rows[4 * ch + r][b] as usize;
                let left = *BC5_PAIR.get(left_index)?;
                let right = *BC5_PAIR.get(right_index)?;
                let row = (left[0] as u64)
                    | ((left[1] as u64) << 3)
                    | ((right[0] as u64) << 6)
                    | ((right[1] as u64) << 9);
                bits |= row << (12 * r);
            }
            let offset = at + 8 * ch;
            out[offset] = e[2 * ch];
            out[offset + 1] = e[2 * ch + 1];
            out[offset + 2..offset + 8].copy_from_slice(&bits.to_le_bytes()[..6]);
        }
    }

    let unknown_blocks = lost.iter().filter(|&&v| v).count();
    if unknown_blocks > (blocks / 1024).max(2) {
        return None;
    }
    Some(Bc5Output {
        blocks: out,
        provisional_entries: provisional.into_iter().collect(),
        unknown_entries: unknown.into_iter().collect(),
        unknown_blocks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn embedded_history_and_pair_table_load() {
        assert_eq!(BC5_PAIR.len(), 64);
        assert_eq!(history().confirmed.iter().filter(|v| v.is_some()).count(), 5);
        assert!(history().provisional.iter().filter(|v| v.is_some()).count() > 100);
    }
}
