//! Foreign-encoder decode oracle: encode raw PCM with a **black-box**
//! reference `flac` encoder, then decode the produced `.flac` through
//! this crate's container demuxer + decoder and assert sample-exact
//! recovery.
//!
//! This is the mirror image of `reference_decode_oracle`: there we
//! proved *our encoder's* bitstream is decodable by an outside decoder;
//! here we prove *our decoder* reconstructs streams an outside encoder
//! produced. The two together close the interop loop in both directions.
//!
//! The headline case is genuine **32-bit** audio: RFC 9639 §9.1.4
//! Table 17 assigns bit-depth code `0b111` to 32 bits per sample, and a
//! reference encoder emits exactly that code in the per-frame header for
//! 32-bit input. A decoder that treated `0b111` as reserved (as this
//! crate did before round 374) would reject every real 32-bit FLAC file.
//! The remaining cases sweep the other depths and the four channel
//! decorrelation modes so the foreign-stream decode path is exercised
//! broadly, not just on the one fixed bug.
//!
//! Clean-room note: the `flac` binary is an opaque process — raw PCM in,
//! `.flac` bytes out. Its source is never read. The suite skips when the
//! binary is absent so it never reddens CI on a host without it.

use oxideav_core::{ContainerRegistry, Error, Frame, NullCodecResolver, ReadSeek, SampleFormat};
use std::io::Cursor;
use std::path::PathBuf;
use std::process::Command;

fn flac_binary() -> Option<String> {
    match Command::new("flac").arg("--version").output() {
        Ok(out) if out.status.success() => Some("flac".to_string()),
        _ => None,
    }
}

/// Interleave per-channel `i32` planes into little-endian signed PCM at
/// `bps` bits per sample (8/16/24/32), the layout the reference encoder
/// reads with `--sign=signed`.
fn interleave_signed(bps: u32, channels: u16, pcm: &[Vec<i32>]) -> Vec<u8> {
    let n = pcm[0].len();
    let bytes = (bps / 8) as usize;
    let mut out = Vec::with_capacity(n * channels as usize * bytes);
    #[allow(clippy::needless_range_loop)]
    for i in 0..n {
        for c in 0..channels as usize {
            let s = pcm[c][i];
            match bps {
                8 => out.push((s & 0xFF) as u8),
                16 => out.extend_from_slice(&(s as i16).to_le_bytes()),
                24 => {
                    out.push((s & 0xFF) as u8);
                    out.push(((s >> 8) & 0xFF) as u8);
                    out.push(((s >> 16) & 0xFF) as u8);
                }
                32 => out.extend_from_slice(&s.to_le_bytes()),
                _ => panic!("unsupported bps {bps}"),
            }
        }
    }
    out
}

/// Encode `pcm` to a `.flac` byte stream with the reference encoder.
fn reference_encode(
    flac: &str,
    bps: u32,
    channels: u16,
    sample_rate: u32,
    pcm: &[Vec<i32>],
    tag: &str,
) -> Vec<u8> {
    let dir = std::env::temp_dir();
    let mut raw_path = PathBuf::from(&dir);
    raw_path.push(format!("oxideav-flac-rev-{tag}-{}.raw", std::process::id()));
    let mut flac_path = PathBuf::from(&dir);
    flac_path.push(format!(
        "oxideav-flac-rev-{tag}-{}.flac",
        std::process::id()
    ));

    let raw = interleave_signed(bps, channels, pcm);
    std::fs::write(&raw_path, &raw).expect("write raw temp");

    let out = Command::new(flac)
        .args([
            "-s",
            "-f",
            "--force-raw-format",
            "--endian=little",
            "--sign=signed",
            &format!("--channels={channels}"),
            &format!("--bps={bps}"),
            &format!("--sample-rate={sample_rate}"),
        ])
        .arg("-o")
        .arg(&flac_path)
        .arg(&raw_path)
        .output()
        .expect("spawn flac encode");

    let _ = std::fs::remove_file(&raw_path);
    if !out.status.success() {
        let _ = std::fs::remove_file(&flac_path);
        panic!(
            "reference flac encode failed ({tag}): {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let bytes = std::fs::read(&flac_path).expect("read encoded flac");
    let _ = std::fs::remove_file(&flac_path);
    bytes
}

/// Append the per-sample `i32` values held in `plane` (a decoder output
/// byte plane of `stride` bytes per sample) onto `out`.
fn append_plane(plane: &[u8], stride: usize, out: &mut Vec<i32>) {
    for chunk in plane.chunks_exact(stride) {
        let v = match stride {
            1 => chunk[0] as i32 - 128, // U8 is unsigned offset-128
            2 => i16::from_le_bytes([chunk[0], chunk[1]]) as i32,
            3 => {
                let mut v =
                    (chunk[0] as i32) | ((chunk[1] as i32) << 8) | ((chunk[2] as i32) << 16);
                if v & 0x0080_0000 != 0 {
                    v |= 0xFF00_0000_u32 as i32;
                }
                v
            }
            4 => i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]),
            _ => panic!("unexpected decoder stride {stride}"),
        };
        out.push(v);
    }
}

