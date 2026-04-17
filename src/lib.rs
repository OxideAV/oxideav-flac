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
//!   uses FIXED predictor order 2 + partitioned Rice; LPC is not yet used
//!   on the encode side, so compressed size is larger than `flac --best`.

pub mod bitreader;
pub mod bitwriter;
pub mod codec;
pub mod container;
pub mod crc;
pub mod decoder;
pub mod encoder;
pub mod frame;
pub mod metadata;
pub mod subframe;

use oxideav_codec::CodecRegistry;
use oxideav_container::ContainerRegistry;

pub const CODEC_ID_STR: &str = "flac";

pub fn register_codecs(reg: &mut CodecRegistry) {
    codec::register(reg);
}

pub fn register_containers(reg: &mut ContainerRegistry) {
    container::register(reg);
}
