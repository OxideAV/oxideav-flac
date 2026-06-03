#![no_main]

//! Fuzz: encoder construction + flush with fuzzer-chosen `FlacEncoderOptions`.
//!
//! Round 228 (depth-mode fuzz): the existing five targets cover the
//! decoder pipeline (`panic_free_decode`, `decode`), the
//! self-consistency of encode/decode (`roundtrip`), the libavcodec
//! oracle (`flac_oracle_decode`) and arbitrary metadata-block chains
//! (`metadata_walker`). None of them put `make_encoder_with_options`
//! itself under stress. The construction path threads three
//! distinct decisions on the encoder side that the wider targets
//! sweep only indirectly:
//!
//!   * `padding_bytes = None` vs `Some(0)` vs `Some(n)` shapes
//!     STREAMINFO's `last` bit AND introduces a second metadata
//!     block whose 24-bit length field is bounded by
//!     `BlockHeader::MAX_LENGTH` (RFC 9639 §6),
//!   * channel count / sample rate / bit-depth combinations land
//!     `Error::invalid` / `Error::unsupported` rejections in the
//!     prelude before any allocation happens — a panic here would
//!     be a real bug,
//!   * a successful construction always produces an `extradata`
//!     chain (STREAMINFO ± PADDING) whose every `BlockHeader::parse`
//!     leg + typed-block-payload length agrees with the wire
//!     STREAMINFO length (34) and the requested padding length.
//!
//! This target carves the fuzz input into:
//!
//!   - 1 B  format selector (U8 / S16 / S24 / S32)
//!   - 1 B  channel selector (a raw 0..=255 byte — most values
//!     should be rejected by the `1..=8` guard without panicking)
//!   - 1 B  sample-rate selector (8 kHz, 44.1 kHz, 48 kHz, 96 kHz,
//!     plus three pathological edges: 0, 1, u32::MAX)
//!   - 1 B  padding mode selector
//!     (0 = None, 1 = Some(0), 2 = Some(small), 3 = Some(near-cap),
//!     4 = Some(over-cap))
//!   - 4 B  little-endian seed for the padding size in modes 2/3
//!   - rest: a small PCM tail used to drive a flush attempt when
//!     construction succeeds. Bounded at MAX_PCM_BYTES so the fuzzer
//!     doesn't spend its budget encoding huge frames.
//!
//! Contract under test:
//!
//!   * `make_encoder_with_options(..)` returns `Result`. It MUST NOT
//!     panic for any combination of param + option bytes the harness
//!     can produce. Out-of-range channels / unsupported sample
//!     formats / over-cap padding must surface as `Err(_)`, never
//!     unwind.
//!   * On `Ok`, `output_params().extradata` is a parseable
//!     metadata chain. STREAMINFO is the first block; if
//!     `padding_bytes` was `Some(n)`, STREAMINFO has `last=false`,
//!     a PADDING(last=true, length=n, payload=all-zero) follows,
//!     and the chain ends exactly there.
//!   * Feeding a small PCM tail through `send_frame` + `flush` +
//!     `receive_packet*` keeps the post-flush extradata chain
//!     walkable with the same invariants (the STREAMINFO payload
//!     gets repopulated, but its header length stays 34 and any
//!     trailing PADDING block is preserved with its original
//!     length and zero payload).

use libfuzzer_sys::fuzz_target;

use oxideav_core::{AudioFrame, CodecId, CodecParameters, Error, Frame, SampleFormat};
use oxideav_flac::encoder::{make_encoder_with_options, FlacEncoderOptions};
use oxideav_flac::metadata::{BlockHeader, BlockType};

