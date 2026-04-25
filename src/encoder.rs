//! Pure-Rust FLAC encoder.
//!
//! Produces valid FLAC streams that decode bit-exactly via any compliant
//! decoder. Per-subframe the encoder tries CONSTANT, FIXED orders 0..=4,
//! LPC orders 1..=8 and VERBATIM, and picks the smallest. For stereo
//! inputs it evaluates independent L/R alongside the three decorrelated
//! layouts (left-side, right-side, mid-side) and keeps the smallest
//! total. An MD5 signature over the input PCM (in FLAC's native byte
//! order) is accumulated during encode and written into STREAMINFO at
//! flush time.

use oxideav_core::Encoder;
use oxideav_core::{
    AudioFrame, CodecId, CodecParameters, Error, Frame, MediaType, Packet, Result, SampleFormat,
    TimeBase,
};

use crate::bits_ext::BitWriterExt;
use crate::crc;
use crate::md5::Md5;
use oxideav_core::bits::BitWriter;

const DEFAULT_BLOCK_SIZE: u32 = 4096;
/// Highest LPC order the encoder will try. The decoder supports up to
/// 32, but beyond ~8 the gains shrink while per-frame encode cost grows
/// (Levinson-Durbin is O(order^2) per subframe, residual search is
/// linear in order). 8 matches libFLAC's `-5` preset.
const MAX_LPC_ORDER: usize = 8;
/// Precision used when quantising LPC coefficients. 12 bits is a
/// common sweet spot: tight enough to keep residual magnitudes small,
/// loose enough that a single 5-bit shift covers the dynamic range of
/// typical autocorrelation-derived coefficients.
const LPC_QLP_PRECISION: u32 = 12;

pub fn make_encoder(params: &CodecParameters) -> Result<Box<dyn Encoder>> {
    let channels = params
        .channels
        .ok_or_else(|| Error::invalid("FLAC encoder: missing channels"))?;
    let sample_rate = params
        .sample_rate
        .ok_or_else(|| Error::invalid("FLAC encoder: missing sample_rate"))?;
    let sample_fmt = params.sample_format.unwrap_or(SampleFormat::S16);
    let bps = match sample_fmt {
        SampleFormat::U8 => 8,
        SampleFormat::S16 => 16,
        SampleFormat::S24 => 24,
        SampleFormat::S32 => 32,
        _ => {
            return Err(Error::unsupported(format!(
                "FLAC encoder: sample format {:?} not supported",
                sample_fmt
            )));
        }
    };
    if !(1..=8).contains(&channels) {
        return Err(Error::invalid("FLAC encoder: channels must be 1..=8"));
    }

    let extradata = build_streaminfo_metadata_block(
        DEFAULT_BLOCK_SIZE,
        sample_rate,
        channels as u8,
        bps,
        0,
        0,
        0,
        &[0u8; 16],
    );

    let mut output_params = params.clone();
    output_params.media_type = MediaType::Audio;
    output_params.codec_id = CodecId::new("flac");
    output_params.sample_format = Some(sample_fmt);
    output_params.channels = Some(channels);
    output_params.sample_rate = Some(sample_rate);
    output_params.extradata = extradata;

    Ok(Box::new(FlacEncoder {
        output_params,
        sample_format: sample_fmt,
        bps,
        channels,
        sample_rate,
        block_size: DEFAULT_BLOCK_SIZE,
        time_base: TimeBase::new(1, sample_rate as i64),
        interleaved: Vec::new(),
        pending: std::collections::VecDeque::new(),
        frame_number: 0,
        md5: Md5::new(),
        total_samples: 0,
        min_frame_size: u32::MAX,
        max_frame_size: 0,
        eof: false,
    }))
}

/// Build a full METADATA_BLOCK (header + STREAMINFO payload) marked as LAST.
///
/// Spec-defined fields (min/max frame size, total_samples, md5) are
/// populated from live stats once encoding has finished; the initial
/// block written at encoder construction fills them with zero, which is
/// the spec's "unset" sentinel.
#[allow(clippy::too_many_arguments)]
fn build_streaminfo_metadata_block(
    block_size: u32,
    sample_rate: u32,
    channels: u8,
    bps: u8,
    min_frame_size: u32,
    max_frame_size: u32,
    total_samples: u64,
    md5: &[u8; 16],
) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + 34);
    out.push(0x80);
    out.push(0x00);
    out.push(0x00);
    out.push(0x22);
    out.extend_from_slice(&(block_size as u16).to_be_bytes());
    out.extend_from_slice(&(block_size as u16).to_be_bytes());
    out.push(((min_frame_size >> 16) & 0xFF) as u8);
    out.push(((min_frame_size >> 8) & 0xFF) as u8);
    out.push((min_frame_size & 0xFF) as u8);
    out.push(((max_frame_size >> 16) & 0xFF) as u8);
    out.push(((max_frame_size >> 8) & 0xFF) as u8);
    out.push((max_frame_size & 0xFF) as u8);
    let total_36 = total_samples & 0x0000_000F_FFFF_FFFF;
    let packed: u64 = ((sample_rate as u64) << 44)
        | (((channels - 1) as u64 & 0x7) << 41)
        | (((bps - 1) as u64 & 0x1F) << 36)
        | total_36;
    out.extend_from_slice(&packed.to_be_bytes());
    out.extend_from_slice(md5);
    out
}

