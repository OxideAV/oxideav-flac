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

    // FLAC decoder remaps bps -> output sample format:
    //   1..=16  -> S16  (so U8 encoded input decodes as S16)
    //   17..=24 -> S24
    //   25..=32 -> S32
    let dec_format = match format.bytes_per_sample() {
        1 | 2 => SampleFormat::S16,
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
