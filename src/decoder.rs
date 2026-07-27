//! Top-level FLAC frame decoder, wired into the [`oxideav_core::Decoder`] trait.

use oxideav_core::Decoder;
use oxideav_core::{
    AudioFrame, CodecId, CodecParameters, Error, Frame, Packet, Result, SampleFormat,
};

use crate::crc;
use crate::frame::{parse_frame_header, ChannelAssignment};
use crate::metadata::{BlockHeader, BlockType, StreamInfo};
use crate::subframe::{decode_subframe, decode_subframe_wide};
use oxideav_core::bits::BitReader;

pub fn make_decoder(params: &CodecParameters) -> Result<Box<dyn Decoder>> {
    let streaminfo = find_streaminfo(&params.extradata)?;
    let bps = streaminfo.bits_per_sample;
    // Map STREAMINFO bps to the narrowest container-friendly output:
    //   8        -> U8  (matches the FLAC spec's "8-bit samples are signed
    //                   two's complement" rendered as WAV-style unsigned
    //                   offset-128 bytes, the format the FLAC container
    //                   surfaces in `params.sample_format`).
    //   9..=16   -> S16
    //   12 sits inside this range and stays S16.
    //   17..=24  -> S24 (also covers the 20-bps spec corner).
    //   25..=32  -> S32
    let output_format = match bps {
        8 => SampleFormat::U8,
        1..=16 => SampleFormat::S16,
        17..=24 => SampleFormat::S24,
        25..=32 => SampleFormat::S32,
        _ => return Err(Error::unsupported(format!("FLAC bps {bps}"))),
    };
    Ok(Box::new(FlacDecoder {
        codec_id: params.codec_id.clone(),
        streaminfo,
        output_format,
        pending: None,
        eof: false,
    }))
}

struct FlacDecoder {
    codec_id: CodecId,
    streaminfo: StreamInfo,
    output_format: SampleFormat,
    pending: Option<Packet>,
    eof: bool,
}

impl Decoder for FlacDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if self.pending.is_some() {
            return Err(Error::other(
                "FLAC decoder: receive_frame must be called before sending another packet",
            ));
        }
        self.pending = Some(packet.clone());
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        let Some(pkt) = self.pending.take() else {
            return if self.eof {
                Err(Error::Eof)
            } else {
                Err(Error::NeedMore)
            };
        };
        decode_one_frame(&pkt.data, &self.streaminfo, self.output_format, pkt.pts)
    }

    fn flush(&mut self) -> Result<()> {
        self.eof = true;
        Ok(())
    }

    // FLAC has no inter-packet DSP state (subframe LPC predictors are
    // initialised from the frame header), so the default drain-then-
    // forget `reset` is sufficient.
}

fn find_streaminfo(extradata: &[u8]) -> Result<StreamInfo> {
    let mut i = 0;
    while i + 4 <= extradata.len() {
        let hdr = BlockHeader::parse(&extradata[i..i + 4])?;
        let payload_start = i + 4;
        let payload_end = payload_start + hdr.length as usize;
        if payload_end > extradata.len() {
            return Err(Error::invalid(
                "FLAC extradata: metadata block exceeds buffer",
            ));
        }
        if hdr.block_type == BlockType::StreamInfo {
            return StreamInfo::parse(&extradata[payload_start..payload_end]);
        }
        if hdr.last {
            break;
        }
        i = payload_end;
    }
    Err(Error::invalid("FLAC decoder: no STREAMINFO in extradata"))
}

/// Fully decoded FLAC frame: post-decorrelation per-channel samples plus
/// the effective bit depth and the byte length consumed (header + body +
/// trailing CRC-16). Returned by [`decode_frame_channels`] so callers that
/// need the raw `i32` sample planes — PCM packing, MD5 verification
/// (RFC 9639 §9.2.1) — share one decode + CRC path.
pub struct DecodedFrame {
    /// One `Vec<i32>` per output channel, after stereo decorrelation.
    pub channels: Vec<Vec<i32>>,
    /// Effective bits-per-sample for these samples (from the frame header,
    /// or STREAMINFO when the header's bps field is the "get from
    /// STREAMINFO" escape).
    pub bits_per_sample: u32,
    /// Total bytes the frame occupies in `data` (header + body + CRC-16).
    pub frame_byte_len: usize,
}

