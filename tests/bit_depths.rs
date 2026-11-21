//! Bit depths that are not a whole number of bytes (RFC 9639 §8.2 allows
//! 4 to 32 bits per sample), and 32-bit stereo.
//!
//! PCM formats carry a narrower depth left-justified: a 12-bit sample is
//! the top 12 bits of an `S16`. The decoder delivers such streams that
//! way and the encoder takes them that way, so full scale stays full
//! scale and a decode → encode round trip keeps the depth.

mod common;

use common::*;
use oxideav_core::bits::BitWriter;
use oxideav_core::{Packet, SampleFormat, TimeBase};
use oxideav_flac::crc::{crc8, crc16};
use oxideav_flac::encoder::FlacEncoderOptions;
use oxideav_flac::metadata::StreamInfo;
use oxideav_flac::verify::{VerifyOutcome, verify_stream};

/// A mono frame of unencoded samples at `bits` per sample, the depth
/// taken from STREAMINFO (sample size code 0b000, §9.1.4).
fn verbatim_mono_frame(bits: u32, samples: &[i32]) -> Vec<u8> {
    let mut w = BitWriter::new();
    w.write_u32(0x3FFE, 14); // sync code
    w.write_u32(0, 2); // reserved, fixed block size
    w.write_u32(0b0110, 4); // block size - 1 in 8 bits
    w.write_u32(0b0000, 4); // sample rate from STREAMINFO
    w.write_u32(0b0000, 4); // one channel
    w.write_u32(0b000, 3); // depth from STREAMINFO
    w.write_u32(0, 1);
    w.write_u32(0, 8); // frame 0
    w.write_u32(samples.len() as u32 - 1, 8);
    let header = w.bytes().to_vec();
    w.write_u32(crc8(&header) as u32, 8);
    w.write_u32(0b0000_0010, 8); // verbatim subframe, no wasted bits
    for &s in samples {
        w.write_i32(s, bits);
    }
    w.align_to_byte();
    let body = w.bytes().to_vec();
    w.write_u32(crc16(&body) as u32, 16);
    w.finish()
}

fn mono_file(bits: u8, samples: &[i32]) -> Vec<u8> {
    let info = StreamInfo {
        min_block_size: 16,
        max_block_size: 4096,
        min_frame_size: 0,
        max_frame_size: 0,
        sample_rate: 44_100,
        channels: 1,
        bits_per_sample: bits,
        total_samples: samples.len() as u64,
        md5: [0; 16],
    };
    let mut file = b"fLaC".to_vec();
    file.extend_from_slice(&[0x80, 0, 0, 34]);
    file.extend_from_slice(&info.write().unwrap());
    file.extend_from_slice(&verbatim_mono_frame(bits as u32, samples));
    file
}

/// A 12-bit stream comes out as `S16` with each sample in the top 12
/// bits: full scale (2047, -2048) is 0x7FF0 and 0x8000.
#[test]
fn twelve_bit_samples_are_left_justified_in_s16() {
    let samples = [0, 1, -1, 2047, -2048, 100];
    let decoded = decode_file(&mono_file(12, &samples)).unwrap();
    assert_eq!(decoded.format, SampleFormat::S16);
    let got: Vec<i16> = decoded
        .pcm
        .as_chunks::<2>()
        .0
        .iter()
        .map(|&c| i16::from_le_bytes(c))
        .collect();
    assert_eq!(got, [0, 16, -16, 0x7FF0, i16::MIN, 1600]);
}

/// A 20-bit stream comes out as `S24`, shifted up by four bits.
#[test]
fn twenty_bit_samples_are_left_justified_in_s24() {
    let samples = [1, -1, 524_287, -524_288];
    let decoded = decode_file(&mono_file(20, &samples)).unwrap();
    assert_eq!(decoded.format, SampleFormat::S24);
    assert_eq!(
        decoded.pcm,
        [
            0x10, 0x00, 0x00, 0xF0, 0xFF, 0xFF, 0xF0, 0xFF, 0x7F, 0x00, 0x00, 0x80
        ]
    );
}

/// Depths below eight bits are streams too (the minimum is 4): they come
/// out as `U8`, left-justified around the 128 offset.
#[test]
fn four_to_seven_bit_streams_decode_to_u8() {
    let decoded = decode_file(&mono_file(4, &[0, 1, -1, 7, -8])).unwrap();
    assert_eq!(decoded.format, SampleFormat::U8);
    assert_eq!(decoded.pcm, [128, 144, 112, 240, 0]);

    let decoded = decode_file(&mono_file(7, &[0, 1, -1, 63, -64])).unwrap();
    assert_eq!(decoded.format, SampleFormat::U8);
    assert_eq!(decoded.pcm, [128, 130, 126, 254, 0]);
}

