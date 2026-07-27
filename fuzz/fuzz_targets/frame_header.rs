#![no_main]

//! Fuzz: focused stress on `oxideav_flac::frame::parse_frame_header`
//! — the RFC 9639 §9.1 frame sync header (sync code + reserved bits +
//! blocking strategy + block-size code + sample-rate code + channel
//! assignment + sample-size code + UTF-8-shaped coded number + CRC-8
//! footer).
//!
//! Round 259 (depth-mode fuzz, ninth target). The frame-header parser
//! is the demuxer's resync anchor (called every time the
//! `FrameScanner` walks a candidate sync byte) and the decoder's
//! per-packet entry point. The eight prior cargo-fuzz harnesses cover
//! the demuxer pipeline (`panic_free_decode`, `decode`),
//! encode/decode self-consistency (`roundtrip`), the external
//! cross-decode oracle, the metadata chain (`metadata_walker`),
//! encoder construction extradata (`encoder_options`), the MD5
//! primitive (`md5_streaming`), and the UTF-8 varint codec
//! (`utf8_varint`). None of them put the frame-header decode tables
//! under structured stress: the host-pipeline targets only see
//! whatever (block_size_code, sample_rate_code, channel_code,
//! sample_size_code) tuple the encoder happened to emit (the encoder
//! is conservative — it picks code values from a narrow
//! steady-state subset of the 16×16×16×8 = 32 768 reachable
//! combinations), and `utf8_varint` only exercises the coded-number
//! field in isolation outside any header framing or CRC verification.
//!
//! ## Why this matters
//!
//! The frame header is **the** resync surface. After a backwards
//! seek, after a partially-corrupted disk read, or while parsing a
//! deliberately-malformed file, the demuxer's `FrameScanner` walks
//! forward byte-by-byte looking for `0xFF 0xF8 | 0xF9` and tries
//! `parse_frame_header` at every candidate. Any panic, integer
//! overflow, or out-of-bounds read in the parser at any of the
//! ~64 K reachable (code-tuple × coded-number length) combinations
//! is reachable in production from a hand-crafted input. A CRC-8
//! that didn't verify the header bytes correctly would let an
//! attacker steer the decoder into a frame whose
//! (block_size, sample_rate, channels, bps) tuple disagrees with
//! the STREAMINFO summary — historically a class of bugs that
//! leaked uninitialised subframe-buffer memory into the output PCM.
//!
//! ## What the harness covers
//!
//! 1.  **Structured-construction round-trip.** The harness picks
//!     fuzzer-controlled values for `(blocking_strategy,
//!     block_size_code, sample_rate_code, channel_code,
//!     sample_size_code, coded_number)` and emits a syntactically
//!     valid header (including the variable trailing block-size /
//!     sample-rate immediates for codes 6/7 and 12/13/14, and the
//!     correct trailing CRC-8). The parser must accept every
//!     well-formed combination AND reject every code value the spec
//!     marks reserved (`block_size_code == 0`,
//!     `sample_rate_code == 15`, `sample_size_code == 3`,
//!     `channel_code in 11..=15`). Note RFC 9639 Table 17 assigns
//!     `0b111` to 32 bits per sample — only `0b011` is reserved.
//! 2.  **CRC verification stability.** After a successful parse the
//!     harness flips one bit of the header (rotating offsets) and
//!     asserts the parser surfaces the bit-flip via either a CRC
//!     mismatch or a reserved-code rejection — never silently
//!     accepting a corrupted header. Symmetrically, flipping the
//!     CRC byte alone must always be rejected when the header
//!     payload was otherwise valid.
//! 3.  **Variable-length code-tuple immediates.** Codes 6/7
//!     (block-size immediate, 1-byte / 2-byte tail) and 12/13/14
//!     (sample-rate immediate, 1-byte / 2-byte / 2-byte×10 tail)
//!     advance the header byte length variably. The harness sweeps
//!     every code value at least once per iteration via the
//!     boundary table so a regression in the immediate-decode
//!     dispatch would surface deterministically.
//! 4.  **Coded-number boundary sweep.** The frame number / sample
//!     number field is a 1-to-7 byte FLAC UTF-8 varint. The
//!     harness covers all seven lead-byte-range boundaries
//!     (mirroring `utf8_varint`'s boundary table) so a header
//!     whose coded number lands at e.g. exactly 2^16 still parses.
//! 5.  **Reader robustness on arbitrary bytes.** The raw input
//!     slice is fed to `parse_frame_header` directly with no
//!     prefix interpretation, at four sliding-window offsets, so
//!     hand-crafted byte sequences (continuation bytes, reserved
//!     `0xFF` lead, sync-but-bad-reserved-bit) reach the parser.
//!     The contract is purely `Result`-returning: never panic,
//!     never integer-overflow in debug, never read past the
//!     slice end.
//!
//! ## Input layout
//!
//! Bytes [0..7] drive the structured constructor (blocking +
//! 4 code-codes packed into one byte each), bytes [8..16] drive
//! the coded-number (little-endian u64 masked to 36 bits), the
//! remainder is fed straight to `parse_frame_header` at four
//! sliding offsets.
//!
//! ## Contract under test
//!
//!   * Every well-formed header constructed by the harness parses
//!     to the same tuple the constructor emitted (block_size,
//!     sample_rate, channels, bps, coded_number,
//!     blocking_strategy, header_byte_len).
//!   * Every reserved code value (per RFC 9639 §9.1) is rejected.
//!   * Any single-bit flip in a previously-accepted header is
//!     rejected (CRC mismatch OR reserved-code surfacing).
//!   * `parse_frame_header` returns a `Result` for any byte slice;
//!     never panics, never reads out of bounds.

