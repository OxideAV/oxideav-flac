//! Cross-implementation encode oracle: encode PCM with this crate's
//! encoder, assemble a complete `.flac` stream (`fLaC` magic + the
//! finalised STREAMINFO/PADDING metadata chain + the frame packets),
//! then hand the bytes to a **black-box** reference `flac` decoder and
//! assert it recovers the original PCM sample-for-sample.
//!
//! This is the strongest correctness evidence the crate can carry: a
//! whole-stream round-trip through an *independent* decoder. The
//! crate's own `roundtrip.rs` proves the encoder and decoder agree with
//! each other; a self-consistent pair could in principle share a
//! bitstream bug that cancels out. Routing the encoder's bytes through a
//! foreign decoder removes that blind spot — if our frame header, LPC
//! warm-up packing, partitioned-Rice residual, channel-decorrelation
//! shift, wasted-bits header, or CRC were off by a bit, an external
//! decoder would mis-reconstruct or reject the stream.
//!
//! Clean-room note: the `flac` binary is invoked purely as an opaque
//! process (input bytes in, decoded PCM out). Its source is never read.
//! The test gracefully skips when the binary is absent (e.g. in CI),
//! exactly like the docs-submodule-gated fixture tests, so it never
//! turns CI red on a host without the validator installed.

use oxideav_core::{AudioFrame, CodecId, CodecParameters, Error, Frame, Packet, SampleFormat};
use std::path::PathBuf;
use std::process::Command;

/// Resolve the reference `flac` binary if it is on `PATH`. Returns
/// `None` (so the caller skips) when it is unavailable.
fn flac_binary() -> Option<String> {
    // `flac --version` exits 0 and prints e.g. "flac 1.5.0" when present.
    match Command::new("flac").arg("--version").output() {
        Ok(out) if out.status.success() => Some("flac".to_string()),
        _ => None,
    }
}

/// Interleave per-channel `i32` planes into the little-endian PCM byte
/// layout the encoder accepts for `format`.
fn interleave(format: SampleFormat, channels: u16, pcm: &[Vec<i32>]) -> Vec<u8> {
    let n = pcm[0].len();
    let bps = format.bytes_per_sample();
    let mut out = Vec::with_capacity(n * channels as usize * bps);
    #[allow(clippy::needless_range_loop)]
    for i in 0..n {
        for c in 0..channels as usize {
            let s = pcm[c][i];
            match format {
                SampleFormat::U8 => out.push(((s + 128) & 0xFF) as u8),
                SampleFormat::S16 => out.extend_from_slice(&(s as i16).to_le_bytes()),
                SampleFormat::S24 => {
                    out.push((s & 0xFF) as u8);
                    out.push(((s >> 8) & 0xFF) as u8);
                    out.push(((s >> 16) & 0xFF) as u8);
                }
                SampleFormat::S32 => out.extend_from_slice(&s.to_le_bytes()),
                _ => panic!("unsupported format {format:?}"),
            }
        }
    }
    out
}

/// Encode the per-channel planes with `make_encoder_with_options` (or
/// the default factory when `opts` is `None`) and return a complete
/// `.flac` byte stream ready for a decoder.
fn encode_to_flac(
    format: SampleFormat,
    channels: u16,
    sample_rate: u32,
    pcm: &[Vec<i32>],
    opts: Option<oxideav_flac::encoder::FlacEncoderOptions>,
) -> Vec<u8> {
    let n = pcm[0].len();
    let mut params = CodecParameters::audio(CodecId::new("flac"));
    params.channels = Some(channels);
    params.sample_rate = Some(sample_rate);
    params.sample_format = Some(format);

    let mut enc = match opts {
        Some(o) => oxideav_flac::encoder::make_encoder_with_options(&params, o)
            .expect("make_encoder_with_options"),
        None => oxideav_flac::encoder::make_encoder(&params).expect("make_encoder"),
    };

    let frame = AudioFrame {
        samples: n as u32,
        pts: Some(0),
        data: vec![interleave(format, channels, pcm)],
    };
    enc.send_frame(&Frame::Audio(frame)).expect("send_frame");
    enc.flush().expect("flush");

    let mut packets: Vec<Packet> = Vec::new();
    loop {
        match enc.receive_packet() {
            Ok(p) => packets.push(p),
            Err(Error::NeedMore) | Err(Error::Eof) => break,
            Err(e) => panic!("unexpected encoder error: {e:?}"),
        }
    }
    assert!(!packets.is_empty(), "encoder produced no packets");

    // After flush(), output_params().extradata is the finalised
    // metadata chain (STREAMINFO with the real MD5 / frame sizes / total
    // samples, plus any PADDING). A complete native FLAC stream is the
    // `fLaC` magic, the metadata chain, then the frame packets.
    let extradata = enc.output_params().extradata.clone();
    let mut flac = Vec::with_capacity(4 + extradata.len());
    flac.extend_from_slice(b"fLaC");
    flac.extend_from_slice(&extradata);
    for p in &packets {
        flac.extend_from_slice(&p.data);
    }
    flac
}

