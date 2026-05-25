// Parallel-array index loops are idiomatic in codec / driver code; skip
// the lint (mirrors the crate root's `#![allow(clippy::needless_range_loop)]`).
#![allow(clippy::needless_range_loop)]

//! Standalone profiling driver for the FLAC encoder and decoder.
//!
//! Round 137 (depth-mode profiling): the three Criterion harnesses
//! (`benches/{encode,decode,roundtrip}.rs`, round 128) measure
//! steady-state throughput in a sampling framework, but they're a poor
//! target for `samply` / `perf record` / `cargo flamegraph` because
//! Criterion's warm-up + sampling layers + estimator math show up in
//! the profile and bury the real codec hot paths. This example is a
//! flat measure-this-thing driver: it builds a deterministic
//! synthesised PCM buffer once, then runs a fixed iteration count of
//! whichever path was requested with a single `Instant::now()` /
//! `elapsed()` pair around the whole loop. Throughput is printed at
//! the end so it doubles as a quick A/B harness for the inner
//! tweak-remeasure loop when Criterion's per-run overhead is too
//! coarse.
//!
//! Usage:
//!
//!     cargo run --example profile_flac --release -- <mode> [<iters>]
//!
//! Modes:
//!
//!     encode      — synth PCM, encode N times (decoder cost excluded)
//!     decode      — synth PCM + encode once outside the loop, decode N
//!                   times against the cached packet stream
//!     roundtrip   — synth PCM, encode + decode every iteration
//!     all         — run every mode (default)
//!
//! With `samply`:
//!
//!     samply record -- ./target/release/examples/profile_flac encode 30
//!     samply record -- ./target/release/examples/profile_flac decode 100
//!
//! With `cargo flamegraph` (needs `cargo install flamegraph`):
//!
//!     cargo flamegraph --example profile_flac -- encode 30
//!
//! No external files are read — every PCM input is synthesised in-
//! driver from a deterministic xorshift32 seed, matching the
//! Criterion bench harnesses so profile output and bench numbers
//! correspond. Inputs cover the same four cost-axis scenarios:
//! mono S16 44.1 kHz 1 s, stereo S16 44.1 kHz 1 s, stereo S24 48 kHz
//! 0.5 s, 6-channel S16 48 kHz 0.25 s.

use std::env;
use std::io::Write;
use std::time::Instant;

use oxideav_core::{AudioFrame, CodecId, CodecParameters, Error, Frame, Packet, SampleFormat};

/// xorshift32 PRNG — same constants the bench harnesses use so the
/// profile and bench inputs are byte-identical.
fn xorshift32(state: &mut u32) -> u32 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    *state
}

/// Build per-channel deinterleaved i32 PCM as a low-frequency triangle
/// envelope plus small per-sample xorshift noise scaled to a quarter
/// of the available range — this is the same shape the Criterion
/// benches use (a pure-DC fixture would compress to CONSTANT and hide
/// the LPC + Rice cost the profile is trying to surface).
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

/// Pack per-channel PCM into FLAC's interleaved little-endian byte
/// layout for the given `SampleFormat`. Matches the helper in
/// `tests/roundtrip.rs` + `benches/*.rs`.
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
                _ => panic!("unsupported format for profile: {:?}", format),
            }
        }
    }
    AudioFrame {
        samples: n as u32,
        pts: Some(0),
        data: vec![interleaved],
    }
}