/// Decode one FLAC frame from `data` to per-channel `i32` planes.
///
/// Parses the frame header, decodes every subframe, undoes stereo
/// decorrelation, and verifies the trailing frame CRC-16 (RFC 9639 §9.3).
/// This is the shared decode core behind both the streaming `Frame::Audio`
/// path and the MD5 verifier in [`crate::verify`]; keeping it as one
/// function means the verifier checks exactly the bytes the decoder emits.
pub fn decode_frame_channels(data: &[u8], streaminfo: &StreamInfo) -> Result<DecodedFrame> {
    let header = parse_frame_header(data)?;
    let body_offset = header.header_byte_len;

    let bps = if header.bits_per_sample != 0 {
        header.bits_per_sample as u32
    } else {
        streaminfo.bits_per_sample as u32
    };

    let mut br = BitReader::new(&data[body_offset..]);

    // A stereo-decorrelated frame at the 32-bit ceiling carries a 33-bit
    // side channel (RFC 9639 §4.2 / Appendix A.2). That one channel does
    // not fit in `i32`, so it is decoded through the wider `i64` path; the
    // reconstructed left/right pair fits back inside `i32` because the
    // original samples are at most 32-bit.
    let decorrelated = !matches!(header.channels, ChannelAssignment::Independent(_));
    let channels: Vec<Vec<i32>> = if bps == 32 && decorrelated {
        decode_decorrelated_wide(&mut br, header.block_size, bps, header.channels)?
    } else {
        let n_channels = header.channels.channel_count() as usize;
        let mut channels: Vec<Vec<i32>> = Vec::with_capacity(n_channels);
        for ch in 0..n_channels {
            let bps_for_channel = match header.channels {
                ChannelAssignment::LeftSide if ch == 1 => bps + 1,
                ChannelAssignment::RightSide if ch == 0 => bps + 1,
                ChannelAssignment::MidSide if ch == 1 => bps + 1,
                _ => bps,
            };
            let samples = decode_subframe(&mut br, header.block_size, bps_for_channel)?;
            if samples.len() != header.block_size as usize {
                return Err(Error::invalid("subframe sample count mismatch"));
            }
            channels.push(samples);
        }
        apply_decorrelation(&mut channels, header.channels);
        channels
    };

    // Skip frame-body padding to reach the byte-aligned 16-bit CRC at end.
    br.align_to_byte();
    let body_used = br.byte_position();
    let frame_byte_len = body_offset + body_used;
    if frame_byte_len + 2 > data.len() {
        return Err(Error::invalid(
            "FLAC frame: not enough bytes for trailing CRC-16",
        ));
    }
    let claimed_crc = u16::from_be_bytes([data[frame_byte_len], data[frame_byte_len + 1]]);
    // Streaming validator: fold the frame bytes through `Crc16` in one
    // call. Equivalent to the one-shot `crc::crc16(&data[..n])` form
    // it replaced, but exercises the streaming API in production so a
    // regression that broke `Crc16::update` would surface on every
    // decode call instead of only in the unit-test path. The cost is
    // identical (single table-driven byte loop either way).
    let mut crc = crc::Crc16::new();
    crc.update(&data[..frame_byte_len]);
    let computed = crc.value();
    if computed != claimed_crc {
        return Err(Error::invalid(format!(
            "FLAC frame CRC-16 mismatch (got {:#06x}, want {:#06x})",
            computed, claimed_crc
        )));
    }

    Ok(DecodedFrame {
        channels,
        bits_per_sample: bps,
        frame_byte_len: frame_byte_len + 2,
    })
}

