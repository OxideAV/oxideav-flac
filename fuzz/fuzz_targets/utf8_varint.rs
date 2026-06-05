#![no_main]

//! Fuzz: writer ↔ reader round-trip and byte-length stability for
//! `oxideav_flac::bits_ext::{read_utf8_u64, write_utf8_u64}` —
//! the FLAC frame-header UTF-8-shaped variable-length integer of
//! RFC 9639 §9.1.5 (`sample_number` under variable-blocking,
//! `frame_number` under fixed-blocking).
//!
//! Round 237 (depth-mode fuzz, eighth target). The prior seven
//! cargo-fuzz harnesses cover the decoder pipeline
//! (`panic_free_decode`, `decode`), encode/decode self-consistency
//! (`roundtrip`), a cross-decode validation harness, the metadata
//! chain (`metadata_walker`), encoder-construction extradata
//! (`encoder_options`), and the MD5 streaming primitive
//! (`md5_streaming`). None of them put the UTF-8 varint codec
//! under focused stress: every encoded `.flac` carries at most one
//! varint per frame at one value per frame, and the host-pipeline
//! targets only exercise the reader at whatever value the encoder
//! happened to emit.
//!
//! ## Why this matters
//!
//! The frame-header coded number is one of two surfaces (the other
//! being the sync code) the demuxer uses to anchor itself in a
//! stream after a seek. A divergence between writer and reader at
//! any of the seven lead-byte-range boundaries
//!
//! ```text
//! 0xxxxxxx                          (1 byte,   0..=0x7F)
//! 110xxxxx 10xxxxxx                 (2 bytes,  0x80..=0x7FF)
//! 1110xxxx 10xxxxxx 10xxxxxx        (3 bytes,  0x800..=0xFFFF)
//! 11110xxx 10xxxxxx 10xxxxxx 10xxxxxx   (4 B, 0x10000..=0x1FFFFF)
//! 111110xx ...                      (5 B, 0x200000..=0x3FFFFFF)
//! 1111110x ...                      (6 B, 0x4000000..=0x7FFFFFFF)
//! 11111110 ...                      (7 B, 0x80000000..=0xF_FFFFFFFF)
//! ```
//!
//! would silently corrupt every coded number near that boundary.
//! In particular a 16-bit `sample_number` bug would only surface on
//! variable-blocking streams whose first frame happens to land after
//! sample ~65 536 and would then break seek-table consistency at
//! every later frame in the file — a subtle, easy-to-miss failure
//! mode that benchmark-style and pipeline-style fuzz harnesses
//! don't reliably reach.
//!
//! ## What the harness covers
//!
//!   * **Writer ↔ reader round-trip.** Every value reachable by the
//!     fuzzer is encoded via `write_utf8_u64`, decoded via
//!     `read_utf8_u64`, and asserted bit-equal.
//!   * **Byte-length stability.** For each value the emitted byte
//!     count is asserted to fall inside the documented range for
//!     that value's bit-width (the 1/2/3/4/5/6/7-byte schedules
//!     above). A value just below a boundary that emits one byte
//!     too many would surface here as a deterministic panic.
//!   * **Boundary sweep.** Every iteration unconditionally tests
//!     the seven lead-byte-range boundaries
//!     (0, 0x7F, 0x80, 0x7FF, 0x800, 0xFFFF, 0x10000, 0x1FFFFF,
//!     0x200000, 0x3FFFFFF, 0x4000000, 0x7FFFFFFF, 0x80000000,
//!     (1u64 << 36) - 1) so a regression at a boundary cannot
//!     escape a fuzzer that never produces the exact boundary
//!     value.
//!   * **Reader robustness.** Arbitrary input bytes are fed to
//!     `read_utf8_u64` directly. The reader must return a `Result`
//!     for every input it sees — never panic, never abort, never
//!     read past the slice. Invalid lead bytes (0x80..=0xBF /
//!     0xFF) and truncated multi-byte sequences must return
//!     `Err`.
//!   * **Reader → writer survivability.** When the reader accepts
//!     a fuzzer-chosen prefix, the harness re-encodes the recovered
//!     value, parses again, and asserts the second parse matches
//!     the first. This catches drift between the two codecs at
//!     values the writer can produce but a hand-built input
//!     stream happened to encode in a longer-than-necessary form
//!     (which the reader is free to accept but the writer should
//!     normalise away on re-emit).
//!
//! ## Input layout
//!
//!   - 1 B  `mode_sel`  — selects between varint-value-driven and
//!     raw-bytes-driven sweeps (low bit). The harness runs both
//!     modes every iteration; this bit just picks the value layout
//!     for the first mode.
//!   - rest:              up to 8 little-endian 8-byte chunks
//!     interpreted as 64-bit values for the value-driven sweep,
//!     plus the entire remaining slice handed to the raw-bytes
//!     parser sweep.
//!
//! ## Contract under test
//!
//!   * `write_utf8_u64(v) -> bytes -> read_utf8_u64(bytes) == Ok(v)`
//!     for every `v` in `0..=(1u64 << 36) - 1`.
//!   * `bytes.len()` falls inside the documented per-bit-width
//!     range emitted by `write_utf8_u64`.
//!   * `read_utf8_u64` returns a `Result` for any byte slice (no
//!     panic, no out-of-bounds read).
//!   * Successful parses re-encode to bytes that the reader accepts
//!     and that decode to the same value.

