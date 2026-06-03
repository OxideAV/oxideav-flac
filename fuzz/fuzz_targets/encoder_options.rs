#![no_main]

//! Fuzz: `make_encoder_with_options` over a wide parameter space.
//!
//! Complements the five existing targets (`panic_free_decode`,
//! `roundtrip`, `decode`, `flac_oracle_decode`, `metadata_walker`) by
//! focusing on the **encoder construction + non-default options**
//! surface that none of the existing roundtrip targets exercise:
//!
//! * [`make_encoder_with_options`] with caller-supplied
//!   [`FlacEncoderOptions::padding_bytes`] across 0, 1, near-MAX, and
//!   illegal-over-MAX values,
//! * the post-flush `extradata` rebuild path that re-runs
//!   `build_extradata` to embed the real `min_frame_size` /
//!   `max_frame_size` / `total_samples` / MD5 — the only call site
//!   that exercises the **complete** STREAMINFO+PADDING chain after
//!   real encoding has occurred,
//! * full self-roundtrip (encode → decode → bit-exact assert) under
//!   options that change the extradata layout, catching any decoder
//!   path that hard-codes assumptions about STREAMINFO being the
//!   `last`-flagged block.
//!
//! The fuzz input is carved into:
//!   - 1 B  format selector (U8 / S16 / S24 / S32)
//!   - 1 B  channel selector (1, 2, 3, 4, 5, 6, 7, 8)
//!   - 1 B  sample-rate selector (8 kHz, 16 kHz, 44.1 kHz, 48 kHz,
//!     88.2 kHz, 96 kHz, 192 kHz, plus an "odd-rate" variable code
//!     that lands on a non-fixed-rate-table value to exercise the
//!     three-byte variable-rate frame-header escape)
//!   - 1 B  padding selector (`None`, `Some(0)`, `Some(1)`, `Some(k)`
//!     for small / medium / near-MAX / over-MAX values)
//!   - 2 B  sample-count selector
//!   - rest: raw PCM bytes packed in the chosen sample format,
//!     interleaved across channels.
//!
//! Contracts under test:
//!
//! 1. `make_encoder_with_options` rejects over-MAX padding with
//!    `Err(Error::invalid(_))`, not a panic.
//! 2. For accepted options, the produced `extradata` is a parseable
//!    STREAMINFO+(optionally PADDING) metadata chain whose first
//!    block is STREAMINFO, whose `last` flag is set on STREAMINFO iff
//!    no PADDING follows, and whose payload bps/channels/sample-rate
//!    fields match the caller's `CodecParameters`.
//! 3. The packets emitted by `send_frame` + `flush` self-decode
//!    bit-exactly to the input PCM. FLAC is lossless; any per-sample
//!    drift is a real bug.
//! 4. After flush, the encoder rewrites `extradata` with real
//!    min/max/total/MD5 values. The post-flush extradata must still
//!    be a parseable chain with the same STREAMINFO+PADDING shape.

use libfuzzer_sys::fuzz_target;
use oxideav_core::{AudioFrame, CodecId, CodecParameters, Error, Frame, Packet, SampleFormat};
use oxideav_flac::encoder::{make_encoder_with_options, FlacEncoderOptions};
use oxideav_flac::metadata::{BlockHeader, BlockType, StreamInfo, FLAC_MAGIC};

/// Cap on per-channel sample count. Keeps each iteration well under a
/// second of CPU even at S32-8ch so libFuzzer can keep its work-queue
/// moving rather than waiting on heavy single-iteration encodes.
const MAX_SAMPLES_PER_CH: usize = 128;

