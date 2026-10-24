//! Compression levels (issue #12): every level is lossless across sample
//! formats and channel layouts, presets behave as documented, the
//! exhaustive preset reproduces the historical encoder byte-for-byte, and
//! the default level stays close to the exhaustive search in size.

use oxideav_core::{AudioFrame, CodecId, CodecParameters, Error, Frame, SampleFormat};
use oxideav_flac::decoder::make_decoder;
use oxideav_flac::encoder::{FlacEncoderOptions, make_encoder_with_options};

/// xorshift32 step.
fn xs(st: &mut u32) -> u32 {
    *st ^= *st << 13;
    *st ^= *st >> 17;
    *st ^= *st << 5;
    *st
}

/// Integer triangle wave of the given period and peak-to-peak amplitude.
fn tri(i: i64, period: i64, amp: i64) -> i64 {
    let ph = i % period;
    let half = period / 2;
    let v = if ph < half { ph } else { period - ph };
    (v * 2 * amp).div_euclid(period) - amp.div_euclid(2)
}

/// Deterministic integer-only stereo S16 signal (no floating point, so the
/// bytes are identical on every platform): a triangle pair plus xorshift
/// noise, right channel partly correlated with the left.
fn pin_signal(n: usize) -> Vec<u8> {
    let mut st = 0x2545_F491u32;
    let mut out = Vec::with_capacity(n * 4);
    for i in 0..n as i64 {
        let noise = (xs(&mut st) >> 24) as i64 - 128;
        let l = tri(i, 101, 12000) + tri(i, 13, 3000) + noise;
        let r = (3 * l).div_euclid(4) + tri(i, 37, 4000) + ((xs(&mut st) >> 26) as i64 - 32);
        out.extend_from_slice(&(l as i16).to_le_bytes());
        out.extend_from_slice(&(r as i16).to_le_bytes());
    }
    out
}

