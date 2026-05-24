#![no_main]

//! Decode arbitrary fuzz-supplied bytes through the FLAC public surface.
//!
//! Mirrors the `ico` / `qoi` / `bmp` / `tta` `decode.rs` shape: a single
//! `fuzz_target!` that funnels the input through every public entry
//! point with attacker-controlled framing. The contract under test is
//! purely that each call *returns* — a malformed stream yields an
//! `Err(...)`, a well-formed one yields `Ok(...)`, and neither path may
//! panic, integer-overflow (in a debug build), index out of bounds, or
//! pre-allocate an attacker-claimed sample / picture / seekpoint /
//! cuesheet-track buffer that exceeds what the input could possibly
//! back. Return values are intentionally discarded.
//!
//! Six entry points are exercised on every input because they are
//! independent public surfaces with distinct offset / allocation /
//! bounds maths (and historically distinct panic budgets):
//!
//! 1. [`oxideav_flac::metadata::BlockHeader::parse`] — METADATA_BLOCK
//!    4-byte header. Trivial but it's the entry to every block-walking
//!    loop below; a crash here cascades.
//! 2. [`oxideav_flac::metadata::StreamInfo::parse`] — STREAMINFO 34-byte
//!    payload. Drives the packed `sample_rate / channels / bps /
//!    total_samples` decode (20+3+5+36 bits across `bytes[10..18]`).
//! 3. [`oxideav_flac::metadata::parse_seektable`] — SEEKTABLE block:
//!    18 bytes per `SeekPoint`. Bounds-arithmetic target (the divisor
//!    must not underflow on a truncated tail).
//! 4. [`oxideav_flac::metadata::parse_cuesheet`] — CUESHEET block: 396
//!    bytes of fixed header + variable per-track + per-index lists with
//!    strict reserved-bit checks. Historically the densest non-frame
//!    target.
//! 5. [`oxideav_flac::frame::parse_frame_header`] — frame sync header
//!    (sync code + variable block-size code + variable sample-rate
//!    code + UTF-8 frame number + CRC-8). Drives the bit-level
//!    extractor that the subframe decoder builds on top of.
//! 6. The **full demux + decode pipeline** — `register_containers` →
//!    `open_demuxer("flac", …)` → up to 8 `next_packet` /
//!    `send_packet` / `receive_frame` iterations. Exercises STREAMINFO
//!    consumption, frame-boundary scan, CRC-16 footer, subframe
//!    routing, LPC order / precision / shift bounds, Rice partition
//!    order, and stereo decorrelation — the same path that triggered
//!    the historical `bps == 0 / bps > 32` panic now caught at the
//!    subframe-header boundary.

use libfuzzer_sys::fuzz_target;
use oxideav_core::{ContainerRegistry, NullCodecResolver};
use oxideav_flac::frame::parse_frame_header;
use oxideav_flac::metadata::{parse_cuesheet, parse_seektable, BlockHeader, StreamInfo};

fuzz_target!(|data: &[u8]| {
    // -----------------------------------------------------------------
    // Direct public-surface parsers. Each is a pure byte→`Result`
    // function so the fuzzer can drive them with zero scaffolding.
    // -----------------------------------------------------------------
    let _ = BlockHeader::parse(data);
    let _ = StreamInfo::parse(data);
    let _ = parse_seektable(data); // infallible (returns Vec); the contract is "must not panic".
    let _ = parse_cuesheet(data);
    let _ = parse_frame_header(data);

    // -----------------------------------------------------------------
    // Full demux + decode pipeline. The container demuxer expects an
    // `fLaC` magic (or ID3v2 prefix) — most random inputs fall out at
    // the magic check, which is fine: the surface area under test is
    // the recovery path, not the happy path. When the input does pass
    // the magic, drain a bounded number of packets to keep each fuzz
    // iteration cheap.
    // -----------------------------------------------------------------
    let mut reg = ContainerRegistry::new();
    oxideav_flac::register_containers(&mut reg);
    let resolver = NullCodecResolver;
    let cursor: Box<dyn oxideav_core::ReadSeek> = Box::new(std::io::Cursor::new(data.to_vec()));
    if let Ok(mut demux) = reg.open_demuxer("flac", cursor, &resolver) {
        let streams = demux.streams().to_vec();
        let mut dec = streams
            .first()
            .and_then(|s| oxideav_flac::decoder::make_decoder(&s.params).ok());
        for _ in 0..8 {
            let pkt = match demux.next_packet() {
                Ok(p) => p,
                Err(_) => break,
            };
            if let Some(d) = dec.as_mut() {
                let _ = d.send_packet(&pkt);
                for _ in 0..4 {
                    if d.receive_frame().is_err() {
                        break;
                    }
                }
            }
        }
    }
});