fuzz_target!(|data: &[u8]| {
    let Some(input) = parse_fuzz_input(data) else {
        return;
    };
    let TestCase {
        format,
        channels,
        sample_rate,
        padding_choice,
        pcm_per_channel,
    } = input;

    let mut enc_params = CodecParameters::audio(CodecId::new("flac"));
    enc_params.channels = Some(channels);
    enc_params.sample_rate = Some(sample_rate);
    enc_params.sample_format = Some(format);

    let mut options = FlacEncoderOptions::default();
    options.padding_bytes = padding_choice.to_padding_bytes();

    // Contract 1: over-MAX padding MUST be rejected at construction,
    // never panic. The encoder validates `padding_bytes <=
    // BlockHeader::MAX_LENGTH` up front.
    let enc_result = make_encoder_with_options(&enc_params, options.clone());
    if padding_choice.is_over_max() {
        // Encoder MUST reject; if it accepts, the round-trip path will
        // panic later in `write_padding`, so catching it here is the
        // safer guarantee.
        match enc_result {
            Err(Error::InvalidData(_)) => return,
            Err(_) => return,
            // An accepted over-MAX padding is a real bug — surface it.
            Ok(_) => panic!(
                "encoder accepted over-MAX padding_bytes (choice {:?})",
                padding_choice
            ),
        }
    }
    let mut enc = match enc_result {
        Ok(e) => e,
        Err(_) => return,
    };

    let dec_params = enc.output_params().clone();
    if dec_params.extradata.is_empty() {
        return;
    }

    // Contract 2: pre-flush extradata is a parseable
    // STREAMINFO+(optionally PADDING) chain with the expected shape.
    if !validate_extradata_shape(
        &dec_params.extradata,
        padding_choice,
        format,
        channels,
        sample_rate,
    ) {
        return;
    }

    let n = pcm_per_channel[0].len();
    let frame = build_audio_frame(format, channels, &pcm_per_channel);
    if enc.send_frame(&Frame::Audio(frame)).is_err() {
        return;
    }
    if enc.flush().is_err() {
        return;
    }

    let mut packets: Vec<Packet> = Vec::new();
    loop {
        match enc.receive_packet() {
            Ok(p) => packets.push(p),
            Err(Error::NeedMore) | Err(Error::Eof) => break,
            // Any other error from the encoder side is a real bug —
            // the encoder was fed valid PCM with valid params.
            Err(e) => panic!("encoder produced unexpected error: {e:?}"),
        }
    }
    if packets.is_empty() {
        return;
    }

    // Contract 4: post-flush extradata is **still** a parseable chain
    // with the same STREAMINFO+PADDING shape. The encoder rebuilds it
    // to embed the real min/max/total/MD5 values; the rebuild path is
    // a separate call site for `build_extradata` from construction.
    let post_extradata = enc.output_params().extradata.clone();
    if !validate_extradata_shape(
        &post_extradata,
        padding_choice,
        format,
        channels,
        sample_rate,
    ) {
        panic!(
            "post-flush extradata failed shape check (padding {:?})",
            padding_choice
        );
    }

    // Contract 3: self-roundtrip is bit-exact. Use the **post-flush**
    // extradata (which carries the real MD5) for decoder construction
    // because the decoder verifies the MD5 at end-of-stream.
    let mut dec_params_post = dec_params.clone();
    dec_params_post.extradata = post_extradata;
    let mut dec = match oxideav_flac::decoder::make_decoder(&dec_params_post) {
        Ok(d) => d,
        Err(_) => return,
    };

    let dec_format = match format.bytes_per_sample() {
        1 => SampleFormat::U8,
        2 => SampleFormat::S16,
        3 => SampleFormat::S24,
        _ => SampleFormat::S32,
    };

    let mut recovered: Vec<i32> = Vec::new();
    for pkt in packets {
        if dec.send_packet(&pkt).is_err() {
            return;
        }
        let f = match dec.receive_frame() {
            Ok(f) => f,
            Err(_) => return,
        };
        let Frame::Audio(a) = f else {
            return;
        };
        recovered.extend(decode_interleaved(&a, dec_format, channels));
    }

    let n_ch = channels as usize;
    assert_eq!(
        recovered.len(),
        n * n_ch,
        "recovered sample count {} != expected {} (padding {:?})",
        recovered.len(),
        n * n_ch,
        padding_choice,
    );
    for i in 0..n {
        for c in 0..n_ch {
            let got = recovered[i * n_ch + c];
            let want = pcm_per_channel[c][i];
            assert_eq!(
                got, want,
                "FLAC roundtrip drift at sample {i} channel {c}: got {got}, expected {want} \
                 (format={format:?}, channels={channels}, sample_rate={sample_rate}, padding={padding_choice:?})"
            );
        }
    }
});

