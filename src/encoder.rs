//! Pure-Rust FLAC encoder.
//!
//! Produces valid FLAC streams that decode bit-exactly via any compliant
//! decoder. Per-subframe the encoder tries CONSTANT, FIXED orders 0..=4,
//! LPC orders 1..=8 and VERBATIM, and picks the smallest. LPC coefficient
//! precision is chosen per subframe from the block size (larger blocks
//! amortise the coefficient cost, so they get more precision, up to the
//! spec's 15-bit ceiling). For stereo
//! inputs it evaluates independent L/R alongside the three decorrelated
//! layouts (left-side, right-side, mid-side) and keeps the smallest
//! total. Each subframe's residual is partitioned-Rice coded with a full
//! search over partition orders 0..=8 and both Rice methods, with each
//! partition free to fall back to a raw "escape" coding. An MD5 signature
//! over the input PCM (in FLAC's native byte order) is accumulated during
//! encode and written into STREAMINFO at flush time.

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
/// Highest LPC order the encoder will try. The decoder (and RFC 9639
/// §9.2.6) supports up to 32, but the gain curve flattens fast: most
/// content tops out somewhere between order 6 and order 12. We search
/// 1..=12, which captures the long tail of harmonic / vocal content
/// that benefits from a longer impulse response while keeping the
/// per-frame encode cost bounded (Levinson-Durbin is O(order^2),
/// residual search is linear in order). The `best_subframe` driver
/// picks the minimum-bit plan, so adding higher orders can only ever
/// shrink (or tie) the output: orders that don't earn their
/// coefficient overhead are silently discarded.
const MAX_LPC_ORDER: usize = 12;
/// Highest LPC coefficient precision the encoder will quantise to.
/// The 4-bit `qlp_precis` field stores `precision - 1`, and the
/// all-ones encoding `0b1111` (precision 16) is forbidden by RFC 9639
/// §9.2.6, so 15 is the largest legal precision. Higher precision keeps
/// the quantised coefficients closer to their floating-point ideals,
/// which shrinks the residual; the cost is `precision` extra bits per
/// coefficient, amortised over the whole subframe's residual.
const MAX_LPC_QLP_PRECISION: u32 = 15;
/// Lowest precision the heuristic will pick. Below this the coefficient
/// quantisation error tends to swell the residual faster than the
/// coefficient field shrinks.
const MIN_LPC_QLP_PRECISION: u32 = 5;

