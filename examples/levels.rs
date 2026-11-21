//! Speed and size of every compression level, on one thread.
//!
//!     cargo run --release --example levels [-- SECONDS [RUNS [WAV_DIR]]]
//!
//! Two programme-like signals are synthesised (no files are read):
//! 16-bit / 44.1 kHz stereo and 24-bit / 48 kHz 5.1. Each is encoded at
//! levels 0..=8 and decoded again. Speeds are MiB of PCM per second of
//! wall clock, the median of `RUNS` runs (default 5) over `SECONDS`
//! seconds of audio (default 60); "size" is the encoded stream as a
//! percentage of the PCM. The decoded PCM is compared with the input on
//! every run.
//!
//! With `WAV_DIR` (`-` for none), the two signals are also written there
//! as WAVE files, so another encoder can be measured on the same input.
//!
//! For a profiler, `levels SECONDS RUNS - encode|decode LEVEL SIGNAL`
//! repeats one operation `RUNS` times on signal 0 or 1 and prints
//! nothing else.

use std::time::Instant;

use oxideav_core::{AudioFrame, CodecId, CodecParameters, Frame, Packet, SampleFormat};
use oxideav_flac::decoder::FrameScratch;
use oxideav_flac::encoder::{FlacEncoderOptions, make_encoder_with_options};
use oxideav_flac::metadata::StreamInfo;

struct Signal {
    name: &'static str,
    file: &'static str,
    format: SampleFormat,
    bits: u32,
    rate: u32,
    channels: usize,
    /// Interleaved little-endian PCM.
    pcm: Vec<u8>,
}

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }
}

/// A deterministic stand-in for music: notes of a pentatonic scale with
/// a few decaying harmonics each, struck at random times and panned
/// across the channels, over a low-passed noise floor. With six channels
/// the fourth carries only the low end.
fn synthesise(
    name: &'static str,
    file: &'static str,
    bits: u32,
    rate: u32,
    channels: usize,
    seconds: u32,
) -> Signal {
    let n = (rate * seconds) as usize;
    let mut planes = vec![vec![0f64; n]; channels];
    let mut rng = Rng(0x1234_5678_9ABC_DEF1 ^ (bits as u64) << 32 ^ channels as u64);
    let scale = [0, 2, 4, 7, 9];
    let notes = seconds as usize * 6;
    for _ in 0..notes {
        let start = (rng.unit() * n as f64) as usize;
        let degree = scale[(rng.next() % 5) as usize] + 12 * (rng.next() % 4) as i32;
        let freq = 110.0 * 2f64.powf(degree as f64 / 12.0);
        let length = ((0.3 + 1.5 * rng.unit()) * rate as f64) as usize;
        let loudness = 0.03 + 0.09 * rng.unit();
        let position = rng.unit();
        let gains: Vec<f64> = (0..channels)
            .map(|c| {
                if channels == 6 && c == 3 {
                    return if freq < 200.0 { 0.8 } else { 0.0 };
                }
                let place = c as f64 / (channels - 1).max(1) as f64;
                (1.0 - (place - position).abs()).max(0.05)
            })
            .collect();
        let decay = 3.0 / length as f64;
        for i in 0..length.min(n - start) {
            let t = i as f64 / rate as f64;
            let envelope = (-(i as f64) * decay).exp() * (i as f64 / 200.0).min(1.0);
            let mut v = 0.0;
            for (h, weight) in [(1.0, 1.0), (2.0, 0.5), (3.0, 0.3), (4.0, 0.15), (5.0, 0.08)] {
                if freq * h < rate as f64 * 0.45 {
                    v += weight * (std::f64::consts::TAU * freq * h * t).sin();
                }
            }
            for (plane, gain) in planes.iter_mut().zip(&gains) {
                plane[start + i] += v * envelope * loudness * gain;
            }
        }
    }
    let full = ((1i64 << (bits - 1)) - 1) as f64;
    let width = (bits / 8) as usize;
    let mut floor = vec![0f64; channels];
    let mut pcm = vec![0u8; n * channels * width];
    for i in 0..n {
        for (c, plane) in planes.iter().enumerate() {
            floor[c] += 0.15 * ((rng.unit() - 0.5) * 0.004 - floor[c]);
            let v = ((plane[i] + floor[c]) * full).round().clamp(-full, full) as i32;
            let at = (i * channels + c) * width;
            pcm[at..at + width].copy_from_slice(&v.to_le_bytes()[..width]);
        }
    }
    Signal {
        name,
        file,
        format: if bits == 16 {
            SampleFormat::S16
        } else {
            SampleFormat::S24
        },
        bits,
        rate,
        channels,
        pcm,
    }
}

fn write_wav(signal: &Signal, dir: &str) -> std::io::Result<()> {
    let width = (signal.bits / 8) as u16;
    let channels = signal.channels as u16;
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&0xFFFEu16.to_le_bytes());
    fmt.extend_from_slice(&channels.to_le_bytes());
    fmt.extend_from_slice(&signal.rate.to_le_bytes());
    fmt.extend_from_slice(&(signal.rate * channels as u32 * width as u32).to_le_bytes());
    fmt.extend_from_slice(&(channels * width).to_le_bytes());
    fmt.extend_from_slice(&(width * 8).to_le_bytes());
    fmt.extend_from_slice(&22u16.to_le_bytes());
    fmt.extend_from_slice(&(width * 8).to_le_bytes());
    let mask: u32 = if channels == 6 { 0x3F } else { 0x3 };
    fmt.extend_from_slice(&mask.to_le_bytes());
    fmt.extend_from_slice(&[
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B,
        0x71,
    ]);
    let mut out = Vec::with_capacity(signal.pcm.len() + 80);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(signal.pcm.len() as u32 + 60).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
    out.extend_from_slice(&fmt);
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(signal.pcm.len() as u32).to_le_bytes());
    out.extend_from_slice(&signal.pcm);
    std::fs::write(std::path::Path::new(dir).join(signal.file), out)
}

