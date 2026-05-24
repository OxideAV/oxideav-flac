//! End-to-end FLAC encoder -> decoder round-trip through the public
//! trait objects (not the internal helper functions). Verifies bit-exact
//! PCM recovery (FLAC is lossless) for several sample formats and
//! channel counts.

use oxideav_core::{AudioFrame, CodecId, CodecParameters, Error, Frame, Packet, SampleFormat};

fn build_audio_frame(
    format: SampleFormat,
    channels: u16,
    sample_rate: u32,
    pcm_per_channel: &[Vec<i32>],
) -> AudioFrame {
    assert_eq!(pcm_per_channel.len(), channels as usize);
    let n = pcm_per_channel[0].len();
    for c in pcm_per_channel {
        assert_eq!(c.len(), n);
    }
    let bytes_per_sample = format.bytes_per_sample();
    let mut interleaved: Vec<u8> = Vec::with_capacity(n * channels as usize * bytes_per_sample);
    #[allow(clippy::needless_range_loop)]
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
                _ => panic!("unsupported format for test: {:?}", format),
            }
        }
    }
    let _ = sample_rate;
    AudioFrame {
        samples: n as u32,
        pts: Some(0),
        data: vec![interleaved],
    }
}

fn decode_interleaved(a: &AudioFrame, format: SampleFormat, channels: u16) -> Vec<i32> {
    let n_ch = channels as usize;
    let bps = format.bytes_per_sample();
    let mut out = Vec::with_capacity(a.samples as usize * n_ch);
    for chunk in a.data[0].chunks_exact(bps * n_ch) {
        for c in 0..n_ch {
            let off = c * bps;
            let v = match format {
                SampleFormat::U8 => (chunk[off] as i32) - 128,
                SampleFormat::S16 => i16::from_le_bytes([chunk[off], chunk[off + 1]]) as i32,
                SampleFormat::S24 => {
                    let mut v = (chunk[off] as i32)
                        | ((chunk[off + 1] as i32) << 8)
                        | ((chunk[off + 2] as i32) << 16);
                    if v & 0x0080_0000 != 0 {
                        v |= 0xFF00_0000_u32 as i32;
                    }
                    v
                }
                SampleFormat::S32 => {
                    i32::from_le_bytes([chunk[off], chunk[off + 1], chunk[off + 2], chunk[off + 3]])
                }
                _ => panic!("unexpected format"),
            };
            out.push(v);
        }
    }
    out
}

fn roundtrip_through_traits(
    format: SampleFormat,
    channels: u16,
    sample_rate: u32,
    pcm_per_channel: Vec<Vec<i32>>,
) {
    let n = pcm_per_channel[0].len();

    let mut enc_params = CodecParameters::audio(CodecId::new("flac"));
    enc_params.channels = Some(channels);
    enc_params.sample_rate = Some(sample_rate);
    enc_params.sample_format = Some(format);
    let mut enc = oxideav_flac::encoder::make_encoder(&enc_params).expect("make_encoder");

    let dec_params = enc.output_params().clone();
    assert!(
        !dec_params.extradata.is_empty(),
        "encoder must emit extradata (STREAMINFO)"
    );
    let mut dec = oxideav_flac::decoder::make_decoder(&dec_params).expect("make_decoder");

    let frame = build_audio_frame(format, channels, sample_rate, &pcm_per_channel);
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
    assert!(
        !packets.is_empty(),
        "encoder must produce at least one packet"
    );

    // FLAC decoder maps bps -> output sample format:
    //   8       -> U8  (matches encoder's accepted U8 input)
    //   9..=16  -> S16
    //   17..=24 -> S24
    //   25..=32 -> S32
    let dec_format = match format.bytes_per_sample() {
        1 => SampleFormat::U8,
        2 => SampleFormat::S16,
        3 => SampleFormat::S24,
        _ => SampleFormat::S32,
    };
    let mut recovered: Vec<i32> = Vec::new();
    for pkt in packets {
        dec.send_packet(&pkt).expect("send_packet");
        let f = dec.receive_frame().expect("receive_frame");
        let Frame::Audio(a) = f else {
            panic!("expected audio frame");
        };
        recovered.extend(decode_interleaved(&a, dec_format, channels));
    }

    let n_ch = channels as usize;
    assert_eq!(recovered.len(), n * n_ch, "sample count mismatch");
    for i in 0..n {
        for c in 0..n_ch {
            assert_eq!(
                recovered[i * n_ch + c],
                pcm_per_channel[c][i],
                "mismatch at sample {i} channel {c}"
            );
        }
    }
}