/// The encoder writes any depth from 4 to 32 bits when told the depth of
/// its left-justified input, and the decoder returns the same bytes.
#[test]
fn every_depth_round_trips_through_the_encoder() {
    for bps in 4..=32u32 {
        for channels in [1usize, 2, 3] {
            let planes = signal(Signal::Tone, bps, channels, 5000, bps as u64);
            let mut options = FlacEncoderOptions::new();
            if !bps.is_multiple_of(8) {
                options = options.with_bits_per_sample(bps as u8);
            }
            let file = encode_file(&planes, bps, 48_000, options, 1234).unwrap();
            let info = StreamInfo::parse(&file[8..42]).unwrap();
            assert_eq!(info.bits_per_sample as u32, bps);
            let decoded = decode_file(&file).unwrap();
            assert_eq!(decoded.format, format_for(bps), "{bps} bits");
            assert_eq!(decoded.pcm, pack(&planes, bps), "{bps} bits, {channels} ch");
            assert_eq!(verify_stream(&file).unwrap(), VerifyOutcome::Match);
        }
    }
}

/// Input with bits set below the declared depth is not audio of that
/// depth; coding it would lose them, so it is refused.
#[test]
fn stray_low_bits_are_refused() {
    let params = encoder_params(1, 12, 44_100);
    let mut enc = oxideav_flac::encoder::make_encoder_with_options(
        &params,
        FlacEncoderOptions::new().with_bits_per_sample(12),
    )
    .unwrap();
    let frame = |v: i16| {
        oxideav_core::Frame::Audio(oxideav_core::AudioFrame {
            samples: 1,
            pts: None,
            data: vec![v.to_le_bytes().to_vec()],
        })
    };
    enc.send_frame(&frame(0x0120)).unwrap();
    assert!(enc.send_frame(&frame(0x0121)).is_err());
    // The refused frame left nothing behind.
    enc.send_frame(&frame(-0x10)).unwrap();
    enc.flush().unwrap();
    let pkt = enc.receive_packet().unwrap();
    let info = StreamInfo::parse(&enc.output_params().extradata[4..38]).unwrap();
    assert_eq!((info.bits_per_sample, info.total_samples), (12, 2));
    let mut dec = oxideav_flac::decoder::make_decoder(enc.output_params()).unwrap();
    dec.send_packet(&Packet::new(0, TimeBase::new(1, 44_100), pkt.data.clone()))
        .unwrap();
    let oxideav_core::Frame::Audio(a) = dec.receive_frame().unwrap() else {
        panic!("audio expected");
    };
    assert_eq!(a.data[0], [0x20, 0x01, 0xF0, 0xFF]);
}

/// A depth that the sample format cannot carry left-justified in its
/// last byte, or below the format's minimum of 4, is refused up front.
#[test]
fn depths_outside_the_sample_format_are_refused() {
    for (format_bits, bad) in [(16u32, 8u8), (16, 17), (8, 3), (8, 9), (24, 16), (32, 24)] {
        let params = encoder_params(2, format_bits, 44_100);
        let made = oxideav_flac::encoder::make_encoder_with_options(
            &params,
            FlacEncoderOptions::new().with_bits_per_sample(bad),
        );
        assert!(made.is_err(), "{bad} bits in a {format_bits}-bit format");
    }
    // The registry option is the same switch.
    let mut params = encoder_params(2, 16, 44_100);
    params.options.insert("bits_per_sample", "12");
    let enc = oxideav_flac::encoder::make_encoder(&params).unwrap();
    let info = StreamInfo::parse(&enc.output_params().extradata[4..38]).unwrap();
    assert_eq!(info.bits_per_sample, 12);
}

/// 32-bit stereo is decorrelated whenever the side channel of a block
/// stays within 32 bits (it is written as a 33-bit subframe, §4.2), and
/// left independent where it does not.
#[test]
fn thirty_two_bit_stereo_is_decorrelated_when_the_side_channel_fits() {
    use oxideav_flac::frame::{ChannelAssignment, parse_frame_header};
    for options in [
        FlacEncoderOptions::level(0),
        FlacEncoderOptions::level(5),
        FlacEncoderOptions::level(8),
        FlacEncoderOptions::exhaustive().with_block_size(1024),
    ] {
        // Nearly identical channels: the side channel is tiny.
        let mut planes = signal(Signal::Same, 32, 2, 6000, 3);
        for (i, s) in planes[1].iter_mut().enumerate() {
            *s = s.saturating_add((i % 5) as i32 - 2);
        }
        let (_, packets) = encode_packets(&planes, 32, 96_000, options.clone(), 6000).unwrap();
        let decorrelated = packets
            .iter()
            .filter(|p| {
                !matches!(
                    parse_frame_header(&p.data).unwrap().channels,
                    ChannelAssignment::Independent(_)
                )
            })
            .count();
        assert_eq!(decorrelated, packets.len());
        let file = encode_file(&planes, 32, 96_000, options.clone(), 6000).unwrap();
        assert_eq!(decode_file(&file).unwrap().pcm, pack(&planes, 32));

        // Opposite full-scale channels: the side channel needs 33 bits.
        let planes = signal(Signal::WideSide, 32, 2, 6000, 3);
        let file = encode_file(&planes, 32, 96_000, options, 6000).unwrap();
        assert_eq!(decode_file(&file).unwrap().pcm, pack(&planes, 32));
        assert_eq!(verify_stream(&file).unwrap(), VerifyOutcome::Match);
    }
}