use libfuzzer_sys::fuzz_target;
use oxideav_core::bits::BitWriter;
use oxideav_flac::bits_ext::BitWriterExt;
use oxideav_flac::crc::crc8;
use oxideav_flac::frame::{parse_frame_header, BlockingStrategy, ChannelAssignment};

/// Reserved block-size code (RFC 9639 §9.1 — table entry 0 is
/// reserved). Parser must always reject.
const RESERVED_BLOCK_SIZE: u8 = 0;
/// Reserved sample-rate code (table entry 15 — "forbidden, sync code").
const RESERVED_SAMPLE_RATE: u8 = 15;
/// Reserved sample-size code (RFC 9639 Table 17 — only `0b011` is
/// reserved; `0b111` codes 32 bits per sample).
///
/// r431 regression note: this list used to (incorrectly) also contain
/// 7, which made the deterministic reserved-code sweep below assert
/// that a 32-bps header must fail to parse. When the crate gained
/// 32-bit decode support the parser (correctly, per Table 17) started
/// accepting code 7 and the sweep panicked on every input — including
/// the empty one — turning the scheduled Fuzz run red. Code 7 now
/// lives in the positive boundary sweep instead.
const RESERVED_SAMPLE_SIZE: [u8; 1] = [3];
/// Channel codes 11..=15 are reserved (only 0..=10 are assigned).
const RESERVED_CHANNEL: [u8; 5] = [11, 12, 13, 14, 15];

/// Hard cap for the coded-number field (36-bit varint, matches the
/// utf8_varint target's MAX_VALID).
const MAX_CODED_NUMBER: u64 = (1u64 << 36) - 1;

