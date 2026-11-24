//! MIT-licensed MDU rANS decoder and test encoder.
//! Decoder and model tables derive from Microsoft's silicon-rans via bcpack-rs.
//! See THIRD_PARTY.md for notices.
pub mod rans;
mod tables;
#[cfg(test)]
pub mod encode;
