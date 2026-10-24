//! Encode a PCM WAV file to FLAC through the public API — the shape of
//! code a `.wav` → `.flac` exporter would ship (issue #12 asked for one).
//!
//!     cargo run --release --example encode_wav -- input.wav output.flac [options]
//!
//! Options:
//!
//!     --level N         compression level 0..=8 (default 5)
//!     --exhaustive      historical exact-cost search (very slow)
//!     --fast            same as --level 4 (`FlacEncoderOptions::fast()`)
//!     --verify          encode-time self-check (`flac --verify`)
//!     --max-lpc N       LPC order ceiling (1..=32; >12 leaves the subset)
//!     --windows N       number of apodization windows to search (1..=5)
//!     --window NAME     add one window: welch | hann | tukey (alpha 1/2) |
//!                       bartlett | blackman-harris (repeatable)
//!     --span N          LPC coefficient-precision sweep below the heuristic
//!     --max-partition N partition-order ceiling (clamped to the subset bound)
//!     --block N         block size (default 4096)
//!     --chunk SECONDS   PCM per send_frame call (default 1; 0 = whole file)
//!
//! The encoder is pull-based: push `AudioFrame`s of any size (it re-blocks
//! them), `flush()`, then drain `receive_packet()`. The final STREAMINFO
//! (total samples, MD5, min/max frame size) only exists after `flush()`, so
//! the packets are collected first and the muxer is opened afterwards with
//! `enc.output_params()` — that is what makes the resulting file pass
//! `flac -t` with a verified MD5.
//!
//! Always build with `--release`: the debug profile is about 20x slower.

use std::env;
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::time::Instant;

use oxideav_core::{
    AudioFrame, CodecId, CodecParameters, Error, Frame, Packet, SampleFormat, StreamInfo, TimeBase,
    WriteSeek,
};
use oxideav_flac::container::{FlacMuxerOptions, open_muxer_with_options};
use oxideav_flac::encoder::{Apodization, FlacEncoderOptions, make_encoder_with_options};

/// Minimal RIFF/WAVE reader: PCM (format tag 1) or WAVE_FORMAT_EXTENSIBLE
/// wrapping PCM, 8/16/24/32-bit integer samples, any channel count.
struct Wav {
    channels: u16,
    sample_rate: u32,
    bits: u16,
    /// Interleaved little-endian samples exactly as stored in the file.
    data: Vec<u8>,
}

fn read_wav(path: &str) -> Result<Wav, String> {
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|mut f| f.read_to_end(&mut bytes))
        .map_err(|e| format!("{path}: {e}"))?;
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(format!("{path}: not a RIFF/WAVE file"));
    }
    let mut pos = 12;
    let mut fmt: Option<(u16, u16, u32, u16)> = None; // tag, channels, rate, bits
    let mut data: Option<Vec<u8>> = None;
    while pos + 8 <= bytes.len() {
        let id = &bytes[pos..pos + 4];
        let len = u32::from_le_bytes(bytes[pos + 4..pos + 8].try_into().unwrap()) as usize;
        let body_start = pos + 8;
        let body_end = (body_start + len).min(bytes.len());
        let body = &bytes[body_start..body_end];
        match id {
            b"fmt " if body.len() >= 16 => {
                let mut tag = u16::from_le_bytes([body[0], body[1]]);
                let channels = u16::from_le_bytes([body[2], body[3]]);
                let rate = u32::from_le_bytes(body[4..8].try_into().unwrap());
                let bits = u16::from_le_bytes([body[14], body[15]]);
                if tag == 0xFFFE && body.len() >= 26 {
                    // WAVE_FORMAT_EXTENSIBLE: the real tag is the first two
                    // bytes of the SubFormat GUID.
                    tag = u16::from_le_bytes([body[24], body[25]]);
                }
                fmt = Some((tag, channels, rate, bits));
            }
            b"data" => data = Some(body.to_vec()),
            _ => {}
        }
        pos = body_start + len + (len & 1); // chunks are word-aligned
    }
    let (tag, channels, sample_rate, bits) = fmt.ok_or("missing fmt chunk")?;
    if tag != 1 {
        return Err(format!("unsupported WAVE format tag {tag} (PCM only)"));
    }
    if !matches!(bits, 8 | 16 | 24 | 32) {
        return Err(format!("unsupported bit depth {bits}"));
    }
    Ok(Wav {
        channels,
        sample_rate,
        bits,
        data: data.ok_or("missing data chunk")?,
    })
}

fn sample_format(bits: u16) -> SampleFormat {
    match bits {
        8 => SampleFormat::U8,
        16 => SampleFormat::S16,
        24 => SampleFormat::S24,
        _ => SampleFormat::S32,
    }
}

