// Parallel-array index loops are idiomatic in codec / bench code; skip
// the lint (mirrors the crate root's `#![allow(clippy::needless_range_loop)]`).
#![allow(clippy::needless_range_loop)]

//! Criterion benchmarks for the FLAC encoder hot paths.
//!
//! Round 128 (depth-mode benchmarks): `oxideav-flac` reached the
//! per-codec saturation point well before this round (decoder feature-
//! complete in early rounds; encoder hardened with adaptive
//! partition-order / LPC precision / apodization searches in r127
//! follow-ups). Per the workspace "saturated -> fuzz/bench/profile"
//! memo this round wires up `criterion` benches so future encoder /
//! decoder rounds can A/B-test their algorithm tweaks. This file
//! covers the **encoder**; sibling files cover `decode` (the cold-
//! start decoder fed our own bytes) and `roundtrip` (encode + decode
//! back-to-back with a sample-count sanity check).
//!
//! The FLAC encoder is the heavier of the two — per subframe it
//! evaluates CONSTANT + FIXED 0..=4 + LPC 1..=12 + VERBATIM, searches
//! a window of LPC coefficient precisions and three apodization
//! windows, runs a full partition-order search 0..=8 for residual
//! Rice coding, and for stereo additionally tries the four channel
//! decorrelation layouts. Each scenario below makes one of those cost
//! axes (channel count, bps, block multiplicity) visible so a future
//! "skip the Hann window when …" / "early-exit LPC precision at …"
//! tweak has a baseline to A/B against.
//!
//! Each scenario synthesises a deterministic xorshift PCM buffer
//! in-bench (no `docs/` fixtures or external files are read) and
//! drives the public `oxideav_flac::encoder::make_encoder` trait
//! object through `send_frame -> flush -> receive_packet*`, the same
//! shape `tests/roundtrip.rs` uses.
//!
//! Scenarios:
//!
//!   - **encode_mono_s16_44k1_1s**: 1 s of synthesised mono S16 PCM at
//!     44.1 kHz — the "single subframe per block" baseline. No stereo
//!     decorrelation, no extra channels — most of the cost is LPC +
//!     partition search.
//!   - **encode_stereo_s16_44k1_1s**: 1 s of stereo S16 PCM at 44.1 kHz
//!     — adds the four-way channel-assignment decorrelation search
//!     (LR / LS / RS / MS) on top of the mono baseline.
//!   - **encode_stereo_s24_48k_500ms**: 0.5 s of stereo S24 PCM at
//!     48 kHz — higher bit depth means wider residuals, which is the
//!     most expensive supported sample format for partition Rice
//!     emit.
//!   - **encode_6ch_s16_48k_250ms**: 0.25 s of 6-channel S16 PCM at
//!     48 kHz — independent subframe per channel (no stereo
//!     decorrelation past 2 ch) so this exercises the LPC + partition
//!     search across the max channel count we routinely fuzz.
//!   - **encode_mono_s24_48k_500ms** *(r150)*: 0.5 s of mono S24 PCM at
//!     48 kHz — isolates the wide-sample single-channel encode cost
//!     without the stereo decorrelation search of `stereo_s24`. Lets a
//!     future S24-specific optimisation A/B the encode cost without
//!     the 2-channel scaffold confounding the delta.
//!   - **encode_mono_wasted_bits_s16_44k1_1s** *(r150)*: 1 s of mono
//!     content whose 16-bit samples carry only an 8-bit effective
//!     range (every sample's low 8 bits are zero). Exercises the
//!     wasted-bits detector + narrowed-bps subframe path that the
//!     tone-plus-noise scenarios bypass (their noise term keeps the
//!     LSB live).
//!   - **encode_mono_constant_s16_44k1_1s** *(r150)*: 1 s of mono DC
//!     content (every sample equals a fixed non-zero value).
//!     Short-circuits each subframe to CONSTANT, so the bench
//!     measures the per-block driver overhead (de-interleave, MD5
//!     feed, frame header) without the LPC + partition cost — a
//!     useful regression guard for per-frame setup work.
//!
//! Run with:
//!     cargo bench -p oxideav-flac --bench encode

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use oxideav_core::{AudioFrame, CodecId, CodecParameters, Error, Frame, SampleFormat};

/// Cheap deterministic xorshift32 — synthesises "natural-ish" per-
/// sample values for bench inputs. A pure-DC fixture would compress
/// to a CONSTANT subframe and hide the LPC / partition cost, so we
/// mix a low-frequency triangle envelope with small per-sample noise
/// scaled to roughly a quarter of the available range for the given
/// `bits_per_sample`. Same shape the tta/cinepak benches use so the
/// numbers are directly comparable across crates.
fn xorshift32(state: &mut u32) -> u32 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    *state
}

