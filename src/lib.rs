// Parallel-array index loops are idiomatic in codec code; skip the lint.
#![allow(clippy::needless_range_loop)]

//! FLAC support: native container + lossless codec.
//!
//! - The **container** parses `fLaC` magic + metadata blocks and emits one
//!   packet per FLAC frame, scanning CRC-verified sync codes to find frame
//!   boundaries. A muxer is also provided for round-trip encoding.
//! - The **codec** (id `flac`) ships both a spec-complete decoder (all
//!   subframe types, all residual partition methods, 8/12/16/20/24/32 bps,
//!   up to 8 channels, stereo decorrelation) and a working pure-Rust
//!   encoder that produces bit-exact round-trippable output. The encoder
//!   tries CONSTANT, FIXED 0..=4, LPC 1..=8 and VERBATIM per subframe,
//!   picks the smallest, evaluates all four stereo channel assignments
//!   for 2-channel input, and writes a full STREAMINFO (including MD5
//!   signature and frame-size/sample-count bookkeeping) at flush time.

pub mod bits_ext;
pub mod codec;
pub mod container;
pub mod crc;
pub mod decoder;
pub mod encoder;
pub mod frame;
pub mod md5;
pub mod metadata;
pub mod subframe;

use oxideav_core::CodecRegistry;
use oxideav_core::ContainerRegistry;

pub const CODEC_ID_STR: &str = "flac";

pub fn register_codecs(reg: &mut CodecRegistry) {
    codec::register(reg);
}

pub fn register_containers(reg: &mut ContainerRegistry) {
    container::register(reg);
}
