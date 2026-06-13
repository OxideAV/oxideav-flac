#![no_main]

//! Fuzz: structure-aware stress on `oxideav_flac::subframe::decode_subframe`
//! — the FLAC subframe decoder (RFC 9639 §9.2): CONSTANT / VERBATIM /
//! FIXED-predictor (orders 0..=4) / LPC (orders 1..=32), each wrapping the
//! §9.2.7 residual coder (Rice partitions, partition orders 0..=15, both
//! 4-bit and 5-bit parameter methods, and the escape partition).
//!
//! Round 292 (depth-mode fuzz, eleventh target). Every prior harness
//! reaches the subframe decoder only obliquely:
//!
//!   * `panic_free_decode` / `decode` funnel arbitrary bytes through the
//!     demuxer, so the subframe path is gated behind a valid `fLaC`
//!     magic, a valid STREAMINFO, and a CRC-8-passing frame header. A
//!     random input almost never threads all three, so the residual /
//!     LPC / FIXED inner loops are reached on a vanishingly small
//!     fraction of iterations.
//!   * `roundtrip` / `flac_oracle_decode` only ever feed the decoder the
//!     *narrow* subframe shapes our own encoder emits (it never escapes a
//!     residual partition, never uses LPC orders above 8, never uses a
//!     wasted-bit count above the handful it bothers to detect, and
//!     never emits a deliberately-truncated bitstream). The decoder's
//!     recovery arithmetic for adversarial input is untested by them.
//!   * `frame_header` stops at the header byte; it never crosses into the
//!     subframe body.
//!
//! ## What this target drives directly
//!
//! `decode_subframe(&mut BitReader, block_size, bps)` is a public entry
//! point (`pub fn`, `pub mod subframe`). The harness constructs a
//! `BitReader` over fuzzer-supplied bytes and calls it with
//! fuzzer-chosen `(block_size, bps)` pulled from the genuinely-reachable
//! production envelope:
//!
//!   * `block_size` ∈ 1..=65535 — the full range a §9.1 frame header can
//!     encode (the 16-bit immediate block-size code caps here). We do
//!     NOT fuzz beyond this: the production decoder only ever calls
//!     `decode_subframe` with a header-validated block size, so a larger
//!     value would probe an unreachable allocation path and report a
//!     phantom OOM that no real stream can trigger.
//!   * `bps` ∈ 1..=33 — 1..=32 are the spec per-sample depths; 33 is the
//!     `bps + 1` side-channel depth that left/right/mid-side
//!     decorrelation asks for when STREAMINFO declares 32-bit samples.
//!     33 must be *rejected* (the §9.2.8 wasted-bits + `read_i32`'s
//!     32-bit ceiling can't represent it) rather than panicking — this
//!     is the exact shape of the 2026-05-10 regression. We also include
//!     0 and a few out-of-range values to confirm the guard.
//!
//! The contract under test is pure panic-freedom: every call MUST return
//! a `Result` for any combination of `(block_size, bps, bitstream)`.
//! Malformed input → `Err`, never a panic, never a debug-build integer
//! overflow, never an out-of-bounds index, never an unbounded
//! pre-allocation. The decoded `Vec<i32>` is otherwise discarded.
//!
//! ## Why each lever matters
//!
//!   * **type_code** (6 bits) selects CONSTANT / VERBATIM / one of the
//!     five FIXED orders / one of the 32 LPC orders / a reserved slot.
//!     The reserved slots must `Err`. FIXED order N and LPC order M each
//!     read N (resp. M) warmup samples before the residual, so the
//!     residual partitioning maths (`partition_size <= predictor_order`)
//!     is exercised across every predictor order the fuzzer can name.
//!   * **wasted bits** are unary-coded; a long zero run is the classic
//!     way to (a) drain the reader (must `Err`, not loop forever — the
//!     core `read_unary` is bounded) and (b) push `wasted >= bps` (must
//!     `Err`). The post-decode `(*s as i64) << wasted` shift is a
//!     debug-overflow candidate this target reaches with `wasted` up to
//!     `bps - 1`.
//!   * **qlp_shift** is a signed 5-bit field; the decoder rejects
//!     negative shifts. The `pred >> qlp_shift` in `apply_lpc` would be a
//!     shift-by-≥64 UB candidate if `qlp_shift` weren't range-checked —
//!     the LPC path here confirms the check holds for every reachable
//!     shift.
//!   * **residual escape** (parameter == 15 for 4-bit / 31 for 5-bit)
//!     switches to a raw 5-bit-bps partition; a `raw_bps` of 0 reads
//!     zero-width samples and a large `raw_bps` drains the reader — both
//!     must terminate in `Err`.
//!
//! ## Coverage-priming sweep
//!
//! Independent of the fuzzer's converged corpus, every iteration also
//! sweeps a small fixed matrix of `(block_size, bps)` pairs against the
//! raw input bytes at two reader offsets, so a regression on any
//! predictor order or the bps guard is reached on the very first input
//! rather than waiting for the corpus to rediscover a valid header.