/// Build per-channel deinterleaved i32 PCM with a tone-plus-noise
/// pattern that gives LPC something to predict and Rice something to
/// compress. Returned as one Vec per channel; the caller interleaves
/// + byte-packs for the encoder's expected AudioFrame layout.
fn build_pcm_per_channel(n_samples: usize, channels: u16, bits_per_sample: u16) -> Vec<Vec<i32>> {
    let nch = channels as usize;
    let mut out: Vec<Vec<i32>> = (0..nch).map(|_| Vec::with_capacity(n_samples)).collect();
    let mut state: u32 = 0xCAFE_F00D;
    let amp = if bits_per_sample <= 16 {
        1 << 13 // ~25% of 16-bit range
    } else {
        1 << 21 // ~25% of 24-bit range
    };
    for s in 0..n_samples {
        let phase = (s % 256) as i32 - 128;
        let env = (phase * amp) / 128;
        for ch in 0..nch {
            let noise = (xorshift32(&mut state) as i32) >> 24; // -128..127
            let chan_bias = (ch as i32) * (amp / 16);
            out[ch].push(env + chan_bias + noise);
        }
    }
    out
}

/// Pack per-channel PCM into FLAC's interleaved little-endian byte
/// layout for the given `SampleFormat`. Matches the helper in
/// `tests/roundtrip.rs` (kept inline so the bench file is self-
/// contained per the cinepak/tta convention).
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

/// One bench-encode pass: build encoder, push a single frame, flush,
/// drain every packet. Returns the total emitted byte count purely so
/// the optimiser doesn't drop the loop body.
fn encode_once(
    format: SampleFormat,
    channels: u16,
    sample_rate: u32,
    pcm_per_channel: &[Vec<i32>],
) -> usize {
    let mut params = CodecParameters::audio(CodecId::new("flac"));
    params.channels = Some(channels);
    params.sample_rate = Some(sample_rate);
    params.sample_format = Some(format);
    let mut enc = oxideav_flac::encoder::make_encoder(&params).expect("make_encoder");

    let frame = build_audio_frame(format, channels, pcm_per_channel);
    enc.send_frame(&Frame::Audio(frame)).expect("send_frame");
    enc.flush().expect("flush");

    let mut total = 0usize;
    loop {
        match enc.receive_packet() {
            Ok(p) => total += p.data.len(),
            Err(Error::NeedMore) | Err(Error::Eof) => break,
            Err(e) => panic!("unexpected encoder error: {:?}", e),
        }
    }
    total
}

fn bench_encode_mono_s16_44k1_1s(c: &mut Criterion) {
    let n = 44_100;
    let pcm = build_pcm_per_channel(n, 1, 16);
    let mut g = c.benchmark_group("encode_mono_s16_44k1_1s");
    g.throughput(Throughput::Bytes((n * 2) as u64));
    g.sample_size(10);
    g.bench_function(BenchmarkId::from_parameter("mono/s16/44k1/1s"), |b| {
        b.iter(|| {
            let bytes = encode_once(SampleFormat::S16, 1, 44_100, criterion::black_box(&pcm));
            criterion::black_box(bytes);
        });
    });
    g.finish();
}

fn bench_encode_stereo_s16_44k1_1s(c: &mut Criterion) {
    let n = 44_100;
    let pcm = build_pcm_per_channel(n, 2, 16);
    let mut g = c.benchmark_group("encode_stereo_s16_44k1_1s");
    g.throughput(Throughput::Bytes((n * 2 * 2) as u64));
    g.sample_size(10);
    g.bench_function(BenchmarkId::from_parameter("stereo/s16/44k1/1s"), |b| {
        b.iter(|| {
            let bytes = encode_once(SampleFormat::S16, 2, 44_100, criterion::black_box(&pcm));
            criterion::black_box(bytes);
        });
    });
    g.finish();
}

fn bench_encode_stereo_s24_48k_500ms(c: &mut Criterion) {
    let n = 24_000; // 0.5 s @ 48 kHz
    let pcm = build_pcm_per_channel(n, 2, 24);
    let mut g = c.benchmark_group("encode_stereo_s24_48k_500ms");
    g.throughput(Throughput::Bytes((n * 3 * 2) as u64));
    g.sample_size(10);
    g.bench_function(BenchmarkId::from_parameter("stereo/s24/48k/500ms"), |b| {
        b.iter(|| {
            let bytes = encode_once(SampleFormat::S24, 2, 48_000, criterion::black_box(&pcm));
            criterion::black_box(bytes);
        });
    });
    g.finish();
}