#[test]
fn roundtrip_s16_stereo_sine_via_traits() {
    let sr = 48_000u32;
    let n = 5000usize;
    let mut l = Vec::with_capacity(n);
    let mut r = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f64 / sr as f64;
        l.push(((t * 440.0 * 2.0 * std::f64::consts::PI).sin() * 18_000.0) as i32);
        r.push(((t * 660.0 * 2.0 * std::f64::consts::PI).sin() * 12_000.0) as i32);
    }
    roundtrip_through_traits(SampleFormat::S16, 2, sr, vec![l, r]);
}

#[test]
fn roundtrip_s24_mono_ramp_via_traits() {
    let n = 3000usize;
    let mut s = Vec::with_capacity(n);
    let mut v: i32 = -0x7F_FFFF;
    for _ in 0..n {
        s.push(v);
        v = v.wrapping_add(0x1234);
        if v > 0x7F_FFFF {
            v = -0x7F_FFFF;
        }
    }
    roundtrip_through_traits(SampleFormat::S24, 1, 44_100, vec![s]);
}

#[test]
fn roundtrip_u8_mono_via_traits() {
    let n = 2048usize;
    let mut s = Vec::with_capacity(n);
    for i in 0..n {
        let v = ((i as i32 % 200) - 100).clamp(-127, 127);
        s.push(v);
    }
    roundtrip_through_traits(SampleFormat::U8, 1, 8_000, vec![s]);
}

#[test]
fn roundtrip_s32_stereo_via_traits() {
    let sr = 96_000u32;
    let n = 2500usize;
    let mut l = Vec::with_capacity(n);
    let mut r = Vec::with_capacity(n);
    let mut acc: i32 = i32::MIN / 2;
    for i in 0..n {
        l.push(acc);
        acc = acc.wrapping_add(0x0010_0001);
        let t = i as f64 / sr as f64;
        r.push(((t * 220.0 * 2.0 * std::f64::consts::PI).sin() * (i32::MAX as f64 * 0.9)) as i32);
    }
    roundtrip_through_traits(SampleFormat::S32, 2, sr, vec![l, r]);
}

#[test]
fn roundtrip_multi_block_via_traits() {
    // More than one 4096-sample default block -> multiple packets.
    let sr = 48_000u32;
    let n = 10_000usize;
    let mut s = Vec::with_capacity(n);
    for i in 0..n {
        s.push((((i * 37) & 0xFFFF) as i32) - 0x8000);
    }
    roundtrip_through_traits(SampleFormat::S16, 1, sr, vec![s]);
}