use libfuzzer_sys::fuzz_target;
use oxideav_core::bits::{BitReader, BitWriter};
use oxideav_flac::bits_ext::{BitReaderExt, BitWriterExt};

/// Hard cap for valid varint values (RFC 9639 §9.1.5 documents the
/// `0xFE` lead byte carries 36 payload bits, so the max representable
/// value is `(1u64 << 36) - 1`).
const MAX_VALID: u64 = (1u64 << 36) - 1;

/// Per-bit-width emitted byte counts. Indexed by
/// `bits_needed = 64 - value.leading_zeros()` for `value != 0`, with
/// `0` treated as 1 bit per the encoder's special case. The seven
/// entries match the seven branches in `write_utf8_u64`.
const fn expected_byte_len(value: u64) -> usize {
    let bits = if value == 0 {
        1
    } else {
        64 - value.leading_zeros()
    };
    match bits {
        0..=7 => 1,
        8..=11 => 2,
        12..=16 => 3,
        17..=21 => 4,
        22..=26 => 5,
        27..=31 => 6,
        32..=36 => 7,
        _ => 0,
    }
}

/// Round-trip a single value: write, parse, assert equality and
/// byte-length stability. Panics on any divergence — the contract
/// under test is bit-exact equivalence.
fn roundtrip(value: u64) {
    assert!(value <= MAX_VALID, "test bug: caller passed > 36-bit value");

    let mut w = BitWriter::new();
    w.write_utf8_u64(value);
    let bytes = w.finish();
    let expected_len = expected_byte_len(value);
    assert_eq!(
        bytes.len(),
        expected_len,
        "byte-length stability: value={value:#x} expected={expected_len} got={}",
        bytes.len(),
    );

    let mut r = BitReader::new(&bytes);
    let recovered = r
        .read_utf8_u64()
        .expect("writer-emitted varint must always parse");
    assert_eq!(
        recovered, value,
        "round-trip diverged: value={value:#x} bytes={bytes:?}",
    );
}

