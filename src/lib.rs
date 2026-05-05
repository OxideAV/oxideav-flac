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
use oxideav_core::RuntimeContext;

pub const CODEC_ID_STR: &str = "flac";

pub fn register_codecs(reg: &mut CodecRegistry) {
    codec::register(reg);
}

pub fn register_containers(reg: &mut ContainerRegistry) {
    container::register(reg);
}

/// Unified entry point: install every codec and container provided by
/// `oxideav-flac` into a [`RuntimeContext`].
///
/// Also auto-registered into [`oxideav_core::REGISTRARS`] via the
/// [`oxideav_core::register!`] macro below so consumers calling
/// [`oxideav_core::RuntimeContext::with_all_features`] pick FLAC up
/// without any explicit umbrella plumbing.
pub fn register(ctx: &mut RuntimeContext) {
    register_codecs(&mut ctx.codecs);
    register_containers(&mut ctx.containers);
}

oxideav_core::register!("flac", register);

#[cfg(test)]
mod register_tests {
    use super::*;

    #[test]
    fn register_via_runtime_context_installs_factories() {
        let mut ctx = RuntimeContext::new();
        register(&mut ctx);
        assert!(
            ctx.codecs.decoder_ids().next().is_some(),
            "register(ctx) should install codec decoder factories"
        );
        assert!(
            ctx.containers.demuxer_names().next().is_some(),
            "register(ctx) should install container demuxer factories"
        );
    }
}
