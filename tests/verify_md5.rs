//! Integration tests for the verify-decoder MD5 path (RFC 9639 §9.2.1).
//!
//! Each reference fixture under `docs/audio/flac/fixtures/<name>/input.flac`
//! is a complete `.flac` stream produced by a black-box reference encoder.
//! Its STREAMINFO carries that encoder's MD5 of the *original* PCM. Running
//! `oxideav_flac::verify::verify_stream` over the file decodes every frame
//! with this crate's own decoder, recomputes the MD5 from the reconstructed
//! samples, and compares: a `VerifyOutcome::Match` is independent proof that
//! our decode is sample-exact against an *external* oracle (the reference
//! encoder), not just self-consistent with our own encoder.
//!
//! When the `docs/` submodule is not checked out, the fixtures are absent;
//! the test logs a skip and passes (CI without the private submodule still
//! goes green).

use std::path::PathBuf;

use oxideav_core::{AudioFrame, CodecId, CodecParameters, Error, Frame, SampleFormat};
use oxideav_flac::verify::{verify_stream, VerifyOutcome};

fn fixtures_root() -> PathBuf {
    PathBuf::from("../../docs/audio/flac/fixtures")
}

/// Every fixture directory that ships an `input.flac` reference stream.
const FIXTURES: &[&str] = &[
    "constant-subframe-silence",
    "fixed-subframe-low-order",
    "left-right-channel-pair",
    "left-side-channel-pair",
    "lpc-subframe-typical-music",
    "mid-side-channel-pair",
    "mono-16bit-44100-fixed-blocksize",
    "mono-8bit-22050",
    "right-side-channel-pair",
    "stereo-16bit-44100-fixed",
    "stereo-16bit-44100-small-blocksize",
    "stereo-24bit-96000",
    "stereo-32bit-192000",
    "surround-7_1-24bit-48000",
    "verbatim-subframe-noise",
    "with-padding-block",
    "with-picture-block",
    "with-vorbis-comment",
];

/// Whole-stream MD5 verification across the reference corpus. Every
/// reference `input.flac` must verify `Match` (or `NoSignature` if the
/// encoder stored no MD5) — a mismatch means our decode diverged from the
/// original PCM the reference encoder hashed.
#[test]
fn reference_corpus_verifies_md5() {
    let root = fixtures_root();
    if !root.exists() {
        eprintln!("skip reference_corpus_verifies_md5: docs/ submodule absent");
        return;
    }

    let mut checked = 0usize;
    let mut matched = 0usize;
    let mut no_sig = 0usize;
    for name in FIXTURES {
        let path = root.join(name).join("input.flac");
        let Ok(bytes) = std::fs::read(&path) else {
            eprintln!("skip {name}: {} not readable", path.display());
            continue;
        };
        checked += 1;
        match verify_stream(&bytes) {
            Ok(VerifyOutcome::Match) => {
                matched += 1;
            }
            Ok(VerifyOutcome::NoSignature) => {
                no_sig += 1;
                eprintln!("note {name}: STREAMINFO has no MD5 signature (unverifiable)");
            }
            Err(e) => panic!("{name}: verify_stream failed: {e}"),
        }
    }

    if checked == 0 {
        eprintln!("skip reference_corpus_verifies_md5: no input.flac fixtures found");
        return;
    }
    eprintln!("verified {checked} fixtures: {matched} Match, {no_sig} NoSignature");
    // Every fixture that ships a signature must verify; at least one of the
    // corpus is expected to carry a real (non-zero) MD5.
    assert!(
        matched + no_sig == checked,
        "all readable fixtures must produce a definite verify outcome",
    );
    assert!(
        matched > 0,
        "at least one reference fixture should carry a real MD5 that verifies Match",
    );
}

/// Flipping a single sample byte inside a frame body must make verification
/// fail — proves the check actually depends on the decoded PCM, not just on
/// metadata parsing succeeding.
#[test]
fn corrupted_sample_fails_verification() {
    let root = fixtures_root();
    if !root.exists() {
        eprintln!("skip corrupted_sample_fails_verification: docs/ submodule absent");
        return;
    }
    // Use a fixture known to carry a real MD5; the mono fixed-blocksize file
    // is a plain mono stream so a body-byte flip lands on real audio.
    let path = root
        .join("mono-16bit-44100-fixed-blocksize")
        .join("input.flac");
    let Ok(mut bytes) = std::fs::read(&path) else {
        eprintln!("skip corrupted_sample_fails_verification: fixture absent");
        return;
    };

    // Confirm the pristine file verifies first; if it has no signature we
    // cannot test corruption detection against it.
    match verify_stream(&bytes) {
        Ok(VerifyOutcome::Match) => {}
        Ok(VerifyOutcome::NoSignature) => {
            eprintln!("skip corrupted_sample_fails_verification: fixture has no MD5");
            return;
        }
        Err(e) => panic!("pristine fixture failed to verify: {e}"),
    }

    // Flip one bit deep in the tail, well past the metadata chain and the
    // first frame header, so we mutate a residual/sample byte rather than a
    // sync code. The per-frame CRC-16 will reject the mutated frame OR the
    // MD5 will mismatch — either way verification must NOT return Match.
    let idx = bytes.len() - 8;
    bytes[idx] ^= 0x01;

    match verify_stream(&bytes) {
        Ok(VerifyOutcome::Match) => {
            panic!("corrupted stream wrongly verified as Match");
        }
        // A non-Match outcome (CRC-16 rejection, MD5 mismatch, or a parse
        // error) is the correct rejection of a corrupted stream.
        Ok(VerifyOutcome::NoSignature) => {
            panic!("corrupted stream unexpectedly reported NoSignature");
        }
        Err(_) => {}
    }
}

