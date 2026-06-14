// Parallel-array index loops are idiomatic in codec / bench code; skip
// the lint (mirrors the crate root's `#![allow(clippy::needless_range_loop)]`).
#![allow(clippy::needless_range_loop)]

//! Criterion benchmarks for the FLAC decoder hot paths.
//!
//! Round 128 (depth-mode benchmarks): paired with `encode.rs` and
//! `roundtrip.rs`. Each scenario encodes its PCM in a setup step with
//! the production `oxideav_flac::encoder::make_encoder` trait object
//! (so the bench sees a realistic mix of CONSTANT / FIXED / LPC /
//! VERBATIM subframes plus the four stereo decorrelation layouts the
//! encoder picks from) and then iterates the decoder on the resulting
//! packet stream. The encode work is **outside** the timed region —
//! only the decoder's per-packet `send_packet -> receive_frame` cost
//! is measured.
//!
//! Scenarios:
//!
//!   - **decode_mono_s16_44k1_1s**: 1 s of synthesised mono S16 PCM
//!     at 44.1 kHz — single subframe per block, no stereo
//!     decorrelation. Exercises the CONSTANT / FIXED / LPC subframe
//!     decoders + partitioned Rice residual + sync-code walk.
//!   - **decode_stereo_s16_44k1_1s**: 1 s of stereo S16 PCM at
//!     44.1 kHz — adds the inverse channel decorrelation step on top
//!     of the mono baseline.
//!   - **decode_stereo_s24_48k_500ms**: 0.5 s of stereo S24 PCM at
//!     48 kHz — highest bps the encoder routinely emits; residuals
//!     are wider through the Rice + LPC inverse paths.
//!   - **decode_6ch_s16_48k_250ms**: 0.25 s of 6-channel S16 PCM at
//!     48 kHz — independent subframe per channel across the max
//!     channel count, no stereo decorrelation.
//!   - **decode_mono_s24_48k_500ms** *(r195)*: 0.5 s of mono S24 PCM
//!     at 48 kHz — wide-sample single-channel decode path uncoupled
//!     from the stereo decorrelation inverse. Pairs with the matching
//!     encoder bench (`encode_mono_s24_48k_500ms`) so encoder + decoder
//!     S24 numbers can be A/B-ed against the same input shape.
//!   - **decode_mono_noise_s16_44k1_1s** *(r195)*: 1 s of mono S16 PCM
//!     filled with uniform xorshift noise scaled to ~half-range. With
//!     near-zero linear predictability the encoder's residuals stay
//!     wide, which pushes the per-partition Rice parameter `k` up
//!     toward (and occasionally past) the escape marker — exercises
//!     the `read_unary -> read_u32(k)` hot path and the
//!     escape-partition raw-bits branch (RFC 9639 §9.2.7) that the
//!     tone-plus-noise scenarios rarely touch.
//!   - **decode_mono_wasted_s16_44k1_1s** *(r307)*: 1 s of mono S16 PCM
//!     whose samples all carry 4 trailing zero bits, so the encoder
//!     tags a nonzero wasted-bits count on every subframe and the
//!     decoder runs its post-predict left-shift-back loop
//!     (`subframe::decode_subframe`'s `wasted > 0` branch, RFC 9639
//!     §9.2.2) on every block — a leg the tone-plus-noise scenarios
//!     bypass because their residual noise leaves no consistent
//!     trailing zeros.
//!   - **decode_stereo_s32_96k_250ms** *(r195)*: 0.25 s of stereo S32
//!     PCM at 96 kHz — exercises the 32-bit sample-width path through
//!     the residual reader, LPC inverse, and S32 little-endian
//!     interleave output that the S16 / S24 scenarios never reach.
//!
//! Run with:
//!     cargo bench -p oxideav-flac --bench decode

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
        1 << 29 // ~25% of 32-bit range — for the r195 S32 scenario
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