fn bench_encode_6ch_s16_48k_250ms(c: &mut Criterion) {
    let n = 12_000; // 0.25 s @ 48 kHz
    let pcm = build_pcm_per_channel(n, 6, 16);
    let mut g = c.benchmark_group("encode_6ch_s16_48k_250ms");
    g.throughput(Throughput::Bytes((n * 2 * 6) as u64));
    g.sample_size(10);
    g.bench_function(BenchmarkId::from_parameter("6ch/s16/48k/250ms"), |b| {
        b.iter(|| {
            let bytes = encode_once(SampleFormat::S16, 6, 48_000, criterion::black_box(&pcm));
            criterion::black_box(bytes);
        });
    });
    g.finish();
}

/// **Round 150 addition** — mono S24 baseline. The existing S24 scenario
/// is stereo, which folds in the channel-decorrelation search; this isolates
/// the cost of a single 24-bit subframe so a future S24-specific tweak
/// (a wider qlp_shift heuristic, an S24-only Rice-method skip, …) has
/// a single-channel A/B target uncoupled from the stereo path.
fn bench_encode_mono_s24_48k_500ms(c: &mut Criterion) {
    let n = 24_000; // 0.5 s @ 48 kHz
    let pcm = build_pcm_per_channel(n, 1, 24);
    let mut g = c.benchmark_group("encode_mono_s24_48k_500ms");
    g.throughput(Throughput::Bytes((n * 3) as u64));
    g.sample_size(10);
    g.bench_function(BenchmarkId::from_parameter("mono/s24/48k/500ms"), |b| {
        b.iter(|| {
            let bytes = encode_once(SampleFormat::S24, 1, 48_000, criterion::black_box(&pcm));
            criterion::black_box(bytes);
        });
    });
    g.finish();
}

/// **Round 150 addition** — wasted-bits content (16-bit container, 8-bit
/// effective dynamic range). Every sample's low 8 bits are zero, so the
/// encoder's wasted-bits detector fires and reduces the effective bps
/// before the subframe search runs. Targets the wasted-bits + narrowed-
/// bps code path that the existing tone-plus-noise scenarios don't
/// exercise (their noise term keeps the LSB live).
fn bench_encode_mono_wasted_bits_s16(c: &mut Criterion) {
    let n = 44_100;
    let raw = build_pcm_per_channel(n, 1, 8);
    // Shift the 8-bit dynamic range up into the 16-bit container so
    // every sample has 8 trailing zero bits.
    let pcm: Vec<Vec<i32>> = raw
        .into_iter()
        .map(|ch| ch.into_iter().map(|v| v << 8).collect())
        .collect();
    let mut g = c.benchmark_group("encode_mono_wasted_bits_s16_44k1_1s");
    g.throughput(Throughput::Bytes((n * 2) as u64));
    g.sample_size(10);
    g.bench_function(
        BenchmarkId::from_parameter("mono/wasted_bits/s16/44k1/1s"),
        |b| {
            b.iter(|| {
                let bytes = encode_once(SampleFormat::S16, 1, 44_100, criterion::black_box(&pcm));
                criterion::black_box(bytes);
            });
        },
    );
    g.finish();
}

/// **Round 150 addition** — CONSTANT-subframe-friendly content. Every
/// sample equals a fixed DC offset, so each block-sized subframe
/// short-circuits to the `samples.iter().all(|&s| s == samples[0])`
/// branch in `best_subframe_with_wasted` and emits a CONSTANT (type-0)
/// subframe. Targets the CONSTANT short-circuit + the per-block driver
/// (de-interleave, MD5 feed, frame-header walk) without the LPC /
/// partition cost that dominates the other scenarios — useful for
/// catching regressions in any per-frame setup work an encoder round
/// might touch.
fn bench_encode_mono_constant_s16(c: &mut Criterion) {
    let n = 44_100;
    // Constant non-zero DC sample everywhere — exercises the CONSTANT
    // short-circuit but not the all-zero (degenerate) wasted-bits branch.
    let pcm = vec![vec![0x1234i32; n]];
    let mut g = c.benchmark_group("encode_mono_constant_s16_44k1_1s");
    g.throughput(Throughput::Bytes((n * 2) as u64));
    g.sample_size(10);
    g.bench_function(
        BenchmarkId::from_parameter("mono/constant/s16/44k1/1s"),
        |b| {
            b.iter(|| {
                let bytes = encode_once(SampleFormat::S16, 1, 44_100, criterion::black_box(&pcm));
                criterion::black_box(bytes);
            });
        },
    );
    g.finish();
}

criterion_group!(
    benches,
    bench_encode_mono_s16_44k1_1s,
    bench_encode_stereo_s16_44k1_1s,
    bench_encode_stereo_s24_48k_500ms,
    bench_encode_6ch_s16_48k_250ms,
    bench_encode_mono_s24_48k_500ms,
    bench_encode_mono_wasted_bits_s16,
    bench_encode_mono_constant_s16,
);
criterion_main!(benches);