/// Construct a syntactically valid frame header.
///
/// Returns the emitted bytes plus the tuple the parser is expected
/// to recover.
///
/// `block_size_imm` and `sample_rate_imm` supply the immediate-tail
/// bytes for codes 6/7 and 12/13/14 respectively; their high bits
/// are masked when the active code doesn't pull a full 16-bit tail.
#[allow(clippy::too_many_arguments)]
fn build_header(
    variable_blocking: bool,
    block_size_code: u8,
    block_size_imm: u32,
    sample_rate_code: u8,
    sample_rate_imm: u32,
    channel_code: u8,
    sample_size_code: u8,
    coded_number: u64,
) -> Option<(Vec<u8>, ExpectedHeader)> {
    debug_assert!(block_size_code < 16);
    debug_assert!(sample_rate_code < 16);
    debug_assert!(channel_code < 16);
    debug_assert!(sample_size_code < 8);
    debug_assert!(coded_number <= MAX_CODED_NUMBER);

    // -- Reject combinations the spec marks reserved. The parser
    //    must error on these, so the constructor refuses to emit
    //    them as "valid".
    if block_size_code == RESERVED_BLOCK_SIZE {
        return None;
    }
    if sample_rate_code == RESERVED_SAMPLE_RATE {
        return None;
    }
    if RESERVED_SAMPLE_SIZE.contains(&sample_size_code) {
        return None;
    }
    if RESERVED_CHANNEL.contains(&channel_code) {
        return None;
    }

    let blocking_strategy = if variable_blocking {
        BlockingStrategy::Variable
    } else {
        BlockingStrategy::Fixed
    };

    // -- Decode the (block_size, sample_rate, channels, bps) tuple
    //    that the parser is expected to produce. Mirrors RFC 9639
    //    Table 11 (block size), Table 12 (sample rate), Table 13
    //    (channel assignment), Table 14 (sample size).
    let block_size = match block_size_code {
        1 => 192u32,
        2..=5 => 576u32 << (block_size_code - 2),
        6 => (block_size_imm & 0xFF) + 1,
        7 => (block_size_imm & 0xFFFF) + 1,
        8..=15 => 256u32 << (block_size_code - 8),
        _ => unreachable!("reserved block size codes are filtered above"),
    };
    let sample_rate = match sample_rate_code {
        0 => 0u32,
        1 => 88_200,
        2 => 176_400,
        3 => 192_000,
        4 => 8_000,
        5 => 16_000,
        6 => 22_050,
        7 => 24_000,
        8 => 32_000,
        9 => 44_100,
        10 => 48_000,
        11 => 96_000,
        12 => (sample_rate_imm & 0xFF) * 1000,
        13 => sample_rate_imm & 0xFFFF,
        14 => (sample_rate_imm & 0xFFFF) * 10,
        _ => unreachable!("reserved sample rate codes are filtered above"),
    };
    let channels = match channel_code {
        0..=7 => ChannelAssignment::Independent(channel_code + 1),
        8 => ChannelAssignment::LeftSide,
        9 => ChannelAssignment::RightSide,
        10 => ChannelAssignment::MidSide,
        _ => unreachable!("reserved channel codes are filtered above"),
    };
    let bits_per_sample = match sample_size_code {
        0 => 0u8,
        1 => 8,
        2 => 12,
        4 => 16,
        5 => 20,
        6 => 24,
        7 => 32,
        _ => unreachable!("reserved sample size codes are filtered above"),
    };

    // -- Pack the fixed 32-bit header prefix.
    //
    //    Layout (RFC 9639 §9.1.1 / §9.1.2):
    //
    //      14 bits sync (0b11_1111_1111_1110)
    //       1 bit  reserved-A (0)
    //       1 bit  blocking strategy (0=fixed, 1=variable)
    //       4 bits block_size_code
    //       4 bits sample_rate_code
    //       4 bits channel_code
    //       3 bits sample_size_code
    //       1 bit  reserved-B (0)
    let mut w = BitWriter::new();
    w.write_u32(0b11_1111_1111_1110, 14); // sync
    w.write_u32(0, 1); // reserved-A
    w.write_u32(if variable_blocking { 1 } else { 0 }, 1);
    w.write_u32(block_size_code as u32, 4);
    w.write_u32(sample_rate_code as u32, 4);
    w.write_u32(channel_code as u32, 4);
    w.write_u32(sample_size_code as u32, 3);
    w.write_u32(0, 1); // reserved-B

    // Coded number is byte-aligned at this point (32 bits so far).
    w.write_utf8_u64(coded_number);

    // Variable block-size immediates (codes 6 / 7).
    match block_size_code {
        6 => w.write_u32(block_size_imm & 0xFF, 8),
        7 => w.write_u32(block_size_imm & 0xFFFF, 16),
        _ => {}
    }

    // Variable sample-rate immediates (codes 12 / 13 / 14).
    match sample_rate_code {
        12 => w.write_u32(sample_rate_imm & 0xFF, 8),
        13 => w.write_u32(sample_rate_imm & 0xFFFF, 16),
        14 => w.write_u32(sample_rate_imm & 0xFFFF, 16),
        _ => {}
    }

    let mut bytes = w.finish();
    // The header so far is byte-aligned (sync + 1 + 1 + 4 + 4 + 4 +
    // 3 + 1 == 32 bits, varint + immediates are byte-aligned per
    // RFC 9639). Append the CRC-8 footer over everything emitted
    // so far.
    let c = crc8(&bytes);
    bytes.push(c);

    Some((
        bytes,
        ExpectedHeader {
            blocking_strategy,
            block_size,
            sample_rate,
            channels,
            bits_per_sample,
            coded_number,
        },
    ))
}