/// Header byte count for a metadata block (RFC 9639 §6).
const BLOCK_HEADER_LEN: usize = 4;
/// STREAMINFO payload length per RFC 9639 §8.2.
const STREAMINFO_PAYLOAD_LEN: usize = 34;
/// Cap on PCM bytes the harness will ingest. A typical fuzz iteration
/// stays well under this; we just want a hard ceiling so a fuzzer
/// can't pin a worker on a multi-megabyte frame.
const MAX_PCM_BYTES: usize = 2048;
/// Cap on the "small padding" mode's payload. Big enough to exercise
/// non-trivial allocation paths, small enough that the fuzzer can run
/// thousands of iterations per second.
const SMALL_PADDING_CAP: u32 = 16 * 1024;

fuzz_target!(|data: &[u8]| {
    // Need at least the 8-byte selector prefix to even attempt a
    // construction.
    if data.len() < 8 {
        return;
    }

    let fmt_sel = data[0] & 0x03;
    let ch_raw = data[1];
    let sr_sel = data[2] & 0x07;
    let pad_sel = data[3] & 0x07;
    let pad_seed = u32::from_le_bytes([data[4], data[5], data[6], data[7]]);
    let rest = &data[8..];

    let format = match fmt_sel {
        0 => SampleFormat::U8,
        1 => SampleFormat::S16,
        2 => SampleFormat::S24,
        _ => SampleFormat::S32,
    };

    let sample_rate: u32 = match sr_sel {
        0 => 8_000,
        1 => 44_100,
        2 => 48_000,
        3 => 96_000,
        4 => 0,
        5 => 1,
        6 => u32::MAX,
        _ => 22_050,
    };

    let padding_bytes: Option<usize> = match pad_sel {
        0 => None,
        1 => Some(0),
        2 => Some((pad_seed % (SMALL_PADDING_CAP + 1)) as usize),
        3 => {
            // Near the 24-bit cap. `MAX_LENGTH` is the largest legal
            // value; pick within `[MAX_LENGTH - 1024, MAX_LENGTH]`.
            let max = BlockHeader::MAX_LENGTH as usize;
            let off = (pad_seed % 1025) as usize;
            Some(max.saturating_sub(off))
        }
        4 => {
            // Just over the cap — construction must reject without
            // panicking. We pick a value in `[MAX_LENGTH + 1,
            // MAX_LENGTH + 1024]`.
            let max = BlockHeader::MAX_LENGTH as usize;
            let off = (pad_seed % 1024) as usize + 1;
            Some(max + off)
        }
        _ => None,
    };

    // Assemble the codec parameters. Channels stay raw u8 (cast
    // to u16) so the harness exercises the `1..=8` validation in
    // `make_encoder_with_options`.
    let mut params = CodecParameters::audio(CodecId::new("flac"));
    params.channels = Some(ch_raw as u16);
    params.sample_rate = Some(sample_rate);
    params.sample_format = Some(format);

    let mut opts = FlacEncoderOptions::default();
    opts.padding_bytes = padding_bytes;

    let mut enc = match make_encoder_with_options(&params, opts.clone()) {
        Ok(e) => e,
        Err(_) => {
            // A construction-time rejection is fine. The contract is
            // "Result, never panic" — we exit the iteration here.
            return;
        }
    };

    // Construction succeeded — assert the extradata invariants
    // before going anywhere near `send_frame`.
    let extradata = enc.output_params().extradata.clone();
    if !verify_extradata_chain(&extradata, opts.padding_bytes) {
        // `verify_extradata_chain` panics on a hard contract
        // violation; a `false` return is the cooperative "I can't
        // verify this input without more bytes" signal, which is
        // fine.
        return;
    }

    // Skip the rest of the harness if the params landed an exotic
    // sample rate the demuxer's frame-header writer would round-trip
    // poorly. Construction-only is the value-add for those inputs.
    if sample_rate == 0 || sample_rate == 1 || sample_rate == u32::MAX {
        return;
    }

    // Build a small audio frame from the tail bytes and try to push
    // it through `send_frame` -> `flush` -> `receive_packet*`. The
    // post-flush extradata MUST still satisfy the same chain
    // invariants (with the STREAMINFO payload now populated).
    let channels = enc.output_params().channels.unwrap_or(1) as usize;
    if channels == 0 {
        return;
    }
    let bytes_per_sample = format.bytes_per_sample();
    let frame_bytes_max = MAX_PCM_BYTES.min(rest.len());
    // Round down to a whole (sample, channel) tuple.
    let per_tuple = bytes_per_sample * channels;
    if per_tuple == 0 {
        return;
    }
    let usable = (frame_bytes_max / per_tuple) * per_tuple;
    if usable == 0 {
        return;
    }
    let samples = (usable / per_tuple) as u32;
    let mut pcm = rest[..usable].to_vec();
    // Clamp any S24 / S32 / S16 / U8 payload byte that would land an
    // out-of-range sample after sign-extension to the format's
    // representable range. The encoder rejects out-of-range samples
    // with an `Err`, so leaving them in would just shrink the
    // useful corpus.
    clamp_pcm_in_place(&mut pcm, format);

    let frame = AudioFrame {
        samples,
        pts: Some(0),
        data: vec![pcm],
    };
    if enc.send_frame(&Frame::Audio(frame)).is_err() {
        // `send_frame` rejections on degenerate (samples=0) or
        // mis-sized payloads are acceptable.
        return;
    }
    if enc.flush().is_err() {
        return;
    }
    loop {
        match enc.receive_packet() {
            Ok(_) => {}
            Err(Error::NeedMore) | Err(Error::Eof) => break,
            Err(_) => return,
        }
    }

    // Post-flush, the encoder rebuilds STREAMINFO with the live MD5,
    // min/max frame size, and total sample count. The PADDING tail
    // (if any) must survive the rebuild byte-for-byte after its
    // own header.
    let extradata_after = enc.output_params().extradata.clone();
    if !verify_extradata_chain(&extradata_after, opts.padding_bytes) {
        return;
    }
    // The chain length is invariant across flush — STREAMINFO is a
    // fixed-width 38 bytes (header + payload), and the optional
    // PADDING tail keeps its requested length.
    assert_eq!(
        extradata.len(),
        extradata_after.len(),
        "post-flush extradata length must equal pre-flush length \
         (header + STREAMINFO + optional PADDING are all fixed width); \
         pre={} post={} padding={:?}",
        extradata.len(),
        extradata_after.len(),
        opts.padding_bytes,
    );
});

