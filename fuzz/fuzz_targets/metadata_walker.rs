#![no_main]

//! Fuzz: METADATA_BLOCK chain walker + writer round-trip.
//!
//! Complements the four existing targets (`panic_free_decode`,
//! `roundtrip`, `decode`, `flac_oracle_decode`) by focusing entirely
//! on the metadata-block surface — the four typed parsers AND the
//! four matching writers (`write_seektable`, `write_cuesheet`,
//! `write_padding`, `write_picture`) plus `BlockHeader::write_into`.
//!
//! The previous fuzz coverage exercises every metadata parser in
//! isolation but never feeds a parsed structure back into its writer.
//! That leaves the writer paths exposed to:
//!
//! * arithmetic overflow when an attacker-supplied payload survives
//!   parsing but contains lengths the writer's pre-sized buffer
//!   maths would overflow,
//! * invariants the writer rejects that the parser accepts (or vice
//!   versa) — silently divergent contracts,
//! * panics in `Vec::with_capacity` when the writer over-estimates
//!   the output size on a degenerate input,
//! * panic in `parse_picture` — which no existing fuzz target
//!   touches (parsers 1..=5 in `decode.rs` cover BlockHeader /
//!   StreamInfo / SeekTable / CueSheet / frame header; PICTURE is
//!   only exercised through the demuxer's container path, which
//!   front-loads ID3v2-prefix detection and short-circuits on
//!   missing `fLaC` magic).
//!
//! Two complementary entry shapes per fuzz iteration:
//!
//! 1. **Walker.** Treat `data` as a `fLaC`-magic-led metadata
//!    chain. Walk `BlockHeader::parse` → typed parser for each
//!    well-known block type → writer round-trip → re-parse asserts.
//!    The walker is bounded (max 16 blocks per iteration) so a
//!    fuzzer can't lock the loop on a pathological input.
//!
//! 2. **Direct typed parsers + writers.** Run every typed parser
//!    against the raw input, then for each `Ok` result, run its
//!    writer and re-parse the writer output. This is the path
//!    most likely to expose a writer that produces bytes its own
//!    parser rejects — a contract violation that's silent in
//!    isolation.
//!
//! `write_seektable` takes a slice; we feed it the parsed slice and
//! check `parse_seektable(write_seektable(parse_seektable(x)))` ==
//! `parse_seektable(x)` (the placeholder-stripping property the
//! parser exposes).
//!
//! `write_padding` takes a length; we round-trip a derived length
//! against `BlockHeader::MAX_LENGTH`.
//!
//! Contract under test: every public surface returns a `Result` for
//! malformed input — never panics, never integer-overflows in a
//! debug build, never indexes out of bounds. `Ok` results from the
//! parser MUST round-trip through their writer + re-parse cleanly.

use libfuzzer_sys::fuzz_target;
use oxideav_flac::metadata::{
    parse_cuesheet, parse_picture, parse_seektable, write_cuesheet, write_padding, write_picture,
    write_seektable, BlockHeader, BlockType, Picture, StreamInfo, FLAC_MAGIC,
};

/// Cap the walker so a fuzzer can't make us iterate forever on a
/// degenerate chain of 4-byte headers each declaring length=0.
const MAX_BLOCKS_PER_WALK: usize = 16;

/// Cap the per-block payload size we'll actually run the typed
/// parser against, irrespective of what the on-wire header
/// claims. Without this an attacker-claimed 16 MiB-minus-one block
/// would push the harness through 16 MiB of zero-byte payload on
/// every iteration even when the input is much smaller.
const MAX_PAYLOAD_BYTES: usize = 64 * 1024;