/// Parsed command line: encoder options plus how many seconds of PCM to
/// hand the encoder per `send_frame` (0 = the whole file at once).
fn parse_options(args: &[String]) -> Result<(FlacEncoderOptions, u32), String> {
    let mut opts = FlacEncoderOptions::default();
    let mut chunk_seconds = 1u32;
    let all_windows = [
        Apodization::Welch,
        Apodization::Hann,
        Apodization::Tukey {
            alpha_num: 1,
            alpha_den: 4,
        },
        Apodization::Bartlett,
        Apodization::BlackmanHarris,
    ];
    let mut i = 0;
    let num = |i: &mut usize, name: &str| -> Result<u32, String> {
        *i += 1;
        args.get(*i)
            .and_then(|v| v.parse::<u32>().ok())
            .ok_or_else(|| format!("{name} needs a numeric argument"))
    };
    while i < args.len() {
        match args[i].as_str() {
            "--level" => {
                let level = num(&mut i, "--level")?.min(8) as u8;
                opts = opts.with_compression_level(level);
            }
            "--exhaustive" => opts = opts.with_exhaustive_search(true),
            "--fast" => opts = opts.with_compression_level(4), // == FlacEncoderOptions::fast()
            "--verify" => opts.verify = true,
            "--max-lpc" => opts.max_lpc_order = Some(num(&mut i, "--max-lpc")? as usize),
            "--windows" => {
                let n = (num(&mut i, "--windows")? as usize).clamp(1, all_windows.len());
                opts.apodization = Some(all_windows[..n].to_vec());
            }
            "--window" => {
                i += 1;
                let w = match args.get(i).map(String::as_str) {
                    Some("welch") => Apodization::Welch,
                    Some("hann") => Apodization::Hann,
                    Some("tukey") => Apodization::Tukey {
                        alpha_num: 1,
                        alpha_den: 2,
                    },
                    Some("bartlett") => Apodization::Bartlett,
                    Some("blackman-harris") => Apodization::BlackmanHarris,
                    other => return Err(format!("unknown window {other:?}")),
                };
                opts.apodization.get_or_insert_with(Vec::new).push(w);
            }
            "--span" => opts.lpc_precision_search_span = Some(num(&mut i, "--span")?),
            "--max-partition" => opts.max_partition_order = Some(num(&mut i, "--max-partition")?),
            "--block" => opts.block_size = Some(num(&mut i, "--block")?),
            "--chunk" => chunk_seconds = num(&mut i, "--chunk")?,
            other => return Err(format!("unknown option {other}")),
        }
        i += 1;
    }
    Ok((opts, chunk_seconds))
}

fn run() -> Result<(), String> {
    let args: Vec<String> = env::args().skip(1).collect();
    if args.len() < 2 {
        return Err("usage: encode_wav <input.wav> <output.flac> [options]".into());
    }
    let wav = read_wav(&args[0])?;
    let (opts, chunk_seconds) = parse_options(&args[2..])?;

    let format = sample_format(wav.bits);
    let frame_bytes = wav.channels as usize * format.bytes_per_sample();
    let total_samples = wav.data.len() / frame_bytes;

    let mut params = CodecParameters::audio(CodecId::new("flac"));
    params.channels = Some(wav.channels);
    params.sample_rate = Some(wav.sample_rate);
    params.sample_format = Some(format);
    let mut enc = make_encoder_with_options(&params, opts).map_err(|e| e.to_string())?;

    let started = Instant::now();
    // Feed the PCM in chunks (one second at a time by default); the encoder
    // re-blocks internally so any chunk size works, including the whole
    // file in a single frame.
    let chunk_samples = match chunk_seconds {
        0 => total_samples.max(1),
        n => wav.sample_rate as usize * n as usize,
    };
    let mut packets: Vec<Packet> = Vec::new();
    let mut pts = 0i64;
    for chunk in wav.data.chunks(chunk_samples * frame_bytes) {
        let samples = (chunk.len() / frame_bytes) as u32;
        let frame = AudioFrame {
            samples,
            pts: Some(pts),
            data: vec![chunk.to_vec()],
        };
        enc.send_frame(&Frame::Audio(frame))
            .map_err(|e| e.to_string())?;
        pts += samples as i64;
        drain(enc.as_mut(), &mut packets)?;
    }
    enc.flush().map_err(|e| e.to_string())?;
    drain(enc.as_mut(), &mut packets)?;
    let elapsed = started.elapsed();

    // The muxer copies the STREAMINFO block out of `output_params()` —
    // final only now, after flush().
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, wav.sample_rate as i64),
        duration: Some(total_samples as i64),
        start_time: Some(0),
        params: enc.output_params().clone(),
    };
    let out = File::create(&args[1]).map_err(|e| format!("{}: {e}", args[1]))?;
    let writer: Box<dyn WriteSeek> = Box::new(BufWriter::new(out));
    let mut mux = open_muxer_with_options(
        writer,
        std::slice::from_ref(&stream),
        FlacMuxerOptions::for_sample_rate(wav.sample_rate),
    )
    .map_err(|e| e.to_string())?;
    mux.write_header().map_err(|e| e.to_string())?;
    for p in &packets {
        mux.write_packet(p).map_err(|e| e.to_string())?;
    }
    mux.write_trailer().map_err(|e| e.to_string())?;
    drop(mux);

    let flac_bytes: usize = packets.iter().map(|p| p.data.len()).sum();
    let seconds = total_samples as f64 / wav.sample_rate as f64;
    let secs = elapsed.as_secs_f64();
    let mut o = std::io::stdout().lock();
    writeln!(
        o,
        "{} ch, {} Hz, {} bit, {:.1} s of audio -> {} frame bytes ({:.1}% of PCM) in {:.3} s ({:.0}x realtime)",
        wav.channels,
        wav.sample_rate,
        wav.bits,
        seconds,
        flac_bytes,
        100.0 * flac_bytes as f64 / wav.data.len() as f64,
        secs,
        seconds / secs
    )
    .ok();
    Ok(())
}

fn drain(enc: &mut dyn oxideav_core::Encoder, out: &mut Vec<Packet>) -> Result<(), String> {
    loop {
        match enc.receive_packet() {
            Ok(p) => out.push(p),
            Err(Error::NeedMore) | Err(Error::Eof) => return Ok(()),
            Err(e) => return Err(e.to_string()),
        }
    }
}

fn main() {
    if let Err(e) = run() {
        eprintln!("encode_wav: {e}");
        std::process::exit(1);
    }
}
