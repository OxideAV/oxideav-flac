//! Opt-out STREAMINFO MD5 (`FlacEncoderOptions::with_md5(false)`, registry
//! key `md5`): RFC 9639 §8.2 defines an all-zero signature as "not
//! computed". Switching the digest off must leave every frame
//! byte-identical and zero exactly the 16 signature bytes, on the serial
//! and the frame-parallel path; our verify path must report the stream as
//! unsigned rather than as a mismatch; and the black-box `flac -t` must
//! accept the file (that part skips when `flac` is not on PATH).

use std::process::Command;

use oxideav_core::{AudioFrame, CodecId, CodecParameters, Error, Frame, SampleFormat};
use oxideav_flac::decoder::make_decoder;
use oxideav_flac::encoder::{FlacEncoderOptions, make_encoder, make_encoder_with_options};
use oxideav_flac::verify::{VerifyOutcome, verify_stream};

/// Offset of the MD5 in the encoder's extradata: the 4-byte STREAMINFO
/// block header, then the 34-byte STREAMINFO whose last 16 bytes are the
/// signature (RFC 9639 §8.2).
const MD5_RANGE: std::ops::Range<usize> = 4 + 18..4 + 34;

/// xorshift32 step.
fn xs(st: &mut u32) -> u32 {
    *st ^= *st << 13;
    *st ^= *st >> 17;
    *st ^= *st << 5;
    *st
}

/// Deterministic integer PCM for `channels` channels in `fmt`: per-channel
/// triangle tones plus noise.
fn signal(n: usize, channels: usize, fmt: SampleFormat) -> Vec<u8> {
    let bits = match fmt {
        SampleFormat::U8 => 8,
        SampleFormat::S16 => 16,
        SampleFormat::S24 => 24,
        _ => 32,
    };
    let amp = 1i64 << (bits - 2);
    let mut st = 0x9E37_79B9u32;
    let mut out = Vec::new();
    for i in 0..n as i64 {
        for c in 0..channels as i64 {
            let period = 60 + 23 * c;
            let ph = i % period;
            let tri = if ph < period / 2 { ph } else { period - ph };
            let noise = (xs(&mut st) as i32 >> (36 - bits).min(31)) as i64;
            let v = (tri * 2 * amp) / period - amp / 2 + noise;
            let v = v.clamp(-(1i64 << (bits - 1)), (1i64 << (bits - 1)) - 1);
            match fmt {
                SampleFormat::U8 => out.push((v + 128) as u8),
                SampleFormat::S16 => out.extend_from_slice(&(v as i16).to_le_bytes()),
                SampleFormat::S24 => out.extend_from_slice(&(v as i32).to_le_bytes()[..3]),
                _ => out.extend_from_slice(&(v as i32).to_le_bytes()),
            }
        }
    }
    out
}

fn params(channels: u16, rate: u32, fmt: SampleFormat) -> CodecParameters {
    let mut p = CodecParameters::audio(CodecId::new("flac"));
    p.channels = Some(channels);
    p.sample_rate = Some(rate);
    p.sample_format = Some(fmt);
    p
}

/// One encode run: frame bytes per packet, packet `(pts, duration)`, and
/// the final STREAMINFO chain.
type Run = (Vec<Vec<u8>>, Vec<(Option<i64>, Option<i64>)>, Vec<u8>);

/// Drain every packet the encoder has ready.
fn drain(enc: &mut Box<dyn oxideav_core::Encoder>, run: &mut Run) {
    loop {
        match enc.receive_packet() {
            Ok(pkt) => {
                run.1.push((pkt.pts, pkt.duration));
                run.0.push(pkt.data);
            }
            Err(Error::NeedMore) | Err(Error::Eof) => break,
            Err(e) => panic!("encoder error: {e:?}"),
        }
    }
}

/// Encode `pcm` with `enc`, 3000 interchannel samples per `send_frame`.
fn run_encoder(mut enc: Box<dyn oxideav_core::Encoder>, pcm: &[u8], frame_bytes: usize) -> Run {
    let mut run: Run = (Vec::new(), Vec::new(), Vec::new());
    for piece in pcm.chunks(3000 * frame_bytes) {
        enc.send_frame(&Frame::Audio(AudioFrame {
            samples: (piece.len() / frame_bytes) as u32,
            pts: None,
            data: vec![piece.to_vec()],
        }))
        .expect("send_frame");
        drain(&mut enc, &mut run);
    }
    enc.flush().expect("flush");
    drain(&mut enc, &mut run);
    run.2 = enc.output_params().extradata.clone();
    run
}

fn encode(pcm: &[u8], p: &CodecParameters, opts: FlacEncoderOptions) -> Run {
    let frame_bytes = p.sample_format.unwrap().bytes_per_sample() * p.channels.unwrap() as usize;
    run_encoder(
        make_encoder_with_options(p, opts).expect("encoder"),
        pcm,
        frame_bytes,
    )
}

/// `fLaC` + metadata chain + frames.
fn file(run: &Run) -> Vec<u8> {
    let mut out = b"fLaC".to_vec();
    out.extend_from_slice(&run.2);
    for f in &run.0 {
        out.extend_from_slice(f);
    }
    out
}