fn decode_one_frame(
    data: &[u8],
    streaminfo: &StreamInfo,
    output_format: SampleFormat,
    pts: Option<i64>,
) -> Result<Frame> {
    let DecodedFrame { channels, .. } = decode_frame_channels(data, streaminfo)?;

    let total_samples = channels[0].len();
    let n_out = channels.len();
    let bytes_per_sample = output_format.bytes_per_sample();
    let mut out = Vec::with_capacity(total_samples * n_out * bytes_per_sample);
    for i in 0..total_samples {
        for c in 0..n_out {
            let s = channels[c][i];
            match output_format {
                // FLAC stores 8-bit samples as signed two's complement;
                // SampleFormat::U8 is unsigned-offset (WAV/PCM convention),
                // so add 128 to land in 0..=255.
                SampleFormat::U8 => out.push(((s + 128) & 0xFF) as u8),
                SampleFormat::S16 => out.extend_from_slice(&(s as i16).to_le_bytes()),
                SampleFormat::S24 => {
                    out.push((s & 0xFF) as u8);
                    out.push(((s >> 8) & 0xFF) as u8);
                    out.push(((s >> 16) & 0xFF) as u8);
                }
                SampleFormat::S32 => out.extend_from_slice(&s.to_le_bytes()),
                _ => {
                    return Err(Error::unsupported(
                        "FLAC decoder output format not supported",
                    ))
                }
            }
        }
    }

    Ok(Frame::Audio(AudioFrame {
        samples: total_samples as u32,
        pts,
        data: vec![out],
    }))
}

/// Decode a stereo-decorrelated frame at the 32-bit ceiling, where the
/// side channel is 33-bit and must be carried in `i64`.
///
/// The two subframes are decoded in wire order (`ch0` then `ch1`), one at
/// the declared 32-bit depth and the other — the side — at 33 bits. The
/// per-channel decode itself is the same code the narrow path uses (the
/// 32-bit channel goes through [`decode_subframe`], the side through
/// [`decode_subframe_wide`]); only the inverse decorrelation runs in `i64`
/// before narrowing the recovered left/right pair back to `i32`.
fn decode_decorrelated_wide(
    br: &mut BitReader,
    block_size: u32,
    bps: u32,
    assign: ChannelAssignment,
) -> Result<Vec<Vec<i32>>> {
    // Which of the two channels is the wide (side) one.
    // LeftSide: ch0=left(32), ch1=side(33). RightSide: ch0=side(33),
    // ch1=right(32). MidSide: ch0=mid(32), ch1=side(33).
    let ch0_wide = matches!(assign, ChannelAssignment::RightSide);

    let read_plane = |br: &mut BitReader, wide: bool| -> Result<Vec<i64>> {
        let plane = if wide {
            decode_subframe_wide(br, block_size, bps + 1)?
        } else {
            decode_subframe(br, block_size, bps)?
                .into_iter()
                .map(|s| s as i64)
                .collect()
        };
        if plane.len() != block_size as usize {
            return Err(Error::invalid("subframe sample count mismatch"));
        }
        Ok(plane)
    };

    let ch0 = read_plane(br, ch0_wide)?;
    let ch1 = read_plane(br, !ch0_wide)?;

    let n = ch0.len();
    let mut left = vec![0i32; n];
    let mut right = vec![0i32; n];
    // Inverse decorrelation runs in `i64` because the side channel is
    // 33-bit (RFC 9639 §4.2). For any *valid* 32-bit stream the operands
    // are bounded — mid/left/right at 32 bits, side at 33 — so every sum or
    // difference below fits comfortably inside `i64` and never overflows.
    // A malformed stream, however, can drive the wide `_wide` predictors
    // (which reconstruct with wrapping arithmetic) to an out-of-range side
    // sample far beyond its spec-legal 33-bit envelope; the reconstruction
    // arithmetic would then overflow `i64`. That can only mean the input is
    // invalid, so the checked ops surface a typed decode error rather than
    // panicking (debug) or silently wrapping to a bogus sample.
    let overflow = || Error::invalid("FLAC 32-bit decorrelation: reconstructed sample exceeds i64");
    match assign {
        ChannelAssignment::LeftSide => {
            // ch0 = left, ch1 = side = left - right → right = left - side.
            for i in 0..n {
                left[i] = ch0[i] as i32;
                right[i] = (ch0[i].checked_sub(ch1[i]).ok_or_else(overflow)?) as i32;
            }
        }
        ChannelAssignment::RightSide => {
            // ch0 = side = left - right, ch1 = right → left = right + side.
            for i in 0..n {
                left[i] = (ch1[i].checked_add(ch0[i]).ok_or_else(overflow)?) as i32;
                right[i] = ch1[i] as i32;
            }
        }
        ChannelAssignment::MidSide => {
            // ch0 = mid (with absorbed LSB), ch1 = side.
            for i in 0..n {
                let mid = ch0[i];
                let side = ch1[i];
                // `mid` is the narrow (≤ 32-bit) channel, so `mid << 1`
                // never overflows; only the ± side steps can.
                let m = (mid << 1) | (side & 1);
                left[i] = (m.checked_add(side).ok_or_else(overflow)? >> 1) as i32;
                right[i] = (m.checked_sub(side).ok_or_else(overflow)? >> 1) as i32;
            }
        }
        ChannelAssignment::Independent(_) => {
            unreachable!("decode_decorrelated_wide is only reached for decorrelated assignments")
        }
    }
    Ok(vec![left, right])
}