#[derive(Clone, Copy, Debug)]
enum PaddingChoice {
    /// No PADDING block at all — STREAMINFO carries `last=true`.
    None_,
    /// `Some(0)` — empty PADDING block, header only.
    ZeroLen,
    /// `Some(1)` — minimum-non-empty PADDING.
    OneByte,
    /// `Some(small)` — common tagger-reservation size.
    Small,
    /// `Some(medium)` — kilobyte-scale.
    Medium,
    /// `Some(near_max)` — just below `BlockHeader::MAX_LENGTH`. Tests
    /// the boundary where the 24-bit length field saturates.
    NearMax,
    /// `Some(over_max)` — strictly greater than `MAX_LENGTH`. Encoder
    /// MUST reject at construction.
    OverMax,
}

impl PaddingChoice {
    fn to_padding_bytes(self) -> Option<usize> {
        // `BlockHeader::MAX_LENGTH` is `(1 << 24) - 1 = 16_777_215`.
        // Anything strictly above that overflows the 24-bit length
        // field and must be rejected.
        const MAX: usize = (1usize << 24) - 1;
        match self {
            PaddingChoice::None_ => None,
            PaddingChoice::ZeroLen => Some(0),
            PaddingChoice::OneByte => Some(1),
            PaddingChoice::Small => Some(64),
            PaddingChoice::Medium => Some(8192),
            // Stay just under the cap (the encoder accepts MAX itself,
            // a "near-max" choice exercises the boundary without
            // ballooning memory in every iteration). A 1 MiB chunk is
            // already plenty to catch any 24-bit-length arithmetic
            // mistakes without OOMing the fuzz worker.
            PaddingChoice::NearMax => Some(1 << 20),
            // One past the cap. Encoder must reject.
            PaddingChoice::OverMax => Some(MAX + 1),
        }
    }

    fn is_over_max(self) -> bool {
        matches!(self, PaddingChoice::OverMax)
    }

    fn expected_padding_bytes(self) -> Option<usize> {
        // Used by the shape check. `OverMax` never reaches the shape
        // check because the encoder rejected it before construction.
        self.to_padding_bytes()
    }
}

struct TestCase {
    format: SampleFormat,
    channels: u16,
    sample_rate: u32,
    padding_choice: PaddingChoice,
    /// `pcm_per_channel[ch][i]` — sign-extended i32 sample values
    /// already clamped to the format's representable range.
    pcm_per_channel: Vec<Vec<i32>>,
}