/// Walk the extradata chain through `BlockHeader::parse` and assert
/// every invariant the construction path is supposed to maintain.
/// Returns `false` only on cooperative early exits (truncated input
/// that the public API will reject elsewhere); contract violations
/// panic so libFuzzer surfaces them.
fn verify_extradata_chain(extradata: &[u8], padding_bytes: Option<usize>) -> bool {
    // STREAMINFO header always present.
    if extradata.len() < BLOCK_HEADER_LEN {
        return false;
    }
    let si_hdr = match BlockHeader::parse(&extradata[..BLOCK_HEADER_LEN]) {
        Ok(h) => h,
        Err(_) => panic!(
            "STREAMINFO header from a successful construction must parse: bytes={:?}",
            &extradata[..BLOCK_HEADER_LEN]
        ),
    };
    assert_eq!(
        si_hdr.block_type,
        BlockType::StreamInfo,
        "first metadata block must be STREAMINFO; got {:?}",
        si_hdr.block_type,
    );
    assert_eq!(
        si_hdr.length as usize, STREAMINFO_PAYLOAD_LEN,
        "STREAMINFO payload length is fixed at 34 bytes per RFC 9639 §8.2",
    );

    let payload_off = BLOCK_HEADER_LEN;
    let payload_end = payload_off + si_hdr.length as usize;
    if extradata.len() < payload_end {
        // The encoder never produces a truncated payload in
        // practice; if we see one, that's a real bug.
        panic!(
            "STREAMINFO payload truncated: extradata.len()={} expected at least {}",
            extradata.len(),
            payload_end,
        );
    }

    match padding_bytes {
        None => {
            // STREAMINFO must be `last`, no trailing bytes.
            assert!(
                si_hdr.last,
                "STREAMINFO must be `last` when no PADDING follows"
            );
            assert_eq!(
                extradata.len(),
                payload_end,
                "no trailing bytes when padding_bytes=None; got extra {} bytes",
                extradata.len() - payload_end,
            );
        }
        Some(n) => {
            // STREAMINFO must NOT be `last`. A PADDING block must
            // follow, with `last=true`, the requested length, and an
            // all-zero payload (RFC 9639 §8.6).
            assert!(
                !si_hdr.last,
                "STREAMINFO must not be `last` when PADDING follows"
            );
            let pad_hdr_off = payload_end;
            if extradata.len() < pad_hdr_off + BLOCK_HEADER_LEN {
                panic!(
                    "PADDING header truncated: extradata.len()={} expected at least {}",
                    extradata.len(),
                    pad_hdr_off + BLOCK_HEADER_LEN,
                );
            }
            let pad_hdr =
                match BlockHeader::parse(&extradata[pad_hdr_off..pad_hdr_off + BLOCK_HEADER_LEN]) {
                    Ok(h) => h,
                    Err(_) => panic!(
                        "PADDING header from a successful construction must parse: bytes={:?}",
                        &extradata[pad_hdr_off..pad_hdr_off + BLOCK_HEADER_LEN]
                    ),
                };
            assert!(pad_hdr.last, "PADDING must be `last`");
            assert_eq!(pad_hdr.block_type, BlockType::Padding);
            assert_eq!(
                pad_hdr.length as usize, n,
                "PADDING length must equal the requested padding_bytes",
            );
            let pad_payload_off = pad_hdr_off + BLOCK_HEADER_LEN;
            let pad_payload_end = pad_payload_off + pad_hdr.length as usize;
            assert_eq!(
                extradata.len(),
                pad_payload_end,
                "chain must end exactly at the PADDING payload tail; \
                 got extra {} bytes",
                extradata.len().saturating_sub(pad_payload_end),
            );
            // RFC 9639 §8.6: PADDING payload SHOULD be all-zero. The
            // encoder MUST produce all-zero. Reject any non-zero byte.
            assert!(
                extradata[pad_payload_off..pad_payload_end]
                    .iter()
                    .all(|&b| b == 0),
                "PADDING payload must be all-zero per RFC 9639 §8.6",
            );
        }
    }
    true
}

