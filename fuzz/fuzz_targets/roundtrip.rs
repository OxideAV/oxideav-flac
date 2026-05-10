#![no_main]

//! Fuzz: random PCM → oxideav-flac encode → oxideav-flac decode.
//!
//! FLAC is **lossless** — every sample MUST round-trip bit-exactly.
//! Any divergence between input PCM and decoded output is a bug.
//!
//! The fuzz input is carved into:
//!   - 1 B  format selector (U8 / S16 / S24 / S32)
//!   - 1 B  channel selector (1, 2, 4 or 8)
//!   - 1 B  sample-rate selector (8 kHz, 44.1 kHz, 48 kHz, 96 kHz)
//!   - 2 B  sample-count multiplier (1..=N small blocks)
//!   - rest: raw PCM bytes packed in the chosen sample format,
//!     interleaved across channels.
//!
//! Total input size is capped (MAX_SAMPLES) so the encoder finishes
//! every iteration in well under a second.

use libfuzzer_sys::fuzz_target;
use oxideav_core::{AudioFrame, CodecId, CodecParameters, Error, Frame, Packet, SampleFormat};

/// Cap on per-channel sample count. Total fuzz-input cap is ~4 KiB by
/// default, and S32 8-channel at MAX samples needs `4 * 8 * MAX` bytes
/// — so we keep MAX small enough that `data.len() >= need` admits most
/// inputs without wasting libFuzzer iterations.
const MAX_SAMPLES_PER_CH: usize = 128;

fuzz_target!(|data: &[u8]| {
    let Some(input) = parse_fuzz_input(data) else {
        return;
    };
    let TestCase {
        format,
        channels,
        sample_rate,
        pcm_per_channel,
    } = input;

    // Build encoder.
    let mut enc_params = CodecParameters::audio(CodecId::new("flac"));
    enc_params.channels = Some(channels);
    enc_params.sample_rate = Some(sample_rate);
    enc_params.sample_format = Some(format);
    let mut enc = match oxideav_flac::encoder::make_encoder(&enc_params) {
        Ok(e) => e,
        Err(_) => return,
    };

    let dec_params = enc.output_params().clone();
    if dec_params.extradata.is_empty() {
        return;
    }
    let mut dec = match oxideav_flac::decoder::make_decoder(&dec_params) {
        Ok(d) => d,
        Err(_) => return,
    };

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
            // the encoder is fed valid PCM with valid params and should
            // always succeed.
            Err(e) => panic!("encoder produced unexpected error: {e:?}"),
        }
    }
    if packets.is_empty() {
        return;
    }

    // FLAC decoder maps STREAMINFO bps → output SampleFormat:
    //   8       → U8
    //   9..=16  → S16
    //   17..=24 → S24
    //   25..=32 → S32
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
        "recovered sample count {} != expected {}",
        recovered.len(),
        n * n_ch,
    );
    for i in 0..n {
        for c in 0..n_ch {
            let got = recovered[i * n_ch + c];
            let want = pcm_per_channel[c][i];
            assert_eq!(
                got, want,
                "FLAC roundtrip drift at sample {i} channel {c}: got {got}, expected {want} \
                 (format={format:?}, channels={channels}, sample_rate={sample_rate})"
            );
        }
    }
});

struct TestCase {
    format: SampleFormat,
    channels: u16,
    sample_rate: u32,
    /// `pcm_per_channel[ch][i]` — sign-extended i32 sample values
    /// already clamped to the format's representable range.
    pcm_per_channel: Vec<Vec<i32>>,
}

fn parse_fuzz_input(data: &[u8]) -> Option<TestCase> {
    if data.len() < 5 {
        return None;
    }
    let fmt_sel = data[0] & 0x03;
    let ch_sel = data[1] & 0x03;
    let sr_sel = data[2] & 0x03;
    let n_sel = u16::from_le_bytes([data[3], data[4]]) as usize;
    let rest = &data[5..];

    let format = match fmt_sel {
        0 => SampleFormat::U8,
        1 => SampleFormat::S16,
        2 => SampleFormat::S24,
        _ => SampleFormat::S32,
    };
    let channels: u16 = match ch_sel {
        0 => 1,
        1 => 2,
        2 => 4,
        _ => 8,
    };
    let sample_rate: u32 = match sr_sel {
        0 => 8_000,
        1 => 44_100,
        2 => 48_000,
        _ => 96_000,
    };

    let bytes_per_sample = format.bytes_per_sample();
    // n_sel modulo MAX, but at least 16 to avoid zero-length frames.
    let n = ((n_sel % MAX_SAMPLES_PER_CH) + 16).min(MAX_SAMPLES_PER_CH);
    let need = n * channels as usize * bytes_per_sample;
    if rest.len() < need {
        return None;
    }

    // Decode the PCM byte stream into per-channel i32 vectors, applying
    // the same sign-extension our test harness uses. Samples are
    // clamped to the representable range so the encoder doesn't reject
    // out-of-range values.
    let mut pcm_per_channel: Vec<Vec<i32>> = (0..channels).map(|_| Vec::with_capacity(n)).collect();
    let bps = bytes_per_sample;
    let max_val: i32 = match format {
        SampleFormat::U8 => 127,
        SampleFormat::S16 => i16::MAX as i32,
        SampleFormat::S24 => 0x7F_FFFF,
        SampleFormat::S32 => i32::MAX,
        _ => return None,
    };
    let min_val: i32 = match format {
        SampleFormat::U8 => -128,
        SampleFormat::S16 => i16::MIN as i32,
        SampleFormat::S24 => -0x80_0000,
        SampleFormat::S32 => i32::MIN,
        _ => return None,
    };
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