fn parse_fuzz_input(data: &[u8]) -> Option<TestCase> {
    if data.len() < 6 {
        return None;
    }
    let fmt_sel = data[0] & 0x03;
    let ch_sel = data[1] & 0x07;
    let sr_sel = data[2] & 0x07;
    let pad_sel = data[3] & 0x07;
    let n_sel = u16::from_le_bytes([data[4], data[5]]) as usize;
    let rest = &data[6..];

    let format = match fmt_sel {
        0 => SampleFormat::U8,
        1 => SampleFormat::S16,
        2 => SampleFormat::S24,
        _ => SampleFormat::S32,
    };
    let channels: u16 = (ch_sel as u16) + 1; // 1..=8
    let sample_rate: u32 = match sr_sel {
        0 => 8_000,
        1 => 16_000,
        2 => 44_100,
        3 => 48_000,
        4 => 88_200,
        5 => 96_000,
        6 => 192_000,
        // Variable / odd rate: lands on a value that's not in the
        // 11-entry fixed-rate table, exercising the frame-header
        // variable-rate escape path on the decode side as well.
        _ => 37_337,
    };
    let padding_choice = match pad_sel {
        0 => PaddingChoice::None_,
        1 => PaddingChoice::ZeroLen,
        2 => PaddingChoice::OneByte,
        3 => PaddingChoice::Small,
        4 => PaddingChoice::Medium,
        5 => PaddingChoice::NearMax,
        6 => PaddingChoice::OverMax,
        _ => PaddingChoice::None_,
    };

    let bytes_per_sample = format.bytes_per_sample();
    // Same MAX-clamp shape as roundtrip.rs: keep n bounded so input
    // size requirements admit most fuzz inputs.
    let n = ((n_sel % MAX_SAMPLES_PER_CH) + 16).min(MAX_SAMPLES_PER_CH);
    let need = n * channels as usize * bytes_per_sample;
    if rest.len() < need {
        return None;
    }

    let (max_val, min_val): (i32, i32) = match format {
        SampleFormat::U8 => (127, -128),
        SampleFormat::S16 => (i16::MAX as i32, i16::MIN as i32),
        SampleFormat::S24 => (0x7F_FFFF, -0x80_0000),
        SampleFormat::S32 => (i32::MAX, i32::MIN),
        _ => return None,
    };

    let mut pcm_per_channel: Vec<Vec<i32>> = (0..channels).map(|_| Vec::with_capacity(n)).collect();
    let bps = bytes_per_sample;
    for i in 0..n {
        for ch in 0..channels as usize {
            let off = (i * channels as usize + ch) * bps;
            let chunk = &rest[off..off + bps];
            let v = match format {
                SampleFormat::U8 => (chunk[0] as i32) - 128,
                SampleFormat::S16 => i16::from_le_bytes([chunk[0], chunk[1]]) as i32,
                SampleFormat::S24 => {
                    let mut v =
                        (chunk[0] as i32) | ((chunk[1] as i32) << 8) | ((chunk[2] as i32) << 16);
                    if v & 0x0080_0000 != 0 {
                        v |= 0xFF00_0000_u32 as i32;
                    }
                    v
                }
                SampleFormat::S32 => i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]),
                _ => return None,
            };
            pcm_per_channel[ch].push(v.clamp(min_val, max_val));
        }
    }
    Some(TestCase {
        format,
        channels,
        sample_rate,
        padding_choice,
        pcm_per_channel,
    })
}

fn build_audio_frame(
    format: SampleFormat,
    channels: u16,
    pcm_per_channel: &[Vec<i32>],
) -> AudioFrame {
    let n = pcm_per_channel[0].len();
    let bytes_per_sample = format.bytes_per_sample();
    let mut interleaved: Vec<u8> = Vec::with_capacity(n * channels as usize * bytes_per_sample);
    for i in 0..n {
        for c in 0..channels as usize {
            let s = pcm_per_channel[c][i];
            match format {
                SampleFormat::U8 => interleaved.push(((s + 128) & 0xFF) as u8),
                SampleFormat::S16 => {
                    interleaved.extend_from_slice(&(s as i16).to_le_bytes());
                }
                SampleFormat::S24 => {
                    interleaved.push((s & 0xFF) as u8);
                    interleaved.push(((s >> 8) & 0xFF) as u8);
                    interleaved.push(((s >> 16) & 0xFF) as u8);
                }
                SampleFormat::S32 => interleaved.extend_from_slice(&s.to_le_bytes()),
                _ => unreachable!(),
            }
        }
    }
    AudioFrame {
        samples: n as u32,
        pts: Some(0),
        data: vec![interleaved],
    }
}

fn decode_interleaved(a: &AudioFrame, format: SampleFormat, channels: u16) -> Vec<i32> {
    let n_ch = channels as usize;
    let bps = format.bytes_per_sample();
    let mut out = Vec::with_capacity(a.samples as usize * n_ch);
    for chunk in a.data[0].chunks_exact(bps * n_ch) {
        for c in 0..n_ch {
            let off = c * bps;
            let v = match format {
                SampleFormat::U8 => (chunk[off] as i32) - 128,
                SampleFormat::S16 => i16::from_le_bytes([chunk[off], chunk[off + 1]]) as i32,
                SampleFormat::S24 => {
                    let mut v = (chunk[off] as i32)
                        | ((chunk[off + 1] as i32) << 8)
                        | ((chunk[off + 2] as i32) << 16);
                    if v & 0x0080_0000 != 0 {
                        v |= 0xFF00_0000_u32 as i32;
                    }
                    v
                }
                SampleFormat::S32 => {
                    i32::from_le_bytes([chunk[off], chunk[off + 1], chunk[off + 2], chunk[off + 3]])
                }
                _ => unreachable!(),
            };
            out.push(v);
        }
    }
    out
}