/// Property-style sweep: a 1-second, 1 kHz mono sine at 8 kHz (one of
/// the spec's fixed sample-rate codes, §9.1.2) is encoded then decoded
/// for several amplitude scales × bit depths × DC offsets. For every
/// combination we assert:
///
/// 1. The round-trip is **bit-exact** in PCM, which is the FLAC
///    losslessness contract (RFC 9639 §1 "FLAC ... is a lossless
///    codec"). One mismatch anywhere is a hard failure.
/// 2. The compressed packet is **strictly smaller than the VERBATIM
///    payload** the encoder would emit if every plan lost out. A
///    1 kHz sine is exactly the kind of signal LPC was designed for,
///    so a healthy encoder should never end up writing the
///    unencoded-sample path; this catches a future regression where a
///    misconfigured search returns VERBATIM by default.
///
/// The sweep spans the two bit-depths the round-trip table already
/// covers (S16 and S24) at three amplitudes each — a near-clip
/// amplitude (0.95 × full scale, exercises long Rice runs at large
/// residuals), a moderate amplitude (0.1 × full scale, exercises the
/// LPC coefficient-precision search where neither too-narrow nor
/// too-wide is obviously better) and a small amplitude (0.01 × full
/// scale, exercises the wasted-bits fast path because most low bits
/// stay zero) — plus three DC offsets including zero. 2 × 3 × 3 = 18
/// independent encode/decode cycles; every one must pass.
#[test]
fn sine_sweep_bit_exact_and_compresses_below_verbatim() {
    const SR: u32 = 8_000; // FLAC fixed sample-rate code 0b0100 (RFC 9639 §9.1.2)
    const SECONDS: usize = 1;
    let n = SR as usize * SECONDS;

    struct Case {
        format: SampleFormat,
        bps: u32,
        // Multiplier in [0, 1] of full-scale amplitude.
        amp_scale: f64,
        // DC offset in PCM-domain LSBs.
        dc: i32,
    }

    fn full_scale(bps: u32) -> f64 {
        // Per RFC 9639 §11 PCM range: signed two's complement at `bps`
        // bits has [-2^(bps-1), 2^(bps-1) - 1]; we stay strictly inside
        // that range so the amp_scale=0.95 cases never clip.
        ((1u64 << (bps - 1)) - 1) as f64
    }

    let mut cases: Vec<Case> = Vec::new();
    for &(format, bps) in &[(SampleFormat::S16, 16u32), (SampleFormat::S24, 24u32)] {
        for &amp_scale in &[0.95f64, 0.1, 0.01] {
            for &dc in &[0i32, 7, -13] {
                cases.push(Case {
                    format,
                    bps,
                    amp_scale,
                    dc,
                });
            }
        }
    }
    assert_eq!(cases.len(), 18, "expected 18 sweep cases");

    for case in &cases {
        // Synthesise a 1 kHz mono sine — one cycle every 8 samples at
        // SR = 8 kHz, so n = 8000 samples is exactly 1000 full cycles.
        let amp = full_scale(case.bps) * case.amp_scale;
        let mut samples = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f64 / SR as f64;
            let s = (t * 1_000.0 * 2.0 * std::f64::consts::PI).sin() * amp;
            // Clamp into the legal PCM range after the DC bias so
            // borderline amp_scale=0.95 + dc values still produce
            // valid samples.
            let max = ((1i64 << (case.bps - 1)) - 1) as i32;
            let min = -(1i64 << (case.bps - 1)) as i32;
            samples.push((s as i32 + case.dc).clamp(min, max));
        }

        // Encode through the public trait interface — the same path a
        // downstream caller would use.
        let mut enc_params = CodecParameters::audio(CodecId::new("flac"));
        enc_params.channels = Some(1);
        enc_params.sample_rate = Some(SR);
        enc_params.sample_format = Some(case.format);
        let mut enc = oxideav_flac::encoder::make_encoder(&enc_params).expect("make_encoder");
        let frame = build_audio_frame(case.format, 1, SR, &[samples.clone()]);
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
        assert!(!packets.is_empty(), "case must emit at least one packet");
        let total_compressed_bits: usize = packets.iter().map(|p| p.data.len() * 8).sum();

        // (1) Compression sanity: stay strictly under VERBATIM-with-
        //     headers cost. VERBATIM literally writes `bps * n` payload
        //     bits per frame; even a fully-LPC frame carries some
        //     overhead (frame header, subframe header, footer / CRC,
        //     residual k parameters), so we compare against the raw
        //     `bps * n` figure as the loosest possible bound.
        let verbatim_bits = case.bps as usize * n;
        assert!(
            total_compressed_bits < verbatim_bits,
            "case bps={} amp={} dc={}: compressed {} bits >= verbatim {} bits",
            case.bps,
            case.amp_scale,
            case.dc,
            total_compressed_bits,
            verbatim_bits,
        );

        // (2) Bit-exact recovery — the lossless contract.
        let dec_format = match case.format.bytes_per_sample() {
            2 => SampleFormat::S16,
            3 => SampleFormat::S24,
            _ => unreachable!("sweep covers only S16 / S24"),
        };
        let dec_params = enc.output_params().clone();
        let mut dec = oxideav_flac::decoder::make_decoder(&dec_params).expect("make_decoder");
        let mut recovered: Vec<i32> = Vec::with_capacity(n);
        for pkt in packets {
            dec.send_packet(&pkt).expect("send_packet");
            let f = dec.receive_frame().expect("receive_frame");
            let Frame::Audio(a) = f else {
                panic!("expected audio frame");
            };
            recovered.extend(decode_interleaved(&a, dec_format, 1));
        }
        assert_eq!(
            recovered.len(),
            n,
            "case bps={} amp={} dc={}: sample count mismatch ({} vs {})",
            case.bps,
            case.amp_scale,
            case.dc,
            recovered.len(),
            n
        );
        for i in 0..n {
            assert_eq!(
                recovered[i], samples[i],
                "case bps={} amp={} dc={}: PCM mismatch at sample {}",
                case.bps, case.amp_scale, case.dc, i
            );
        }
    }
}