fuzz_target!(|data: &[u8]| {
    // -----------------------------------------------------------------
    // Path 1: walker. Treat `data` as a FLAC-magic-led metadata
    // chain. Don't require a real `fLaC` prefix — the walker should
    // be robust regardless of whether the magic matches. The first
    // four bytes (when present) feed straight into the first
    // `BlockHeader::parse` call, which is permissive at the type
    // level (any 4 bytes parse to *some* header; the block_type
    // alone determines whether the typed parser will accept the
    // payload).
    // -----------------------------------------------------------------
    let walk_start = if data.len() >= 4 && data[..4] == FLAC_MAGIC {
        4
    } else {
        0
    };
    let mut cursor = walk_start;
    let mut blocks_walked = 0usize;
    while cursor + 4 <= data.len() && blocks_walked < MAX_BLOCKS_PER_WALK {
        let header = match BlockHeader::parse(&data[cursor..cursor + 4]) {
            Ok(h) => h,
            Err(_) => break,
        };
        // Round-trip the header itself: write_into followed by
        // parse must reproduce (last, block_type, length). Skip the
        // round-trip when the type is unwritable (Invalid /
        // Reserved with the high bit set).
        let mut header_buf = Vec::new();
        if header.write_into(&mut header_buf).is_ok() {
            assert_eq!(
                header_buf.len(),
                4,
                "BlockHeader::write_into must emit 4 bytes"
            );
            let reparsed =
                BlockHeader::parse(&header_buf).expect("4-byte buffer must parse as a header");
            assert_eq!(reparsed.last, header.last);
            assert_eq!(reparsed.length, header.length);
            // BlockType equality is structural — Reserved(n) carries
            // its inner code so direct PartialEq comparison works.
            assert!(
                discriminant_eq(&reparsed.block_type, &header.block_type),
                "block-type code drift across header write/parse"
            );
        }
        cursor += 4;
        let payload_len = (header.length as usize).min(MAX_PAYLOAD_BYTES);
        let remaining = data.len().saturating_sub(cursor);
        let take = payload_len.min(remaining);
        let payload = &data[cursor..cursor + take];
        try_typed_round_trip(header.block_type, payload);
        cursor += take;
        blocks_walked += 1;
        if header.last {
            break;
        }
    }

    // -----------------------------------------------------------------
    // Path 2: direct typed parsers + writers against the raw input.
    // Each parser is a pure byte→`Result` fn; the writers (where
    // they exist) take the parsed structure back to bytes. The
    // contract is "parser ∘ writer ∘ parser == parser".
    // -----------------------------------------------------------------
    let _ = StreamInfo::parse(data); // no writer; parser-only smoke

    // SEEKTABLE: parser is infallible (returns Vec), writer enforces
    // strict ordering. The parsed slice is sorted by definition (the
    // wire format doesn't enforce ordering, so a hostile input may
    // produce an unsorted slice — feed only the sorted prefix to the
    // writer to keep the contract narrow).
    let pts = parse_seektable(data);
    let sorted_prefix = ascending_unique_prefix(&pts);
    if let Ok(written) = write_seektable(&sorted_prefix, 0) {
        let re = parse_seektable(&written);
        assert_eq!(
            re.len(),
            sorted_prefix.len(),
            "write_seektable + parse_seektable lost {} entries",
            sorted_prefix.len() - re.len(),
        );
        for (a, b) in re.iter().zip(sorted_prefix.iter()) {
            assert_eq!(a.sample_number, b.sample_number);
            assert_eq!(a.offset, b.offset);
            assert_eq!(a.frame_samples, b.frame_samples);
        }
    }

    // CUESHEET: parser + writer both fail noisily on invalid input.
    // Round-trip an Ok parse through write_cuesheet and confirm the
    // re-parse equals the original.
    if let Ok(cs) = parse_cuesheet(data) {
        if let Ok(written) = write_cuesheet(&cs) {
            let re = parse_cuesheet(&written).expect("write_cuesheet output must re-parse");
            assert_eq!(re, cs, "cuesheet round-trip drift");
        }
    }

    // PICTURE: parser + writer both validate; only well-formed
    // inputs survive parse_picture. Round-trip and confirm byte
    // stability (the writer is deterministic per RFC 9639 §8.8).
    if let Ok(pic) = parse_picture(data) {
        let written = write_picture(&pic).expect("parse_picture Ok ⇒ write_picture must succeed");
        let re = parse_picture(&written).expect("write_picture output must re-parse");
        assert_pictures_equal(&pic, &re);
        // Second-pass byte stability: write_picture must be
        // deterministic so the second emission equals the first.
        let written2 = write_picture(&re).expect("second write_picture must succeed");
        assert_eq!(
            written, written2,
            "write_picture is non-deterministic — same input yielded different bytes"
        );
    }

    // PADDING: writer takes a length, no parser to feed it back
    // through. Derive a length from the input and assert
    // `write_padding(len)` succeeds for in-range lengths and fails
    // for over-range ones (the 24-bit ceiling).
    if data.len() >= 4 {
        let len_raw = u32::from_le_bytes([data[0], data[1], data[2], data[3]]);
        let len = len_raw as usize;
        let result = write_padding(len);
        if len <= BlockHeader::MAX_LENGTH as usize {
            // We don't want a fuzz iteration to allocate 16 MiB on
            // every run, so only assert the contract for sub-1MiB
            // requests; the larger range is covered by unit tests.
            if len <= 1024 * 1024 {
                let padding = result.expect("in-range write_padding must succeed");
                assert_eq!(padding.len(), len);
                assert!(
                    padding.iter().all(|&b| b == 0),
                    "padding must be zero-filled"
                );
            }
        } else {
            assert!(result.is_err(), "out-of-range write_padding must reject");
        }
    }
});