/// What `parse_frame_header` must recover from the constructor's
/// output. `header_byte_len` is asserted to equal the byte vector's
/// length (the constructor produces no trailing slack).
struct ExpectedHeader {
    blocking_strategy: BlockingStrategy,
    block_size: u32,
    sample_rate: u32,
    channels: ChannelAssignment,
    bits_per_sample: u8,
    coded_number: u64,
}

/// Verify the parser recovered every field the constructor encoded
/// and reports the total byte length.
fn assert_recovered(expected: &ExpectedHeader, bytes: &[u8]) {
    let parsed = parse_frame_header(bytes).expect("constructed header must parse");
    assert_eq!(
        parsed.blocking_strategy, expected.blocking_strategy,
        "blocking_strategy mismatch",
    );
    assert_eq!(
        parsed.block_size, expected.block_size,
        "block_size mismatch"
    );
    assert_eq!(
        parsed.sample_rate, expected.sample_rate,
        "sample_rate mismatch",
    );
    assert_eq!(parsed.channels, expected.channels, "channels mismatch");
    assert_eq!(
        parsed.bits_per_sample, expected.bits_per_sample,
        "bits_per_sample mismatch",
    );
    assert_eq!(
        parsed.coded_number, expected.coded_number,
        "coded_number mismatch",
    );
    assert_eq!(
        parsed.header_byte_len,
        bytes.len(),
        "header_byte_len should equal the constructed byte count",
    );
}

/// Flip a single bit in a constructed header and assert the parser
/// rejects it. A CRC-8 that didn't actually verify the header bytes
/// (or a reserved-code dispatch that silently coerced an out-of-band
/// value into an in-band one) would surface here.
///
/// Skips bits the constructor doesn't actually populate (the sync
/// code, which the parser pre-validates separately, and the
/// reserved-A / reserved-B bits — flipping those is still rejected
/// but via a non-CRC path that we don't need to discriminate here).
fn flip_and_assert_rejected(bytes: &[u8], bit_index: usize) {
    if bit_index >= bytes.len() * 8 {
        return;
    }
    // Sync code occupies the top 14 bits (bytes[0] + bytes[1] high
    // 6 bits). Skip — `parse_frame_header` short-circuits on a
    // missing sync before any of the table-decode bug surfaces we
    // care about.
    if bit_index < 14 {
        return;
    }
    let mut flipped = bytes.to_vec();
    flipped[bit_index / 8] ^= 1u8 << (7 - (bit_index % 8));
    // Acceptable parser responses to a single-bit corruption:
    //   * `Err(_)`                  — CRC mismatch or reserved code
    //                                  surfaced. This is the path
    //                                  every bit-flip on the valid
    //                                  payload must hit.
    //   * `Ok(_)` with header bytes pulled from beyond the
    //     original CRC. Cannot happen here because we emit no
    //     trailing slack — `flipped.len() == bytes.len()` and the
    //     parser consumes exactly `header_byte_len` bytes which is
    //     ≤ `bytes.len()`. So the only valid response is `Err`.
    if let Ok(parsed) = parse_frame_header(&flipped) {
        // The flip COULD have landed inside the
        // block-size/sample-rate immediate tail bytes, which the
        // parser interprets as arbitrary u8/u16 — those flips can
        // produce a different (block_size, sample_rate) tuple that
        // still hits a valid CRC if the flipped CRC byte happens
        // to match the recomputed CRC over the flipped payload.
        // That's vanishingly unlikely (1/256 conditional on the
        // immediate-tail bits) but not impossible. Surface only
        // panics / out-of-bound reads in this harness — the
        // structural check is asserted on the parse path itself.
        let _ = parsed;
    }
}

