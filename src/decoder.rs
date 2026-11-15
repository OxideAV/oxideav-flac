//! Top-level FLAC frame decoder, wired into the [`oxideav_core::Decoder`] trait.

use oxideav_core::Decoder;
use oxideav_core::{
    AudioFrame, CodecId, CodecParameters, Error, Frame, Packet, Result, SampleFormat,
};

use crate::crc;
use crate::frame::{ChannelAssignment, parse_frame_header};
use crate::metadata::{BlockHeader, BlockType, StreamInfo};
use crate::reservoir::BitReservoir;
use crate::subframe::{EmptyFirstPartition, decode_subframe_with};

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
        scratch: FrameScratch::new(),
        pending: Vec::new(),
        pending_pts: None,
        has_pending: false,
        eof: false,
    }))
}

struct FlacDecoder {
    codec_id: CodecId,
    streaminfo: StreamInfo,
    output_format: SampleFormat,
    /// Sample planes reused from frame to frame.
    scratch: FrameScratch,
    /// Bytes of the packet waiting for `receive_frame`; the buffer is
    /// kept between packets so queuing one does not allocate.
    pending: Vec<u8>,
    pending_pts: Option<i64>,
    has_pending: bool,
    eof: bool,
}

impl Decoder for FlacDecoder {
    fn codec_id(&self) -> &CodecId {
        &self.codec_id
    }

    fn send_packet(&mut self, packet: &Packet) -> Result<()> {
        if self.has_pending {
            return Err(Error::other(
                "FLAC decoder: receive_frame must be called before sending another packet",
            ));
        }
        self.pending.clear();
        self.pending.extend_from_slice(&packet.data);
        self.pending_pts = packet.pts;
        self.has_pending = true;
        Ok(())
    }

    fn receive_frame(&mut self) -> Result<Frame> {
        if !self.has_pending {
            return if self.eof {
                Err(Error::Eof)
            } else {
                Err(Error::NeedMore)
            };
        }
        self.has_pending = false;
        let shape = self.scratch.decode(&self.pending, &self.streaminfo)?;
        // RFC 9639 §9: channel count and bit depth are repeated in every
        // frame header and a decoder MAY stop when they change. The output
        // layout is fixed by the stream parameters (STREAMINFO), so a
        // frame that disagrees with them cannot be delivered.
        if shape.channels != self.streaminfo.channels as usize
            || shape.bits_per_sample != self.streaminfo.bits_per_sample as u32
        {
            return Err(Error::invalid(format!(
                "FLAC frame: {} channel(s) at {} bits, but STREAMINFO declares {} at {}",
                shape.channels,
                shape.bits_per_sample,
                self.streaminfo.channels,
                self.streaminfo.bits_per_sample
            )));
        }
        let planes = &self.scratch.planes[..shape.channels];
        let data = interleave(planes, self.output_format)?;
        Ok(Frame::Audio(AudioFrame {
            samples: shape.block_size as u32,
            pts: self.pending_pts,
            data: vec![data],
        }))
    }

    fn flush(&mut self) -> Result<()> {
        self.eof = true;
        Ok(())
    }

    /// Frames are independent of each other (RFC 9639 §10), so there is
    /// no predictor state to wipe: a reset drops the packet that was
    /// waiting, if any, and clears the end-of-stream mark so the decoder
    /// asks for input again instead of reporting the end.
    fn reset(&mut self) -> Result<()> {
        self.has_pending = false;
        self.pending.clear();
        self.eof = false;
        Ok(())
    }

