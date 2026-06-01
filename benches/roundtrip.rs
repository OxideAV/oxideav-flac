// Parallel-array index loops are idiomatic in codec / bench code; skip
// the lint (mirrors the crate root's `#![allow(clippy::needless_range_loop)]`).
#![allow(clippy::needless_range_loop)]

//! Criterion benchmarks for the FLAC encode + decode roundtrip — the
//! realistic "encode a clip, decode every sample back" path.
//!
//! Round 128 (depth-mode benchmarks): the decoder and encoder land in
//! lockstep against each other (FLAC is lossless, RFC 9639 §11
//! requires the decoded PCM to match the input bit-exactly; the
//! crate's `tests/roundtrip.rs` enforces that, this bench measures
//! it). Each iteration also asserts the recovered byte count matches
//! the input so a state-machine drift would surface as a `panic!` in
//! the bench output rather than silent miscompression.
//!
//! Scenarios:
//!
//!   - **roundtrip_mono_s16_44k1_1s**: single channel baseline.
//!   - **roundtrip_stereo_s16_44k1_1s**: stereo baseline — adds the
//!     four-way decorrelation search on encode + the inverse step on
//!     decode.
//!   - **roundtrip_stereo_s24_48k_500ms**: highest-bps stereo mode
//!     (24-bit), wider residuals through Rice + LPC.
//!   - **roundtrip_6ch_s16_48k_250ms**: max channel count (6),
//!     independent subframe per channel.
//!   - **roundtrip_mono_s24_48k_500ms** *(r207)*: wide-sample mono path
//!     end-to-end. Pairs with `encode_mono_s24_48k_500ms` /
//!     `decode_mono_s24_48k_500ms` so the wide-sample single-channel
//!     pipeline can be A/B-ed against the combined encode+decode shape.
//!   - **roundtrip_mono_noise_s16_44k1_1s** *(r207)*: uniform-noise
//!     content that keeps the per-partition Rice parameter `k` wide
//!     and occasionally trips the escape-partition raw-bits branch
//!     (RFC 9639 §9.2.7). Round-trip variant of the matching encoder /
//!     decoder noise benches; the bench-body invariant
//!     (recovered-bytes == input) doubles as a regression guard that
//!     the escape branch never silently drops or duplicates samples.
//!   - **roundtrip_stereo_s32_96k_250ms** *(r207)*: full-width 32-bit
//!     pipeline at 96 kHz. Exercises the wider residual path through
//!     both directions (32-bit encoder subframe coding + the matching
//!     decoder inverse + S32 little-endian de-interleave) that the
//!     existing S16/S24 scenarios never reach.
//!
//! Run with:
//!     cargo bench -p oxideav-flac --bench roundtrip

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use oxideav_core::{AudioFrame, CodecId, CodecParameters, Error, Frame, Packet, SampleFormat};

fn xorshift32(state: &mut u32) -> u32 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    *state
}

fn build_pcm_per_channel(n_samples: usize, channels: u16, bits_per_sample: u16) -> Vec<Vec<i32>> {
    let nch = channels as usize;
    let mut out: Vec<Vec<i32>> = (0..nch).map(|_| Vec::with_capacity(n_samples)).collect();
    let mut state: u32 = 0xCAFE_F00D;
    let amp = if bits_per_sample <= 16 {
        1 << 13 // ~25% of 16-bit range
    } else if bits_per_sample <= 24 {
        1 << 21 // ~25% of 24-bit range
    } else {
        1 << 29 // ~25% of 32-bit range — r207 stereo_s32 scenario
    };
    for s in 0..n_samples {
        let phase = (s % 256) as i32 - 128;
        let env = (phase * amp) / 128;
        for ch in 0..nch {
            let noise = (xorshift32(&mut state) as i32) >> 24;
            let chan_bias = (ch as i32) * (amp / 16);
            out[ch].push(env + chan_bias + noise);
        }
    }
    out
}

/// **Round 207 addition** — uniform-noise PCM with a slow triangular
/// ramp so FIXED order-1/2 stays a clear win over VERBATIM (otherwise
/// the encoder short-circuits to VERBATIM for pure noise and the
/// residual reader / writer never run). Mirrors the matching helper in
/// `benches/decode.rs` so the roundtrip noise scenario uses the same
/// shape the decoder bench's `bench_decode_mono_noise_s16_44k1_1s`
/// exercises.
fn build_noise_pcm_per_channel(
    n_samples: usize,
    channels: u16,
    bits_per_sample: u16,
) -> Vec<Vec<i32>> {
    let nch = channels as usize;
    let mut out: Vec<Vec<i32>> = (0..nch).map(|_| Vec::with_capacity(n_samples)).collect();
    let mut state: u32 = 0xDEAD_BEEF;
    let (ramp_amp, noise_amp) = if bits_per_sample <= 16 {
        (1i32 << 12, 1i32 << 13)
    } else if bits_per_sample <= 24 {
        (1i32 << 20, 1i32 << 21)
    } else {
        (1i32 << 28, 1i32 << 29)
    };
    for s in 0..n_samples {
        let phase = ((s as i32) % 1024) - 512;
        let env = phase * ramp_amp / 512;
        for ch in 0..nch {
            let raw = xorshift32(&mut state) as i32;
            let noise = ((raw as i64) * (noise_amp as i64) / (i32::MAX as i64)) as i32;
            let chan_bias = (ch as i32) * (ramp_amp / 16);
            out[ch].push(env + chan_bias + noise);
        }
    }
    out
}