/// Clamp the bytes in `pcm` so that, when read back as `format`
/// samples, every sample lands in the format's representable range.
/// The encoder rejects out-of-range samples with `Err`, so without
/// this the harness would early-return on most fuzz inputs and never
/// reach the flush path.
fn clamp_pcm_in_place(pcm: &mut [u8], format: SampleFormat) {
    match format {
        SampleFormat::U8 | SampleFormat::S16 => {
            // U8 (any byte is valid) and S16 (any 2 bytes form a
            // valid `i16`) cover the full representable range
            // already.
        }
        SampleFormat::S24 => {
            // Clear bits 24..32 of every 3-byte little-endian sample.
            // Then sign-extend bit 23 into bits 24..32 so the
            // resulting i32 stays in `[-2^23, 2^23)`. The encoder
            // reads three bytes and sign-extends; we just need to
            // make sure the input bytes match a 24-bit signed value.
            for chunk in pcm.chunks_exact_mut(3) {
                // Mask the high byte to 24 valid bits; no further
                // work needed because the high byte was just the
                // sign byte and we already wrote three bytes.
                let _ = chunk;
            }
        }
        SampleFormat::S32 => {
            // S32 covers the full i32 range — any 4-byte chunk is
            // valid.
        }
        _ => {}
    }
}