/// Interleave per-channel i32 PCM into the encoder's expected byte layout
/// for `format`, matching the decode-side packing in `src/decoder.rs`.
#[allow(clippy::needless_range_loop)]
fn interleave(format: SampleFormat, per_channel: &[Vec<i32>]) -> Vec<u8> {
    let channels = per_channel.len();
    let n = per_channel[0].len();
    let bps = format.bytes_per_sample();
    let mut out = Vec::with_capacity(n * channels * bps);
    for i in 0..n {
        for c in 0..channels {
            let s = per_channel[c][i];
            match format {
                SampleFormat::U8 => out.push(((s + 128) & 0xFF) as u8),
                SampleFormat::S16 => out.extend_from_slice(&(s as i16).to_le_bytes()),
                SampleFormat::S24 => {
                    out.push((s & 0xFF) as u8);
                    out.push(((s >> 8) & 0xFF) as u8);
                    out.push(((s >> 16) & 0xFF) as u8);
                }
                SampleFormat::S32 => out.extend_from_slice(&s.to_le_bytes()),
                _ => panic!("unsupported test format {format:?}"),
            }
        }
    }
    out
}

/// Encode PCM through the public encoder, assemble a complete `fLaC` stream
/// from the post-flush extradata (the metadata chain, MD5 included) and the
/// emitted frame packets, then `verify_stream` it. A `Match` proves the
/// encoder hashed exactly the audio it encoded and our decoder reconstructs
/// it sample-exactly — the encode/decode loop closed through the MD5 the
/// encoder itself stamped into STREAMINFO at flush.
fn encode_then_verify(format: SampleFormat, channels: u16, sr: u32, per_channel: Vec<Vec<i32>>) {
    let n = per_channel[0].len();

    let mut params = CodecParameters::audio(CodecId::new("flac"));
    params.channels = Some(channels);
    params.sample_rate = Some(sr);
    params.sample_format = Some(format);
    let mut enc = oxideav_flac::encoder::make_encoder(&params).expect("make_encoder");

    let frame = AudioFrame {
        samples: n as u32,
        pts: Some(0),
        data: vec![interleave(format, &per_channel)],
    };
    enc.send_frame(&Frame::Audio(frame)).expect("send_frame");
    enc.flush().expect("flush");

    let mut frame_bytes: Vec<u8> = Vec::new();
    loop {
        match enc.receive_packet() {
            Ok(p) => frame_bytes.extend_from_slice(&p.data),
            Err(Error::NeedMore) | Err(Error::Eof) => break,
            Err(e) => panic!("encoder error: {e:?}"),
        }
    }

    // The post-flush extradata carries the live STREAMINFO (with the MD5
    // the encoder computed over the input PCM). Assemble fLaC + chain +
    // frames into a complete stream.
    let extradata = enc.output_params().extradata.clone();
    let mut stream = Vec::with_capacity(4 + extradata.len() + frame_bytes.len());
    stream.extend_from_slice(b"fLaC");
    stream.extend_from_slice(&extradata);
    stream.extend_from_slice(&frame_bytes);

    assert_eq!(
        verify_stream(&stream).expect("verify_stream"),
        VerifyOutcome::Match,
        "encode->verify roundtrip must match the encoder-stamped MD5 \
         ({channels}ch {format:?} @ {sr} Hz)",
    );
}

/// Encode/decode loop closed through the encoder-stamped MD5 across bit
/// depths and channel layouts (mono, stereo — exercising the stereo
/// decorrelation search — and 7.1 surround).
#[test]
fn encode_roundtrip_verifies_md5() {
    // Mono 8-bit.
    let m8: Vec<i32> = (0..2000).map(|i| (i % 200) - 100).collect();
    encode_then_verify(SampleFormat::U8, 1, 8_000, vec![m8]);

    // Stereo 16-bit sine pair (drives the L/R, L-S, R-S, M-S search).
    let sr = 48_000u32;
    let n = 5000usize;
    let mut l = Vec::with_capacity(n);
    let mut r = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f64 / sr as f64;
        l.push(((t * 440.0 * std::f64::consts::TAU).sin() * 18_000.0) as i32);
        r.push(((t * 660.0 * std::f64::consts::TAU).sin() * 12_000.0) as i32);
    }
    encode_then_verify(SampleFormat::S16, 2, sr, vec![l, r]);

    // Mono 24-bit ramp.
    let mut s24 = Vec::with_capacity(3000);
    let mut v: i32 = -0x7F_FFFF;
    for _ in 0..3000 {
        s24.push(v);
        v = v.wrapping_add(0x1234);
        if v > 0x7F_FFFF {
            v = -0x7F_FFFF;
        }
    }
    encode_then_verify(SampleFormat::S24, 1, 44_100, vec![s24]);

    // 7.1 surround 24-bit: 8 independent channels.
    let n8 = 1500usize;
    let mut surround: Vec<Vec<i32>> = Vec::with_capacity(8);
    for ch in 0..8 {
        let mut chan = Vec::with_capacity(n8);
        for i in 0..n8 {
            let t = i as f64 / 48_000.0;
            let f = 100.0 * (ch as f64 + 1.0);
            chan.push(((t * f * std::f64::consts::TAU).sin() * 500_000.0) as i32);
        }
        surround.push(chan);
    }
    encode_then_verify(SampleFormat::S24, 8, 48_000, surround);
}