/// The raw-PCM flags the reference decoder needs to emit headerless
/// little-endian signed samples for `format` (the byte width the
/// decoder writes per sample matches `format.bytes_per_sample()`,
/// except U8 which the decoder emits as unsigned 8-bit).
struct RawFlags {
    /// Width in bytes the reference decoder writes per output sample.
    out_bytes: usize,
    sign: &'static str,
}

fn raw_flags(format: SampleFormat) -> RawFlags {
    match format {
        SampleFormat::U8 => RawFlags {
            out_bytes: 1,
            sign: "unsigned",
        },
        SampleFormat::S16 => RawFlags {
            out_bytes: 2,
            sign: "signed",
        },
        SampleFormat::S24 => RawFlags {
            out_bytes: 3,
            sign: "signed",
        },
        SampleFormat::S32 => RawFlags {
            out_bytes: 4,
            sign: "signed",
        },
        _ => panic!("unsupported format {format:?}"),
    }
}

/// Decode `flac_bytes` with the reference binary into interleaved
/// per-channel `i32` samples (one inner Vec per channel).
fn reference_decode(
    flac: &str,
    flac_bytes: &[u8],
    format: SampleFormat,
    channels: u16,
    tag: &str,
) -> Vec<Vec<i32>> {
    let dir = std::env::temp_dir();
    let mut in_path = PathBuf::from(&dir);
    in_path.push(format!(
        "oxideav-flac-oracle-{tag}-{}.flac",
        std::process::id()
    ));
    let mut out_path = PathBuf::from(&dir);
    out_path.push(format!(
        "oxideav-flac-oracle-{tag}-{}.raw",
        std::process::id()
    ));

    std::fs::write(&in_path, flac_bytes).expect("write flac temp");

    let rf = raw_flags(format);
    // On decode the output geometry (channels / bps) comes from
    // STREAMINFO; only the raw container framing needs spelling out.
    let out = Command::new(flac)
        .args([
            "-d",
            "-s",
            "-f",
            "--force-raw-format",
            "--endian=little",
            &format!("--sign={}", rf.sign),
        ])
        .arg("-o")
        .arg(&out_path)
        .arg(&in_path)
        .output()
        .expect("spawn flac");

    if !out.status.success() {
        let _ = std::fs::remove_file(&in_path);
        panic!(
            "reference flac decode failed ({tag}): {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    let raw = std::fs::read(&out_path).expect("read decoded raw");
    let _ = std::fs::remove_file(&in_path);
    let _ = std::fs::remove_file(&out_path);

    // Deinterleave. Each output sample is `out_bytes` bytes
    // little-endian; U8 is unsigned (subtract 128 to recover the
    // encoder's signed input), the others are signed two's-complement.
    let mut planes: Vec<Vec<i32>> = vec![Vec::new(); channels as usize];
    let stride = rf.out_bytes;
    for (idx, chunk) in raw.chunks_exact(stride).enumerate() {
        let v = match format {
            SampleFormat::U8 => chunk[0] as i32 - 128,
            SampleFormat::S16 => i16::from_le_bytes([chunk[0], chunk[1]]) as i32,
            SampleFormat::S24 => {
                let mut v =
                    (chunk[0] as i32) | ((chunk[1] as i32) << 8) | ((chunk[2] as i32) << 16);
                if v & 0x0080_0000 != 0 {
                    v |= 0xFF00_0000_u32 as i32;
                }
                v
            }
            SampleFormat::S32 => i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]),
            _ => unreachable!(),
        };
        planes[idx % channels as usize].push(v);
    }
    planes
}