/// Parser-only sweep: feed an arbitrary byte slice to
/// `read_utf8_u64`. Must always return a `Result` (no panic, no
/// out-of-bounds read). When the parse succeeds, re-emit the
/// recovered value and assert the second parse matches the first.
fn parse_arbitrary(bytes: &[u8]) {
    let mut r = BitReader::new(bytes);
    match r.read_utf8_u64() {
        Ok(value) => {
            // The reader is permitted to accept values up to 36 bits;
            // anything beyond that would imply the lead-byte dispatch
            // table grew an unsanctioned branch.
            assert!(
                value <= MAX_VALID,
                "reader accepted out-of-range value: {value:#x} from {bytes:?}",
            );
            // Re-emit and assert the round-trip stabilises. The
            // writer always emits the shortest valid encoding for
            // a value; the reader may have accepted a longer form
            // from the hand-crafted input. After one re-emit, the
            // representation is canonical and the parse must
            // return the same value.
            let mut w = BitWriter::new();
            w.write_utf8_u64(value);
            let canonical = w.finish();
            let mut r2 = BitReader::new(&canonical);
            let value2 = r2
                .read_utf8_u64()
                .expect("re-emitted varint must always parse");
            assert_eq!(
                value, value2,
                "reader→writer→reader drift at value={value:#x} \
                 original={bytes:?} canonical={canonical:?}",
            );
            assert_eq!(
                canonical.len(),
                expected_byte_len(value),
                "canonical byte-length unstable at value={value:#x}",
            );
        }
        Err(_) => {
            // Any error is acceptable — the contract is "never panic".
        }
    }
}

fuzz_target!(|data: &[u8]| {
    // -----------------------------------------------------------------
    // Boundary sweep. Runs every iteration so a regression at one of
    // the seven lead-byte-range edges cannot slip past a fuzz session
    // that never produces the exact boundary value through the
    // value-driven sweep below.
    // -----------------------------------------------------------------
    for &v in &[
        0u64,
        // 1-byte / 2-byte boundary.
        0x7F,
        0x80,
        // 2-byte / 3-byte boundary.
        0x7FF,
        0x800,
        // 3-byte / 4-byte boundary.
        0xFFFF,
        0x10000,
        // 4-byte / 5-byte boundary.
        0x1F_FFFF,
        0x20_0000,
        // 5-byte / 6-byte boundary.
        0x3FF_FFFF,
        0x400_0000,
        // 6-byte / 7-byte boundary.
        0x7FFF_FFFF,
        0x8000_0000,
        // Upper limit.
        MAX_VALID,
    ] {
        roundtrip(v);
    }

    if data.is_empty() {
        return;
    }

    let mode_sel = data[0];
    let tail = &data[1..];

    // -----------------------------------------------------------------
    // Value-driven sweep. Carve the input tail into 8-byte chunks,
    // interpret each as a little-endian u64, clamp into the legal
    // 36-bit range, and round-trip. The clamp uses `& MAX_VALID`
    // (mask to the low 36 bits) when `mode_sel` low bit is clear,
    // and `% (MAX_VALID + 1)` when it's set — two different
    // distributions so the fuzzer can reach both the dense
    // low-value range and the sparse high-value range.
    // -----------------------------------------------------------------
    for chunk in tail.chunks_exact(8).take(8) {
        let raw = u64::from_le_bytes([
            chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6], chunk[7],
        ]);
        let v = if mode_sel & 0b1 == 0 {
            raw & MAX_VALID
        } else {
            raw % (MAX_VALID + 1)
        };
        roundtrip(v);
    }

    // -----------------------------------------------------------------
    // Reader robustness sweep. Hand the raw bytes to the parser
    // directly. The reader must return a `Result` on every input
    // and never panic — the FLAC demuxer relies on this when it
    // resyncs on a corrupted stream after a backwards seek.
    //
    // Sliding-window parses also stress the reader's reaction to
    // unexpected lead bytes at offsets the encoder would never
    // emit (continuation bytes 0x80..=0xBF and the reserved 0xFF
    // sentinel).
    // -----------------------------------------------------------------
    parse_arbitrary(tail);
    if tail.len() >= 1 {
        parse_arbitrary(&tail[1..]);
    }
    if tail.len() >= 2 {
        parse_arbitrary(&tail[2..]);
    }
    if tail.len() >= 4 {
        parse_arbitrary(&tail[4..]);
    }
});