fn apply_decorrelation(channels: &mut [Vec<i32>], assign: ChannelAssignment) {
    match assign {
        ChannelAssignment::Independent(_) => {}
        ChannelAssignment::LeftSide => {
            // ch[0] = left, ch[1] = side = left - right → right = left - side.
            let n = channels[0].len();
            for i in 0..n {
                channels[1][i] = channels[0][i].wrapping_sub(channels[1][i]);
            }
        }
        ChannelAssignment::RightSide => {
            // ch[0] = side = left - right, ch[1] = right → left = right + side.
            let n = channels[0].len();
            for i in 0..n {
                channels[0][i] = channels[1][i].wrapping_add(channels[0][i]);
            }
        }
        ChannelAssignment::MidSide => {
            // ch[0] = mid (with absorbed LSB), ch[1] = side.
            let n = channels[0].len();
            for i in 0..n {
                let mid = channels[0][i] as i64;
                let side = channels[1][i] as i64;
                let m = (mid << 1) | (side & 1);
                channels[0][i] = ((m + side) >> 1) as i32;
                channels[1][i] = ((m - side) >> 1) as i32;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxideav_core::CodecId;

    /// Construct a STREAMINFO metadata block with the given bps. Other
    /// fields take placeholder values; only the bps slot affects the
    /// codepath under test.
    fn streaminfo_extradata(bps: u8) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + 34);
        // METADATA_BLOCK header: last=1, type=0 (StreamInfo), length=34.
        out.extend_from_slice(&[0x80, 0x00, 0x00, 0x22]);
        let mut block = vec![0u8; 34];
        block[0..2].copy_from_slice(&4096u16.to_be_bytes());
        block[2..4].copy_from_slice(&4096u16.to_be_bytes());
        // packed: sr(20)=44100, channels-1(3)=0, bps-1(5)=bps-1, total=0.
        let packed: u64 = (44_100u64 << 44) | (((bps - 1) as u64) << 36);
        block[10..18].copy_from_slice(&packed.to_be_bytes());
        out.extend_from_slice(&block);
        out
    }

    fn make_params(bps: u8) -> CodecParameters {
        let mut p = CodecParameters::audio(CodecId::new("flac"));
        p.channels = Some(1);
        p.sample_rate = Some(44_100);
        p.extradata = streaminfo_extradata(bps);
        p
    }

    #[test]
    fn make_decoder_accepts_8_12_16_20_24_32_bps() {
        // 8 bps used to construct a decoder that emitted S16 bytes
        // even though the container surfaced U8 in
        // `params.sample_format`. 12 and 20 bps used to be rejected
        // outright by the container. After the fix both work end to
        // end; here we just check the decoder constructor accepts each
        // spec-allowed bit depth.
        for &bps in &[8u8, 12, 16, 20, 24, 32] {
            let r = make_decoder(&make_params(bps));
            assert!(r.is_ok(), "make_decoder({bps} bps) failed: {:?}", r.err());
        }
    }

    // --- 32-bit stereo-decorrelated frame (33-bit side channel) --------
    use crate::bits_ext::BitWriterExt;
    use oxideav_core::bits::BitWriter;

    fn write_verbatim(w: &mut BitWriter, samples: &[i64], bps: u32) {
        w.write_u32(0, 1); // subframe header pad
        w.write_u32(0b000001, 6); // VERBATIM
        w.write_u32(0, 1); // no wasted bits
        for &s in samples {
            w.write_u64(s as u64, bps);
        }
    }

    /// Assemble one complete, CRC-correct 32-bit FLAC frame for the given
    /// channel assignment from raw left/right planes, encoding both
    /// subframes VERBATIM (so no predictor search is needed) with the side
    /// channel written at 33 bits.
    fn build_32bit_frame(assign_code: u8, left: &[i64], right: &[i64]) -> Vec<u8> {
        let n = left.len();
        let mut fw = BitWriter::new();
        // Frame header (fixed blocking, 192 kHz, 32-bit).
        fw.write_u32(0b11111111111110, 14); // sync
        fw.write_u32(0, 1); // reserved
        fw.write_u32(0, 1); // blocking = fixed
        fw.write_u32(7, 4); // block size code 7 → explicit 16-bit
        fw.write_u32(3, 4); // sample rate code 3 → 192000
        fw.write_u32(assign_code as u32, 4); // channel assignment
        fw.write_u32(7, 3); // sample size code 7 → 32 bits
        fw.write_u32(0, 1); // reserved
        fw.write_utf8_u64(0); // frame number 0
        fw.write_u32((n - 1) as u32, 16); // explicit block size
                                          // Header is byte-aligned here; append its CRC-8.
        let hdr = fw.bytes().to_vec();
        let c8 = crc::crc8(&hdr);
        fw.write_u32(c8 as u32, 8);

        // Build the two decorrelated subframes for this assignment.
        let side: Vec<i64> = left.iter().zip(right).map(|(&l, &r)| l - r).collect();
        let mid: Vec<i64> = left
            .iter()
            .zip(right)
            .map(|(&l, &r)| (l + r) >> 1)
            .collect();
        match assign_code {
            8 => {
                // Left-side: ch0 = left (32), ch1 = side (33).
                write_verbatim(&mut fw, left, 32);
                write_verbatim(&mut fw, &side, 33);
            }
            9 => {
                // Right-side: ch0 = side (33), ch1 = right (32).
                write_verbatim(&mut fw, &side, 33);
                write_verbatim(&mut fw, right, 32);
            }
            10 => {
                // Mid-side: ch0 = mid (32), ch1 = side (33).
                write_verbatim(&mut fw, &mid, 32);
                write_verbatim(&mut fw, &side, 33);
            }
            _ => unreachable!(),
        }

        fw.align_to_byte();
        let body = fw.bytes().to_vec();
        let c16 = crc::crc16(&body);
        fw.write_u32(c16 as u32, 16);
        fw.finish()
    }

    fn decode_32bit_frame(assign_code: u8, left: &[i64], right: &[i64]) -> (Vec<i32>, Vec<i32>) {
        let frame = build_32bit_frame(assign_code, left, right);
        let extradata = streaminfo_extradata(32);
        let si = StreamInfo::parse(&extradata[4..]).unwrap();
        let decoded = decode_frame_channels(&frame, &si).unwrap();
        assert_eq!(decoded.bits_per_sample, 32);
        (decoded.channels[0].clone(), decoded.channels[1].clone())
    }

    /// A 32-bit stereo frame under each decorrelation mode must reconstruct
    /// the exact left/right pair even when the side channel (`L - R`)
    /// overflows `i32` — the case the decoder used to reject outright.
    #[test]
    fn decode_32bit_decorrelated_frame_recovers_full_range() {
        // side = L - R reaches 4_000_000_000 (> i32::MAX), so a 32-bit
        // side buffer would corrupt the reconstruction.
        let left: Vec<i64> = vec![2_000_000_000, -2_000_000_000, 100, -100, 7];
        let right: Vec<i64> = vec![-2_000_000_000, 2_000_000_000, -100, 100, -7];
        let expect_l: Vec<i32> = left.iter().map(|&x| x as i32).collect();
        let expect_r: Vec<i32> = right.iter().map(|&x| x as i32).collect();

        for &code in &[8u8, 9, 10] {
            let (l, r) = decode_32bit_frame(code, &left, &right);
            assert_eq!(l, expect_l, "left mismatch for assignment {code}");
            assert_eq!(r, expect_r, "right mismatch for assignment {code}");
        }
    }

    /// End-to-end through the public streaming decoder: a 32-bit
    /// left-side frame must surface as interleaved little-endian S32 PCM,
    /// exercising the `decode_one_frame` packing path (not just the
    /// `decode_frame_channels` core the other test drives).
    #[test]
    fn streaming_decoder_emits_s32_for_32bit_decorrelated_frame() {
        let left: Vec<i64> = vec![2_000_000_000, -2_000_000_000, 100, -100];
        let right: Vec<i64> = vec![-2_000_000_000, 2_000_000_000, -100, 100];
        let frame = build_32bit_frame(8, &left, &right); // left-side
        let params = make_params(32);
        let mut dec = make_decoder(&params).unwrap();

        let pkt = Packet::new(0, oxideav_core::TimeBase::new(1, 192_000), frame);
        dec.send_packet(&pkt).unwrap();
        let Frame::Audio(af) = dec.receive_frame().unwrap() else {
            panic!("expected an audio frame");
        };
        assert_eq!(af.samples, left.len() as u32);

        // Rebuild the expected interleaved S32 LE byte stream.
        let mut expected = Vec::new();
        for i in 0..left.len() {
            expected.extend_from_slice(&(left[i] as i32).to_le_bytes());
            expected.extend_from_slice(&(right[i] as i32).to_le_bytes());
        }
        assert_eq!(af.data[0], expected);
    }

    /// The verify-decoder MD5 path (RFC 9639 §8.2) must reach `Match` for a
    /// 32-bit decorrelated frame: the wide decode feeds the same serialiser
    /// as any other depth, and 32-bit samples pack four bytes each. Proves
    /// the whole decode → PCM-serialise → digest pipeline is lossless for
    /// the 33-bit-side case.
    #[test]
    fn verify_frame_stream_matches_for_32bit_decorrelated() {
        let left: Vec<i64> = vec![2_000_000_000, -2_000_000_000, 100, -100];
        let right: Vec<i64> = vec![-2_000_000_000, 2_000_000_000, -100, 100];
        let frame = build_32bit_frame(10, &left, &right); // mid-side

        let base = streaminfo_extradata(32);
        let mut si = StreamInfo::parse(&base[4..]).unwrap();
        si.channels = 2;
        si.bits_per_sample = 32;
        // Compute the expected digest from the decoded PCM, then store it.
        let decoded = decode_frame_channels(&frame, &si).unwrap();
        let mut v = crate::verify::Md5Verifier::new(32);
        v.update(&decoded.channels);
        si.md5 = v.finalize();

        let outcome = crate::verify::verify_frame_stream(&frame, &si).unwrap();
        assert_eq!(outcome, crate::verify::VerifyOutcome::Match);
    }
}