    /// The output description, known from STREAMINFO at construction and
    /// constant for the life of the decoder: frames that would change
    /// the channel count or bit depth are refused by `receive_frame`.
    fn output_params(&self) -> Option<CodecParameters> {
        let mut p = CodecParameters::audio(self.codec_id.clone());
        p.channels = Some(self.streaminfo.channels as u16);
        p.sample_format = Some(self.output_format);
        // §8.2: a sample rate of 0 marks a stream that is not audio.
        if self.streaminfo.sample_rate != 0 {
            p.sample_rate = Some(self.streaminfo.sample_rate);
        }
        Some(p)
    }
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
#[non_exhaustive]
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
///
/// Each call allocates the planes it returns. A caller decoding many
/// frames keeps a [`FrameScratch`] instead and reuses its planes.
pub fn decode_frame_channels(data: &[u8], streaminfo: &StreamInfo) -> Result<DecodedFrame> {
    let mut scratch = FrameScratch::new();
    let shape = scratch.decode(data, streaminfo)?;
    scratch.planes.truncate(shape.channels);
    Ok(DecodedFrame {
        channels: scratch.planes,
        bits_per_sample: shape.bits_per_sample,
        frame_byte_len: shape.frame_byte_len,
    })
}

/// What [`FrameScratch::decode`] found in a frame.
#[doc(hidden)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub struct FrameShape {
    /// Number of channels; the first that many planes hold the samples.
    pub channels: usize,
    /// Samples per channel.
    pub block_size: usize,
    /// Bit depth of the decoded samples.
    pub bits_per_sample: u32,
    /// Bytes the frame occupies (header + subframes + CRC-16).
    pub frame_byte_len: usize,
}

/// Reusable sample planes for decoding frame after frame without
/// allocating: the planes grow to the largest block seen and stay.
#[doc(hidden)]
#[derive(Default)]
pub struct FrameScratch {
    planes: Vec<Vec<i32>>,
    /// The 33-bit side channel of a 32-bit stereo-decorrelated frame.
    side: Vec<i64>,
    empty_first: EmptyFirstPartition,
}

impl FrameScratch {
    /// Empty scratch; nothing is allocated until the first frame.
    pub fn new() -> Self {
        Self::default()
    }

    /// A scratch that refuses what RFC 9639 forbids even where decoding
    /// would be unambiguous (see [`EmptyFirstPartition`]). The encoder
    /// checks its own frames with this.
    pub fn strict() -> Self {
        Self {
            empty_first: EmptyFirstPartition::Reject,
            ..Self::default()
        }
    }

    /// The planes of the most recently decoded frame. Only the first
    /// [`FrameShape::channels`] entries are meaningful.
    pub fn planes(&self) -> &[Vec<i32>] {
        &self.planes
    }

    /// Decode one frame from the start of `data` into the planes: header,
    /// subframes, inverse stereo decorrelation, CRC-16 check (RFC 9639
    /// §9). Bytes after the frame are ignored.
    pub fn decode(&mut self, data: &[u8], streaminfo: &StreamInfo) -> Result<FrameShape> {
        let header = parse_frame_header(data)?;
        let body_offset = header.header_byte_len;
        let block_size = header.block_size as usize;

        let bps = if header.bits_per_sample != 0 {
            header.bits_per_sample as u32
        } else {
            streaminfo.bits_per_sample as u32
        };
        if bps == 0 || bps > 32 {
            return Err(Error::unsupported(format!(
                "FLAC frame: bit depth {bps} out of supported range 1..=32"
            )));
        }

        let n_channels = header.channels.channel_count() as usize;
        if self.planes.len() < n_channels {
            self.planes.resize_with(n_channels, Vec::new);
        }
        for plane in &mut self.planes[..n_channels] {
            plane.resize(block_size, 0);
        }

        let mut br = BitReservoir::new(&data[body_offset..]);
        // Which subframe, if any, is the side channel: it carries one
        // extra bit (RFC 9639 §4.2).
        let side_index = match header.channels {
            ChannelAssignment::Independent(_) => None,
            ChannelAssignment::LeftSide | ChannelAssignment::MidSide => Some(1),
            ChannelAssignment::RightSide => Some(0),
        };
        // At 32 bits the side channel is 33 bits wide and does not fit an
        // `i32` plane; it is decoded into the `i64` side buffer and the
        // left/right pair, which does fit, is rebuilt from there.
        let wide_side = bps == 32 && side_index.is_some();
        if wide_side {
            self.side.resize(block_size, 0);
        }
        let rule = self.empty_first;
        for (index, plane) in self.planes[..n_channels].iter_mut().enumerate() {
            if side_index == Some(index) {
                if wide_side {
                    decode_subframe_with(&mut br, &mut self.side[..], bps + 1, rule)?;
                } else {
                    decode_subframe_with(&mut br, &mut plane[..], bps + 1, rule)?;
                }
            } else {
                decode_subframe_with(&mut br, &mut plane[..], bps, rule)?;
            }
        }

        // §9.3: zero bits up to the byte boundary, then the CRC-16 over
        // everything before it, sync code included.
        br.align_to_byte();
        let crc_offset = body_offset + br.byte_position();
        let Some(stored) = data.get(crc_offset..crc_offset + 2) else {
            return Err(Error::invalid(
                "FLAC frame: not enough bytes for trailing CRC-16",
            ));
        };
        let claimed_crc = u16::from_be_bytes([stored[0], stored[1]]);
        let mut crc = crc::Crc16::new();
        crc.update(&data[..crc_offset]);
        let computed = crc.value();
        if computed != claimed_crc {
            return Err(Error::invalid(format!(
                "FLAC frame CRC-16 mismatch (got {:#06x}, want {:#06x})",
                computed, claimed_crc
            )));
        }

        if wide_side {
            let (first, second) = self.planes.split_at_mut(1);
            undo_decorrelation_wide(&mut first[0], &mut second[0], &self.side, header.channels)?;
        } else if side_index.is_some() {
            let (first, second) = self.planes.split_at_mut(1);
            undo_decorrelation(&mut first[0], &mut second[0], header.channels);
        }

        Ok(FrameShape {
            channels: n_channels,
            block_size,
            bits_per_sample: bps,
            frame_byte_len: crc_offset + 2,
        })
    }
}