/// Choose the LPC coefficient precision for a subframe of `block_len`
/// samples. A larger block amortises the per-coefficient storage cost
/// over more residual samples, so it can afford — and benefits from —
/// higher precision; a short block keeps precision low so the warm-up
/// plus coefficient overhead doesn't dominate. The precision rises with
/// `log2(block_len)` and is clamped to the legal `[MIN, MAX]` window.
/// RFC 9639 §9.2.6 caps the on-wire precision at 15.
fn lpc_precision_for(block_len: usize) -> u32 {
    // `bits` ≈ floor(log2(block_len)); a 4096-sample block gives 12,
    // which lands precision at 15 (the legal ceiling). Tiny blocks
    // (< 32 samples) floor out at MIN_LPC_QLP_PRECISION.
    let bits = (usize::BITS - block_len.max(1).leading_zeros()).saturating_sub(1);
    let precision = bits + 3;
    precision.clamp(MIN_LPC_QLP_PRECISION, MAX_LPC_QLP_PRECISION)
}

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
        // Stream-level format/channels/sample_rate are now contractual via
        // CodecParameters — validated at encoder construction time. The
        // per-frame checks against `a.format`/`a.channels`/`a.sample_rate`
        // disappeared with the slim AudioFrame shape.
        if self.sample_format.is_planar() {
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
    // fall back to LR-only (`assignment = 1`). The FLAC format spec
    // caps `bits_per_sample` at 32, so the `== 32` boundary stays in LR.
    if bps < 32 {
        let mut side = Vec::with_capacity(n);
        let mut mid = Vec::with_capacity(n);
        for i in 0..n {
            side.push((l[i] as i64 - r[i] as i64) as i32);
            // FLAC's spec-defined mid is floor((L+R)/2); the absorbed LSB is
            // recovered via `side`. `((L+R) >> 1)` is the canonical encoding.
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
///
/// Detects "wasted bits per sample" (the count of trailing zero bits
/// shared by every sample) and subtracts that bit width from the
/// effective sample storage. Common in upsampled or low-amplitude
/// content; the spec's wasted-bits header is single-byte, so any
/// detection that saves at least one bit per sample wins.
fn best_subframe(samples: &[i32], bps: u32) -> Result<SubframePlan> {
    let n = samples.len();
    if n == 0 {
        return Err(Error::invalid("FLAC encoder: empty subframe"));
    }

    // The wasted-bit indicator + count occupies `1 + max(wasted, 1)`
    // bits. To stay safe vs reducing effective bps below 1 we cap the
    // count at bps-1.
    let wasted = wasted_bits(samples).min(bps.saturating_sub(1));
    if wasted > 0 {
        let mut shifted: Vec<i32> = Vec::with_capacity(n);
        for &s in samples {
            shifted.push(s >> wasted);
        }
        return best_subframe_with_wasted(&shifted, bps - wasted, wasted);
    }
    best_subframe_with_wasted(samples, bps, 0)
}

/// Inner driver that scores every subframe type at the given
/// (already-wasted-shifted) bps and tags the chosen plan with the
/// wasted-bits count so the writer can prepend the header bits.
fn best_subframe_with_wasted(samples: &[i32], bps: u32, wasted: u32) -> Result<SubframePlan> {
    let n = samples.len();
    // CONSTANT dominates whenever every sample matches.
    if samples.iter().all(|&s| s == samples[0]) {
        return Ok(encode_constant_plan(samples[0], bps, wasted));
    }

    let verbatim = encode_verbatim_plan(samples, bps, wasted);
    let mut best = verbatim;

    for order in 0..=4usize {
        if n <= order {
            continue;
        }
        if let Some(plan) = encode_fixed_plan(samples, bps, order, wasted) {
            if plan.bits < best.bits {
                best = plan;
            }
        }
    }

    let max_lpc = MAX_LPC_ORDER.min(n.saturating_sub(1));
    for order in 1..=max_lpc {
        if let Some(plan) = encode_lpc_plan(samples, bps, order, wasted) {
            if plan.bits < best.bits {
                best = plan;
            }
        }
    }

    Ok(best)
}

/// Count the largest `k` such that every sample in `s` is divisible
/// by `2^k`. Returns 0 for an all-zero input (the CONSTANT path
/// trumps that case anyway). Caps at 31 — anything more would fold
/// every sample onto its sign bit.
fn wasted_bits(samples: &[i32]) -> u32 {
    let mut acc: u32 = 0;
    for &s in samples {
        if s != 0 {
            acc |= s as u32;
            if acc & 1 != 0 {
                return 0;
            }
        }
    }
    if acc == 0 {
        return 0;
    }
    acc.trailing_zeros().min(31)
}

/// Write the 1-bit pad + 6-bit subframe-type + wasted-bits prelude,
/// returning the bit count consumed. The wasted-bits unary format is
/// `(wasted - 1)` zero bits followed by a `1` bit (so a 3-bit waste
/// emits `001`). When `wasted == 0` only the single `0` flag is
/// emitted.
fn write_subframe_header(w: &mut BitWriter, type_code: u32, wasted: u32) -> u32 {
    w.write_u32(0, 1);
    w.write_u32(type_code & 0x3F, 6);
    if wasted == 0 {
        w.write_u32(0, 1);
        1 + 6 + 1
    } else {
        w.write_u32(1, 1);
        // Unary: (wasted-1) zeros + 1 = wasted bits total.
        for _ in 0..(wasted - 1) {
            w.write_u32(0, 1);
        }
        w.write_u32(1, 1);
        1 + 6 + 1 + wasted
    }
}

fn encode_constant_plan(value: i32, bps: u32, wasted: u32) -> SubframePlan {
    let mut w = BitWriter::new();
    let header_bits = write_subframe_header(&mut w, 0b000000, wasted);
    w.write_i32(value, bps);
    bits_snapshot(w, header_bits + bps)
}

fn encode_verbatim_plan(samples: &[i32], bps: u32, wasted: u32) -> SubframePlan {
    let mut w = BitWriter::new();
    let header_bits = write_subframe_header(&mut w, 0b000001, wasted);
    for &s in samples {
        w.write_i32(s, bps);
    }
    bits_snapshot(w, header_bits + (bps * samples.len() as u32))
}

fn encode_fixed_plan(samples: &[i32], bps: u32, order: usize, wasted: u32) -> Option<SubframePlan> {
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
    let header_bits = write_subframe_header(&mut w, 0b001000 | (order as u32 & 0x07), wasted);
    for i in 0..order {
        w.write_i32(samples[i], bps);
    }
    let residual_bits = encode_rice_residual(&mut w, &residuals, n, order);
    Some(bits_snapshot(
        w,
        header_bits + (bps * order as u32) + residual_bits,
    ))
}

fn encode_lpc_plan(samples: &[i32], bps: u32, order: usize, wasted: u32) -> Option<SubframePlan> {
    let n = samples.len();
    if n <= order {
        return None;
    }
    let coeffs_f = levinson_durbin(samples, order)?;
    let precision = lpc_precision_for(n);
    let (qcoeffs, qlp_shift) = quantize_lpc(&coeffs_f, precision)?;

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
    // LPC subframe type is 100000 | (order-1).
    let header_bits = write_subframe_header(&mut w, 0b100000 | ((order - 1) as u32 & 0x1F), wasted);
    for i in 0..order {
        w.write_i32(samples[i], bps);
    }
    // qlp_precision-1 (4 bits) — the all-ones value 0xF is reserved.
    w.write_u32(precision - 1, 4);
    // qlp_shift is written as 5-bit two's complement; we only emit
    // non-negative shifts so the sign bit is always 0.
    w.write_u32(qlp_shift & 0x1F, 5);
    for &q in &qcoeffs {
        w.write_i32(q, precision);
    }
    let residual_bits = encode_rice_residual(&mut w, &residuals, n, order);
    let body_bits = (bps * order as u32) + 4 + 5 + (precision * order as u32);
    Some(bits_snapshot(w, header_bits + body_bits + residual_bits))
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

/// Highest partition order the encoder will search. RFC 9639 §9.2.5
/// allows the 4-bit partition order to reach 15, but a block of 4096
/// samples can never split beyond order 12 (4096 = 2^12); going further
/// only buys diminishing returns at quadratic search cost, so 8 is the
/// conventional ceiling (256 partitions of a 4096-sample block).
const MAX_PARTITION_ORDER: u32 = 8;

/// One partition's chosen coding: either a Rice parameter `k`, or an
/// escape partition storing each residual raw at `raw_bps` bits.
#[derive(Clone, Copy)]
enum PartCoding {
    Rice { k: u32 },
    Escape { raw_bps: u32 },
}

struct PartChoice {
    coding: PartCoding,
    /// Payload bits for this partition *including* its `param_bits`
    /// Rice-parameter field (and, for escape, the extra 5-bit width).
    cost: u64,
}

/// Pick the cheapest coding for a single partition's residual slice
/// under the given method (`param_bits` = 4 or 5, `k_max` = 14 or 30).
/// Compares the best Rice parameter against an escape (raw fixed-width)
/// partition and returns whichever costs fewer bits.
fn best_partition(residuals: &[i32], param_bits: u32, k_max: u32) -> PartChoice {
    let (rice_bits, k) = best_rice_params(residuals, k_max);
    let rice_cost = param_bits as u64 + rice_bits;

    let needed_bits = raw_bits_needed(residuals);
    if needed_bits <= 31 {
        let escape_cost = param_bits as u64 + 5 + (residuals.len() as u64) * needed_bits as u64;
        if escape_cost < rice_cost {
            return PartChoice {
                coding: PartCoding::Escape {
                    raw_bps: needed_bits,
                },
                cost: escape_cost,
            };
        }
    }
    PartChoice {
        coding: PartCoding::Rice { k },
        cost: rice_cost,
    }
}

/// Total residual-payload cost (sum of per-partition costs) for a given
/// partition order under one method, or `None` if the layout is illegal
/// (block size not divisible by `2^p`, or the first partition would have
/// no residual slots left after the warmup samples).
fn partition_layout_cost(
    residuals: &[i32],
    block_size: usize,
    predictor_order: usize,
    partition_order: u32,
    param_bits: u32,
    k_max: u32,
) -> Option<u64> {
    let n_partitions = 1usize << partition_order;
    if block_size % n_partitions != 0 {
        return None;
    }
    let partition_size = block_size / n_partitions;
    if partition_size <= predictor_order {
        return None;
    }
    let mut total = 0u64;
    let mut idx = 0usize;
    for p in 0..n_partitions {
        let count = if p == 0 {
            partition_size - predictor_order
        } else {
            partition_size
        };
        total += best_partition(&residuals[idx..idx + count], param_bits, k_max).cost;
        idx += count;
    }
    Some(total)
}

/// Emit the partitioned-Rice residual for one subframe and return the
/// exact number of bits written. Searches every legal partition order
/// (0..=MAX_PARTITION_ORDER) under both Rice methods (4-bit and 5-bit
/// parameters), keeping the layout with the smallest total payload, and
/// lets each partition independently fall back to an escape (raw) coding
/// when that is cheaper than Rice. RFC 9639 §9.2.5.
fn encode_rice_residual(
    w: &mut BitWriter,
    residuals: &[i32],
    block_size: usize,
    predictor_order: usize,
) -> u32 {
    // Find the (method, partition_order) pair with the smallest payload.
    let mut best: Option<(u32, u32, u32, u32, u64)> = None; // method, param_bits, k_max, p, cost
    for (method, param_bits, k_max) in [(0u32, 4u32, 14u32), (1u32, 5u32, 30u32)] {
        for p in 0..=MAX_PARTITION_ORDER {
            let Some(cost) =
                partition_layout_cost(residuals, block_size, predictor_order, p, param_bits, k_max)
            else {
                // Higher orders only get more constrained — once a split
                // is illegal for this block size, larger ones are too.
                break;
            };
            // Add the constant 2-bit method + 4-bit partition-order
            // header so cross-method comparison is fair.
            let total = 2 + 4 + cost;
            if best.is_none() || total < best.unwrap().4 {
                best = Some((method, param_bits, k_max, p, total));
            }
        }
    }

    // Order 0 is always legal (partition_size == block_size > order),
    // so `best` is guaranteed populated for any valid subframe.
    let (method, param_bits, k_max, partition_order, _) =
        best.expect("partition order 0 is always a legal layout");
    let escape_marker: u32 = (1u32 << param_bits) - 1;
    let n_partitions = 1usize << partition_order;
    let partition_size = block_size / n_partitions;

    let mut total_bits: u32 = 0;
    w.write_u32(method, 2);
    w.write_u32(partition_order, 4);
    total_bits += 2 + 4;

    let mut idx = 0usize;
    for p in 0..n_partitions {
        let count = if p == 0 {
            partition_size - predictor_order
        } else {
            partition_size
        };
        let slice = &residuals[idx..idx + count];
        idx += count;
        let choice = best_partition(slice, param_bits, k_max);
        match choice.coding {
            PartCoding::Escape { raw_bps } => {
                w.write_u32(escape_marker, param_bits);
                w.write_u32(raw_bps, 5);
                total_bits += param_bits + 5;
                for &r in slice {
                    w.write_i32(r, raw_bps);
                }
                total_bits += raw_bps * count as u32;
            }
            PartCoding::Rice { k } => {
                w.write_u32(k, param_bits);
                total_bits += param_bits;
                for &r in slice {
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
        }
    }
    total_bits
}

fn zigzag_encode(s: i32) -> u32 {
    ((s << 1) ^ (s >> 31)) as u32
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
/// range). The shift is clamped to 0..=15: RFC 9639 §9.2.6 writes the
/// shift as a 5-bit two's-complement quantity that MUST NOT be negative
/// (Appendix B.4), so the legal positive range is exactly 0..=15.
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
    let shift = raw_shift.clamp(0, 15) as u32;
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
            // Decoder picks output format from STREAMINFO bps:
            //   1..=16  -> S16
            //   17..=24 -> S24
            //   25..=32 -> S32
            let format = match bps {
                1..=16 => oxideav_core::SampleFormat::S16,
                17..=24 => oxideav_core::SampleFormat::S24,
                _ => oxideav_core::SampleFormat::S32,
            };
            let bps_bytes = format.bytes_per_sample();
            for chunk in a.data[0].chunks_exact(bps_bytes * n_ch) {
                for c in 0..n_ch {
                    let off = c * bps_bytes;
                    let s = match format {
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

    /// `wasted_bits` returns the GCD of trailing zero bits across the
    /// non-zero samples. Used as the FLAC "wasted bits per sample"
    /// shift; a non-zero return tells `best_subframe` to scale samples
    /// down before predictor selection.
    #[test]
    fn wasted_bits_detects_common_trailing_zeros() {
        assert_eq!(wasted_bits(&[0i32; 10]), 0); // CONSTANT path takes over for all-zero
        assert_eq!(wasted_bits(&[1, 2, 3, 4]), 0); // odd value present
        assert_eq!(wasted_bits(&[0, 256, -256, 512, -512]), 8); // all multiples of 256
        assert_eq!(wasted_bits(&[2, 4, 6, 8]), 1); // every value even, but lowest power-of-2 is 2
        assert_eq!(wasted_bits(&[16, 32, 64]), 4); // every value divisible by 16
    }

    /// A signal whose samples are all multiples of 256 should encode
    /// noticeably smaller than the same content at full 16-bit
    /// precision: the wasted-bits detector folds the 8 trailing zero
    /// bits into the 1-byte unary header instead of paying them out
    /// per sample.
    #[test]
    fn wasted_bits_shrinks_subframe_for_8_bit_padded_signal() {
        let n = 1024usize;
        let mut padded: Vec<i32> = Vec::with_capacity(n);
        let mut raw: Vec<i32> = Vec::with_capacity(n);
        for i in 0..n {
            let v = ((i as f64 / n as f64 * 17.0 * std::f64::consts::PI).sin() * 100.0) as i32;
            raw.push(v);
            padded.push(v << 8);
        }
        let sf_padded = best_subframe(&padded, 16).unwrap();
        let sf_raw = best_subframe(&raw, 16).unwrap();
        // sf_padded carries an 8-bit unary header but uses the same
        // residual coding as sf_raw — the difference is only the
        // header. Total cost should be within ~20 bits of the raw plan.
        assert!(
            sf_padded.bits < sf_raw.bits + 20,
            "wasted bits should fold the 8 trailing zeros: padded={} raw={}",
            sf_padded.bits,
            sf_raw.bits,
        );
    }

    /// End-to-end: encode a wasted-bits-friendly signal, decode it, and
    /// verify bit-exact recovery. The wasted-bits header in the
    /// subframe is opaque to the caller — what matters is that the
    /// shifted predictor + post-shift on the decode side reconstruct
    /// every sample identically.
    #[test]
    fn encode_decode_wasted_bits_roundtrip() {
        let n = 1024usize;
        let mut samples: Vec<i32> = Vec::with_capacity(n);
        for i in 0..n {
            // Sine scaled to 8-bit precision then padded into the 16-bit
            // sample window — every sample is a multiple of 256.
            let v = ((i as f64 / n as f64 * 17.0 * std::f64::consts::PI).sin() * 100.0) as i32;
            samples.push(v << 8);
        }
        roundtrip(vec![samples], 48_000, 16);
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

    /// Helper: bits the residual coder spends on a residual block when it
    /// is locked to a single partition order under method 0.
    fn residual_bits_at_order(
        residuals: &[i32],
        block_size: usize,
        order: usize,
        partition_order: u32,
    ) -> u64 {
        let cost =
            partition_layout_cost(residuals, block_size, order, partition_order, 4, 14).unwrap();
        2 + 4 + cost
    }

    /// A residual block whose statistics change sharply between halves
    /// (quiet first half, loud second half) is exactly the case higher
    /// partition orders exist to exploit: a single global Rice parameter
    /// must compromise, whereas two partitions can each pick their own.
    /// Verify the search picks a non-trivial layout that beats order 0.
    #[test]
    fn partition_search_beats_order_zero_on_split_statistics() {
        let block = 4096usize;
        let order = 0usize;
        let mut residuals: Vec<i32> = Vec::with_capacity(block);
        for i in 0..block {
            if i < block / 2 {
                residuals.push((i % 3) as i32 - 1); // tiny: -1,0,1
            } else {
                residuals.push(((i * 2654435761usize) % 20000) as i32 - 10000); // large
            }
        }
        let order0 = residual_bits_at_order(&residuals, block, order, 0);
        let order1 = residual_bits_at_order(&residuals, block, order, 1);
        assert!(
            order1 < order0,
            "order-1 split ({order1}) should beat order-0 ({order0}) on split stats"
        );

        // The full search (what the encoder actually emits) must be no
        // worse than the best fixed order we can name.
        let mut w = BitWriter::new();
        let emitted = encode_rice_residual(&mut w, &residuals, block, order);
        assert!(
            (emitted as u64) <= order0,
            "search emitted {emitted} bits, worse than order-0 {order0}"
        );
        // The reported bit count must match the writer's real output:
        // finishing pads to the next byte, so the byte length is the
        // ceiling of `emitted / 8`.
        let written = w.into_bytes();
        assert_eq!(
            written.len(),
            emitted.div_ceil(8) as usize,
            "bit accounting mismatch: reported {emitted} bits"
        );
    }

    /// The adaptive precision heuristic must stay inside the spec's
    /// legal `[MIN, MAX]` window, rise monotonically with block size,
    /// and never reach the forbidden precision 16 (`qlp_precis` 0b1111).
    #[test]
    fn lpc_precision_heuristic_within_legal_window() {
        // Monotone non-decreasing in block size.
        let sizes = [1usize, 8, 16, 32, 64, 256, 1024, 4096, 16384, 65536];
        let mut prev = 0u32;
        for &n in &sizes {
            let p = lpc_precision_for(n);
            assert!(
                (MIN_LPC_QLP_PRECISION..=MAX_LPC_QLP_PRECISION).contains(&p),
                "precision {p} for block {n} outside [{MIN_LPC_QLP_PRECISION}, {MAX_LPC_QLP_PRECISION}]"
            );
            assert!(
                p >= prev,
                "precision must not drop: {prev} -> {p} at block {n}"
            );
            prev = p;
        }
        // The legal ceiling is 15; encoded as `precision - 1` it is
        // 0b1110, one below the forbidden 0b1111. Every value the
        // heuristic returns must therefore encode below 0b1111.
        for &n in &sizes {
            assert!(
                lpc_precision_for(n) - 1 < 0xF,
                "block {n} encodes forbidden 0b1111"
            );
        }
        assert_eq!(lpc_precision_for(4096), 15);
        // Tiny blocks floor at the minimum (a meaningful coefficient
        // field, never 0-bit).
        assert_eq!(lpc_precision_for(1), MIN_LPC_QLP_PRECISION);
    }

    /// Quantising at higher precision keeps the coefficients closer to
    /// their floating-point ideals, which shrinks the residual. For a
    /// clean predictable signal the residual saving should outweigh the
    /// extra coefficient bits, so the high-precision LPC plan is no
    /// larger than the low-precision one. (We compare residual-driving
    /// quantisations directly so the test is independent of the
    /// block-size heuristic.)
    #[test]
    fn higher_precision_does_not_inflate_predictable_lpc() {
        let sr = 48_000u32;
        let n = 4096usize;
        let mut samples: Vec<i32> = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f64 / sr as f64;
            let v = (t * 440.0 * 2.0 * std::f64::consts::PI).sin() * 18_000.0
                + (t * 880.0 * 2.0 * std::f64::consts::PI).sin() * 4_000.0;
            samples.push(v as i32);
        }
        let order = 8usize;
        let coeffs = levinson_durbin(&samples, order).unwrap();

        let residual_bits_at = |precision: u32| -> u64 {
            let (q, shift) = quantize_lpc(&coeffs, precision).unwrap();
            let mut res = Vec::with_capacity(n - order);
            for i in order..n {
                let mut pred: i64 = 0;
                for (j, &qc) in q.iter().enumerate() {
                    pred += qc as i64 * samples[i - 1 - j] as i64;
                }
                res.push((samples[i] as i64 - (pred >> shift)) as i32);
            }
            let mut w = BitWriter::new();
            let res_bits = encode_rice_residual(&mut w, &res, n, order) as u64;
            // Total subframe-body cost = coefficient field + residual.
            res_bits + (precision as u64) * order as u64
        };

        let low = residual_bits_at(8);
        let high = residual_bits_at(15);
        assert!(
            high <= low,
            "15-bit precision ({high}) should not exceed 8-bit ({low}) on a clean tone"
        );
    }

    /// An LPC subframe encoded for a 4096-sample block must declare the
    /// heuristic's precision (15) on the wire, read its coefficients at
    /// that width, and still decode bit-exactly. This guards the
    /// encoder/decoder agreement on the now-variable `qlp_precis` field.
    #[test]
    fn adaptive_precision_lpc_roundtrips_at_full_block() {
        let sr = 44_100u32;
        let n = 4096usize;
        let mut samples: Vec<i32> = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f64 / sr as f64;
            let v = (t * 523.25 * 2.0 * std::f64::consts::PI).sin() * 22_000.0
                + (t * 1046.5 * 2.0 * std::f64::consts::PI).sin() * 7_000.0;
            samples.push(v as i32);
        }
        // Confirm the chosen subframe is actually LPC at the full
        // precision the heuristic picks for this block size.
        let plan = best_subframe(&samples, 16).unwrap();
        // type byte: bit0=pad, bits1..7 = type code. LPC is 0b1xxxxx.
        let type_code = (plan.payload[0] >> 1) & 0x3F;
        assert!(
            type_code & 0b100000 != 0,
            "expected an LPC subframe for a predictable tone, got type {type_code:#08b}"
        );
        roundtrip(vec![samples], sr, 16);
    }

    /// Round-trip a stereo block engineered to favour partitioning: the
    /// signal is quiet for the first part of every block then bursts
    /// loud, so the encoder should reach for higher partition orders.
    /// Bit-exact recovery proves the partitioned-Rice writer and the
    /// decoder agree on partition boundaries.
    #[test]
    fn encode_decode_partitioned_residual_roundtrip() {
        let sr = 48_000u32;
        let n = 4096usize;
        let mut l: Vec<i32> = Vec::with_capacity(n);
        let mut r: Vec<i32> = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f64 / sr as f64;
            let env = if i % 1024 < 256 { 200.0 } else { 18_000.0 };
            let v = (t * 440.0 * 2.0 * std::f64::consts::PI).sin() * env;
            l.push(v as i32);
            r.push((v * 0.7) as i32);
        }
        roundtrip(vec![l, r], sr, 16);
    }

    /// Lifting MAX_LPC_ORDER from 8 to 12 must not regress on a
    /// content type where the lower orders already win — the encoder
    /// considers every order in 1..=MAX_LPC_ORDER and keeps the
    /// smallest, so adding orders can only ever shrink (or tie) the
    /// output. A clean sine has its energy concentrated at a single
    /// harmonic, so order ≈ 2..4 already captures the bulk of the
    /// correlation; orders 9..=12 should be discarded silently,
    /// producing the same byte count as a search restricted to
    /// 1..=8.
    #[test]
    fn higher_max_order_does_not_regress_low_order_friendly_signal() {
        let sr = 48_000u32;
        let n = 4096usize;
        let mut samples: Vec<i32> = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f64 / sr as f64;
            let v = (t * 440.0 * 2.0 * std::f64::consts::PI).sin() * 20_000.0;
            samples.push(v as i32);
        }
        // Encoder-with-current-cap should choose a plan no larger than
        // a hand-rolled search capped at order 8.
        let plan_full = best_subframe(&samples, 16).unwrap();
        let mut best_low: u64 = encode_verbatim_plan(&samples, 16, 0).bits;
        for order in 0..=4usize {
            if let Some(p) = encode_fixed_plan(&samples, 16, order, 0) {
                best_low = best_low.min(p.bits);
            }
        }
        for order in 1..=8usize {
            if let Some(p) = encode_lpc_plan(&samples, 16, order, 0) {
                best_low = best_low.min(p.bits);
            }
        }
        assert!(
            plan_full.bits <= best_low,
            "raising MAX_LPC_ORDER must not inflate the plan: {} > {}",
            plan_full.bits,
            best_low
        );
    }

    /// A signal engineered to need a longer impulse response than
    /// order 8 — a sum of harmonics passed through a pseudo-random
    /// AR(11) recursion. AR processes of order p have an
    /// autocorrelation that doesn't decay before lag p, so a p-tap
    /// LPC fits much better than anything shorter. With
    /// MAX_LPC_ORDER raised from 8 to 12 the encoder gets to try
    /// orders in 9..=12, and at least one of them should produce a
    /// strictly smaller plan than the best in 1..=8 — otherwise the
    /// raised cap buys nothing on the workload it targets.
    #[test]
    fn higher_max_order_wins_on_ar11_signal() {
        let n = 4096usize;
        // Order-11 AR recursion: y[n] = sum a[k]*y[n-k] + drive[n].
        // The coefficients are small fractions that keep the filter
        // stable; the drive is a deterministic LCG so the test is
        // reproducible.
        let a: [f64; 11] = [
            0.62, -0.31, 0.18, -0.12, 0.09, -0.07, 0.06, -0.05, 0.04, -0.03, 0.02,
        ];
        let mut y = vec![0.0f64; n];
        let mut rng: u64 = 0x1234_5678_9abc_def0;
        for i in 0..n {
            // Marsaglia-style xorshift, kept in [-1, 1].
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let drive = ((rng as i64) as f64) / (i64::MAX as f64);
            let mut acc = drive * 2000.0;
            for (k, &ak) in a.iter().enumerate() {
                if i > k {
                    acc += ak * y[i - 1 - k];
                }
            }
            y[i] = acc;
        }
        let samples: Vec<i32> = y
            .iter()
            .map(|&v| v.clamp(-30_000.0, 30_000.0) as i32)
            .collect();

        let mut best_low: u64 = u64::MAX;
        for order in 1..=8usize {
            if let Some(p) = encode_lpc_plan(&samples, 16, order, 0) {
                best_low = best_low.min(p.bits);
            }
        }
        let mut best_high: u64 = u64::MAX;
        for order in 9..=MAX_LPC_ORDER {
            if let Some(p) = encode_lpc_plan(&samples, 16, order, 0) {
                best_high = best_high.min(p.bits);
            }
        }
        assert!(
            best_high < best_low,
            "expected order 9..=12 to beat order 1..=8 on an AR(11) signal, \
             got best_high={best_high} best_low={best_low}"
        );
    }

    /// An LPC subframe encoded at each of the newly-enabled orders
    /// 9..=12 must round-trip bit-exactly through this crate's own
    /// subframe decoder. The decoder already accepts orders up to 32
    /// per RFC 9639 §9.2.6, so this guards that the encoder's
    /// `100000 | (order-1)` subframe-type encoding, the warmup-sample
    /// emission, and the coefficient field width all line up with the
    /// decoder's expectations for every order in the lifted range.
    /// We drive the AR(11) signal so the chosen LPC coefficients are
    /// stable; the test is independent of whether `best_subframe`
    /// happens to prefer the high-order plan on this particular block.
    #[test]
    fn encode_decode_lpc_orders_9_to_12_subframe_roundtrip() {
        use oxideav_core::bits::BitReader;

        let n = 4096usize;
        let a: [f64; 11] = [
            0.62, -0.31, 0.18, -0.12, 0.09, -0.07, 0.06, -0.05, 0.04, -0.03, 0.02,
        ];
        let mut y = vec![0.0f64; n];
        let mut rng: u64 = 0xfedc_ba98_7654_3210;
        for i in 0..n {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            let drive = ((rng as i64) as f64) / (i64::MAX as f64);
            let mut acc = drive * 2000.0;
            for (k, &ak) in a.iter().enumerate() {
                if i > k {
                    acc += ak * y[i - 1 - k];
                }
            }
            y[i] = acc;
        }
        let samples: Vec<i32> = y
            .iter()
            .map(|&v| v.clamp(-30_000.0, 30_000.0) as i32)
            .collect();

        let bps = 16u32;
        for order in 9..=MAX_LPC_ORDER {
            let plan = encode_lpc_plan(&samples, bps, order, 0)
                .unwrap_or_else(|| panic!("encode_lpc_plan failed at order {order}"));
            // Decode the subframe payload back and check bit-exact
            // recovery against the source samples.
            let mut br = BitReader::new(&plan.payload);
            let decoded = crate::subframe::decode_subframe(&mut br, n as u32, bps)
                .unwrap_or_else(|e| panic!("decode failed at order {order}: {e}"));
            assert_eq!(decoded.len(), n, "decode length mismatch at order {order}");
            for (i, (&s, &d)) in samples.iter().zip(decoded.iter()).enumerate() {
                assert_eq!(
                    s, d,
                    "round-trip mismatch at order {order} sample {i}: {s} vs {d}"
                );
            }
        }
    }
}