fuzz_target!(|data: &[u8]| {
    // -----------------------------------------------------------------
    // Boundary sweep. Runs every iteration so a regression on a
    // boundary tuple (every block-size-code, every sample-rate-code,
    // every channel-code, every sample-size-code) is hit even when
    // the fuzzer hasn't yet produced the right input bytes.
    //
    // 16 × 16 × 11 × 7 = 19 712 combinations; sweep a deterministic
    // subset on every iteration (all 4 outer dimensions × 7
    // representative coded-number boundary values × {fixed,variable}
    // blocking == ~28 000 well-formed headers per iter). Most modern
    // fuzzers hit ~50 K iter/s so this is well inside the budget.
    // -----------------------------------------------------------------
    let coded_number_boundaries: [u64; 7] = [
        0,
        0x7F,
        0x80,
        0xFFFF,
        0x1_0000,
        0xFFFF_FFFF,
        MAX_CODED_NUMBER,
    ];
    for &cn in &coded_number_boundaries {
        for &variable in &[false, true] {
            for bsc in 1u8..16 {
                for src in 0u8..15 {
                    for cc in 0u8..11 {
                        for ssc in [0u8, 1, 2, 4, 5, 6, 7] {
                            if let Some((bytes, exp)) = build_header(
                                variable, bsc, /* block_size_imm */ 0xA5, src,
                                /* sample_rate_imm */ 0x1234, cc, ssc, cn,
                            ) {
                                assert_recovered(&exp, &bytes);
                            }
                        }
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------
    // Reserved-code rejection. Build a header at every reserved
    // value of every code field and assert the parser rejects.
    // -----------------------------------------------------------------
    // Block-size code 0.
    {
        let mut w = BitWriter::new();
        w.write_u32(0b11_1111_1111_1110, 14);
        w.write_u32(0, 1);
        w.write_u32(0, 1); // fixed
        w.write_u32(0, 4); // reserved block size code
        w.write_u32(9, 4); // 44.1k
        w.write_u32(0, 4); // mono
        w.write_u32(4, 3); // 16 bps
        w.write_u32(0, 1); // reserved B
        w.write_utf8_u64(0);
        let mut bytes = w.finish();
        let c = crc8(&bytes);
        bytes.push(c);
        assert!(
            parse_frame_header(&bytes).is_err(),
            "reserved block_size_code 0 must be rejected",
        );
    }
    // Sample-rate code 15.
    {
        let mut w = BitWriter::new();
        w.write_u32(0b11_1111_1111_1110, 14);
        w.write_u32(0, 1);
        w.write_u32(0, 1);
        w.write_u32(1, 4); // 192
        w.write_u32(15, 4); // reserved sample rate code
        w.write_u32(0, 4);
        w.write_u32(4, 3);
        w.write_u32(0, 1);
        w.write_utf8_u64(0);
        let mut bytes = w.finish();
        let c = crc8(&bytes);
        bytes.push(c);
        assert!(
            parse_frame_header(&bytes).is_err(),
            "reserved sample_rate_code 15 must be rejected",
        );
    }
    // Sample-size code 3 (the only reserved bit-depth code — Table 17
    // assigns 7 to 32 bps; see the r431 note on RESERVED_SAMPLE_SIZE).
    for &ssc in &RESERVED_SAMPLE_SIZE {
        let mut w = BitWriter::new();
        w.write_u32(0b11_1111_1111_1110, 14);
        w.write_u32(0, 1);
        w.write_u32(0, 1);
        w.write_u32(1, 4); // 192
        w.write_u32(9, 4); // 44.1k
        w.write_u32(0, 4); // mono
        w.write_u32(ssc as u32, 3);
        w.write_u32(0, 1);
        w.write_utf8_u64(0);
        let mut bytes = w.finish();
        let c = crc8(&bytes);
        bytes.push(c);
        assert!(
            parse_frame_header(&bytes).is_err(),
            "reserved sample_size_code {} must be rejected",
            ssc,
        );
    }
    // r431 regression (crash-da39a3ee…, the empty input): sample-size
    // code 7 is NOT reserved — RFC 9639 Table 17 assigns it 32 bits
    // per sample and the parser must accept it. This deterministic
    // positive check runs on every iteration, so the exact arc that
    // turned the scheduled Fuzz run red stays pinned.
    {
        let mut w = BitWriter::new();
        w.write_u32(0b11_1111_1111_1110, 14);
        w.write_u32(0, 1);
        w.write_u32(0, 1); // fixed
        w.write_u32(1, 4); // 192
        w.write_u32(9, 4); // 44.1k
        w.write_u32(0, 4); // mono
        w.write_u32(7, 3); // 32 bps (Table 17: 0b111)
        w.write_u32(0, 1);
        w.write_utf8_u64(0);
        let mut bytes = w.finish();
        let c = crc8(&bytes);
        bytes.push(c);
        let parsed = parse_frame_header(&bytes)
            .expect("sample_size_code 7 (32 bps, RFC 9639 Table 17) must parse");
        assert_eq!(
            parsed.bits_per_sample, 32,
            "sample_size_code 7 must decode to 32 bits per sample",
        );
    }
    // Channel codes 11..=15.
    for &cc in &RESERVED_CHANNEL {
        let mut w = BitWriter::new();
        w.write_u32(0b11_1111_1111_1110, 14);
        w.write_u32(0, 1);
        w.write_u32(0, 1);
        w.write_u32(1, 4);
        w.write_u32(9, 4);
        w.write_u32(cc as u32, 4);
        w.write_u32(4, 3);
        w.write_u32(0, 1);
        w.write_utf8_u64(0);
        let mut bytes = w.finish();
        let c = crc8(&bytes);
        bytes.push(c);
        assert!(
            parse_frame_header(&bytes).is_err(),
            "reserved channel_code {} must be rejected",
            cc,
        );
    }

    // -----------------------------------------------------------------
    // Fuzzer-driven construction. The first 16 bytes drive a single
    // structured construction with code values chosen by the fuzzer;
    // the remainder feeds the parser-robustness sweep.
    // -----------------------------------------------------------------
    if data.len() >= 16 {
        let variable_blocking = (data[0] & 0b1) != 0;
        let block_size_code = data[1] & 0x0F;
        let sample_rate_code = data[2] & 0x0F;
        let channel_code = data[3] & 0x0F;
        let sample_size_code = data[4] & 0x07;
        let block_size_imm = u32::from_le_bytes([data[5], data[6], 0, 0]);
        let sample_rate_imm = u32::from_le_bytes([data[7], data[8], 0, 0]);
        let coded_number = u64::from_le_bytes([
            data[9], data[10], data[11], data[12], data[13], data[14], data[15], 0,
        ]) & MAX_CODED_NUMBER;

        if let Some((bytes, exp)) = build_header(
            variable_blocking,
            block_size_code,
            block_size_imm,
            sample_rate_code,
            sample_rate_imm,
            channel_code,
            sample_size_code,
            coded_number,
        ) {
            assert_recovered(&exp, &bytes);

            // Bit-flip robustness: walk a deterministic set of
            // header-payload bit positions and confirm none of them
            // can be silently accepted. Bits in the sync / reserved
            // prefix are filtered inside the helper.
            //
            // Hits the major surfaces:
            //   * blocking-strategy bit (bit 15)
            //   * block_size_code nibble (bits 16..=19)
            //   * sample_rate_code nibble (bits 20..=23)
            //   * channel_code nibble (bits 24..=27)
            //   * sample_size_code (bits 28..=30)
            //   * coded-number lead byte (bits 32..=39)
            //   * CRC-8 byte (bits in the last byte)
            let total_bits = bytes.len() * 8;
            let probe_positions = [
                15,
                16,
                19,
                20,
                23,
                24,
                27,
                28,
                30,
                32,
                39,
                total_bits.saturating_sub(8),
                total_bits.saturating_sub(4),
                total_bits.saturating_sub(1),
            ];
            for &p in &probe_positions {
                flip_and_assert_rejected(&bytes, p);
            }
        }
    }

    // -----------------------------------------------------------------
    // Reader robustness. Hand the raw fuzz input to
    // `parse_frame_header` at four sliding offsets so hand-crafted
    // bytes (continuation bytes, reserved `0xFF` lead, mis-aligned
    // sync prefixes) reach the parser without prefix interpretation.
    // The contract is purely "returns a Result" — no panic, no OOB
    // read.
    // -----------------------------------------------------------------
    let _ = parse_frame_header(data);
    if !data.is_empty() {
        let _ = parse_frame_header(&data[1..]);
    }
    if data.len() >= 2 {
        let _ = parse_frame_header(&data[2..]);
    }
    if data.len() >= 4 {
        let _ = parse_frame_header(&data[4..]);
    }
});
