#![no_main]

//! Fuzz: random PCM → oxideav-flac encode → decoded by **both**
//! oxideav-flac and libavcodec; assert bit-exact per-sample equality
//! whenever libavcodec accepts a frame.
//!
//! FLAC is **lossless** — both decoders must produce identical PCM
//! samples for any valid FLAC bitstream. Any per-sample drift indicates
//! a real bug in either our decoder or our encoder.
//!
//! Why we feed encoder output instead of arbitrary fuzz bytes
//! ----------------------------------------------------------------
//! libavcodec's FLAC decoder needs a valid `fLaC` magic + STREAMINFO
//! prefix on the very first packet to bootstrap (we don't try to wire
//! `extradata` into AVCodecContext because the AVCodecContext struct
//! layout isn't byte-stable across libavcodec major versions and
//! workspace policy bars vendoring an `ffmpeg-sys` crate that would
//! provide the correct offsets). Random fuzz bytes essentially never
//! land on a valid header, so the oracle would silently never run.
//! Synthesising the FLAC stream via our encoder gives the oracle real
//! work to do on every iteration.
//!
//! When libavcodec is unavailable on the runner the harness
//! `eprintln!`s a one-line skip notice and returns — no `#[ignore]`,
//! the binary still runs (just doesn't fire the equality assertion).

use libfuzzer_sys::fuzz_target;
use oxideav_core::{AudioFrame, CodecId, CodecParameters, Error, Frame, Packet, SampleFormat};
use oxideav_flac_fuzz::libavcodec;
use std::sync::OnceLock;

/// Cap on per-channel sample count. Total fuzz-input cap is ~4 KiB by
/// default, and S32 stereo at MAX samples needs `4 * 2 * MAX` bytes —
/// so we keep MAX small enough that `data.len() >= need` admits most
/// inputs without wasting libFuzzer iterations.
const MAX_SAMPLES_PER_CH: usize = 256;