/// Pack per-channel planes into one interleaved little-endian buffer in
/// `format`.
fn interleave(planes: &[Vec<i32>], format: SampleFormat) -> Result<Vec<u8>> {
    let width = match format {
        SampleFormat::U8 => 1,
        SampleFormat::S16 => 2,
        SampleFormat::S24 => 3,
        SampleFormat::S32 => 4,
        _ => {
            return Err(Error::unsupported(
                "FLAC decoder output format not supported",
            ));
        }
    };
    let samples = planes.first().map_or(0, Vec::len);
    let stride = width * planes.len();
    let mut out = vec![0u8; samples * stride];
    for (channel, plane) in planes.iter().enumerate() {
        let at = channel * width;
        let frames = out.chunks_exact_mut(stride).zip(plane);
        match format {
            // FLAC stores 8-bit samples as signed two's complement;
            // SampleFormat::U8 is unsigned with a +128 offset.
            SampleFormat::U8 => {
                for (frame, &s) in frames {
                    frame[at] = (s as u8).wrapping_add(128);
                }
            }
            SampleFormat::S16 => {
                for (frame, &s) in frames {
                    frame[at..at + 2].copy_from_slice(&(s as i16).to_le_bytes());
                }
            }
            SampleFormat::S24 => {
                for (frame, &s) in frames {
                    frame[at..at + 3].copy_from_slice(&s.to_le_bytes()[..3]);
                }
            }
            _ => {
                for (frame, &s) in frames {
                    frame[at..at + 4].copy_from_slice(&s.to_le_bytes());
                }
            }
        }
    }
    Ok(out)
}

/// Undo stereo decorrelation (RFC 9639 §4.2) where the side channel is at
/// most 32 bits wide. `first` and `second` hold the two subframes in
/// bitstream order and receive left and right.
fn undo_decorrelation(first: &mut [i32], second: &mut [i32], assign: ChannelAssignment) {
    match assign {
        ChannelAssignment::Independent(_) => {}
        ChannelAssignment::LeftSide => {
            // left, side = left - right  →  right = left - side.
            for (&left, side) in first.iter().zip(second.iter_mut()) {
                *side = left.wrapping_sub(*side);
            }
        }
        ChannelAssignment::RightSide => {
            // side = left - right, right  →  left = right + side.
            for (side, &right) in first.iter_mut().zip(second.iter()) {
                *side = right.wrapping_add(*side);
            }
        }
        ChannelAssignment::MidSide => {
            // mid = (left + right) >> 1, side = left - right. The bit the
            // shift dropped is the parity of side.
            for (mid, side) in first.iter_mut().zip(second.iter_mut()) {
                let s = *side as i64;
                let sum = ((*mid as i64) << 1) | (s & 1);
                *mid = ((sum + s) >> 1) as i32;
                *side = ((sum - s) >> 1) as i32;
            }
        }
    }
}

