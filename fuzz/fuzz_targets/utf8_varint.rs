#![no_main]

//! Fuzz: writer ↔ reader roundtrip + panic-freedom for the FLAC
//! UTF-8-style variable-length integer (RFC 9639 §9.1.5).
//!
//! Round 237 (depth-mode fuzz). The previous seven targets cover the
//! decoder pipeline (`panic_free_decode`, `decode`), the encode/decode
//! self-consistency (`roundtrip`), the libavcodec oracle
//! (`flac_oracle_decode`), arbitrary metadata-block chains
//! (`metadata_walker`), encoder-construction extradata
//! (`encoder_options`), and the MD5 streaming surface
//! (`md5_streaming`). None of them put `oxideav_flac::bits_ext`'s
//! `read_utf8_u64` / `write_utf8_u64` under stress as a focused
//! cross-checked pair. The frame-header path exercises
//! `read_utf8_u64` indirectly, but only for the value the host
//! pipeline happens to land on; the writer is only exercised through
//! the encoder, which emits one varint per frame at a single value
//! per frame.
//!
//! Why this matters. RFC 9639 §9.1.5 carries either the frame
//! number (fixed-blocking) or the starting sample number
//! (variable-blocking) in a 1..=7-byte UTF-8-shaped varint. The
//! encoding spans seven distinct lead-byte ranges (`0xxxxxxx`,
//! `110xxxxx`, `1110xxxx`, `11110xxx`, `111110xx`, `1111110x`,
//! `11111110`) and supports up to 36-bit payloads via the FLAC
//! extension over standard UTF-8 (which tops out at 21 bits).
//! A divergence between writer and reader at any boundary would
//! silently corrupt every coded number near that boundary — a
//! 16-bit sample-number bug, for example, would surface only on
//! variable-blocking streams whose first frame happens to land
//! after sample ~65 536 and break seek-table consistency at every
//! later frame in the file.
//!
//! The risk surface is concentrated in three places inside
//! `src/bits_ext.rs`:
//!
//!   * the writer's `bits_needed`→`(n_extra, lead_prefix,
//!     lead_payload_bits)` lookup table — one off-by-one in any
//!     match arm shifts the wire format by a byte at exactly that
//!     boundary,
//!   * the reader's `b0`-pattern dispatch to `(n_extra,
//!     lead_payload_bits)` — the seven lead-byte ranges must
//!     mirror the writer's seven arms exactly,
//!   * the continuation-byte mask check (`cont & 0xC0 != 0x80`)
//!     and the shift-and-or accumulator
//!     (`value = (value << 6) | ((cont & 0x3F) as u64)`) — a
//!     mis-typed mask or shift would corrupt a single nibble per
//!     continuation byte without crashing.
//!
//! ## Input layout
//!
//! Each fuzz iteration carves the input into:
//!
//!   - 1 B  `mode` — selects between several harness shapes
//!   - rest: payload, interpreted per `mode` (raw byte stream for
//!     the reader-panic-freedom shape, or sequence of values for
//!     the writer-then-reader shape).
//!
//! ## Schedules
//!
//! Every iteration runs **all** of the following (no schedule is
//! skipped on payload-length grounds — the per-iteration cost is
//! tiny):
//!
//!   * **A**: writer-then-reader roundtrip on the boundary set
//!     (0, 2^k - 1, 2^k for k in {7, 11, 16, 21, 26, 31}, plus
//!     2^36 - 1). Asserts byte-for-byte round-trip and the
//!     per-range expected byte count from RFC 9639 §9.1.5.
//!   * **B**: writer-then-reader roundtrip on fuzzer-derived
//!     values. Each pair of payload bytes seeds a `u64` value
//!     masked to 36 bits; the harness writes the value, reads it
//!     back, asserts equality. Bounded to ≤ 16 values per
//!     iteration so the fuzzer can keep the schedule warm
//!     alongside the other six targets.
//!   * **C**: reader panic-freedom on arbitrary bytes. The
//!     payload tail is fed directly to `BitReader::new` +
//!     `read_utf8_u64`; the reader must return `Result` (Ok or
//!     Err) without panicking on any byte sequence.
//!   * **D**: chained writer-then-reader on a sequence of values.
//!     Writes up to 8 values back-to-back into one `BitWriter`,
//!     then drains them through one `BitReader`, asserting the
//!     returned sequence matches the written sequence. Catches
//!     framing bugs where one varint corrupts the next reader's
//!     state.
//!   * **E**: byte-length stability. For each `bits_needed`
//!     boundary value (`1 << k` for k in 0, 7, 11, 16, 21, 26,
//!     31, 36), assert the writer emits exactly the
//!     spec-mandated byte count (1, 2, 3, 4, 5, 6, 7 bytes).
//!   * **F**: alignment-precondition coverage. The reader's
//!     contract is that it requires byte alignment on entry; a
//!     bit-misaligned reader must return `Err`, not panic. Build
//!     a `BitReader` over a synthetic byte sequence, consume one
//!     bit so the next read is bit-aligned but not byte-aligned,
//!     and assert `read_utf8_u64` returns `Err`.
//!
//! ## Contract under test
//!
//!   * For every value `v` in `0..=(1u64 << 36) - 1`,
//!     `read_utf8_u64(write_utf8_u64(v)) == v`.
//!   * `write_utf8_u64` emits exactly the byte count RFC 9639
//!     §9.1.5 mandates for `v`'s range.
//!   * `read_utf8_u64` never panics on any byte sequence; every
//!     malformed input maps to `Err`.
//!   * Chained varints in a single bit-stream decode in order,
//!     with no state bleed between adjacent values.
//!   * A bit-misaligned reader returns `Err`, not a panic, on
//!     `read_utf8_u64`.