fn build_audio_frame(
    format: SampleFormat,
    channels: u16,
    pcm_per_channel: &[Vec<i32>],
) -> AudioFrame {
    assert_eq!(pcm_per_channel.len(), channels as usize);
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
                _ => panic!("unsupported format for bench: {:?}", format),
            }
        }
    }
    AudioFrame {
        samples: n as u32,
        pts: Some(0),
        data: vec![interleaved],
    }
}

fn roundtrip_once(
    format: SampleFormat,
    channels: u16,
    sample_rate: u32,
    pcm_per_channel: &[Vec<i32>],
    expected_input_bytes: usize,
) {
    let mut params = CodecParameters::audio(CodecId::new("flac"));
    params.channels = Some(channels);
    params.sample_rate = Some(sample_rate);
    params.sample_format = Some(format);
    let mut enc = oxideav_flac::encoder::make_encoder(&params).expect("make_encoder");

    let frame = build_audio_frame(format, channels, pcm_per_channel);
    enc.send_frame(&Frame::Audio(frame)).expect("send_frame");
    enc.flush().expect("flush");

    let mut packets: Vec<Packet> = Vec::new();
    loop {
        match enc.receive_packet() {
            Ok(p) => packets.push(p),
            Err(Error::NeedMore) | Err(Error::Eof) => break,
            Err(e) => panic!("unexpected encoder error: {:?}", e),
        }
    }
    let dec_params = enc.output_params().clone();

    let mut dec = oxideav_flac::decoder::make_decoder(&dec_params).expect("make_decoder");
    let mut total = 0usize;
    for pkt in &packets {
        dec.send_packet(pkt).expect("send_packet");
        let f = dec.receive_frame().expect("receive_frame");
        if let Frame::Audio(a) = f {
            total += a.data[0].len();
        }
    }
    assert_eq!(
        total, expected_input_bytes,
        "roundtrip recovered byte count must match the input"
    );
}

fn bench_roundtrip_mono_s16_44k1_1s(c: &mut Criterion) {
    let n = 44_100;
    let pcm = build_pcm_per_channel(n, 1, 16);
    let expected = n * 2; // mono s16
    let mut g = c.benchmark_group("roundtrip_mono_s16_44k1_1s");
    g.throughput(Throughput::Bytes(expected as u64));
    g.sample_size(10);
    g.bench_function(BenchmarkId::from_parameter("mono/s16/44k1/1s"), |b| {
        b.iter(|| {
            roundtrip_once(
                SampleFormat::S16,
                1,
                44_100,
                criterion::black_box(&pcm),
                expected,
            );
        });
    });
    g.finish();
}

fn bench_roundtrip_stereo_s16_44k1_1s(c: &mut Criterion) {
    let n = 44_100;
    let pcm = build_pcm_per_channel(n, 2, 16);
    let expected = n * 2 * 2; // stereo s16
    let mut g = c.benchmark_group("roundtrip_stereo_s16_44k1_1s");
    g.throughput(Throughput::Bytes(expected as u64));
    g.sample_size(10);
    g.bench_function(BenchmarkId::from_parameter("stereo/s16/44k1/1s"), |b| {
        b.iter(|| {
            roundtrip_once(
                SampleFormat::S16,
                2,
                44_100,
                criterion::black_box(&pcm),
                expected,
            );
        });
    });
    g.finish();
}

fn bench_roundtrip_stereo_s24_48k_500ms(c: &mut Criterion) {
    let n = 24_000;
    let pcm = build_pcm_per_channel(n, 2, 24);
    let expected = n * 3 * 2; // stereo s24 (3 bytes/sample)
    let mut g = c.benchmark_group("roundtrip_stereo_s24_48k_500ms");
    g.throughput(Throughput::Bytes(expected as u64));
    g.sample_size(10);
    g.bench_function(BenchmarkId::from_parameter("stereo/s24/48k/500ms"), |b| {
        b.iter(|| {
            roundtrip_once(
                SampleFormat::S24,
                2,
                48_000,
                criterion::black_box(&pcm),
                expected,
            );
        });
    });
    g.finish();
}