/// Encode `pcm_per_channel` to a packet stream + clone the encoder's
/// finalised output_params (carries the post-flush STREAMINFO the
/// decoder needs in `extradata`). Both feed into the timed bench body.
fn prepare_flac_stream(
    format: SampleFormat,
    channels: u16,
    sample_rate: u32,
    pcm_per_channel: &[Vec<i32>],
) -> (CodecParameters, Vec<Packet>) {
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
    (dec_params, packets)
}

fn run_decode(dec_params: &CodecParameters, packets: &[Packet]) -> usize {
    let mut dec = oxideav_flac::decoder::make_decoder(dec_params).expect("make_decoder");
    let mut total = 0usize;
    for pkt in packets {
        dec.send_packet(pkt).expect("send_packet");
        let f = dec.receive_frame().expect("receive_frame");
        if let Frame::Audio(a) = f {
            total += a.data[0].len();
        }
    }
    total
}

fn bench_decode_mono_s16_44k1_1s(c: &mut Criterion) {
    let n = 44_100;
    let pcm = build_pcm_per_channel(n, 1, 16);
    let (dec_params, packets) = prepare_flac_stream(SampleFormat::S16, 1, 44_100, &pcm);
    let mut g = c.benchmark_group("decode_mono_s16_44k1_1s");
    g.throughput(Throughput::Bytes((n * 2) as u64));
    g.bench_function(BenchmarkId::from_parameter("mono/s16/44k1/1s"), |b| {
        b.iter(|| {
            let bytes = run_decode(
                criterion::black_box(&dec_params),
                criterion::black_box(&packets),
            );
            criterion::black_box(bytes);
        });
    });
    g.finish();
}

fn bench_decode_stereo_s16_44k1_1s(c: &mut Criterion) {
    let n = 44_100;
    let pcm = build_pcm_per_channel(n, 2, 16);
    let (dec_params, packets) = prepare_flac_stream(SampleFormat::S16, 2, 44_100, &pcm);
    let mut g = c.benchmark_group("decode_stereo_s16_44k1_1s");
    g.throughput(Throughput::Bytes((n * 2 * 2) as u64));
    g.bench_function(BenchmarkId::from_parameter("stereo/s16/44k1/1s"), |b| {
        b.iter(|| {
            let bytes = run_decode(
                criterion::black_box(&dec_params),
                criterion::black_box(&packets),
            );
            criterion::black_box(bytes);
        });
    });
    g.finish();
}

fn bench_decode_stereo_s24_48k_500ms(c: &mut Criterion) {
    let n = 24_000;
    let pcm = build_pcm_per_channel(n, 2, 24);
    let (dec_params, packets) = prepare_flac_stream(SampleFormat::S24, 2, 48_000, &pcm);
    let mut g = c.benchmark_group("decode_stereo_s24_48k_500ms");
    g.throughput(Throughput::Bytes((n * 3 * 2) as u64));
    g.bench_function(BenchmarkId::from_parameter("stereo/s24/48k/500ms"), |b| {
        b.iter(|| {
            let bytes = run_decode(
                criterion::black_box(&dec_params),
                criterion::black_box(&packets),
            );
            criterion::black_box(bytes);
        });
    });
    g.finish();
}

fn bench_decode_6ch_s16_48k_250ms(c: &mut Criterion) {
    let n = 12_000;
    let pcm = build_pcm_per_channel(n, 6, 16);
    let (dec_params, packets) = prepare_flac_stream(SampleFormat::S16, 6, 48_000, &pcm);
    let mut g = c.benchmark_group("decode_6ch_s16_48k_250ms");
    g.throughput(Throughput::Bytes((n * 2 * 6) as u64));
    g.bench_function(BenchmarkId::from_parameter("6ch/s16/48k/250ms"), |b| {
        b.iter(|| {
            let bytes = run_decode(
                criterion::black_box(&dec_params),
                criterion::black_box(&packets),
            );
            criterion::black_box(bytes);
        });
    });
    g.finish();
}