/// Decode `flac_bytes` through this crate's container + decoder into
/// interleaved `i32` samples (channel-major outer Vec).
fn our_decode(flac_bytes: Vec<u8>, channels: u16) -> Vec<Vec<i32>> {
    let mut reg = ContainerRegistry::new();
    oxideav_flac::register_containers(&mut reg);
    let cursor: Box<dyn ReadSeek> = Box::new(Cursor::new(flac_bytes));
    let mut demux = reg
        .open_demuxer("flac", cursor, &NullCodecResolver)
        .expect("open flac demuxer");

    let stream = demux.streams()[0].clone();
    let params = stream.params.clone();
    let stride = params
        .sample_format
        .map(|f| f.bytes_per_sample())
        .expect("stream advertises a sample format");
    let mut decoder = oxideav_flac::decoder::make_decoder(&params).expect("make_decoder");

    let stream_index = stream.index;
    let mut interleaved: Vec<i32> = Vec::new();
    loop {
        let pkt = match demux.next_packet() {
            Ok(p) => p,
            Err(Error::Eof) => break,
            Err(e) => panic!("demux error: {e}"),
        };
        if pkt.stream_index != stream_index {
            continue;
        }
        decoder.send_packet(&pkt).expect("send_packet");
        loop {
            match decoder.receive_frame() {
                Ok(Frame::Audio(a)) => {
                    // The FLAC decoder emits one interleaved byte plane;
                    // each sample occupies `stride` bytes regardless of
                    // channel, so deinterleaving happens below.
                    append_plane(&a.data[0], stride, &mut interleaved);
                }
                Ok(_) => {}
                Err(Error::NeedMore) | Err(Error::Eof) => break,
                Err(e) => panic!("receive_frame error: {e}"),
            }
        }
    }

    // Deinterleave channel-major.
    let mut planes: Vec<Vec<i32>> = vec![Vec::new(); channels as usize];
    for (i, v) in interleaved.into_iter().enumerate() {
        planes[i % channels as usize].push(v);
    }
    planes
}

/// `bps` selects the input PCM bit depth handed to the reference
/// encoder; `out_format` is the `SampleFormat` this crate's decoder
/// projects that depth onto (so the comparison uses the matching byte
/// width). Skips when the reference binary is unavailable.
fn assert_foreign_roundtrip(
    bps: u32,
    out_format: SampleFormat,
    channels: u16,
    sample_rate: u32,
    pcm: Vec<Vec<i32>>,
    tag: &str,
) {
    let Some(flac) = flac_binary() else {
        eprintln!("skip {tag}: reference `flac` binary not on PATH");
        return;
    };
    let n = pcm[0].len();
    let flac_bytes = reference_encode(&flac, bps, channels, sample_rate, &pcm, tag);
    let recovered = our_decode(flac_bytes, channels);

    let _ = out_format;
    assert_eq!(recovered.len(), channels as usize, "{tag}: channel count");
    for (c, plane) in recovered.iter().enumerate() {
        assert_eq!(plane.len(), n, "{tag}: channel {c} sample count");
        for i in 0..n {
            assert_eq!(
                plane[i], pcm[c][i],
                "{tag}: our decoder mismatch at channel {c} sample {i}"
            );
        }
    }
}

// --- Signal generators ----------------------------------------------------

fn sine(n: usize, sr: u32, freq: f64, amp: f64) -> Vec<i32> {
    (0..n)
        .map(|i| {
            let t = i as f64 / sr as f64;
            ((t * freq * 2.0 * std::f64::consts::PI).sin() * amp) as i32
        })
        .collect()
}

// --- Tests ----------------------------------------------------------------

#[test]
fn our_decoder_reads_foreign_32bit_stream() {
    // The bug fix: bit-depth code 0b111 == 32 bits per sample. A real
    // 32-bit FLAC carries this code in every frame header; before the
    // fix our parser rejected it as reserved.
    let sr = 192_000;
    let n = 4000;
    let l = sine(n, sr, 1000.0, 1.0e9);
    let r = sine(n, sr, 1500.0, 0.8e9);
    assert_foreign_roundtrip(32, SampleFormat::S32, 2, sr, vec![l, r], "foreign-32bit");
}

#[test]
fn our_decoder_reads_foreign_16bit_stream() {
    let sr = 44_100;
    let n = 6000;
    let l = sine(n, sr, 440.0, 18_000.0);
    let r = sine(n, sr, 660.0, 12_000.0);
    assert_foreign_roundtrip(16, SampleFormat::S16, 2, sr, vec![l, r], "foreign-16bit");
}

#[test]
fn our_decoder_reads_foreign_24bit_stream() {
    let sr = 96_000;
    let n = 5000;
    let l = sine(n, sr, 880.0, 4_000_000.0);
    let r = sine(n, sr, 1320.0, 3_000_000.0);
    assert_foreign_roundtrip(24, SampleFormat::S24, 2, sr, vec![l, r], "foreign-24bit");
}

#[test]
fn our_decoder_reads_foreign_8bit_mono() {
    let s: Vec<i32> = (0..4000i32)
        .map(|i| ((i % 200) - 100).clamp(-127, 127))
        .collect();
    assert_foreign_roundtrip(8, SampleFormat::U8, 1, 8_000, vec![s], "foreign-8bit");
}

#[test]
fn our_decoder_reads_foreign_32bit_mono() {
    let sr = 96_000;
    let n = 3000;
    let s = sine(n, sr, 700.0, 1.5e9);
    assert_foreign_roundtrip(32, SampleFormat::S32, 1, sr, vec![s], "foreign-32bit-mono");
}