fn bench_roundtrip_6ch_s16_48k_250ms(c: &mut Criterion) {
    let n = 12_000;
    let pcm = build_pcm_per_channel(n, 6, 16);
    let expected = n * 2 * 6; // 6ch s16
    let mut g = c.benchmark_group("roundtrip_6ch_s16_48k_250ms");
    g.throughput(Throughput::Bytes(expected as u64));
    g.sample_size(10);
    g.bench_function(BenchmarkId::from_parameter("6ch/s16/48k/250ms"), |b| {
        b.iter(|| {
            roundtrip_once(
                SampleFormat::S16,
                6,
                48_000,
                criterion::black_box(&pcm),
                expected,
            );
        });
    });
    g.finish();
}

/// **Round 207 addition** — wide-sample mono roundtrip. Pairs with
/// `encode_mono_s24_48k_500ms` (added r150) and
/// `decode_mono_s24_48k_500ms` (added r195) so the wide-sample single-
/// channel pipeline can be A/B-ed end-to-end without the stereo
/// decorrelation search confounding the delta.
fn bench_roundtrip_mono_s24_48k_500ms(c: &mut Criterion) {
    let n = 24_000; // 0.5 s @ 48 kHz
    let pcm = build_pcm_per_channel(n, 1, 24);
    let expected = n * 3; // mono s24 (3 bytes/sample)
    let mut g = c.benchmark_group("roundtrip_mono_s24_48k_500ms");
    g.throughput(Throughput::Bytes(expected as u64));
    g.sample_size(10);
    g.bench_function(BenchmarkId::from_parameter("mono/s24/48k/500ms"), |b| {
        b.iter(|| {
            roundtrip_once(
                SampleFormat::S24,
                1,
                48_000,
                criterion::black_box(&pcm),
                expected,
            );
        });
    });
    g.finish();
}

/// **Round 207 addition** — uniform-noise roundtrip. With near-zero
/// linear predictability the encoder's residuals stay wide, which
/// pushes the per-partition Rice parameter `k` up toward (and
/// occasionally past) the escape marker (RFC 9639 §9.2.7). Round-trip
/// shape of the matching encoder bench scenarios — the body invariant
/// (`recovered_bytes == input`) doubles as a regression guard that the
/// escape branch never silently drops or duplicates samples in either
/// direction.
fn bench_roundtrip_mono_noise_s16_44k1_1s(c: &mut Criterion) {
    let n = 44_100;
    let pcm = build_noise_pcm_per_channel(n, 1, 16);
    let expected = n * 2; // mono s16
    let mut g = c.benchmark_group("roundtrip_mono_noise_s16_44k1_1s");
    g.throughput(Throughput::Bytes(expected as u64));
    g.sample_size(10);
    g.bench_function(BenchmarkId::from_parameter("mono/noise/s16/44k1/1s"), |b| {
        b.iter(|| {
            roundtrip_once(
                SampleFormat::S16,
                1,
                44_100,
                criterion::black_box(&pcm),
                expected,
            );
        });
    });
    g.finish();
}

/// **Round 207 addition** — full-width 32-bit roundtrip. The existing
/// scenarios cap at S24; this one exercises the 32-bit branch of the
/// residual writer + reader, LPC forward + inverse, and the S32 little-
/// endian interleave plus de-interleave that the S16/S24 paths never
/// reach. Pairs with `decode_stereo_s32_96k_250ms` (r195) so encoder +
/// decoder S32 numbers can be compared against the combined end-to-end
/// cost.
fn bench_roundtrip_stereo_s32_96k_250ms(c: &mut Criterion) {
    let n = 24_000; // 0.25 s @ 96 kHz
    let pcm = build_pcm_per_channel(n, 2, 32);
    let expected = n * 4 * 2; // stereo s32 (4 bytes/sample)
    let mut g = c.benchmark_group("roundtrip_stereo_s32_96k_250ms");
    g.throughput(Throughput::Bytes(expected as u64));
    g.sample_size(10);
    g.bench_function(BenchmarkId::from_parameter("stereo/s32/96k/250ms"), |b| {
        b.iter(|| {
            roundtrip_once(
                SampleFormat::S32,
                2,
                96_000,
                criterion::black_box(&pcm),
                expected,
            );
        });
    });
    g.finish();
}

criterion_group!(
    benches,
    bench_roundtrip_mono_s16_44k1_1s,
    bench_roundtrip_stereo_s16_44k1_1s,
    bench_roundtrip_stereo_s24_48k_500ms,
    bench_roundtrip_6ch_s16_48k_250ms,
    bench_roundtrip_mono_s24_48k_500ms,
    bench_roundtrip_mono_noise_s16_44k1_1s,
    bench_roundtrip_stereo_s32_96k_250ms,
);
criterion_main!(benches);