/// Build per-channel PCM as a slow ramp plus large-amplitude uniform
/// xorshift32 noise. The ramp keeps FIXED order-1/2 a clear win over
/// VERBATIM (otherwise the encoder would short-circuit to VERBATIM
/// for pure noise and the residual reader would never run); the loud
/// noise term keeps the FIXED residual wide, which forces the
/// per-partition Rice parameter `k` up — frequently into double
/// digits and occasionally hitting the escape marker (`k == 15` for
/// method 0 / `k == 31` for method 1, RFC 9639 §9.2.7), tripping
/// the raw-bits branch in `subframe::decode_residual` that the
/// tone-plus-small-noise scenarios bypass.
fn build_noise_pcm_per_channel(
    n_samples: usize,
    channels: u16,
    bits_per_sample: u16,
) -> Vec<Vec<i32>> {
    let nch = channels as usize;
    let mut out: Vec<Vec<i32>> = (0..nch).map(|_| Vec::with_capacity(n_samples)).collect();
    let mut state: u32 = 0xDEAD_BEEF;
    let (ramp_amp, noise_amp) = if bits_per_sample <= 16 {
        (1i32 << 12, 1i32 << 13) // 16-bit container
    } else if bits_per_sample <= 24 {
        (1i32 << 20, 1i32 << 21) // 24-bit container
    } else {
        (1i32 << 28, 1i32 << 29) // 32-bit container
    };
    for s in 0..n_samples {
        // Slow triangular ramp so FIXED order-1/2 still earns its keep
        // over VERBATIM — without it the encoder picks VERBATIM and the
        // bench would skip the residual reader entirely.
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

/// **Round 195 addition** — mono S24 decoder counterpart to the
/// encoder bench `encode_mono_s24_48k_500ms`. The existing S24 scenario
/// is stereo, which folds in the channel-decorrelation inverse; this
/// isolates the cost of a single wide-sample subframe so the encoder
/// and decoder S24 deltas can be A/B-ed against the same input shape.
fn bench_decode_mono_s24_48k_500ms(c: &mut Criterion) {
    let n = 24_000; // 0.5 s @ 48 kHz
    let pcm = build_pcm_per_channel(n, 1, 24);
    let (dec_params, packets) = prepare_flac_stream(SampleFormat::S24, 1, 48_000, &pcm);
    let mut g = c.benchmark_group("decode_mono_s24_48k_500ms");
    g.throughput(Throughput::Bytes((n * 3) as u64));
    g.bench_function(BenchmarkId::from_parameter("mono/s24/48k/500ms"), |b| {
        b.iter(|| {
            let bytes = run_decode(
                criterion::black_box(&dec_params),
                criterion::black_box(&packets),
            );
            criterion::black_box(bytes);
        });
    });
    g.finish();
}

/// **Round 195 addition** — uniform-noise scenario targeting the
/// Rice unpack hot path. With near-zero linear predictability the
/// encoder's residuals stay wide; per-partition `k` is forced up
/// toward (and occasionally past) the escape marker (RFC 9639 §9.2.7),
/// so the bench body runs the `read_unary + read_u32(k)` loop more
/// often plus the escape-partition raw-bits branch the existing
/// tone-shape scenarios rarely touch.
fn bench_decode_mono_noise_s16_44k1_1s(c: &mut Criterion) {
    let n = 44_100;
    let pcm = build_noise_pcm_per_channel(n, 1, 16);
    let (dec_params, packets) = prepare_flac_stream(SampleFormat::S16, 1, 44_100, &pcm);
    let mut g = c.benchmark_group("decode_mono_noise_s16_44k1_1s");
    g.throughput(Throughput::Bytes((n * 2) as u64));
    g.bench_function(BenchmarkId::from_parameter("mono/noise/s16/44k1/1s"), |b| {
        b.iter(|| {
            let bytes = run_decode(
                criterion::black_box(&dec_params),
                criterion::black_box(&packets),
            );
            criterion::black_box(bytes);
        });
    });
    g.finish();
}

/// Build per-channel PCM whose samples all carry `shift` trailing zero
/// bits — i.e. every value is a multiple of `1 << shift`. This forces
/// the encoder's wasted-bits detector (RFC 9639 §9.2.2) to tag a
/// nonzero count on every subframe, which in turn makes the **decoder**
/// run its post-predict left-shift-back loop (`subframe::decode_subframe`
/// `wasted > 0` branch) on every block. The decode benches that
/// round-trip ordinary tone-plus-noise PCM almost never trip that branch
/// (the xorshift noise term leaves no consistent trailing zeros), so the
/// shift-back leg was previously unmeasured.
fn build_wasted_pcm_per_channel(
    n_samples: usize,
    channels: u16,
    bits_per_sample: u16,
    shift: u32,
) -> Vec<Vec<i32>> {
    // Start from the ordinary tone-plus-noise generator, then clear the
    // low `shift` bits of every sample so each channel has exactly
    // `shift` wasted bits. Done by shifting right then left so the
    // payload still fits the container after the encoder re-shifts.
    let base = build_pcm_per_channel(n_samples, channels, bits_per_sample);
    base.into_iter()
        .map(|ch| ch.into_iter().map(|s| (s >> shift) << shift).collect())
        .collect()
}

/// **Round 307 addition** — wasted-bits decode path. Every sample in the
/// stream carries 4 trailing zero bits, so the encoder emits a nonzero
/// wasted-bits header on each subframe and the decoder runs its
/// post-predict left-shift-back loop (`subframe::decode_subframe`'s
/// `wasted > 0` branch, RFC 9639 §9.2.2) on every decoded block — a leg
/// the existing tone-plus-noise scenarios bypass because their residual
/// noise leaves no consistent trailing zeros to waste.
fn bench_decode_mono_wasted_s16_44k1_1s(c: &mut Criterion) {
    let n = 44_100;
    let pcm = build_wasted_pcm_per_channel(n, 1, 16, 4);
    let (dec_params, packets) = prepare_flac_stream(SampleFormat::S16, 1, 44_100, &pcm);
    let mut g = c.benchmark_group("decode_mono_wasted_s16_44k1_1s");
    g.throughput(Throughput::Bytes((n * 2) as u64));
    g.bench_function(
        BenchmarkId::from_parameter("mono/wasted4/s16/44k1/1s"),
        |b| {
            b.iter(|| {
                let bytes = run_decode(
                    criterion::black_box(&dec_params),
                    criterion::black_box(&packets),
                );
                criterion::black_box(bytes);
            });
        },
    );
    g.finish();
}

/// **Round 195 addition** — full-width 32-bit sample scenario. The
/// existing scenarios cap at S24; this one exercises the 32-bit
/// branch of the residual reader, LPC inverse, and the S32 little-
/// endian interleave output that the S16/S24 paths never reach.
fn bench_decode_stereo_s32_96k_250ms(c: &mut Criterion) {
    let n = 24_000; // 0.25 s @ 96 kHz
    let pcm = build_pcm_per_channel(n, 2, 32);
    let (dec_params, packets) = prepare_flac_stream(SampleFormat::S32, 2, 96_000, &pcm);
    let mut g = c.benchmark_group("decode_stereo_s32_96k_250ms");
    g.throughput(Throughput::Bytes((n * 4 * 2) as u64));
    g.bench_function(BenchmarkId::from_parameter("stereo/s32/96k/250ms"), |b| {
        b.iter(|| {
            let bytes = run_decode(
                criterion::black_box(&dec_params),
                criterion::black_box(&packets),
            );
            criterion::black_box(bytes);
        });
    });
    g.finish();
}

criterion_group!(
    benches,
    bench_decode_mono_s16_44k1_1s,
    bench_decode_stereo_s16_44k1_1s,
    bench_decode_stereo_s24_48k_500ms,
    bench_decode_6ch_s16_48k_250ms,
    bench_decode_mono_s24_48k_500ms,
    bench_decode_mono_noise_s16_44k1_1s,
    bench_decode_mono_wasted_s16_44k1_1s,
    bench_decode_stereo_s32_96k_250ms,
);
criterion_main!(benches);