/// A clean stereo sine is exactly the kind of signal LPC + M/S should
/// obliterate. Verifies bit-exact round-trip through the full codec
/// trait interface plus the post-flush STREAMINFO updates (MD5 set,
/// min/max frame size populated, total sample count recorded).
#[test]
fn lpc_and_stereo_decorrelation_roundtrip_and_streaminfo() {
    let sr = 48_000u32;
    let n = 8192usize;
    let mut l = Vec::with_capacity(n);
    let mut r = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f64 / sr as f64;
        let base = (t * 440.0 * 2.0 * std::f64::consts::PI).sin() * 18_000.0;
        l.push(base as i32);
        r.push((base * 0.95 + 17.0) as i32);
    }

    let mut enc_params = CodecParameters::audio(CodecId::new("flac"));
    enc_params.channels = Some(2);
    enc_params.sample_rate = Some(sr);
    enc_params.sample_format = Some(SampleFormat::S16);
    let mut enc = oxideav_flac::encoder::make_encoder(&enc_params).expect("make_encoder");

    let frame = build_audio_frame(SampleFormat::S16, 2, sr, &[l.clone(), r.clone()]);
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
    assert!(!packets.is_empty());

    let final_params = enc.output_params().clone();
    let total_compressed: usize = packets.iter().map(|p| p.data.len()).sum();
    let verbatim_bits = n * 2 * 16;
    assert!(
        total_compressed * 8 < verbatim_bits / 2,
        "LPC + stereo decorrelation should compress below 50% of VERBATIM size; \
         got {total_compressed} bytes vs {} bytes verbatim-equivalent",
        verbatim_bits / 8
    );

    // STREAMINFO checks on the post-flush extradata.
    let md5_bytes = &final_params.extradata[4 + 18..4 + 34];
    assert!(
        md5_bytes.iter().any(|&b| b != 0),
        "post-flush STREAMINFO MD5 must be non-zero"
    );
    let min_fs = ((final_params.extradata[4 + 4] as u32) << 16)
        | ((final_params.extradata[4 + 5] as u32) << 8)
        | (final_params.extradata[4 + 6] as u32);
    let max_fs = ((final_params.extradata[4 + 7] as u32) << 16)
        | ((final_params.extradata[4 + 8] as u32) << 8)
        | (final_params.extradata[4 + 9] as u32);
    assert!(min_fs > 0 && max_fs >= min_fs, "frame-size bounds recorded");
    let packed = u64::from_be_bytes(
        final_params.extradata[4 + 10..4 + 18]
            .try_into()
            .expect("8 bytes"),
    );
    assert_eq!(packed & 0x0000_000F_FFFF_FFFF, n as u64, "total samples");

    // Decode back and verify bit-exact recovery via the decorrelation path.
    let mut dec = oxideav_flac::decoder::make_decoder(&final_params).expect("make_decoder");
    let mut recovered = Vec::new();
    for pkt in packets {
        dec.send_packet(&pkt).expect("send_packet");
        let f = dec.receive_frame().expect("receive_frame");
        let Frame::Audio(a) = f else {
            panic!("expected audio");
        };
        recovered.extend(decode_interleaved(&a, SampleFormat::S16, 2));
    }
    for i in 0..n {
        assert_eq!(recovered[i * 2], l[i]);
        assert_eq!(recovered[i * 2 + 1], r[i]);
    }
}