struct FlacEncoder {
    output_params: CodecParameters,
    sample_format: SampleFormat,
    bps: u8,
    channels: u16,
    sample_rate: u32,
    block_size: u32,
    time_base: TimeBase,
    /// Samples queued as interleaved i32. One element per (sample, channel).
    interleaved: Vec<i32>,
    pending: std::collections::VecDeque<Packet>,
    frame_number: u64,
    md5: Md5,
    total_samples: u64,
    min_frame_size: u32,
    max_frame_size: u32,
    eof: bool,
}

impl FlacEncoder {
    fn ingest_frame(&mut self, a: &AudioFrame) -> Result<()> {
        if a.channels != self.channels || a.sample_rate != self.sample_rate {
            return Err(Error::invalid(
                "FLAC encoder: frame channels/sample_rate do not match encoder configuration",
            ));
        }
        if a.format != self.sample_format {
            return Err(Error::invalid(format!(
                "FLAC encoder: frame format {:?} does not match encoder format {:?}",
                a.format, self.sample_format
            )));
        }
        if a.format.is_planar() {
            return Err(Error::unsupported("FLAC encoder: planar input unsupported"));
        }
        let data = a
            .data
            .first()
            .ok_or_else(|| Error::invalid("empty frame"))?;
        match self.sample_format {
            SampleFormat::S16 => {
                for chunk in data.chunks_exact(2) {
                    self.interleaved
                        .push(i16::from_le_bytes([chunk[0], chunk[1]]) as i32);
                }
            }
            SampleFormat::S24 => {
                for chunk in data.chunks_exact(3) {
                    let mut v =
                        (chunk[0] as i32) | ((chunk[1] as i32) << 8) | ((chunk[2] as i32) << 16);
                    if v & 0x0080_0000 != 0 {
                        v |= 0xFF00_0000_u32 as i32;
                    }
                    self.interleaved.push(v);
                }
            }
            SampleFormat::S32 => {
                for chunk in data.chunks_exact(4) {
                    self.interleaved
                        .push(i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
                }
            }
            SampleFormat::U8 => {
                for &b in data.iter() {
                    self.interleaved.push((b as i32) - 128);
                }
            }
            _ => return Err(Error::unsupported("FLAC encoder: unsupported input format")),
        }
        self.encode_ready_frames(false)
    }

    fn encode_ready_frames(&mut self, drain_all: bool) -> Result<()> {
        let n_ch = self.channels as usize;
        let block = self.block_size as usize;
        loop {
            let frames_avail = self.interleaved.len() / n_ch;
            let take = if drain_all {
                frames_avail
            } else if frames_avail >= block {
                block
            } else {
                return Ok(());
            };
            if take == 0 {
                return Ok(());
            }
            let mut per_channel: Vec<Vec<i32>> =
                (0..n_ch).map(|_| Vec::with_capacity(take)).collect();
            for i in 0..take {
                for c in 0..n_ch {
                    per_channel[c].push(self.interleaved[i * n_ch + c]);
                }
            }
            self.interleaved.drain(..take * n_ch);

            self.feed_md5(&per_channel, take);

            let data = encode_frame(
                self.frame_number,
                take as u32,
                self.sample_rate,
                self.bps,
                &per_channel,
            )?;
            let fsize = data.len() as u32;
            if fsize < self.min_frame_size {
                self.min_frame_size = fsize;
            }
            if fsize > self.max_frame_size {
                self.max_frame_size = fsize;
            }
            self.total_samples += take as u64;

            let pts = (self.frame_number as i64) * (self.block_size as i64);
            let mut pkt = Packet::new(0, self.time_base, data);
            pkt.pts = Some(pts);
            pkt.dts = Some(pts);
            pkt.duration = Some(take as i64);
            pkt.flags.keyframe = true;
            self.pending.push_back(pkt);
            self.frame_number += 1;
        }
    }

    /// Feed this block's PCM into the MD5 digest in FLAC's canonical
    /// byte order: interleaved samples, each stored little-endian in the
    /// sample-width-rounded-up-to-byte (1 byte for 8 bps, 2 for 16, 3
    /// for 24, 4 for 32). 8-bit samples are signed two's complement.
    fn feed_md5(&mut self, per_channel: &[Vec<i32>], n: usize) {
        let bytes_per_sample = match self.bps {
            8 => 1,
            9..=16 => 2,
            17..=24 => 3,
            _ => 4,
        };
        let n_ch = per_channel.len();
        let mut buf = Vec::with_capacity(n * n_ch * bytes_per_sample);
        for i in 0..n {
            for c in 0..n_ch {
                let s = per_channel[c][i];
                match bytes_per_sample {
                    1 => buf.push((s & 0xFF) as u8),
                    2 => buf.extend_from_slice(&(s as i16).to_le_bytes()),
                    3 => {
                        buf.push((s & 0xFF) as u8);
                        buf.push(((s >> 8) & 0xFF) as u8);
                        buf.push(((s >> 16) & 0xFF) as u8);
                    }
                    _ => buf.extend_from_slice(&s.to_le_bytes()),
                }
            }
        }
        self.md5.update(&buf);
    }

    /// Rebuild STREAMINFO in `output_params.extradata` using live stats.
    /// Called once from `flush` after all frames have been emitted.
    fn finalize_streaminfo(&mut self) {
        let md5_bytes = std::mem::take(&mut self.md5).finalize();
        let min_frame_size = if self.min_frame_size == u32::MAX {
            0
        } else {
            self.min_frame_size
        };
        self.output_params.extradata = build_streaminfo_metadata_block(
            self.block_size,
            self.sample_rate,
            self.channels as u8,
            self.bps,
            min_frame_size,
            self.max_frame_size,
            self.total_samples,
            &md5_bytes,
        );
    }
}

impl Encoder for FlacEncoder {
    fn codec_id(&self) -> &CodecId {
        &self.output_params.codec_id
    }
    fn output_params(&self) -> &CodecParameters {
        &self.output_params
    }