/// Full oracle: encode `pcm` (optionally with `opts`), decode the bytes
/// with the reference binary, and assert sample-exact recovery on every
/// channel. Skips (returns) when the reference binary is unavailable.
fn assert_reference_roundtrip(
    format: SampleFormat,
    channels: u16,
    sample_rate: u32,
    pcm: Vec<Vec<i32>>,
    opts: Option<oxideav_flac::encoder::FlacEncoderOptions>,
    tag: &str,
) {
    let Some(flac) = flac_binary() else {
        eprintln!("skip {tag}: reference `flac` binary not on PATH");
        return;
    };
    assert_eq!(pcm.len(), channels as usize);
    let n = pcm[0].len();
    for p in &pcm {
        assert_eq!(p.len(), n, "channel length mismatch");
    }

    let flac_bytes = encode_to_flac(format, channels, sample_rate, &pcm, opts);
    let recovered = reference_decode(&flac, &flac_bytes, format, channels, tag);

    assert_eq!(
        recovered.len(),
        channels as usize,
        "{tag}: channel count mismatch"
    );
    for (c, plane) in recovered.iter().enumerate() {
        assert_eq!(
            plane.len(),
            n,
            "{tag}: channel {c} sample count mismatch ({} vs {n})",
            plane.len()
        );
        for i in 0..n {
            assert_eq!(
                plane[i], pcm[c][i],
                "{tag}: reference decoder mismatch at channel {c} sample {i}"
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

/// A deterministic pseudo-random walk bounded to `±amp`: exercises the
/// near-white-noise regime where VERBATIM / high Rice parameters bite.
fn noisy_walk(n: usize, amp: i32, seed: u64) -> Vec<i32> {
    let mut s = seed;
    let mut v: i32 = 0;
    (0..n)
        .map(|_| {
            s = s
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let step = ((s >> 33) as i32 & 0x7FF) - 0x400;
            v = (v + step).clamp(-amp, amp);
            v
        })
        .collect()
}

// --- Tests ----------------------------------------------------------------

#[test]
fn reference_decodes_s16_stereo_tonal() {
    let sr = 44_100;
    let n = 9000;
    let l = sine(n, sr, 440.0, 18_000.0);
    let r = sine(n, sr, 660.0, 12_000.0);
    assert_reference_roundtrip(
        SampleFormat::S16,
        2,
        sr,
        vec![l, r],
        None,
        "s16-stereo-tonal",
    );
}

#[test]
fn reference_decodes_s16_mono_noise() {
    let n = 7000;
    let s = noisy_walk(n, 30_000, 0xDEAD_BEEF);
    assert_reference_roundtrip(
        SampleFormat::S16,
        1,
        48_000,
        vec![s],
        None,
        "s16-mono-noise",
    );
}

#[test]
fn reference_decodes_u8_mono() {
    let s: Vec<i32> = (0..4096i32)
        .map(|i| ((i % 200) - 100).clamp(-127, 127))
        .collect();
    assert_reference_roundtrip(SampleFormat::U8, 1, 8_000, vec![s], None, "u8-mono");
}

#[test]
fn reference_decodes_s24_mono_ramp() {
    let n = 5000;
    let mut s = Vec::with_capacity(n);
    let mut v: i32 = -0x7F_FFFF;
    for _ in 0..n {
        s.push(v);
        v = v.wrapping_add(0x1234);
        if v > 0x7F_FFFF {
            v = -0x7F_FFFF;
        }
    }
    assert_reference_roundtrip(SampleFormat::S24, 1, 96_000, vec![s], None, "s24-mono-ramp");
}

#[test]
fn reference_decodes_s24_stereo_tonal() {
    let sr = 96_000;
    let n = 6000;
    let l = sine(n, sr, 880.0, 4_000_000.0);
    let r = sine(n, sr, 1320.0, 3_000_000.0);
    assert_reference_roundtrip(
        SampleFormat::S24,
        2,
        sr,
        vec![l, r],
        None,
        "s24-stereo-tonal",
    );
}

#[test]
fn reference_decodes_s32_stereo() {
    let sr = 192_000;
    let n = 4000;
    let l = sine(n, sr, 1000.0, 1.0e9);
    let r = sine(n, sr, 1500.0, 0.8e9);
    assert_reference_roundtrip(SampleFormat::S32, 2, sr, vec![l, r], None, "s32-stereo");
}

#[test]
fn reference_decodes_surround_7_1() {
    let sr = 48_000;
    let n = 3000;
    let pcm: Vec<Vec<i32>> = (0..8)
        .map(|c| sine(n, sr, 200.0 + 80.0 * c as f64, 2_000_000.0))
        .collect();
    assert_reference_roundtrip(SampleFormat::S24, 8, sr, pcm, None, "surround-7_1");
}

#[test]
fn reference_decodes_constant_silence() {
    // All-zero block -> CONSTANT subframes. Exercises the CONSTANT
    // subframe path through a foreign decoder.
    let n = 4096;
    let z = vec![0i32; n];
    assert_reference_roundtrip(
        SampleFormat::S16,
        2,
        44_100,
        vec![z.clone(), z],
        None,
        "constant-silence",
    );
}

#[test]
fn reference_decodes_wasted_bits() {
    // Every sample is a multiple of 64 -> 6 wasted bits per subframe.
    // Confirms the foreign decoder reconstructs the shift correctly.
    let sr = 44_100;
    let n = 5000;
    let l: Vec<i32> = sine(n, sr, 440.0, 280.0).iter().map(|v| v << 6).collect();
    let r: Vec<i32> = sine(n, sr, 550.0, 200.0).iter().map(|v| v << 6).collect();
    assert_reference_roundtrip(SampleFormat::S16, 2, sr, vec![l, r], None, "wasted-bits");
}

#[test]
fn reference_decodes_small_block_size() {
    // Forces many short frames -> the last-frame remainder path and the
    // per-frame header / CRC are exercised repeatedly.
    let sr = 44_100;
    let n = 5000;
    let l = sine(n, sr, 440.0, 18_000.0);
    let r = noisy_walk(n, 20_000, 0x1234_5678);
    let mut opts = oxideav_flac::encoder::FlacEncoderOptions::default();
    opts.block_size = Some(256);
    assert_reference_roundtrip(
        SampleFormat::S16,
        2,
        sr,
        vec![l, r],
        Some(opts),
        "small-block",
    );
}

#[test]
fn reference_decodes_with_padding() {
    let sr = 44_100;
    let n = 4000;
    let l = sine(n, sr, 330.0, 16_000.0);
    let r = sine(n, sr, 495.0, 14_000.0);
    let mut opts = oxideav_flac::encoder::FlacEncoderOptions::default();
    opts.padding_bytes = Some(1024);
    assert_reference_roundtrip(
        SampleFormat::S16,
        2,
        sr,
        vec![l, r],
        Some(opts),
        "with-padding",
    );
}

#[test]
fn reference_decodes_non_subset_high_lpc_and_partition() {
    // Non-subset configuration: LPC order ceiling 32 and partition order
    // ceiling 15. A spec-complete decoder must still recover it exactly.
    let sr = 96_000;
    let n = 6000;
    let l = sine(n, sr, 700.0, 5_000_000.0);
    let r = noisy_walk(n, 4_000_000, 0xCAFE_F00D);
    let mut opts = oxideav_flac::encoder::FlacEncoderOptions::default();
    opts.max_lpc_order = Some(32);
    opts.allow_non_subset_partition_order = true;
    assert_reference_roundtrip(
        SampleFormat::S24,
        2,
        sr,
        vec![l, r],
        Some(opts),
        "non-subset",
    );
}

#[test]
fn reference_decodes_fast_preset() {
    let sr = 44_100;
    let n = 6000;
    let l = sine(n, sr, 440.0, 18_000.0);
    let r = sine(n, sr, 660.0, 12_000.0);
    assert_reference_roundtrip(
        SampleFormat::S16,
        2,
        sr,
        vec![l, r],
        Some(oxideav_flac::encoder::FlacEncoderOptions::fast()),
        "fast-preset",
    );
}