fn encode(signal: &Signal, level: u8) -> (StreamInfo, Vec<Packet>) {
    let mut params = CodecParameters::audio(CodecId::new("flac"));
    params.channels = Some(signal.channels as u16);
    params.sample_rate = Some(signal.rate);
    params.sample_format = Some(signal.format);
    let mut enc = make_encoder_with_options(&params, FlacEncoderOptions::level(level)).unwrap();
    let stride = signal.channels * (signal.bits / 8) as usize;
    let mut packets = Vec::new();
    // About a tenth of a second per call, as a capture loop would send.
    for chunk in signal.pcm.chunks(stride * 4096) {
        let frame = AudioFrame {
            samples: (chunk.len() / stride) as u32,
            pts: None,
            data: vec![chunk.to_vec()],
        };
        enc.send_frame(&Frame::Audio(frame)).unwrap();
        while let Ok(p) = enc.receive_packet() {
            packets.push(p);
        }
    }
    enc.flush().unwrap();
    while let Ok(p) = enc.receive_packet() {
        packets.push(p);
    }
    let info = StreamInfo::parse(&enc.output_params().extradata[4..38]).unwrap();
    (info, packets)
}

/// Decode every packet into sample planes and fold them into a sum, so
/// the work cannot be optimised away; returns the sum.
fn decode(info: &StreamInfo, packets: &[Packet], scratch: &mut FrameScratch) -> i64 {
    let mut sum = 0i64;
    for p in packets {
        let shape = scratch.decode(&p.data, info).unwrap();
        for plane in &scratch.planes()[..shape.channels] {
            sum = sum.wrapping_add(plane[0] as i64 + plane[shape.block_size - 1] as i64);
        }
    }
    sum
}

/// Decoding is fast; each run times this many passes over the packets.
const DECODE_PASSES: usize = 5;

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(f64::total_cmp);
    v[v.len() / 2]
}

fn main() {
    let mut args = std::env::args().skip(1);
    let seconds: u32 = args.next().and_then(|a| a.parse().ok()).unwrap_or(60);
    let runs: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(5);
    let wav_dir = args.next().filter(|d| d != "-");
    let profile = args.next();
    let profile_level: u8 = args.next().and_then(|a| a.parse().ok()).unwrap_or(5);
    let profile_signal: usize = args.next().and_then(|a| a.parse().ok()).unwrap_or(0);

    let signals = [
        synthesise(
            "16-bit / 44.1 kHz stereo",
            "stereo16.wav",
            16,
            44_100,
            2,
            seconds,
        ),
        synthesise(
            "24-bit / 48 kHz 5.1",
            "surround24.wav",
            24,
            48_000,
            6,
            seconds,
        ),
    ];
    if let Some(operation) = profile {
        let signal = &signals[profile_signal.min(1)];
        let (info, packets) = encode(signal, profile_level);
        let mut scratch = FrameScratch::new();
        let start = Instant::now();
        for _ in 0..runs {
            if operation == "encode" {
                std::hint::black_box(encode(signal, profile_level));
            } else {
                std::hint::black_box(decode(&info, &packets, &mut scratch));
            }
        }
        let mib = signal.pcm.len() as f64 * runs as f64 / (1024.0 * 1024.0);
        println!(
            "{operation}: {:.0} MiB/s",
            mib / start.elapsed().as_secs_f64()
        );
        return;
    }
    for signal in &signals {
        if let Some(dir) = &wav_dir {
            write_wav(signal, dir).expect("write WAVE file");
        }
        let mib = signal.pcm.len() as f64 / (1024.0 * 1024.0);
        println!("\n{} — {seconds} s, {mib:.1} MiB of PCM", signal.name);
        println!("| level | encode MiB/s | decode MiB/s | size |");
        println!("|---:|---:|---:|---:|");
        for level in 0..=8u8 {
            let mut encode_times = Vec::new();
            let mut decode_times = Vec::new();
            let mut size = 0usize;
            let mut scratch = FrameScratch::new();
            for _ in 0..runs {
                let start = Instant::now();
                let (info, packets) = encode(signal, level);
                encode_times.push(start.elapsed().as_secs_f64());
                size = packets.iter().map(|p| p.data.len()).sum::<usize>() + 42;

                let start = Instant::now();
                for _ in 0..DECODE_PASSES {
                    std::hint::black_box(decode(&info, &packets, &mut scratch));
                }
                decode_times.push(start.elapsed().as_secs_f64() / DECODE_PASSES as f64);
                assert_eq!(
                    oxideav_flac::verify::verify_frame_stream(
                        &packets
                            .iter()
                            .flat_map(|p| p.data.clone())
                            .collect::<Vec<u8>>(),
                        &info
                    )
                    .unwrap(),
                    oxideav_flac::verify::VerifyOutcome::Match
                );
            }
            println!(
                "| {level} | {:.0} | {:.0} | {:.2} % |",
                mib / median(encode_times),
                mib / median(decode_times),
                100.0 * size as f64 / signal.pcm.len() as f64
            );
        }
    }
}