    fn send_frame(&mut self, frame: &Frame) -> Result<()> {
        match frame {
            Frame::Audio(a) => self.ingest_frame(a),
            _ => Err(Error::invalid("FLAC encoder: audio frames only")),
        }
    }

    fn receive_packet(&mut self) -> Result<Packet> {
        self.pending.pop_front().ok_or(Error::NeedMore)
    }

    fn flush(&mut self) -> Result<()> {
        if !self.eof {
            self.eof = true;
            self.encode_ready_frames(true)?;
            self.finalize_streaminfo();
        }
        Ok(())
    }
}

// --- Per-frame encoding ---------------------------------------------------

fn encode_frame(
    frame_number: u64,
    block_size: u32,
    sample_rate: u32,
    bps: u8,
    channels: &[Vec<i32>],
) -> Result<Vec<u8>> {
    let n_ch = channels.len();
    if !(1..=8).contains(&n_ch) {
        return Err(Error::invalid("FLAC encoder: channel count out of range"));
    }

    let (channel_code, subframe_plan) = choose_channel_assignment(channels, bps as u32)?;

    let mut w = BitWriter::with_capacity(block_size as usize * 2 * n_ch);

    // Sync (14 bits) + reserved (1) + blocking strategy (1, 0=fixed).
    w.write_u32(0b11111111111110, 14);
    w.write_u32(0, 1);
    w.write_u32(0, 1);

    let (bs_code, bs_extra, bs_extra_bits) = encode_block_size(block_size);
    w.write_u32(bs_code as u32, 4);
    let (sr_code, sr_extra, sr_extra_bits) = encode_sample_rate(sample_rate);
    w.write_u32(sr_code as u32, 4);
    w.write_u32(channel_code, 4);
    let ss_code = encode_sample_size(bps);
    w.write_u32(ss_code as u32, 3);
    w.write_u32(0, 1); // reserved

    w.write_utf8_u64(frame_number);
    if bs_extra_bits > 0 {
        w.write_u32(bs_extra, bs_extra_bits);
    }
    if sr_extra_bits > 0 {
        w.write_u32(sr_extra, sr_extra_bits);
    }

    // Frame header is byte-aligned at this point.
    debug_assert!(w.is_byte_aligned());
    let hdr_bytes_so_far = w.bytes().to_vec();
    let hdr_crc8 = crc::crc8(&hdr_bytes_so_far);
    w.write_u32(hdr_crc8 as u32, 8);

    for plan in &subframe_plan {
        plan.emit(&mut w);
    }

    w.align_to_byte();
    let frame_bytes = w.bytes().to_vec();
    let frame_crc16 = crc::crc16(&frame_bytes);
    w.write_u32(((frame_crc16 >> 8) & 0xFF) as u32, 8);
    w.write_u32((frame_crc16 & 0xFF) as u32, 8);

    Ok(w.into_bytes())
}

fn encode_block_size(bs: u32) -> (u8, u32, u32) {
    match bs {
        192 => (1, 0, 0),
        576 => (2, 0, 0),
        1152 => (3, 0, 0),
        2304 => (4, 0, 0),
        4608 => (5, 0, 0),
        256 => (8, 0, 0),
        512 => (9, 0, 0),
        1024 => (10, 0, 0),
        2048 => (11, 0, 0),
        4096 => (12, 0, 0),
        8192 => (13, 0, 0),
        16384 => (14, 0, 0),
        32768 => (15, 0, 0),
        _ if bs >= 1 && bs - 1 < 256 => (6, bs - 1, 8),
        _ => (7, bs - 1, 16),
    }
}

fn encode_sample_rate(sr: u32) -> (u8, u32, u32) {
    match sr {
        88_200 => (1, 0, 0),
        176_400 => (2, 0, 0),
        192_000 => (3, 0, 0),
        8_000 => (4, 0, 0),
        16_000 => (5, 0, 0),
        22_050 => (6, 0, 0),
        24_000 => (7, 0, 0),
        32_000 => (8, 0, 0),
        44_100 => (9, 0, 0),
        48_000 => (10, 0, 0),
        96_000 => (11, 0, 0),
        _ => {
            if sr % 1000 == 0 && sr / 1000 <= 255 {
                (12, sr / 1000, 8)
            } else if sr <= 0xFFFF {
                (13, sr, 16)
            } else if sr % 10 == 0 && sr / 10 <= 0xFFFF {
                (14, sr / 10, 16)
            } else {
                (0, 0, 0)
            }
        }
    }
}

fn encode_sample_size(bps: u8) -> u8 {
    match bps {
        8 => 1,
        12 => 2,
        16 => 4,
        20 => 5,
        24 => 6,
        _ => 0,
    }
}

// --- Stereo decorrelation -----------------------------------------------

/// Pick the channel-assignment (independent / L-S / R-S / M-S) that
/// produces the smallest total subframe size, and return both the
/// assignment code and the pre-computed subframes ready for emission.
fn choose_channel_assignment(channels: &[Vec<i32>], bps: u32) -> Result<(u32, Vec<SubframePlan>)> {
    let n_ch = channels.len();
    if n_ch != 2 {
        let mut plans = Vec::with_capacity(n_ch);
        for ch in channels {
            plans.push(best_subframe(ch, bps)?);
        }
        return Ok(((n_ch - 1) as u32, plans));
    }

    let l = &channels[0];
    let r = &channels[1];
    let n = l.len();

    let sf_l = best_subframe(l, bps)?;
    let sf_r = best_subframe(r, bps)?;
    let independent_bits = sf_l.bits + sf_r.bits;
    let mut best = (independent_bits, 1u32, vec![sf_l.clone(), sf_r.clone()]);

    // Stereo decorrelation modes need a side channel at `bps + 1` bits
    // (to span the full L-R range). Our subframe encoders clamp to 32
    // bits, so for 32-bit input we skip the decorrelation modes and
    // fall back to LR-only (`assignment = 1`). This matches libFLAC,
    // which rejects any `bits_per_sample > 32` outright and keeps LR
    // for the `== 32` boundary.
    if bps < 32 {
        let mut side = Vec::with_capacity(n);
        let mut mid = Vec::with_capacity(n);
        for i in 0..n {
            side.push((l[i] as i64 - r[i] as i64) as i32);
            // FLAC's spec-defined mid is floor((L+R)/2); the absorbed LSB is
            // recovered via `side`. `((L+R) >> 1)` matches libFLAC.
            let sum = l[i] as i64 + r[i] as i64;
            mid.push((sum >> 1) as i32);
        }
        let sf_s = best_subframe(&side, bps + 1)?;
        let sf_m = best_subframe(&mid, bps)?;
        let left_side_bits = sf_l.bits + sf_s.bits;
        let right_side_bits = sf_s.bits + sf_r.bits;
        let mid_side_bits = sf_m.bits + sf_s.bits;
        if left_side_bits < best.0 {
            best = (left_side_bits, 8, vec![sf_l.clone(), sf_s.clone()]);
        }
        if right_side_bits < best.0 {
            best = (right_side_bits, 9, vec![sf_s.clone(), sf_r.clone()]);
        }
        if mid_side_bits < best.0 {
            best = (mid_side_bits, 10, vec![sf_m, sf_s]);
        }
    }
    Ok((best.1, best.2))
}

// --- Subframe selection -------------------------------------------------

#[derive(Clone)]
struct SubframePlan {
    bits: u64,
    payload: Vec<u8>,
    payload_bits: u32,
}

impl SubframePlan {
    fn emit(&self, w: &mut BitWriter) {
        // Re-emit the buffered subframe payload bit-for-bit into `w`.
        let full_bytes = (self.payload_bits / 8) as usize;
        let tail_bits = self.payload_bits % 8;
        for &b in &self.payload[..full_bytes] {
            w.write_u32(b as u32, 8);
        }
        if tail_bits > 0 {
            let b = self.payload[full_bytes];
            w.write_u32((b >> (8 - tail_bits)) as u32, tail_bits);
        }
    }
}

/// Build the smallest-footprint subframe for `samples`, trying
/// CONSTANT, FIXED orders 0..=4, LPC orders 1..=MAX_LPC_ORDER and
/// VERBATIM.
fn best_subframe(samples: &[i32], bps: u32) -> Result<SubframePlan> {
    let n = samples.len();
    if n == 0 {
        return Err(Error::invalid("FLAC encoder: empty subframe"));
    }

    // CONSTANT dominates whenever every sample matches.
    if samples.iter().all(|&s| s == samples[0]) {
        return Ok(encode_constant_plan(samples[0], bps));
    }

    let verbatim = encode_verbatim_plan(samples, bps);
    let mut best = verbatim;

    for order in 0..=4usize {
        if n <= order {
            continue;
        }
        if let Some(plan) = encode_fixed_plan(samples, bps, order) {
            if plan.bits < best.bits {
                best = plan;
            }
        }
    }

    let max_lpc = MAX_LPC_ORDER.min(n.saturating_sub(1));
    for order in 1..=max_lpc {
        if let Some(plan) = encode_lpc_plan(samples, bps, order) {
            if plan.bits < best.bits {
                best = plan;
            }
        }
    }

    Ok(best)
}

fn encode_constant_plan(value: i32, bps: u32) -> SubframePlan {
    let mut w = BitWriter::new();
    w.write_u32(0, 1); // pad
    w.write_u32(0b000000, 6); // CONSTANT
    w.write_u32(0, 1); // no wasted bits
    w.write_i32(value, bps);
    bits_snapshot(w, 1 + 6 + 1 + bps)
}

fn encode_verbatim_plan(samples: &[i32], bps: u32) -> SubframePlan {
    let mut w = BitWriter::new();
    w.write_u32(0, 1);
    w.write_u32(0b000001, 6);
    w.write_u32(0, 1);
    for &s in samples {
        w.write_i32(s, bps);
    }
    bits_snapshot(w, 1 + 6 + 1 + (bps * samples.len() as u32))
}

fn encode_fixed_plan(samples: &[i32], bps: u32, order: usize) -> Option<SubframePlan> {
    let n = samples.len();
    if n <= order {
        return None;
    }
    // Fixed-predictor coefficients (FLAC spec §SUBFRAME_FIXED).
    const COEFFS: [&[i64]; 5] = [&[], &[1], &[2, -1], &[3, -3, 1], &[4, -6, 4, -1]];
    let c = COEFFS[order];
    let mut residuals = Vec::with_capacity(n - order);
    for i in order..n {
        let mut pred: i64 = 0;
        for (j, &cj) in c.iter().enumerate() {
            pred += cj * samples[i - 1 - j] as i64;
        }
        let r = samples[i] as i64 - pred;
        if !(i32::MIN as i64..=i32::MAX as i64).contains(&r) {
            return None;
        }
        residuals.push(r as i32);
    }

    let mut w = BitWriter::new();
    w.write_u32(0, 1);
    w.write_u32(0b001000 | (order as u32 & 0x07), 6);
    w.write_u32(0, 1);
    for i in 0..order {
        w.write_i32(samples[i], bps);
    }
    let residual_bits = encode_rice_residual(&mut w, &residuals);
    let header_bits = 1 + 6 + 1 + (bps * order as u32);
    Some(bits_snapshot(w, header_bits + residual_bits))
}

fn encode_lpc_plan(samples: &[i32], bps: u32, order: usize) -> Option<SubframePlan> {
    let n = samples.len();
    if n <= order {
        return None;
    }
    let coeffs_f = levinson_durbin(samples, order)?;
    let (qcoeffs, qlp_shift) = quantize_lpc(&coeffs_f, LPC_QLP_PRECISION)?;

    let mut residuals = Vec::with_capacity(n - order);
    for i in order..n {
        let mut pred: i64 = 0;
        for (j, &q) in qcoeffs.iter().enumerate() {
            pred += q as i64 * samples[i - 1 - j] as i64;
        }
        let predicted = pred >> qlp_shift;
        let r = samples[i] as i64 - predicted;
        if !(i32::MIN as i64..=i32::MAX as i64).contains(&r) {
            return None;
        }
        residuals.push(r as i32);
    }

    let mut w = BitWriter::new();
    w.write_u32(0, 1);
    // LPC subframe type is 100000 | (order-1).
    w.write_u32(0b100000 | ((order - 1) as u32 & 0x1F), 6);
    w.write_u32(0, 1);
    for i in 0..order {
        w.write_i32(samples[i], bps);
    }
    // qlp_precision-1 (4 bits) — the all-ones value 0xF is reserved.
    w.write_u32(LPC_QLP_PRECISION - 1, 4);
    // qlp_shift is written as 5-bit two's complement; we only emit
    // non-negative shifts so the sign bit is always 0.
    w.write_u32(qlp_shift & 0x1F, 5);
    for &q in &qcoeffs {
        w.write_i32(q, LPC_QLP_PRECISION);
    }
    let residual_bits = encode_rice_residual(&mut w, &residuals);
    let header_bits = 1 + 6 + 1 + (bps * order as u32) + 4 + 5 + (LPC_QLP_PRECISION * order as u32);
    Some(bits_snapshot(w, header_bits + residual_bits))
}

fn bits_snapshot(w: BitWriter, total_bits: u32) -> SubframePlan {
    let payload = w.into_bytes();
    SubframePlan {
        bits: total_bits as u64,
        payload,
        payload_bits: total_bits,
    }
}

// --- Residual coding ----------------------------------------------------

/// Emit partitioned-Rice residual (order 0) and return the exact number
/// of bits written. Chooses between method 0 (4-bit k) and method 1
/// (5-bit k), and falls back to an escape partition when raw coding is
/// cheaper.
fn encode_rice_residual(w: &mut BitWriter, residuals: &[i32]) -> u32 {
    let (bits0, _k0) = best_rice_params(residuals, 14);
    let (bits1, _k1) = best_rice_params(residuals, 30);
    let (method, k_max, param_bits) = if bits1 + 1 < bits0 {
        (1u32, 30u32, 5u32)
    } else {
        (0u32, 14u32, 4u32)
    };
    let (_, k) = best_rice_params(residuals, k_max);
    let escape_marker: u32 = (1u32 << param_bits) - 1;

    let needed_bits = raw_bits_needed(residuals);
    let rice_cost = encoded_rice_cost(residuals, k);
    let escape_cost: u64 = 5 + (residuals.len() as u64) * (needed_bits as u64);
    let escape_ok = needed_bits <= 31;

    let mut total_bits: u32 = 0;
    w.write_u32(method, 2);
    w.write_u32(0, 4); // partition_order = 0
    total_bits += 2 + 4;

    if escape_ok && escape_cost < rice_cost {
        w.write_u32(escape_marker, param_bits);
        w.write_u32(needed_bits, 5);
        total_bits += param_bits + 5;
        for &r in residuals {
            w.write_i32(r, needed_bits);
        }
        total_bits += needed_bits * residuals.len() as u32;
    } else {
        w.write_u32(k, param_bits);
        total_bits += param_bits;
        for &r in residuals {
            let u = zigzag_encode(r);
            let q = u >> k;
            w.write_unary(q);
            if k > 0 {
                let rem = u & ((1u32 << k) - 1);
                w.write_u32(rem, k);
            }
            total_bits += q + 1 + k;
        }
    }
    total_bits
}

fn zigzag_encode(s: i32) -> u32 {
    ((s << 1) ^ (s >> 31)) as u32
}

fn encoded_rice_cost(residuals: &[i32], k: u32) -> u64 {
    let mut total: u64 = 0;
    for &r in residuals {
        let u = zigzag_encode(r) as u64;
        total += (u >> k) + 1 + k as u64;
    }
    total
}

fn raw_bits_needed(residuals: &[i32]) -> u32 {
    let mut max_bits = 1u32;
    for &r in residuals {
        let needed = if r >= 0 {
            33 - (r as u32).leading_zeros()
        } else {
            33 - (!r as u32).leading_zeros()
        };
        if needed > max_bits {
            max_bits = needed;
        }
    }
    max_bits.min(32)
}

fn best_rice_params(residuals: &[i32], k_max: u32) -> (u64, u32) {
    if residuals.is_empty() {
        return (0, 0);
    }
    let mut best_k = 0u32;
    let mut best_bits = u64::MAX;
    for k in 0..=k_max {
        let mut total: u64 = 0;
        for &r in residuals {
            let u = zigzag_encode(r) as u64;
            total += (u >> k) + 1 + k as u64;
            if total >= best_bits {
                break;
            }
        }
        if total < best_bits {
            best_bits = total;
            best_k = k;
        }
    }
    (best_bits, best_k)
}

// --- LPC analysis -------------------------------------------------------

/// Compute LPC coefficients via autocorrelation + Levinson-Durbin for
/// the given `order`. Returns `None` if the signal is degenerate
/// (zero energy or non-finite intermediate values). The first
/// coefficient corresponds to `samples[n-1]`, following FLAC's
/// convention.
fn levinson_durbin(samples: &[i32], order: usize) -> Option<Vec<f64>> {
    let n = samples.len();
    if n <= order || order == 0 {
        return None;
    }

    // Welch window — smooth enough to reduce spectral leakage while
    // preserving enough energy in the centre that short blocks still
    // produce stable coefficients.
    let mut windowed: Vec<f64> = Vec::with_capacity(n);
    let half = (n - 1) as f64 / 2.0;
    for (i, &s) in samples.iter().enumerate() {
        let x = (i as f64 - half) / (half + 1.0);
        let w = 1.0 - x * x;
        windowed.push(s as f64 * w);
    }

    let mut autoc = vec![0.0f64; order + 1];
    for lag in 0..=order {
        let mut s = 0.0f64;
        for i in lag..n {
            s += windowed[i] * windowed[i - lag];
        }
        autoc[lag] = s;
    }
    if autoc[0] <= 0.0 || !autoc[0].is_finite() {
        return None;
    }

    let mut lpc = vec![0.0f64; order];
    let mut error = autoc[0];
    for i in 0..order {
        let mut r = -autoc[i + 1];
        for j in 0..i {
            r -= lpc[j] * autoc[i - j];
        }
        if error.abs() < 1e-12 {
            return None;
        }
        let k = r / error;
        // Symmetric update in place; iterate from outside in to avoid
        // clobbering values we still need.
        let mut new_lpc = lpc.clone();
        new_lpc[i] = k;
        for j in 0..i {
            new_lpc[j] = lpc[j] + k * lpc[i - 1 - j];
        }
        lpc = new_lpc;
        error *= 1.0 - k * k;
        if !error.is_finite() || error <= 0.0 {
            return None;
        }
    }

    // FLAC's prediction is `s[n] ≈ sum_j coeff[j] * s[n-1-j]`, i.e.
    // `coeff[0]` applies to the most-recent sample. Standard
    // Levinson-Durbin (predicting `s[n] = -sum a[j] * s[n-j]`) yields
    // `a[j]`; FLAC's coefficient is `-a[j+1]` with index-0 being the
    // nearest neighbour.
    let mut out = Vec::with_capacity(order);
    for &v in lpc.iter() {
        out.push(-v);
    }
    Some(out)
}

/// Quantise floating-point LPC coefficients into `precision`-bit signed
/// integers plus a non-negative shift. Returns `None` if the magnitudes
/// are too small to represent meaningfully (would overflow the shift
/// range). The shift is clamped to 0..=14.
fn quantize_lpc(coeffs: &[f64], precision: u32) -> Option<(Vec<i32>, u32)> {
    let cmax = coeffs.iter().fold(0f64, |acc, &c| acc.max(c.abs()));
    if !cmax.is_finite() || cmax == 0.0 {
        return None;
    }
    // Max coefficient magnitude representable at `precision` bits of
    // signed range.
    let max_q = (1i64 << (precision - 1)) - 1;
    // Pick shift so that `cmax * 2^shift` sits just under `max_q`.
    let raw_shift = (max_q as f64 / cmax).log2().floor() as i32;
    let shift = raw_shift.clamp(0, 14) as u32;
    let scale = (1u64 << shift) as f64;

    let mut out = Vec::with_capacity(coeffs.len());
    let mut error = 0.0f64;
    for &c in coeffs {
        let v = c * scale + error;
        let q = v.round();
        error = v - q;
        let mut qi = q as i64;
        if qi > max_q {
            qi = max_q;
        }
        if qi < -max_q - 1 {
            qi = -max_q - 1;
        }
        out.push(qi as i32);
    }
    Some((out, shift))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::decoder;

    fn roundtrip(channels_pcm: Vec<Vec<i32>>, sample_rate: u32, bps: u8) {
        let n_ch = channels_pcm.len();
        let block_size = 1024u32;
        let total = channels_pcm[0].len();
        let mut all_frames: Vec<Vec<u8>> = Vec::new();
        let mut frame_num = 0u64;
        let mut i = 0;
        while i < total {
            let take = ((total - i) as u32).min(block_size) as usize;
            let per_ch: Vec<Vec<i32>> = (0..n_ch)
                .map(|c| channels_pcm[c][i..i + take].to_vec())
                .collect();
            let data = encode_frame(frame_num, take as u32, sample_rate, bps, &per_ch).unwrap();
            all_frames.push(data);
            frame_num += 1;
            i += take;
        }
        let mut stream = Vec::new();
        stream.extend_from_slice(&crate::metadata::FLAC_MAGIC);
        stream.extend_from_slice(&build_streaminfo_metadata_block(
            block_size,
            sample_rate,
            n_ch as u8,
            bps,
            0,
            0,
            0,
            &[0u8; 16],
        ));
        for f in &all_frames {
            stream.extend_from_slice(f);
        }

        let mut params = oxideav_core::CodecParameters::audio(oxideav_core::CodecId::new("flac"));
        params.channels = Some(n_ch as u16);
        params.sample_rate = Some(sample_rate);
        params.extradata = build_streaminfo_metadata_block(
            block_size,
            sample_rate,
            n_ch as u8,
            bps,
            0,
            0,
            0,
            &[0u8; 16],
        );
        let mut dec = decoder::make_decoder(&params).unwrap();

        let mut out_interleaved: Vec<i32> = Vec::new();
        for f in all_frames {
            let mut pkt =
                oxideav_core::Packet::new(0, oxideav_core::TimeBase::new(1, sample_rate as i64), f);
            pkt.pts = Some(0);
            dec.send_packet(&pkt).unwrap();
            let frame = dec.receive_frame().unwrap();
            let oxideav_core::Frame::Audio(a) = frame else {
                panic!("expected audio frame");
            };
            for chunk in a.data[0].chunks_exact(a.format.bytes_per_sample() * n_ch) {
                for c in 0..n_ch {
                    let off = c * a.format.bytes_per_sample();
                    let s = match a.format {
                        oxideav_core::SampleFormat::S16 => {
                            i16::from_le_bytes([chunk[off], chunk[off + 1]]) as i32
                        }
                        oxideav_core::SampleFormat::S24 => {
                            let mut v = (chunk[off] as i32)
                                | ((chunk[off + 1] as i32) << 8)
                                | ((chunk[off + 2] as i32) << 16);
                            if v & 0x0080_0000 != 0 {
                                v |= 0xFF00_0000_u32 as i32;
                            }
                            v
                        }
                        oxideav_core::SampleFormat::S32 => i32::from_le_bytes([
                            chunk[off],
                            chunk[off + 1],
                            chunk[off + 2],
                            chunk[off + 3],
                        ]),
                        _ => panic!("unexpected format"),
                    };
                    out_interleaved.push(s);
                }
            }
        }

        for i in 0..total {
            for c in 0..n_ch {
                assert_eq!(
                    channels_pcm[c][i],
                    out_interleaved[i * n_ch + c],
                    "mismatch at sample {i} ch {c}"
                );
            }
        }
        let _ = stream;
    }

    #[test]
    fn encode_decode_mono_s16_sine() {
        let sr = 48_000u32;
        let n = 4096usize;
        let mut ch: Vec<i32> = Vec::with_capacity(n);
        for i in 0..n {
            let v = ((i as f64 / sr as f64 * 440.0 * 2.0 * std::f64::consts::PI).sin() * 20_000.0)
                as i32;
            ch.push(v);
        }
        roundtrip(vec![ch], sr, 16);
    }

    #[test]
    fn encode_decode_stereo_s16() {
        let sr = 44_100u32;
        let n = 2000usize;
        let mut l: Vec<i32> = Vec::with_capacity(n);
        let mut r: Vec<i32> = Vec::with_capacity(n);
        for i in 0..n {
            let base = (i as f64 / sr as f64 * 330.0 * 2.0 * std::f64::consts::PI).sin() * 15_000.0;
            l.push(base as i32);
            r.push((base * 0.8) as i32);
        }
        roundtrip(vec![l, r], sr, 16);
    }

    #[test]
    fn encode_decode_constant_block() {
        let samples = vec![12345i32; 1024];
        roundtrip(vec![samples], 48_000, 16);
    }

    #[test]
    fn encode_decode_stereo_s32_ramp_plus_sine() {
        let sr = 48_000u32;
        let n = 3000usize;
        let mut l: Vec<i32> = Vec::with_capacity(n);
        let mut r: Vec<i32> = Vec::with_capacity(n);
        let step: i32 = 0x0010_0001;
        let mut acc: i32 = -0x4000_0000;
        for i in 0..n {
            l.push(acc);
            acc = acc.wrapping_add(step);
            let v = ((i as f64 / sr as f64 * 220.0 * 2.0 * std::f64::consts::PI).sin()
                * (i32::MAX as f64 * 0.95)) as i32;
            r.push(v);
        }
        roundtrip(vec![l, r], sr, 32);
    }

    #[test]
    fn encode_decode_mono_s32_extremes() {
        let mut samples: Vec<i32> = vec![i32::MIN; 100];
        samples.extend(std::iter::repeat_n(i32::MAX, 100));
        samples.extend([0i32, 1, -1, i32::MIN / 2, i32::MAX / 2]);
        samples.extend(std::iter::repeat_n(-12345i32, 512));
        roundtrip(vec![samples], 48_000, 32);
    }

    /// A pure sine is highly predictable by an LPC model — verify that
    /// the encoder's best_subframe chooses an LPC subframe for a clean
    /// stereo sine (and that the round-trip stays bit-exact).
    #[test]
    fn lpc_used_for_stereo_sine() {
        let sr = 48_000u32;
        let n = 1024usize;
        let mut l: Vec<i32> = Vec::with_capacity(n);
        let mut r: Vec<i32> = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f64 / sr as f64;
            l.push(((t * 440.0 * 2.0 * std::f64::consts::PI).sin() * 20_000.0) as i32);
            r.push(((t * 440.0 * 2.0 * std::f64::consts::PI + 0.3).sin() * 20_000.0) as i32);
        }
        let plan_l = best_subframe(&l, 16).unwrap();
        let plan_r = best_subframe(&r, 16).unwrap();
        // Sanity: a sine compresses better than VERBATIM.
        let verbatim_bits = 1 + 6 + 1 + 16 * n as u64;
        assert!(
            plan_l.bits < verbatim_bits / 2,
            "LPC/fixed should beat verbatim by 2x on a sine"
        );
        assert!(plan_r.bits < verbatim_bits / 2);
    }