/// One encode pass — build encoder, push the frame, flush, drain every
/// packet. Returns total emitted byte count (also doubles as a black-
/// box sink so the optimiser doesn't drop the loop body).
fn encode_once(
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

/// One decode pass over a pre-encoded packet stream — fresh decoder
/// each call so the cost of `make_decoder` (STREAMINFO parse,
/// initial state table allocation) is folded into the measurement
/// rather than amortised away.
fn decode_once(dec_params: &CodecParameters, packets: &[Packet]) -> usize {
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

/// All four cost-axis scenarios, mirroring the Criterion benches.
struct Scenario {
    name: &'static str,
    format: SampleFormat,
    channels: u16,
    sample_rate: u32,
    n_samples: usize,
    /// Bytes per channel-sample in the input PCM (S16=2, S24=3).
    bytes_per_sample: usize,
    /// Suggested default iteration count for the encode path; the
    /// decode path is roughly 3-5x cheaper so the driver scales this
    /// up for decode-only runs.
    encode_iters_default: u32,
}

fn scenarios() -> &'static [Scenario] {
    &[
        Scenario {
            name: "mono/s16/44k1/1s",
            format: SampleFormat::S16,
            channels: 1,
            sample_rate: 44_100,
            n_samples: 44_100,
            bytes_per_sample: 2,
            encode_iters_default: 20,
        },
        Scenario {
            name: "stereo/s16/44k1/1s",
            format: SampleFormat::S16,
            channels: 2,
            sample_rate: 44_100,
            n_samples: 44_100,
            bytes_per_sample: 2,
            encode_iters_default: 12,
        },
        Scenario {
            name: "stereo/s24/48k/500ms",
            format: SampleFormat::S24,
            channels: 2,
            sample_rate: 48_000,
            n_samples: 24_000,
            bytes_per_sample: 3,
            encode_iters_default: 20,
        },
        Scenario {
            name: "6ch/s16/48k/250ms",
            format: SampleFormat::S16,
            channels: 6,
            sample_rate: 48_000,
            n_samples: 12_000,
            bytes_per_sample: 2,
            encode_iters_default: 16,
        },
    ]
}

fn print_throughput_line(label: &str, scen: &Scenario, iters: u32, elapsed_secs: f64, extra: &str) {
    let pcm_bytes_per_iter = scen.n_samples * (scen.channels as usize) * scen.bytes_per_sample;
    let total_bytes = pcm_bytes_per_iter * iters as usize;
    let per_iter_ms = elapsed_secs * 1000.0 / iters as f64;
    let mib_per_s = (total_bytes as f64) / elapsed_secs / (1024.0 * 1024.0);
    // The "audio realtime ratio" — how many seconds of audio this
    // path consumes per wall-second. Useful sanity check (FLAC
    // decode should be hundreds-of-x realtime on any modern CPU,
    // encode tens-of-x).
    let audio_secs_per_iter = scen.n_samples as f64 / scen.sample_rate as f64;
    let realtime_x = (audio_secs_per_iter * iters as f64) / elapsed_secs;
    println!(
        "  {label:9} {name:24} iters={iters:>3} {per_iter_ms:7.2} ms/iter  {mib_per_s:7.2} MiB/s  {realtime_x:8.1}x realtime{extra}",
        name = scen.name,
    );
}

fn profile_encode(iters_override: Option<u32>) {
    println!("== encode ==");
    for scen in scenarios() {
        let pcm = build_pcm_per_channel(
            scen.n_samples,
            scen.channels,
            match scen.format {
                SampleFormat::U8 => 8,
                SampleFormat::S16 => 16,
                SampleFormat::S24 => 24,
                SampleFormat::S32 => 32,
                _ => unreachable!(),
            },
        );
        let iters = iters_override.unwrap_or(scen.encode_iters_default);

        // One warm-up so any first-call lazy init isn't charged to
        // iteration #1's bucket.
        let _ = encode_once(scen.format, scen.channels, scen.sample_rate, &pcm);

        let t = Instant::now();
        let mut total_packet_bytes = 0u64;
        for _ in 0..iters {
            let (_p, packets) = encode_once(
                scen.format,
                scen.channels,
                scen.sample_rate,
                std::hint::black_box(&pcm),
            );
            for pk in &packets {
                total_packet_bytes += pk.data.len() as u64;
            }
            std::hint::black_box(packets);
        }
        let elapsed = t.elapsed().as_secs_f64();
        let compressed_bytes_per_iter = total_packet_bytes / iters.max(1) as u64;
        let pcm_bytes_per_iter = scen.n_samples * (scen.channels as usize) * scen.bytes_per_sample;
        let ratio = compressed_bytes_per_iter as f64 / pcm_bytes_per_iter as f64;
        let extra = format!("  out={compressed_bytes_per_iter}B/iter ({ratio:.3} of input)");
        print_throughput_line("encode", scen, iters, elapsed, &extra);
        std::io::stdout().flush().ok();
    }
}

fn profile_decode(iters_override: Option<u32>) {
    println!("== decode ==");
    for scen in scenarios() {
        let pcm = build_pcm_per_channel(
            scen.n_samples,
            scen.channels,
            match scen.format {
                SampleFormat::U8 => 8,
                SampleFormat::S16 => 16,
                SampleFormat::S24 => 24,
                SampleFormat::S32 => 32,
                _ => unreachable!(),
            },
        );
        // Decode is the cheaper side — scale the iteration count up
        // when the caller didn't pick one explicitly.
        let iters = iters_override.unwrap_or(scen.encode_iters_default * 5);

        // Encode once OUTSIDE the timed region.
        let (dec_params, packets) = encode_once(scen.format, scen.channels, scen.sample_rate, &pcm);

        // Warm up: one decode pass.
        let _ = decode_once(&dec_params, &packets);

        let t = Instant::now();
        let mut sink = 0usize;
        for _ in 0..iters {
            sink ^= decode_once(
                std::hint::black_box(&dec_params),
                std::hint::black_box(&packets),
            );
        }
        std::hint::black_box(sink);
        let elapsed = t.elapsed().as_secs_f64();
        print_throughput_line("decode", scen, iters, elapsed, "");
        std::io::stdout().flush().ok();
    }
}

fn profile_roundtrip(iters_override: Option<u32>) {
    println!("== roundtrip ==");
    for scen in scenarios() {
        let pcm = build_pcm_per_channel(
            scen.n_samples,
            scen.channels,
            match scen.format {
                SampleFormat::U8 => 8,
                SampleFormat::S16 => 16,
                SampleFormat::S24 => 24,
                SampleFormat::S32 => 32,
                _ => unreachable!(),
            },
        );
        let iters = iters_override.unwrap_or(scen.encode_iters_default);
        let expected_bytes = scen.n_samples * (scen.channels as usize) * scen.bytes_per_sample;

        // Warm-up.
        {
            let (dp, pk) = encode_once(scen.format, scen.channels, scen.sample_rate, &pcm);
            let n = decode_once(&dp, &pk);
            assert_eq!(n, expected_bytes, "warm-up roundtrip drift");
        }

        let t = Instant::now();
        for _ in 0..iters {
            let (dp, pk) = encode_once(
                scen.format,
                scen.channels,
                scen.sample_rate,
                std::hint::black_box(&pcm),
            );
            let n = decode_once(&dp, &pk);
            assert_eq!(n, expected_bytes, "roundtrip drift inside profile loop");
        }
        let elapsed = t.elapsed().as_secs_f64();
        print_throughput_line("roundtrip", scen, iters, elapsed, "");
        std::io::stdout().flush().ok();
    }
}

fn main() {
    let mut args = env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| "all".to_string());
    let iters_override: Option<u32> = args.next().and_then(|s| s.parse().ok());

    println!(
        "=== oxideav-flac profile (mode={mode}, iters={}) ===",
        iters_override
            .map(|n| n.to_string())
            .unwrap_or_else(|| "default".to_string()),
    );
    println!();

    match mode.as_str() {
        "encode" => profile_encode(iters_override),
        "decode" => profile_decode(iters_override),
        "roundtrip" => profile_roundtrip(iters_override),
        "all" => {
            profile_encode(iters_override);
            println!();
            profile_decode(iters_override);
            println!();
            profile_roundtrip(iters_override);
        }
        other => {
            eprintln!("unknown mode: {other:?}");
            eprintln!("usage: profile_flac [encode|decode|roundtrip|all] [<iters>]");
            std::process::exit(2);
        }
    }
}