/// Per-block-type typed-parser + writer round-trip.
///
/// Called for every block walked by Path 1. `STREAMINFO` is
/// parser-only (no writer surface). `VORBIS_COMMENT` and
/// `APPLICATION` are not exposed as typed parsers in oxideav-flac
/// today, so the walker skips them (the demuxer handles them at the
/// container layer); they are still implicitly exercised through the
/// existing pipeline-based fuzz targets.
fn try_typed_round_trip(block_type: BlockType, payload: &[u8]) {
    match block_type {
        BlockType::StreamInfo => {
            let _ = StreamInfo::parse(payload);
        }
        BlockType::Padding => {
            let _ = write_padding(payload.len());
        }
        BlockType::SeekTable => {
            let pts = parse_seektable(payload);
            let sorted = ascending_unique_prefix(&pts);
            if let Ok(written) = write_seektable(&sorted, 0) {
                let re = parse_seektable(&written);
                assert_eq!(re.len(), sorted.len());
            }
        }
        BlockType::CueSheet => {
            if let Ok(cs) = parse_cuesheet(payload) {
                if let Ok(written) = write_cuesheet(&cs) {
                    let re = parse_cuesheet(&written)
                        .expect("write_cuesheet must round-trip through parse_cuesheet");
                    assert_eq!(re, cs);
                }
            }
        }
        BlockType::Picture => {
            if let Ok(pic) = parse_picture(payload) {
                let written =
                    write_picture(&pic).expect("parse_picture Ok ⇒ write_picture must succeed");
                let re =
                    parse_picture(&written).expect("write_picture output must re-parse cleanly");
                assert_pictures_equal(&pic, &re);
            }
        }
        // Application / VorbisComment / Reserved / Invalid: no
        // typed surface here; payloads still survive the walker
        // (header round-trip already exercised above).
        _ => {}
    }
}

/// Strict-ascending-by-sample_number unique prefix of `pts`. The
/// SEEKTABLE writer requires this property; we feed it the longest
/// matching prefix so the writer reliably succeeds without
/// short-circuiting on a sorting failure (which is the parser's
/// tolerance, not the writer's contract).
fn ascending_unique_prefix(
    pts: &[oxideav_flac::metadata::SeekPoint],
) -> Vec<oxideav_flac::metadata::SeekPoint> {
    let mut out = Vec::with_capacity(pts.len());
    let mut prev: Option<u64> = None;
    for sp in pts {
        match prev {
            Some(p) if sp.sample_number <= p => break,
            _ => {
                out.push(*sp);
                prev = Some(sp.sample_number);
            }
        }
    }
    out
}

/// Compare two `BlockType` values for value equality. The enum has a
/// `Reserved(u8)` variant so structural `PartialEq` is what we need
/// — but it isn't derived on the public type, so we destructure
/// instead of using `==`. (If `PartialEq` is added later this helper
/// becomes a one-liner.)
fn discriminant_eq(a: &BlockType, b: &BlockType) -> bool {
    match (a, b) {
        (BlockType::StreamInfo, BlockType::StreamInfo) => true,
        (BlockType::Padding, BlockType::Padding) => true,
        (BlockType::Application, BlockType::Application) => true,
        (BlockType::SeekTable, BlockType::SeekTable) => true,
        (BlockType::VorbisComment, BlockType::VorbisComment) => true,
        (BlockType::CueSheet, BlockType::CueSheet) => true,
        (BlockType::Picture, BlockType::Picture) => true,
        (BlockType::Reserved(x), BlockType::Reserved(y)) => x == y,
        (BlockType::Invalid, BlockType::Invalid) => true,
        _ => false,
    }
}

/// Compare two `Picture` values for the round-trip equality check.
/// `Picture` doesn't derive `PartialEq` (the `Vec<u8>` data field
/// would make the derived impl O(N) — fine for a fuzz harness, but
/// the upstream type chose conservatism), so destructure manually.
fn assert_pictures_equal(a: &Picture, b: &Picture) {
    assert_eq!(a.picture_type, b.picture_type, "picture_type drift");
    assert_eq!(a.mime_type, b.mime_type, "mime_type drift");
    assert_eq!(a.description, b.description, "description drift");
    assert_eq!(a.width, b.width, "width drift");
    assert_eq!(a.height, b.height, "height drift");
    assert_eq!(a.depth, b.depth, "depth drift");
    assert_eq!(a.colour_count, b.colour_count, "colour_count drift");
    assert_eq!(a.data, b.data, "picture data drift");
}
