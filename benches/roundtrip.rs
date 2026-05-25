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
//!   - **roundtrip_stereo_s24_48k_500ms**: highest-bps supported mode
//!     (24-bit), wider residuals through Rice + LPC.
//!   - **roundtrip_6ch_s16_48k_250ms**: max channel count (6),
//!     independent subframe per channel.
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

criterion_group!(
    benches,
    bench_roundtrip_mono_s16_44k1_1s,
    bench_roundtrip_stereo_s16_44k1_1s,
    bench_roundtrip_stereo_s24_48k_500ms,
    bench_roundtrip_6ch_s16_48k_250ms,
);
criterion_main!(benches);