/// Interleaved PCM for `channels` channels in `fmt`: per-channel tones of
/// different periods plus noise, scaled to most of the format's range.
fn layout_signal(n: usize, channels: usize, fmt: SampleFormat) -> Vec<u8> {
    let bits = match fmt {
        SampleFormat::U8 => 8,
        SampleFormat::S16 => 16,
        SampleFormat::S24 => 24,
        _ => 32,
    };
    let amp = 1i64 << (bits - 2);
    let mut st = 0xC0FF_EE11u32;
    let mut out = Vec::new();
    for i in 0..n as i64 {
        for c in 0..channels as i64 {
            let noise = (xs(&mut st) as i32 >> (36 - bits).min(31)) as i64;
            let v = tri(i, 50 + 17 * c, amp) + tri(i, 7 + c, amp / 8) + noise;
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

/// Encode `pcm`, feeding it `chunk` interchannel samples per frame
/// (`usize::MAX` = all at once). Returns the concatenated frame bytes, the
/// per-frame packets and the final output parameters.
fn encode(
    pcm: &[u8],
    p: &CodecParameters,
    opts: FlacEncoderOptions,
    chunk: usize,
) -> (Vec<u8>, Vec<Vec<u8>>, CodecParameters) {
    let fmt = p.sample_format.unwrap();
    let frame_bytes = fmt.bytes_per_sample() * p.channels.unwrap() as usize;
    let mut enc = make_encoder_with_options(p, opts).expect("encoder");
    let mut packets = Vec::new();
    let step = chunk.saturating_mul(frame_bytes).max(frame_bytes);
    for piece in pcm.chunks(step) {
        enc.send_frame(&Frame::Audio(AudioFrame {
            samples: (piece.len() / frame_bytes) as u32,
            pts: None,
            data: vec![piece.to_vec()],
        }))
        .expect("send_frame");
        while let Ok(pkt) = enc.receive_packet() {
            packets.push(pkt.data);
        }
    }
    enc.flush().expect("flush");
    loop {
        match enc.receive_packet() {
            Ok(pkt) => packets.push(pkt.data),
            Err(Error::NeedMore) | Err(Error::Eof) => break,
            Err(e) => panic!("encoder error: {e:?}"),
        }
    }
    (packets.concat(), packets, enc.output_params().clone())
}

fn decode(packets: &[Vec<u8>], p: &CodecParameters) -> Vec<u8> {
    let mut dec = make_decoder(p).expect("decoder");
    let mut out = Vec::new();
    for data in packets {
        let pkt = oxideav_core::Packet::new(0, oxideav_core::TimeBase::new(1, 1), data.clone());
        dec.send_packet(&pkt).expect("send_packet");
        while let Ok(Frame::Audio(a)) = dec.receive_frame() {
            out.extend_from_slice(&a.data[0]);
        }
    }
    out
}

#[test]
fn every_level_is_lossless_across_formats_and_layouts() {
    let cases = [
        (1u16, 22_050u32, SampleFormat::U8),
        (2, 44_100, SampleFormat::S16),
        (2, 96_000, SampleFormat::S24),
        (2, 48_000, SampleFormat::S32),
        (6, 48_000, SampleFormat::S16),
    ];
    for (ch, rate, fmt) in cases {
        // Two full blocks plus a short tail, so the last-frame path runs.
        let pcm = layout_signal(9_000, ch as usize, fmt);
        let p = params(ch, rate, fmt);
        for level in 0..=8u8 {
            let (bytes, packets, out) = encode(&pcm, &p, FlacEncoderOptions::level(level), 4096);
            assert!(!bytes.is_empty());
            assert_eq!(
                decode(&packets, &out),
                pcm,
                "level {level}: {ch} ch {fmt:?} is not lossless"
            );
        }
    }
}

#[test]
fn verify_mode_accepts_every_level() {
    let pcm = pin_signal(10_000);
    let p = params(2, 44_100, SampleFormat::S16);
    for level in 0..=8u8 {
        let plain = encode(&pcm, &p, FlacEncoderOptions::level(level), 4096).0;
        let checked = encode(
            &pcm,
            &p,
            FlacEncoderOptions::level(level).with_verify(true),
            4096,
        )
        .0;
        assert_eq!(plain, checked, "level {level}: verify changed the bytes");
    }
}

#[test]
fn level_presets_resolve_as_documented() {
    assert_eq!(FlacEncoderOptions::new().compression_level, None);
    assert_eq!(FlacEncoderOptions::level(3).compression_level, Some(3));
    assert_eq!(FlacEncoderOptions::level(200).compression_level, Some(8));
    assert_eq!(FlacEncoderOptions::fast().compression_level, Some(4));
    assert!(FlacEncoderOptions::exhaustive().exhaustive_search);

    let pcm = pin_signal(12_000);
    let p = params(2, 44_100, SampleFormat::S16);
    let bytes = |o: FlacEncoderOptions| encode(&pcm, &p, o, 4096).0;
    // Default == level 5; out-of-range levels clamp to 8; fast() == level 4.
    assert_eq!(
        bytes(FlacEncoderOptions::default()),
        bytes(FlacEncoderOptions::level(5))
    );
    assert_eq!(
        bytes(FlacEncoderOptions::default().with_compression_level(99)),
        bytes(FlacEncoderOptions::level(8))
    );
    assert_eq!(
        bytes(FlacEncoderOptions::fast()),
        bytes(FlacEncoderOptions::level(4))
    );
}

#[test]
fn chunking_does_not_change_the_output() {
    // The encoder re-blocks internally: one huge send_frame, block-sized
    // frames and odd-sized frames must all produce identical bytes.
    let pcm = pin_signal(13_000);
    let p = params(2, 44_100, SampleFormat::S16);
    let whole = encode(&pcm, &p, FlacEncoderOptions::default(), usize::MAX).0;
    for chunk in [4096, 1000, 17] {
        assert_eq!(
            encode(&pcm, &p, FlacEncoderOptions::default(), chunk).0,
            whole,
            "chunk {chunk}"
        );
    }
}

#[test]
fn exhaustive_preset_is_byte_identical_to_the_historical_default() {
    // Frame bytes of the encoder's default output before compression levels
    // existed (commit 298a3a4, `make_encoder` defaults), for `pin_signal`.
    const HISTORICAL_LEN: usize = 31_971;
    const HISTORICAL_MD5: &str = "be4f2177c705619bd20fc411ab4a17eb";
    let pcm = pin_signal(13_000);
    let p = params(2, 44_100, SampleFormat::S16);
    let (bytes, _, _) = encode(&pcm, &p, FlacEncoderOptions::exhaustive(), 4096);
    let digest: String = oxideav_flac::md5::compute(&bytes)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    assert_eq!(bytes.len(), HISTORICAL_LEN);
    assert_eq!(digest, HISTORICAL_MD5);
}

#[test]
fn default_level_stays_close_to_the_exhaustive_search() {
    let pcm = pin_signal(13_000);
    let p = params(2, 44_100, SampleFormat::S16);
    let exhaustive = encode(&pcm, &p, FlacEncoderOptions::exhaustive(), 4096)
        .0
        .len();
    let default = encode(&pcm, &p, FlacEncoderOptions::default(), 4096)
        .0
        .len();
    let level8 = encode(&pcm, &p, FlacEncoderOptions::level(8), 4096).0.len();
    assert!(
        default as f64 <= exhaustive as f64 * 1.03,
        "default {default} B vs exhaustive {exhaustive} B"
    );
    assert!(
        level8 <= default,
        "level 8 {level8} B > default {default} B"
    );
}

#[test]
fn higher_levels_compress_better_than_fixed_only_levels() {
    let pcm = layout_signal(16_384, 2, SampleFormat::S16);
    let p = params(2, 44_100, SampleFormat::S16);
    let size = |l: u8| encode(&pcm, &p, FlacEncoderOptions::level(l), 4096).0.len();
    let (l0, l5, l8) = (size(0), size(5), size(8));
    assert!(l5 < l0, "level 5 {l5} B not below level 0 {l0} B");
    assert!(l8 <= l5, "level 8 {l8} B above level 5 {l5} B");
}

#[test]
fn explicit_knobs_override_the_level() {
    let pcm = pin_signal(12_000);
    let p = params(2, 44_100, SampleFormat::S16);
    let bytes = |o: FlacEncoderOptions| encode(&pcm, &p, o, 4096).0;
    // Level 0 has no LPC; an explicit LPC ceiling turns it on.
    let fixed_only = bytes(FlacEncoderOptions::level(0));
    let with_lpc = bytes(FlacEncoderOptions::level(0).with_max_lpc_order(8));
    assert_ne!(fixed_only, with_lpc);
    // A partition-order request above the subset bound clamps to it.
    assert_eq!(
        bytes(FlacEncoderOptions::level(5).with_max_partition_order(15)),
        bytes(FlacEncoderOptions::level(5)),
    );
    // A window override replaces the level's window set and stays lossless.
    let welch = FlacEncoderOptions::level(5)
        .with_apodization(vec![oxideav_flac::encoder::Apodization::Welch]);
    let (_, packets, out) = encode(&pcm, &p, welch, 4096);
    assert_eq!(decode(&packets, &out), pcm);
}