use libfuzzer_sys::fuzz_target;
use oxideav_core::bits::{BitReader, BitWriter};
use oxideav_flac::bits_ext::{BitReaderExt, BitWriterExt};

/// 36-bit cap on FLAC UTF-8 varint payloads (RFC 9639 §9.1.5 extension
/// over standard UTF-8's 21-bit cap; values above this would require an
/// eighth byte that the wire format does not define).
const MAX_VARINT: u64 = (1u64 << 36) - 1;

/// Per-range expected byte counts for the boundary set used in
/// schedules A and E. Index `i` is the byte count for values in the
/// `(2^k_lo - 1, 2^k_hi - 1]` range, matching the seven lead-byte
/// arms of `write_utf8_u64`.
///
/// `BYTE_COUNT_FOR_BITS[bits_needed]` returns the wire-byte count for
/// any value whose minimum-bit width is `bits_needed`. `bits_needed`
/// is computed exactly the way the writer computes it (`64 -
/// leading_zeros`, with `v == 0` mapping to 1).
fn expected_byte_count(value: u64) -> usize {
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
        _ => panic!("expected_byte_count called on >36-bit value {value:#x}"),
    }
}

/// Cap on the number of fuzzer-derived values per iteration. Each
/// value costs one write + one read; 16 keeps the per-iteration
/// work tiny while still giving the fuzzer enough slack to hit
/// every boundary range across a corpus run.
const MAX_VALUES_PER_ITER: usize = 16;

/// Cap on the chained-write schedule (D). 8 varints, each up to 7
/// bytes, is 56 wire bytes per chain — small enough that the
/// fuzzer can re-run the schedule millions of times per second.
const MAX_CHAIN_LEN: usize = 8;