/// Undo stereo decorrelation for a 32-bit frame, whose side channel is 33
/// bits wide and lives in `side`. `first` and `second` are the two output
/// planes; the one that was not the side subframe already holds its
/// 32-bit channel (left, right or mid).
///
/// A side sample outside the 33-bit range cannot come from two 32-bit
/// channels, so it marks the stream invalid; with the side bounded, none
/// of the sums below can leave `i64`.
fn undo_decorrelation_wide(
    first: &mut [i32],
    second: &mut [i32],
    side: &[i64],
    assign: ChannelAssignment,
) -> Result<()> {
    const SIDE_MIN: i64 = -(1 << 32);
    const SIDE_MAX: i64 = (1 << 32) - 1;
    if side.iter().any(|s| !(SIDE_MIN..=SIDE_MAX).contains(s)) {
        return Err(Error::invalid(
            "FLAC 32-bit decorrelation: side sample exceeds 33 bits",
        ));
    }
    match assign {
        ChannelAssignment::Independent(_) => {}
        ChannelAssignment::LeftSide => {
            for ((&left, right), &s) in first.iter().zip(second.iter_mut()).zip(side) {
                *right = (left as i64 - s) as i32;
            }
        }
        ChannelAssignment::RightSide => {
            for ((left, &right), &s) in first.iter_mut().zip(second.iter()).zip(side) {
                *left = (right as i64 + s) as i32;
            }
        }
        ChannelAssignment::MidSide => {
            for ((mid, right), &s) in first.iter_mut().zip(second.iter_mut()).zip(side) {
                let sum = ((*mid as i64) << 1) | (s & 1);
                *mid = ((sum + s) >> 1) as i32;
                *right = ((sum - s) >> 1) as i32;
            }
        }
    }
    Ok(())
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

    #[test]
    fn output_params_come_from_streaminfo() {
        for (bps, format) in [
            (8u8, SampleFormat::U8),
            (12, SampleFormat::S16),
            (16, SampleFormat::S16),
            (20, SampleFormat::S24),
            (24, SampleFormat::S24),
            (32, SampleFormat::S32),
        ] {
            let dec = make_decoder(&make_params(bps)).unwrap();
            let out = dec.output_params().expect("known from STREAMINFO");
            assert_eq!(out.sample_format, Some(format));
            assert_eq!(out.channels, Some(1));
            assert_eq!(out.sample_rate, Some(44_100));
            assert_eq!(dec.output_params_generation(), 0);
        }
    }

    #[test]
    fn reset_drops_the_pending_packet_and_the_end_mark() {
        let mut dec = make_decoder(&make_params(16)).unwrap();
        let tb = oxideav_core::TimeBase::new(1, 44_100);
        // Not a frame: decoding it would fail.
        dec.send_packet(&Packet::new(0, tb, vec![0u8; 16])).unwrap();
        dec.flush().unwrap();
        dec.reset().unwrap();
        // The bad packet is gone and the decoder is not at its end.
        assert!(matches!(dec.receive_frame(), Err(Error::NeedMore)));
        dec.send_packet(&Packet::new(0, tb, vec![0u8; 16])).unwrap();
        assert!(matches!(dec.receive_frame(), Err(Error::InvalidData(_))));
        // A failed frame leaves the decoder ready for the next packet.
        dec.send_packet(&Packet::new(0, tb, vec![0u8; 16])).unwrap();
        assert!(dec.send_packet(&Packet::new(0, tb, vec![0u8; 16])).is_err());
        dec.flush().unwrap();
        assert!(dec.receive_frame().is_err());
        assert!(matches!(dec.receive_frame(), Err(Error::Eof)));
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
        // The stream parameters declare what the frames carry: stereo.
        let mut params = make_params(32);
        params.channels = Some(2);
        params.extradata[4 + 12] |= 1 << 1; // channels - 1 = 1
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
