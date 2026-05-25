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
        1 << 13
    } else {
        1 << 21
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

criterion_group!(
    benches,
    bench_decode_mono_s16_44k1_1s,
    bench_decode_stereo_s16_44k1_1s,
    bench_decode_stereo_s24_48k_500ms,
    bench_decode_6ch_s16_48k_250ms,
);
criterion_main!(benches);