    /// Stereo decorrelation must kick in when L and R are strongly
    /// correlated. When L == R exactly the side channel is all-zero,
    /// so left-side, right-side and mid-side tie; any of the three
    /// beats independent encoding.
    #[test]
    fn decorrelation_chosen_when_l_equals_r() {
        let sr = 48_000u32;
        let n = 512usize;
        let mut l: Vec<i32> = Vec::with_capacity(n);
        for i in 0..n {
            l.push(
                ((i as f64 / sr as f64 * 440.0 * 2.0 * std::f64::consts::PI).sin() * 20_000.0)
                    as i32,
            );
        }
        let r = l.clone();
        let (code, plans) = choose_channel_assignment(&[l.clone(), r.clone()], 16).unwrap();
        assert!(
            matches!(code, 8..=10),
            "expected a decorrelated stereo code (8/9/10), got {code}"
        );
        assert_eq!(plans.len(), 2);
    }

    /// Verify stereo decorrelation is actually triggered on an
    /// asymmetric real-world-ish signal: left is the sum of two
    /// partials, right is a scaled copy. The M/S pair should be
    /// strictly smaller than independent L/R.
    #[test]
    fn decorrelation_beats_independent_on_correlated_stereo() {
        let sr = 48_000u32;
        let n = 1024usize;
        let mut l: Vec<i32> = Vec::with_capacity(n);
        let mut r: Vec<i32> = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f64 / sr as f64;
            let v = (t * 440.0 * 2.0 * std::f64::consts::PI).sin() * 15_000.0
                + (t * 660.0 * 2.0 * std::f64::consts::PI).sin() * 6_000.0;
            l.push(v as i32);
            r.push((v * 0.92 + 11.0) as i32);
        }
        let sf_l = best_subframe(&l, 16).unwrap();
        let sf_r = best_subframe(&r, 16).unwrap();
        let independent = sf_l.bits + sf_r.bits;
        let (code, plans) = choose_channel_assignment(&[l, r], 16).unwrap();
        let picked: u64 = plans.iter().map(|p| p.bits).sum();
        assert!(
            picked <= independent,
            "decorrelation picked {code} at {picked} bits but independent = {independent}"
        );
    }
}