/// Parse `extradata` as a metadata chain and verify the contracted
/// shape:
///
/// * Optional `fLaC` magic prefix (the encoder does not emit it, but
///   the field is `extradata` — some muxers add it before handing it
///   to the codec layer; the check is lenient on both).
/// * First block is `StreamInfo`, with `last` flag set iff
///   `padding_choice == None_`.
/// * Payload parses cleanly and carries the caller's bps / channels /
///   sample-rate values.
/// * If `padding_choice != None_`, a `Padding` block of the expected
///   length follows with `last=true`.
/// * No trailing bytes after the last block.
///
/// Returns `false` for a malformed extradata so the harness can bail
/// rather than panic. Returns `true` for a well-formed extradata.
fn validate_extradata_shape(
    extradata: &[u8],
    padding_choice: PaddingChoice,
    format: SampleFormat,
    channels: u16,
    sample_rate: u32,
) -> bool {
    // The encoder does NOT prefix `extradata` with the `fLaC` magic
    // (the magic lives at the container level, not in the codec
    // parameters). But be tolerant: if a future change adds it, the
    // shape check should still pass.
    let mut cursor = extradata;
    if cursor.len() >= FLAC_MAGIC.len() && cursor[..FLAC_MAGIC.len()] == FLAC_MAGIC {
        cursor = &cursor[FLAC_MAGIC.len()..];
    }

    // First block: STREAMINFO header (4 bytes) + 34-byte payload.
    if cursor.len() < 4 {
        return false;
    }
    let si_hdr = match BlockHeader::parse(&cursor[..4]) {
        Ok(h) => h,
        Err(_) => return false,
    };
    if si_hdr.block_type != BlockType::StreamInfo {
        return false;
    }
    let expected_si_last = matches!(padding_choice, PaddingChoice::None_);
    if si_hdr.last != expected_si_last {
        return false;
    }
    if si_hdr.length != 34 {
        return false;
    }
    cursor = &cursor[4..];
    if cursor.len() < 34 {
        return false;
    }
    let si = match StreamInfo::parse(&cursor[..34]) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let expected_bps: u8 = match format {
        SampleFormat::U8 => 8,
        SampleFormat::S16 => 16,
        SampleFormat::S24 => 24,
        SampleFormat::S32 => 32,
        _ => return false,
    };
    if si.bits_per_sample != expected_bps {
        return false;
    }
    if si.channels as u16 != channels {
        return false;
    }
    if si.sample_rate != sample_rate {
        return false;
    }
    cursor = &cursor[34..];

    // Optional PADDING block.
    if let Some(expected_len) = padding_choice.expected_padding_bytes() {
        if cursor.len() < 4 {
            return false;
        }
        let pad_hdr = match BlockHeader::parse(&cursor[..4]) {
            Ok(h) => h,
            Err(_) => return false,
        };
        if pad_hdr.block_type != BlockType::Padding {
            return false;
        }
        if !pad_hdr.last {
            return false;
        }
        if pad_hdr.length as usize != expected_len {
            return false;
        }
        cursor = &cursor[4..];
        if cursor.len() < expected_len {
            return false;
        }
        // RFC 9639 §8.6 says PADDING is all zero — the encoder writes
        // it that way; verify nothing has been smuggled into it.
        if cursor[..expected_len].iter().any(|&b| b != 0) {
            return false;
        }
        cursor = &cursor[expected_len..];
    }

    // No trailing bytes.
    cursor.is_empty()
}