#[test]
fn md5_off_zeroes_only_the_signature_at_every_thread_count() {
    let cases = [
        (2u16, 44_100u32, SampleFormat::S16),
        (1, 22_050, SampleFormat::U8),
        (2, 96_000, SampleFormat::S24),
        (6, 48_000, SampleFormat::S32),
    ];
    for (ch, rate, fmt) in cases {
        // More frames than the 16-block in-flight window of 8 threads,
        // plus a short tail frame.
        let pcm = signal(30_000, ch as usize, fmt);
        let p = params(ch, rate, fmt);
        for level in [0u8, 5, 8] {
            let opts = || FlacEncoderOptions::level(level).with_block_size(1024);
            let signed = encode(&pcm, &p, opts());
            assert!(
                signed.2[MD5_RANGE].iter().any(|&b| b != 0),
                "default encode must carry a signature"
            );
            let mut want = signed.clone();
            want.2[MD5_RANGE].fill(0);
            for threads in [1usize, 2, 4, 8] {
                let got = encode(&pcm, &p, opts().with_threads(threads).with_md5(false));
                assert!(
                    got == want,
                    "{ch} ch {fmt:?} level {level}, {threads} threads: md5=false must \
                     differ from the signed encode in the 16 signature bytes only"
                );
                // md5 on stays byte-identical to the serial encoder.
                assert!(
                    encode(&pcm, &p, opts().with_threads(threads).with_md5(true)) == signed,
                    "{ch} ch {fmt:?} level {level}, {threads} threads: md5=true changed"
                );
            }
        }
    }
}

#[test]
fn md5_off_with_verify_and_padding_keeps_the_frames() {
    let pcm = signal(20_000, 2, SampleFormat::S16);
    let p = params(2, 44_100, SampleFormat::S16);
    let opts = || {
        FlacEncoderOptions::level(8)
            .with_verify(true)
            .with_padding_bytes(64)
            .with_block_size(1152)
    };
    let mut want = encode(&pcm, &p, opts());
    want.2[MD5_RANGE].fill(0);
    for threads in [1usize, 2, 4, 8] {
        assert!(encode(&pcm, &p, opts().with_threads(threads).with_md5(false)) == want);
    }
}

#[test]
fn registry_md5_key_round_trips() {
    let pcm = signal(12_000, 2, SampleFormat::S16);
    let base = params(2, 44_100, SampleFormat::S16);
    let via_options = |opts: &[(&str, &str)]| {
        let mut p = base.clone();
        for &(k, v) in opts {
            p.options.insert(k, v);
        }
        Ok::<_, Error>(run_encoder(make_encoder(&p)?, &pcm, 4))
    };
    let typed = |o: FlacEncoderOptions| encode(&pcm, &base, o);

    let off = via_options(&[("md5", "false")]).unwrap();
    assert!(off == typed(FlacEncoderOptions::default().with_md5(false)));
    assert!(off.2[MD5_RANGE].iter().all(|&b| b == 0));

    let on = via_options(&[("md5", "true")]).unwrap();
    assert!(on == typed(FlacEncoderOptions::default()));
    assert!(on == via_options(&[]).unwrap(), "md5 defaults to true");
    assert!(on.2[MD5_RANGE].iter().any(|&b| b != 0));

    // Combines with the other keys; strict parsing still applies.
    let combo = via_options(&[("compression_level", "8"), ("md5", "false")]).unwrap();
    assert!(combo == typed(FlacEncoderOptions::level(8).with_md5(false)));
    assert!(via_options(&[("md5", "maybe")]).is_err());
}

#[test]
fn unsigned_stream_verifies_as_no_signature_and_decodes() {
    let pcm = signal(25_000, 2, SampleFormat::S16);
    let p = params(2, 44_100, SampleFormat::S16);
    let signed = encode(&pcm, &p, FlacEncoderOptions::default());
    let unsigned = encode(&pcm, &p, FlacEncoderOptions::default().with_md5(false));

    assert_eq!(verify_stream(&file(&signed)).unwrap(), VerifyOutcome::Match);
    // An all-zero signature is "not computed", never a mismatch.
    assert_eq!(
        verify_stream(&file(&unsigned)).unwrap(),
        VerifyOutcome::NoSignature
    );
    // Control: a non-zero signature that disagrees is still a mismatch.
    let mut wrong = signed.clone();
    wrong.2[MD5_RANGE.start] ^= 0x01;
    assert!(verify_stream(&file(&wrong)).is_err());

    // The streaming decoder, handed the unsigned STREAMINFO, reconstructs
    // the input exactly.
    let mut dp = p.clone();
    dp.extradata = unsigned.2.clone();
    let mut dec = make_decoder(&dp).expect("decoder");
    let mut out = Vec::new();
    for data in &unsigned.0 {
        let pkt = oxideav_core::Packet::new(0, oxideav_core::TimeBase::new(1, 1), data.clone());
        dec.send_packet(&pkt).expect("send_packet");
        while let Ok(Frame::Audio(a)) = dec.receive_frame() {
            out.extend_from_slice(&a.data[0]);
        }
    }
    assert_eq!(out, pcm);
}

/// Black box: the reference `flac` command-line tool's `-t` (test) mode
/// accepts both the signed and the unsigned file.
#[test]
fn reference_flac_test_accepts_unsigned_stream() {
    if Command::new("flac").arg("--version").output().is_err() {
        eprintln!("skip reference_flac_test_accepts_unsigned_stream: `flac` not on PATH");
        return;
    }
    let pcm = signal(40_000, 2, SampleFormat::S16);
    let p = params(2, 44_100, SampleFormat::S16);
    let dir = std::env::temp_dir().join(format!("oxideav-flac-md5-off-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (name, threads, md5) in [("signed", 1usize, true), ("unsigned", 4, false)] {
        let run = encode(
            &pcm,
            &p,
            FlacEncoderOptions::default()
                .with_threads(threads)
                .with_md5(md5),
        );
        let path = dir.join(format!("{name}.flac"));
        std::fs::write(&path, file(&run)).unwrap();
        let out = Command::new("flac")
            .arg("-t")
            .arg("--silent")
            .arg(&path)
            .output()
            .expect("run flac -t");
        assert!(
            out.status.success(),
            "flac -t rejected the {name} stream: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let _ = std::fs::remove_dir_all(&dir);
}