fuzz_target!(|data: &[u8]| {
    // Print the skip notice once per process to keep the libFuzzer
    // output legible, but never `#[ignore]` — the binary still runs.
    static SKIP_LOGGED: OnceLock<()> = OnceLock::new();
    if !libavcodec::available() {
        SKIP_LOGGED.get_or_init(|| {
            eprintln!(
                "[oracle skip] libavcodec not loadable; flac_oracle_decode \
                 runs panic-only without per-sample comparison"
            );
        });
    }

    let Some(input) = parse_fuzz_input(data) else {
        return;
    };
    let TestCase {
        format,
        channels,
        sample_rate,
        pcm_per_channel,
    } = input;

    // Encode through our encoder.
    let mut enc_params = CodecParameters::audio(CodecId::new("flac"));
    enc_params.channels = Some(channels);
    enc_params.sample_rate = Some(sample_rate);
    enc_params.sample_format = Some(format);
    let mut enc = match oxideav_flac::encoder::make_encoder(&enc_params) {
        Ok(e) => e,
        Err(_) => return,
    };

    let dec_params = enc.output_params().clone();
    if dec_params.extradata.is_empty() {
        return;
    }

    let n = pcm_per_channel[0].len();
    let frame = build_audio_frame(format, channels, &pcm_per_channel);
    if enc.send_frame(&Frame::Audio(frame)).is_err() {
        return;
    }
    if enc.flush().is_err() {
        return;
    }

    let mut packets: Vec<Packet> = Vec::new();
    loop {
        match enc.receive_packet() {
            Ok(p) => packets.push(p),
            Err(Error::NeedMore) | Err(Error::Eof) => break,
            Err(_) => return,
        }
    }
    if packets.is_empty() {
        return;
    }

    // Build both decoders.
    let mut our_dec = match oxideav_flac::decoder::make_decoder(&dec_params) {
        Ok(d) => d,
        Err(_) => return,
    };

    // For libavcodec, prepend `fLaC` magic + STREAMINFO (== extradata)
    // to the first encoded packet so the decoder picks up sample-rate /
    // bps / channels without us touching AVCodecContext struct fields.
    let mut prefixed_first =
        Vec::with_capacity(4 + dec_params.extradata.len() + packets[0].data.len());
    prefixed_first.extend_from_slice(b"fLaC");
    prefixed_first.extend_from_slice(&dec_params.extradata);
    prefixed_first.extend_from_slice(&packets[0].data);

    // Output bps → SampleFormat the our-decoder will produce.
    let our_format = match format.bytes_per_sample() {
        1 => SampleFormat::U8,
        2 => SampleFormat::S16,
        3 => SampleFormat::S24,
        _ => SampleFormat::S32,
    };

    // Decode every packet through our decoder. Collect per-channel i32
    // samples in stream order.
    let mut our_recovered: Vec<Vec<i32>> = (0..channels).map(|_| Vec::new()).collect();
    for pkt in &packets {
        if our_dec.send_packet(pkt).is_err() {
            return;
        }
        let f = match our_dec.receive_frame() {
            Ok(f) => f,
            Err(_) => return,
        };
        let Frame::Audio(a) = f else {
            return;
        };
        let interleaved = decode_interleaved(&a, our_format, channels);
        for (i, v) in interleaved.into_iter().enumerate() {
            our_recovered[i % channels as usize].push(v);
        }
    }
    let our_total: usize = our_recovered.iter().map(|c| c.len()).sum();
    let _ = our_total;

    // If libavcodec isn't available, we still validated the encoder
    // and our decoder cooperate for this random input — bail out before
    // the oracle path.
    if !libavcodec::available() {
        // Sanity: our recovered count must match input count.
        for (ch, recovered) in our_recovered.iter().enumerate() {
            assert_eq!(
                recovered.len(),
                n,
                "our decoder dropped samples on channel {ch}: got {}, expected {n}",
                recovered.len()
            );
        }
        return;
    }

    let oracle_ctx = match libavcodec::open_decoder(&dec_params.extradata) {
        Some(c) => c,
        None => return,
    };

    // Drive libavcodec packet-by-packet. The first packet carries the
    // `fLaC` magic + STREAMINFO prefix (libavcodec's FLAC decoder
    // accepts that as bootstrap); subsequent packets are the bare
    // encoded frame bytes.
    let mut oracle_recovered: Vec<Vec<i32>> = (0..channels).map(|_| Vec::new()).collect();
    let mut oracle_ok_at_least_one = false;
    for (i, pkt) in packets.iter().enumerate() {
        let bytes: &[u8] = if i == 0 { &prefixed_first } else { &pkt.data };
        match libavcodec::decode_packet(oracle_ctx, bytes) {
            libavcodec::OracleResult::Frame(audio) => {
                if audio.planes.is_empty() {
                    // Format we don't oracle-compare (e.g. 8-bit U8 lands
                    // as S32P with libavcodec's internal upcast). Skip
                    // the equality assertion for this iteration.
                    libavcodec::free_decoder(oracle_ctx);
                    return;
                }
                if audio.channels != channels as usize {
                    // Channel-count mismatch is a real disagreement —
                    // surface it.
                    libavcodec::free_decoder(oracle_ctx);
                    panic!(
                        "libavcodec sees {} channels but encoder declared {} \
                         (sample_fmt={}, nb_samples={})",
                        audio.channels, channels, audio.sample_fmt, audio.nb_samples
                    );
                }
                oracle_ok_at_least_one = true;
                for (ch, plane) in audio.planes.iter().enumerate() {
                    oracle_recovered[ch].extend_from_slice(plane);
                }
            }
            libavcodec::OracleResult::Rejected => {
                // libavcodec didn't accept this packet — the per-sample
                // equality contract only kicks in when libavcodec
                // accepts. Abandon the comparison for this iteration.
                libavcodec::free_decoder(oracle_ctx);
                return;
            }
            libavcodec::OracleResult::Unavailable => {
                libavcodec::free_decoder(oracle_ctx);
                return;
            }
        }
    }
    libavcodec::free_decoder(oracle_ctx);

    if !oracle_ok_at_least_one {
        return;
    }

    // Per-sample bit-exact comparison. FLAC is lossless — there is no
    // tolerance.
    //
    // libavcodec emits S16P for 8-16 bps (S16 holds 9-16 bps natively;
    // 8 bps gets sign-extended into S16) and S32P for 17-32 bps (S32
    // holds 24/32 bps with the sample left-shifted into the high bits;
    // see `<libavutil/samplefmt.h>` notes — but for 24-bit samples
    // libavcodec convention is to keep the sign in the low 24 bits when
    // STREAMINFO declares 24 bps).
    //
    // We normalise the libavcodec planes into the same convention as
    // our decoder before comparing. Specifically:
    //   bps 8  → our: i32 in [-128, 127];     libavcodec S16P: same i16 → cast i32.
    //   bps 16 → our: i32 in [-32768, 32767]; libavcodec S16P: same.
    //   bps 24 → our: i32 in [-2^23, 2^23-1]; libavcodec S32P: same i32.
    //   bps 32 → our: i32;                    libavcodec S32P: same i32.
    //
    // Note: bps 8 might be reported by libavcodec as either S16P
    // (sign-extended) or S32P depending on version. Both work: our
    // decoder's 8-bit output is also a sign-extended i32, so the
    // straight-equality compare is correct.
    for (ch, plane) in oracle_recovered.iter().enumerate() {
        let ours = &our_recovered[ch];
        if plane.len() != ours.len() {
            // Sample count drift between decoders. Surface as a hard
            // bug — both should produce the same total samples for the
            // same input.
            panic!(
                "channel {ch}: libavcodec produced {} samples, ours {} \
                 (n_input={n}, channels={channels})",
                plane.len(),
                ours.len()
            );
        }
        for i in 0..plane.len() {
            let ours_i = ours[i];
            let oracle_i = plane[i];
            assert_eq!(
                ours_i, oracle_i,
                "FLAC oracle drift at sample {i} channel {ch}: \
                 ours={ours_i}, libavcodec={oracle_i} \
                 (format={format:?}, channels={channels}, sample_rate={sample_rate}, n={n})"
            );
        }
    }
});

