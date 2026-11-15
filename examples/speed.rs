//! Encode and decode speed in multiples of real time.
//!
//!     cargo run --release --example speed [-- SECONDS [RUNS]]
//!
//! Two programme-like signals are synthesised (no files are read):
//! 16-bit / 44.1 kHz stereo and 24-bit / 96 kHz stereo. Each is encoded
//! at the default and at the maximum compression level, single-threaded,
//! and the resulting packets are decoded again. Every figure is the
//! median of `RUNS` runs (default 5) over `SECONDS` seconds of audio
//! (default 30), expressed as "seconds of audio per second of wall
//! clock". The decoded PCM is compared with the input on every run, so a
//! number is only printed for a lossless round trip.

use std::time::Instant;

use oxideav_core::{AudioFrame, CodecId, CodecParameters, Error, Frame, Packet, SampleFormat};
use oxideav_flac::encoder::{
    DEFAULT_COMPRESSION_LEVEL, FlacEncoderOptions, MAX_COMPRESSION_LEVEL, make_encoder_with_options,
};

struct Signal {
    name: &'static str,
    format: SampleFormat,
    bits: u32,
    rate: u32,
    seconds: u32,
    /// Interleaved little-endian stereo PCM.
    pcm: Vec<u8>,
}

/// A deterministic stand-in for music: a handful of slowly modulated
/// partials per channel (shared fundamentals, different phases, so the
/// channels are correlated but not identical) over a noise floor shaped
/// by a one-pole low-pass. Amplitudes sit around -12 dBFS.
fn synthesise(name: &'static str, bits: u32, rate: u32, seconds: u32) -> Signal {
    let n = (rate * seconds) as usize;
    let bytes = (bits / 8) as usize;
    let full = ((1i64 << (bits - 1)) - 1) as f64;
    let mut pcm = Vec::with_capacity(n * 2 * bytes);
    let partials: [(f64, f64); 6] = [
        (110.0, 0.10),
        (220.0, 0.07),
        (331.0, 0.05),
        (523.3, 0.035),
        (1318.5, 0.02),
        (4186.0, 0.008),
    ];
    let mut seed: u32 = 0x2545_F491;
    let mut noise = [0.0f64; 2];
    let tau = std::f64::consts::TAU;
    for i in 0..n {
        let t = i as f64 / rate as f64;
        let envelope = 0.6 + 0.4 * (tau * 0.37 * t).sin();
        for (ch, floor) in noise.iter_mut().enumerate() {
            let mut v = 0.0;
            for (k, &(freq, amp)) in partials.iter().enumerate() {
                let wobble = 1.0 + 0.002 * (tau * (0.11 + 0.07 * k as f64) * t).sin();
                let phase = ch as f64 * 0.6 * (k as f64 + 1.0);
                v += amp * (tau * freq * wobble * t + phase).sin();
            }
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            let white = (seed as i32 as f64) / 2_147_483_648.0;
            *floor += 0.15 * (white - *floor);
            v = envelope * v + 0.004 * *floor + 0.000_3 * white;
            let s = (v * full).round() as i64;
            pcm.extend_from_slice(&s.to_le_bytes()[..bytes]);
        }
    }
    Signal {
        name,
        format: if bits == 16 {
            SampleFormat::S16
        } else {
            SampleFormat::S24
        },
        bits,
        rate,
        seconds,
        pcm,
    }
}

fn encode(sig: &Signal, level: u8) -> (Vec<Packet>, CodecParameters) {
    let mut params = CodecParameters::audio(CodecId::new("flac"));
    params.channels = Some(2);
    params.sample_rate = Some(sig.rate);
    params.sample_format = Some(sig.format);
    let mut enc = make_encoder_with_options(&params, FlacEncoderOptions::level(level))
        .expect("encoder construction");
    let frame_bytes = 2 * (sig.bits / 8) as usize;
    // One second of PCM per call, the shape a file exporter uses.
    for chunk in sig.pcm.chunks(sig.rate as usize * frame_bytes) {
        enc.send_frame(&Frame::Audio(AudioFrame {
            samples: (chunk.len() / frame_bytes) as u32,
            pts: None,
            data: vec![chunk.to_vec()],
        }))
        .expect("send_frame");
    }
    enc.flush().expect("flush");
    let mut packets = Vec::new();
    loop {
        match enc.receive_packet() {
            Ok(p) => packets.push(p),
            Err(Error::NeedMore) | Err(Error::Eof) => break,
            Err(e) => panic!("receive_packet: {e:?}"),
        }
    }
    (packets, enc.output_params().clone())
}

fn decode(packets: &[Packet], params: &CodecParameters, out: &mut Vec<u8>) {
    let mut dec = oxideav_flac::decoder::make_decoder(params).expect("decoder construction");
    out.clear();
    for p in packets {
        dec.send_packet(p).expect("send_packet");
        match dec.receive_frame().expect("receive_frame") {
            Frame::Audio(a) => out.extend_from_slice(&a.data[0]),
            _ => panic!("expected audio"),
        }
    }
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

fn main() {
    let mut args = std::env::args().skip(1);
    let seconds: u32 = args.next().and_then(|s| s.parse().ok()).unwrap_or(30);
    let runs: usize = args.next().and_then(|s| s.parse().ok()).unwrap_or(5);

    let signals = [
        synthesise("16-bit / 44.1 kHz stereo", 16, 44_100, seconds),
        synthesise("24-bit / 96 kHz stereo", 24, 96_000, seconds),
    ];
    println!("{seconds} s of audio per signal, median of {runs} runs, one thread");
    println!(
        "{:<26} {:>5} {:>8} {:>14} {:>14}",
        "signal", "level", "ratio", "encode x RT", "decode x RT"
    );
    for sig in &signals {
        for level in [DEFAULT_COMPRESSION_LEVEL, MAX_COMPRESSION_LEVEL] {
            let mut enc_times = Vec::new();
            let mut dec_times = Vec::new();
            let mut size = 0usize;
            let mut out = Vec::with_capacity(sig.pcm.len());
            for _ in 0..runs {
                let t = Instant::now();
                let (packets, params) = encode(sig, level);
                enc_times.push(t.elapsed().as_secs_f64());
                size = packets.iter().map(|p| p.data.len()).sum();

                let t = Instant::now();
                decode(&packets, &params, &mut out);
                dec_times.push(t.elapsed().as_secs_f64());
                assert!(out == sig.pcm, "round trip is not lossless");
            }
            let audio = sig.seconds as f64;
            println!(
                "{:<26} {:>5} {:>7.1}% {:>14.0} {:>14.0}",
                sig.name,
                level,
                100.0 * size as f64 / sig.pcm.len() as f64,
                audio / median(enc_times),
                audio / median(dec_times),
            );
        }
    }
}