use libfuzzer_sys::fuzz_target;
use oxideav_core::bits::BitReader;
use oxideav_flac::subframe::decode_subframe;

/// Largest block size a §9.1 frame header can encode (16-bit immediate).
const MAX_BLOCK_SIZE: u32 = 65535;

fuzz_target!(|data: &[u8]| {
    // Need at least a couple of bytes to derive the (block_size, bps)
    // levers; the remainder is the subframe bitstream.
    if data.len() < 4 {
        // Still exercise the guard rails on tiny inputs.
        for &bps in &[0u32, 1, 16, 32, 33] {
            let mut br = BitReader::new(data);
            let _ = decode_subframe(&mut br, 1, bps);
        }
        return;
    }

    // -----------------------------------------------------------------
    // Primary fuzzer-controlled call. Derive (block_size, bps) from the
    // first three bytes; the rest is the subframe body.
    // -----------------------------------------------------------------
    let raw_bs = u16::from_le_bytes([data[0], data[1]]) as u32;
    // Map into 1..=MAX_BLOCK_SIZE (production-reachable). 0 is not a
    // legal block size, so bias it to 1.
    let block_size = raw_bs.clamp(1, MAX_BLOCK_SIZE);

    // bps: cover 0 (rejected), 1..=32 (valid), 33 (rejected side-channel
    // depth), plus a couple beyond 33 to keep the guard honest.
    let bps = (data[2] % 36) as u32; // 0..=35

    let body = &data[3..];
    {
        let mut br = BitReader::new(body);
        let _ = decode_subframe(&mut br, block_size, bps);
    }

    // A second call with the block_size and bps swapped through a
    // different reduction, so the fuzzer can independently steer the
    // predictor-order vs. partition-order interaction (both read from
    // the same body but at a different (block_size, bps) framing).
    {
        // Smaller block sizes reach the "first partition smaller than
        // predictor order" and "block_size not divisible by partition
        // count" branches far more often.
        let small_bs = (raw_bs % 4096).max(1);
        let bps2 = ((data[3] % 32) + 1) as u32; // 1..=32 — always in-range
        let mut br = BitReader::new(body);
        let _ = decode_subframe(&mut br, small_bs, bps2);
    }

    // -----------------------------------------------------------------
    // Coverage-priming sweep: a fixed matrix run against the raw input
    // at two offsets every iteration. Guarantees the bps guard, every
    // FIXED order, and the LPC dispatch are reached even before the
    // corpus converges on a body the inner loops accept.
    // -----------------------------------------------------------------
    for off in [0usize, 1] {
        if off >= data.len() {
            break;
        }
        let slice = &data[off..];
        for &bs in &[1u32, 2, 5, 16, 192, 576, 4096, 65535] {
            for &bp in &[0u32, 1, 4, 8, 16, 24, 32, 33] {
                let mut br = BitReader::new(slice);
                let _ = decode_subframe(&mut br, bs, bp);
            }
        }
    }
});