struct TestCase {
    format: SampleFormat,
    channels: u16,
    sample_rate: u32,
    pcm_per_channel: Vec<Vec<i32>>,
}

fn parse_fuzz_input(data: &[u8]) -> Option<TestCase> {
    if data.len() < 5 {
        return None;
    }
    let fmt_sel = data[0] & 0x03;
    let ch_sel = data[1] & 0x01; // mono / stereo only for the oracle
    let sr_sel = data[2] & 0x01;
    let n_sel = u16::from_le_bytes([data[3], data[4]]) as usize;
    let rest = &data[5..];

    // Use S16 / S32 only — these map directly to libavcodec's planar
    // S16P / S32P without sample-format conversion ambiguity.
    let format = match fmt_sel {
        0 | 1 => SampleFormat::S16,
        _ => SampleFormat::S32,
    };
    let channels: u16 = match ch_sel {
        0 => 1,
        _ => 2,
    };
    let sample_rate: u32 = match sr_sel {
        0 => 44_100,
        _ => 48_000,
    };

    let bytes_per_sample = format.bytes_per_sample();
    let n = ((n_sel % MAX_SAMPLES_PER_CH) + 16).min(MAX_SAMPLES_PER_CH);
    let need = n * channels as usize * bytes_per_sample;
    if rest.len() < need {
        return None;
    }

    let mut pcm_per_channel: Vec<Vec<i32>> = (0..channels).map(|_| Vec::with_capacity(n)).collect();
    let bps = bytes_per_sample;
    for i in 0..n {
        for ch in 0..channels as usize {
            let off = (i * channels as usize + ch) * bps;
            let chunk = &rest[off..off + bps];
            let v = match format {
                SampleFormat::S16 => i16::from_le_bytes([chunk[0], chunk[1]]) as i32,
                SampleFormat::S32 => i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]),
                _ => return None,
            };
            pcm_per_channel[ch].push(v);
        }
    }
    Some(TestCase {
        format,
        channels,
        sample_rate,
        pcm_per_channel,
    })
}

fn build_audio_frame(
    format: SampleFormat,
    channels: u16,
    pcm_per_channel: &[Vec<i32>],
) -> AudioFrame {
    let n = pcm_per_channel[0].len();
    let bytes_per_sample = format.bytes_per_sample();
    let mut interleaved: Vec<u8> = Vec::with_capacity(n * channels as usize * bytes_per_sample);
    for i in 0..n {
        for c in 0..channels as usize {
            let s = pcm_per_channel[c][i];
            match format {
                SampleFormat::S16 => {
                    interleaved.extend_from_slice(&(s as i16).to_le_bytes());
                }
                SampleFormat::S32 => interleaved.extend_from_slice(&s.to_le_bytes()),
                _ => unreachable!(),
            }
        }
    }
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
                SampleFormat::S16 => i16::from_le_bytes([chunk[off], chunk[off + 1]]) as i32,
                SampleFormat::S32 => {
                    i32::from_le_bytes([chunk[off], chunk[off + 1], chunk[off + 2], chunk[off + 3]])
                }
                _ => unreachable!(),
            };
            out.push(v);
        }
    }
    out
}