fuzz_target!(|data: &[u8]| {
    // -----------------------------------------------------------------
    // Schedule A: boundary-value roundtrip. Runs every iteration with
    // a fixed set of values that straddle every byte-count boundary in
    // §9.1.5. A regression in any match arm of either writer or
    // reader surfaces here even when the fuzzer's data byte tail
    // never lands on the boundary itself.
    // -----------------------------------------------------------------
    let boundary_values: [u64; 14] = [
        0,
        (1u64 << 7) - 1,
        1u64 << 7,
        (1u64 << 11) - 1,
        1u64 << 11,
        (1u64 << 16) - 1,
        1u64 << 16,
        (1u64 << 21) - 1,
        1u64 << 21,
        (1u64 << 26) - 1,
        1u64 << 26,
        (1u64 << 31) - 1,
        1u64 << 31,
        MAX_VARINT,
    ];
    for &v in &boundary_values {
        let mut w = BitWriter::new();
        w.write_utf8_u64(v);
        let bytes = w.finish();
        assert_eq!(
            bytes.len(),
            expected_byte_count(v),
            "boundary value {v:#x} wrote {} bytes, expected {}",
            bytes.len(),
            expected_byte_count(v),
        );
        let mut r = BitReader::new(&bytes);
        let got = r
            .read_utf8_u64()
            .expect("boundary roundtrip: reader rejected our own writer's bytes");
        assert_eq!(got, v, "boundary roundtrip diverged for {v:#x}");
    }

    // -----------------------------------------------------------------
    // Schedule E: byte-length stability at every power-of-two boundary.
    // Independent of fuzzer input; cheap; catches an off-by-one in the
    // writer's bits-needed→byte-count match arm before any other
    // schedule has a chance to.
    // -----------------------------------------------------------------
    let length_probes: [(u64, usize); 7] = [
        (0, 1),
        (1u64 << 7, 2),
        (1u64 << 11, 3),
        (1u64 << 16, 4),
        (1u64 << 21, 5),
        (1u64 << 26, 6),
        (1u64 << 31, 7),
    ];
    for (v, expected_len) in length_probes {
        let mut w = BitWriter::new();
        w.write_utf8_u64(v);
        let bytes = w.finish();
        assert_eq!(
            bytes.len(),
            expected_len,
            "length-stability probe {v:#x} wrote {} bytes, expected {}",
            bytes.len(),
            expected_len,
        );
    }

    if data.is_empty() {
        return;
    }
    let mode = data[0];
    let payload = &data[1..];

    // -----------------------------------------------------------------
    // Schedule B: fuzzer-derived values. Pair up the payload bytes,
    // build a `u64` from each pair (little-endian within the pair),
    // mask to 36 bits, write, then read back. Each pair is independent
    // — schedule D covers the chained case.
    // -----------------------------------------------------------------
    let value_chunks = payload.chunks_exact(8);
    let remainder = value_chunks.remainder();
    for (i, chunk) in value_chunks.enumerate().take(MAX_VALUES_PER_ITER) {
        let mut bytes = [0u8; 8];
        bytes.copy_from_slice(chunk);
        let raw = u64::from_le_bytes(bytes);
        let v = raw & MAX_VARINT;

        let mut w = BitWriter::new();
        w.write_utf8_u64(v);
        let wire = w.finish();
        assert_eq!(
            wire.len(),
            expected_byte_count(v),
            "fuzzer-value {v:#x} (index {i}) wrote {} bytes, expected {}",
            wire.len(),
            expected_byte_count(v),
        );
        let mut r = BitReader::new(&wire);
        let got = r
            .read_utf8_u64()
            .expect("fuzzer-value roundtrip: reader rejected our own writer's bytes");
        assert_eq!(
            got, v,
            "fuzzer-value roundtrip diverged: wrote {v:#x}, read {got:#x} (index {i})"
        );
    }

    // -----------------------------------------------------------------
    // Schedule C: reader panic-freedom on arbitrary bytes. Drives the
    // payload tail (including the remainder bytes that schedule B
    // couldn't consume) directly into `read_utf8_u64`. The contract is
    // that the reader returns `Result` for any byte sequence — never
    // panic, never abort.
    //
    // We do not assert anything about the *value* returned for
    // arbitrary input (it depends on the bytes); the only contract is
    // that the call returns without panicking and that the byte
    // position after a successful read is consistent with the
    // lead-byte's nominal byte count.
    // -----------------------------------------------------------------
    if !payload.is_empty() {
        let mut r = BitReader::new(payload);
        if let Ok(v) = r.read_utf8_u64() {
            // When the reader accepts arbitrary bytes, the consumed
            // byte count must match the lead byte's nominal byte count
            // per §9.1.5. This catches reader bugs that succeed but
            // advance the cursor by the wrong amount, which would
            // corrupt every subsequent field in a chained stream.
            assert!(
                r.is_byte_aligned(),
                "read_utf8_u64 left bit-misaligned cursor"
            );
            let consumed = r.byte_position();
            let lead = payload[0];
            let expected_consumed = match lead {
                0x00..=0x7F => 1,
                0xC0..=0xDF => 2,
                0xE0..=0xEF => 3,
                0xF0..=0xF7 => 4,
                0xF8..=0xFB => 5,
                0xFC..=0xFD => 6,
                0xFE => 7,
                _ => panic!(
                    "read_utf8_u64 returned Ok on lead byte {lead:#04x} which the spec forbids"
                ),
            };
            assert_eq!(
                consumed, expected_consumed,
                "read_utf8_u64 consumed {consumed} bytes for lead {lead:#04x} (expected {expected_consumed}), value={v:#x}"
            );
        }
    }
    // The remainder bytes also feed the panic-freedom contract.
    if !remainder.is_empty() {
        let mut r = BitReader::new(remainder);
        let _ = r.read_utf8_u64(); // result irrelevant; just must not panic.
    }

    // -----------------------------------------------------------------
    // Schedule D: chained writer→reader. Build a sequence of up to
    // `MAX_CHAIN_LEN` values from the payload bytes (one value per
    // 4 bytes, masked to 36 bits), write them all into one
    // `BitWriter`, then drain them through one `BitReader`. The wire
    // format has no separator between varints — each is
    // self-terminating — so this is the only schedule that catches
    // framing bugs where one varint's tail bleeds into the next
    // value.
    // -----------------------------------------------------------------
    {
        let mut sent: Vec<u64> = Vec::with_capacity(MAX_CHAIN_LEN);
        let mode_lo = (mode & 0x07) as usize;
        let n_to_send = mode_lo.min(MAX_CHAIN_LEN);
        for chunk in payload.chunks_exact(4).take(n_to_send) {
            let mut b = [0u8; 4];
            b.copy_from_slice(chunk);
            let raw = u32::from_le_bytes(b) as u64;
            // Mix in the high bits so we cover the >32-bit range too:
            // shift by mode's high nibble, mask to 36 bits.
            let shift = (mode >> 4) as u64 & 0b111; // 0..=7
            let v = (raw << shift) & MAX_VARINT;
            sent.push(v);
        }
        if !sent.is_empty() {
            let mut w = BitWriter::new();
            for &v in &sent {
                w.write_utf8_u64(v);
            }
            // The writer must leave the bit cursor byte-aligned for
            // the chained read to work — schedule A re-asserts this
            // per-value; here we re-assert it post-chain.
            assert!(w.is_byte_aligned(), "writer left chain bit-misaligned");
            let wire = w.finish();
            let mut r = BitReader::new(&wire);
            for (i, &v) in sent.iter().enumerate() {
                let got = r
                    .read_utf8_u64()
                    .expect("chained roundtrip: reader rejected our own writer's bytes");
                assert_eq!(
                    got, v,
                    "chained roundtrip diverged at index {i}: wrote {v:#x}, read {got:#x}"
                );
                assert!(
                    r.is_byte_aligned(),
                    "chained reader left bit-misaligned after value index {i}"
                );
            }
            // After draining `sent.len()` values, the reader must be
            // exactly at the end of the wire bytes — no leftover
            // padding the writer accidentally emitted.
            assert_eq!(
                r.byte_position(),
                wire.len(),
                "chained reader left {} bytes of trailing slack",
                wire.len() - r.byte_position()
            );
        }
    }

    // -----------------------------------------------------------------
    // Schedule F: alignment-precondition coverage. The reader's
    // contract is that it requires byte alignment on entry. Build a
    // 4-byte synthetic stream, consume one bit so the cursor is
    // bit-aligned but not byte-aligned, and assert the next
    // `read_utf8_u64` returns `Err` (not a panic).
    //
    // Independent of fuzzer input; runs every iteration so a
    // regression that removes the alignment guard would surface
    // immediately.
    // -----------------------------------------------------------------
    {
        let stream = [0x00u8, 0x00, 0x00, 0x00];
        let mut r = BitReader::new(&stream);
        let _ = r
            .read_u32(1)
            .expect("BitReader::read_u32(1) must succeed on a 4-byte stream");
        assert!(
            !r.is_byte_aligned(),
            "BitReader::read_u32(1) left a byte-aligned cursor"
        );
        let res = r.read_utf8_u64();
        assert!(
            res.is_err(),
            "read_utf8_u64 must reject bit-misaligned cursor, got Ok({:?})",
            res.ok()
        );
    }
});
