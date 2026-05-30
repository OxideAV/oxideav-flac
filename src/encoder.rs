//! Pure-Rust FLAC encoder.
//!
//! Produces valid FLAC streams that decode bit-exactly via any compliant
//! decoder. Per-subframe the encoder tries CONSTANT, FIXED orders 0..=4,
//! LPC orders 1..=12 and VERBATIM, and picks the smallest. Each LPC order
//! additionally searches the coefficient precision over a small window
//! beneath a block-size-derived ceiling (larger blocks amortise the
//! coefficient cost, so the ceiling rises with `log2(block_len)` up to
//! the spec's 15-bit limit) and keeps whichever precision encodes fewest
//! bits. For stereo
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
use crate::metadata::{write_padding, BlockHeader, BlockType};
use oxideav_core::bits::BitWriter;

/// Caller-supplied tuning knobs for [`make_encoder_with_options`]. All
/// fields default to "match the historical `make_encoder` behaviour", so
/// constructing the default and passing it in is observationally
/// identical to calling `make_encoder` directly.
///
/// The struct is `#[non_exhaustive]` so additions in later rounds (e.g.
/// per-frame block-size override, LPC-order cap) don't break existing
/// callers; build one with `FlacEncoderOptions::default()` and mutate
/// the fields you want to set.
#[derive(Clone, Debug, Default)]
#[non_exhaustive]
pub struct FlacEncoderOptions {
    /// Reserve `n` bytes of PADDING (RFC 9639 §8.6) after STREAMINFO in
    /// the produced `extradata` metadata chain. `None` (the default)
    /// emits STREAMINFO as the last and only metadata block — the
    /// historical pre-round-158 behaviour. `Some(n)` emits STREAMINFO
    /// with `last=false`, then a `n`-byte all-zero PADDING block with
    /// `last=true`, letting downstream tools (taggers, cuesheet tools)
    /// rewrite metadata in place without having to shift the audio
    /// frames. Capped at `BlockHeader::MAX_LENGTH` (2^24 − 1) — a
    /// larger value would overflow the 24-bit length field, so
    /// `make_encoder_with_options` returns `Error::invalid` rather
    /// than silently truncating.
    pub padding_bytes: Option<usize>,
}

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
/// How many precisions below the block-size heuristic the per-subframe
/// search will additionally evaluate. The heuristic value is the
/// search window's *ceiling* (the conservative "as much precision as
/// the block can amortise" choice), and the search tries each precision
/// down to `heuristic - LPC_PRECISION_SEARCH_SPAN`, clamped at
/// `MIN_LPC_QLP_PRECISION`. RFC 9639 §9.2.6 leaves the precision a free
/// encoder parameter (it is written verbatim into `qlp_precis` and read
/// back at that width), so picking it per subframe is purely a
/// rate decision: a coefficient field that buys more residual reduction
/// than its extra bits cost is kept, otherwise a narrower field wins.
/// A span of 4 keeps the extra encode work bounded (at most five
/// quantise+residual passes per LPC order) while covering the band where
/// the coefficient-vs-residual trade-off actually flips for typical
/// content.
const LPC_PRECISION_SEARCH_SPAN: u32 = 4;

/// Reusable scratch buffers threaded through the per-subframe candidate
/// sweep so the residuals / zigzag / prefix / raw-bits / Levinson tables
/// are allocated **once** per encoder (not once per `(method, order,
/// window, precision)` combination the search visits).
///
/// The pre-round-191 hot path allocated a fresh `Vec<i32>` residual for
/// each FIXED 0..=4 and LPC 1..=12 candidate, a fresh `Vec<u32>` zigzag
/// plus `Vec<u64>` prefix plus `Vec<u8>` raw-bits triple inside
/// `encode_rice_residual` for each surviving plan, and three `Vec<f64>`
/// scratch buffers (`windowed`, `autoc`, `lpc`) plus a `Vec<f64>`
/// output for each apodisation window times LPC order pair. Profile-147
/// surfaced the resulting allocator activity in the encode flamegraph's
/// "remaining 25 %" sink.
///
/// Hoisting them to subframe scope (re-used across every candidate)
/// and storing the scratch on the encoder itself (re-used across
/// frames) lets the candidate sweep run entirely on the previous
/// frame's heap allocations: `Vec::clear` keeps capacity, the next
/// `push` reuses it. Capacity converges within the first block to
/// the maximum the encoder will ever need, and stays there.
///
/// `lpc_coeffs` holds the final FLAC-convention coefficients
/// (`-a[j]` for `j=1..=order`) that `levinson_durbin_s` would have
/// returned by `Vec`; the candidate-precision sweep in
/// `encode_lpc_plan_s` borrows it as `&[f64]` instead of cloning.
#[derive(Default)]
struct SubframeScratch {
    /// Residual values: `samples[order..] - prediction` for FIXED and LPC.
    residuals: Vec<i32>,
    /// Zigzag-encoded view of `residuals` so the partition search reads
    /// unsigned widths directly.
    zigzag: Vec<u32>,
    /// `prefix[i] = sum(zigzag[0..i])` — `sum_u` for any partition is
    /// `prefix[end] - prefix[start]`.
    prefix: Vec<u64>,
    /// Per-residual raw width (1..=32 bits) so the escape-cost test is
    /// a `max` over a `u8` slice instead of `leading_zeros` per element.
    raw_bits: Vec<u8>,
    /// Windowed signal used by `levinson_durbin_s` (samples × window
    /// weight). One row per Levinson call, reused across windows.
    windowed: Vec<f64>,
    /// Autocorrelation vector (length `order + 1`).
    autoc: Vec<f64>,
    /// In-place Levinson-Durbin coefficient state (length `order`).
    lpc_state: Vec<f64>,
    /// Final FLAC-convention LPC coefficients (`-a[j+1]`), borrowed by
    /// `encode_lpc_plan_at_s` for the candidate-precision sweep.
    lpc_coeffs: Vec<f64>,
    /// Quantised LPC coefficients (length `order`).
    qcoeffs: Vec<i32>,
    /// Wasted-bits-shifted sample buffer used by `best_subframe_s`.
    shifted: Vec<i32>,
}

/// Analysis windows the encoder applies to a subframe before computing
/// the autocorrelation that feeds Levinson-Durbin. RFC 9639 only ties
/// down what reaches the bitstream — the quantised coefficients, the
/// precision, and the shift — and explicitly notes (Acknowledgments,
/// "Norman Levinson and James Durbin") that deriving the coefficients
/// from the autocorrelation is an *encoder* concern. Which window
/// tapers the signal before that autocorrelation is therefore a free
/// analysis choice with **no** decoder-side footprint: the decoder
/// never sees it. Different windows trade spectral leakage against
/// retained edge energy, and the best one is signal-dependent, so the
/// encoder computes coefficients under each, residual-codes them, and
/// keeps whichever LPC subframe ends up smallest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ApodizationWindow {
    /// Parabolic taper `1 - x^2`. Cheap, smooth, retains a lot of centre
    /// energy; the long-standing default for this encoder and a strong
    /// all-rounder on steady tonal content.
    Welch,
    /// Raised-cosine `0.5 - 0.5*cos(2*pi*i/(N-1))`. Lower side lobes
    /// than Welch, which suppresses spectral leakage on signals with
    /// closely-spaced partials at the cost of more aggressive edge
    /// attenuation.
    Hann,
    /// Tapered-cosine ("Tukey") with taper fraction `alpha`: a flat top
    /// over the central `(1-alpha)` of the block with cosine ramps over
    /// the outer `alpha`. With a small `alpha` it keeps almost the whole
    /// block at full weight (good for transient-light blocks where edge
    /// energy still matters) while smoothing the discontinuity at the
    /// block boundary that an un-windowed autocorrelation would alias.
    Tukey { alpha_num: u32, alpha_den: u32 },
}

/// The windows the per-subframe LPC search evaluates, in priority order
/// (ties are broken toward the earlier entry, which keeps the historical
/// Welch result when nothing beats it). Three windows with materially
/// different leakage / edge-energy trade-offs cover the common cases
/// without ballooning the per-frame solve count: Welch (centre-heavy,
/// the prior default), Hann (low leakage), and a shallow Tukey(1/4)
/// (mostly flat, light edge smoothing).
const APODIZATION_WINDOWS: [ApodizationWindow; 3] = [
    ApodizationWindow::Welch,
    ApodizationWindow::Hann,
    ApodizationWindow::Tukey {
        alpha_num: 1,
        alpha_den: 4,
    },
];

impl ApodizationWindow {
    /// Window weight at sample index `i` of an `n`-sample block. All
    /// windows are symmetric and normalised to peak at 1.0 in the
    /// centre, so they only re-weight the signal (Levinson-Durbin is
    /// scale-invariant in the coefficients it produces). For `n <= 1`
    /// every weight is 1.0 (degenerate block; the LPC path bails out
    /// earlier anyway).
    fn weight(self, i: usize, n: usize) -> f64 {
        if n <= 1 {
            return 1.0;
        }
        let last = (n - 1) as f64;
        match self {
            ApodizationWindow::Welch => {
                let half = last / 2.0;
                let x = (i as f64 - half) / (half + 1.0);
                1.0 - x * x
            }
            ApodizationWindow::Hann => {
                let t = i as f64 / last;
                0.5 - 0.5 * (2.0 * std::f64::consts::PI * t).cos()
            }
            ApodizationWindow::Tukey {
                alpha_num,
                alpha_den,
            } => {
                let alpha = alpha_num as f64 / alpha_den as f64;
                if alpha <= 0.0 {
                    return 1.0; // rectangular
                }
                let alpha = alpha.min(1.0);
                let t = i as f64 / last; // in [0, 1]
                let edge = alpha / 2.0;
                if t < edge {
                    0.5 * (1.0 + (std::f64::consts::PI * (t / edge - 1.0)).cos())
                } else if t <= 1.0 - edge {
                    1.0
                } else {
                    0.5 * (1.0 + (std::f64::consts::PI * ((t - 1.0) / edge + 1.0)).cos())
                }
            }
        }
    }
}

/// Choose the upper bound on LPC coefficient precision for a subframe of
/// `block_len` samples. A larger block amortises the per-coefficient
/// storage cost over more residual samples, so it can afford — and
/// usually benefits from — higher precision; a short block keeps
/// precision low so the warm-up plus coefficient overhead doesn't
/// dominate. The precision rises with `log2(block_len)` and is clamped
/// to the legal `[MIN, MAX]` window. RFC 9639 §9.2.6 caps the on-wire
/// precision at 15. This is the *ceiling* of the per-subframe search in
/// `encode_lpc_plan`, which may settle on a lower value if it encodes
/// smaller.
fn lpc_precision_for(block_len: usize) -> u32 {
    // `bits` ≈ floor(log2(block_len)); a 4096-sample block gives 12,
    // which lands precision at 15 (the legal ceiling). Tiny blocks
    // (< 32 samples) floor out at MIN_LPC_QLP_PRECISION.
    let bits = (usize::BITS - block_len.max(1).leading_zeros()).saturating_sub(1);
    let precision = bits + 3;
    precision.clamp(MIN_LPC_QLP_PRECISION, MAX_LPC_QLP_PRECISION)
}

pub fn make_encoder(params: &CodecParameters) -> Result<Box<dyn Encoder>> {
    make_encoder_with_options(params, FlacEncoderOptions::default())
}

/// Same as [`make_encoder`] but accepts a [`FlacEncoderOptions`] for
/// caller-tunable knobs (padding reservation, future LPC / block-size
/// overrides). `make_encoder` is a thin wrapper that passes
/// `FlacEncoderOptions::default()` through.
pub fn make_encoder_with_options(
    params: &CodecParameters,
    options: FlacEncoderOptions,
) -> Result<Box<dyn Encoder>> {
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
    // Validate padding length up front so the failure point is
    // construction, not the first call to `flush`.
    if let Some(n) = options.padding_bytes {
        if n > BlockHeader::MAX_LENGTH as usize {
            return Err(Error::invalid(format!(
                "FLAC encoder: padding_bytes {} exceeds 24-bit metadata length max ({})",
                n,
                BlockHeader::MAX_LENGTH
            )));
        }
    }

    let extradata = build_extradata(
        DEFAULT_BLOCK_SIZE,
        sample_rate,
        channels as u8,
        bps,
        0,
        0,
        0,
        &[0u8; 16],
        options.padding_bytes,
    )?;

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
        padding_bytes: options.padding_bytes,
        scratch: SubframeScratch::default(),
    }))
}

/// Build a full METADATA_BLOCK (header + STREAMINFO payload).
///
/// `last` controls the high bit of the block-type byte: `true` marks
/// STREAMINFO as the final block in the chain (the historical
/// behaviour when no extra metadata blocks follow); `false` lets
/// callers chain additional blocks (PADDING, etc.) afterwards.
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
    last: bool,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + 34);
    // Block-header byte 0: high bit = `last`, low 7 bits = STREAMINFO
    // type code (0).
    out.push(if last { 0x80 } else { 0x00 });
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

/// Build the full `extradata` metadata chain the encoder hands the
/// muxer: STREAMINFO, optionally followed by a `last`-flagged PADDING
/// block. When `padding_bytes` is `None`, STREAMINFO carries the
/// `last` flag itself and the chain stops there — byte-identical to
/// the pre-round-158 behaviour. When `padding_bytes` is `Some(n)`,
/// STREAMINFO is non-last and a `n`-byte all-zero PADDING block with
/// `last=true` follows. The PADDING payload comes from
/// [`metadata::write_padding`] so the same bound check that protects
/// the standalone writer also protects callers that route through the
/// encoder.
#[allow(clippy::too_many_arguments)]
fn build_extradata(
    block_size: u32,
    sample_rate: u32,
    channels: u8,
    bps: u8,
    min_frame_size: u32,
    max_frame_size: u32,
    total_samples: u64,
    md5: &[u8; 16],
    padding_bytes: Option<usize>,
) -> Result<Vec<u8>> {
    let last_streaminfo = padding_bytes.is_none();
    let mut out = build_streaminfo_metadata_block(
        block_size,
        sample_rate,
        channels,
        bps,
        min_frame_size,
        max_frame_size,
        total_samples,
        md5,
        last_streaminfo,
    );
    if let Some(n) = padding_bytes {
        let payload = write_padding(n)?;
        let header = BlockHeader {
            last: true,
            block_type: BlockType::Padding,
            length: payload.len() as u32,
        };
        header.write_into(&mut out)?;
        out.extend_from_slice(&payload);
    }
    Ok(out)
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
    /// Number of PADDING bytes to reserve after STREAMINFO in the
    /// final `extradata` chain. `None` means no PADDING block — the
    /// pre-round-158 behaviour. See [`FlacEncoderOptions::padding_bytes`].
    padding_bytes: Option<usize>,
    /// Reusable scratch buffers for the per-subframe candidate sweep.
    /// First frame grows them to the maximum the encoder will need;
    /// subsequent frames reuse the capacity in place. See
    /// [`SubframeScratch`] for details.
    scratch: SubframeScratch,
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
                &mut self.scratch,
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
    /// When `padding_bytes` was set on construction, the rebuilt chain
    /// preserves the trailing PADDING block — the bound check happened
    /// at construction time, so the rebuild is infallible here.
    fn finalize_streaminfo(&mut self) {
        let md5_bytes = std::mem::take(&mut self.md5).finalize();
        let min_frame_size = if self.min_frame_size == u32::MAX {
            0
        } else {
            self.min_frame_size
        };
        // `build_extradata` only returns Err if PADDING was set above
        // BlockHeader::MAX_LENGTH; we already rejected that at
        // construction time, so this can't fail here.
        self.output_params.extradata = build_extradata(
            self.block_size,
            self.sample_rate,
            self.channels as u8,
            self.bps,
            min_frame_size,
            self.max_frame_size,
            self.total_samples,
            &md5_bytes,
            self.padding_bytes,
        )
        .expect("padding_bytes already validated at encoder construction");
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
    scratch: &mut SubframeScratch,
) -> Result<Vec<u8>> {
    let n_ch = channels.len();
    if !(1..=8).contains(&n_ch) {
        return Err(Error::invalid("FLAC encoder: channel count out of range"));
    }

    let (channel_code, subframe_plan) = choose_channel_assignment(channels, bps as u32, scratch)?;

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
fn choose_channel_assignment(
    channels: &[Vec<i32>],
    bps: u32,
    scratch: &mut SubframeScratch,
) -> Result<(u32, Vec<SubframePlan>)> {
    let n_ch = channels.len();
    if n_ch != 2 {
        let mut plans = Vec::with_capacity(n_ch);
        for ch in channels {
            plans.push(best_subframe_s(ch, bps, scratch)?);
        }
        return Ok(((n_ch - 1) as u32, plans));
    }

    let l = &channels[0];
    let r = &channels[1];
    let n = l.len();

    let sf_l = best_subframe_s(l, bps, scratch)?;
    let sf_r = best_subframe_s(r, bps, scratch)?;
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
        let sf_s = best_subframe_s(&side, bps + 1, scratch)?;
        let sf_m = best_subframe_s(&mid, bps, scratch)?;
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
#[cfg(test)]
fn best_subframe(samples: &[i32], bps: u32) -> Result<SubframePlan> {
    let mut scratch = SubframeScratch::default();
    best_subframe_s(samples, bps, &mut scratch)
}

/// Scratch-buffered variant of [`best_subframe`] used by the encoder
/// hot path. Buffer-reuse keeps the candidate sweep's heap activity
/// constant across frames; see [`SubframeScratch`] for the buffer
/// inventory and reuse contract.
fn best_subframe_s(
    samples: &[i32],
    bps: u32,
    scratch: &mut SubframeScratch,
) -> Result<SubframePlan> {
    let n = samples.len();
    if n == 0 {
        return Err(Error::invalid("FLAC encoder: empty subframe"));
    }

    // The wasted-bit indicator + count occupies `1 + max(wasted, 1)`
    // bits. To stay safe vs reducing effective bps below 1 we cap the
    // count at bps-1.
    let wasted = wasted_bits(samples).min(bps.saturating_sub(1));
    if wasted > 0 {
        scratch.shifted.clear();
        scratch.shifted.reserve(n);
        for &s in samples {
            scratch.shifted.push(s >> wasted);
        }
        // Hand the shifted slice to the inner driver without aliasing
        // the scratch — `best_subframe_with_wasted_s` uses every other
        // buffer on the struct but never `shifted`, so a clone-free
        // borrow via `std::mem::take` + restore is safe.
        let shifted = std::mem::take(&mut scratch.shifted);
        let result = best_subframe_with_wasted_s(&shifted, bps - wasted, wasted, scratch);
        scratch.shifted = shifted;
        return result;
    }
    best_subframe_with_wasted_s(samples, bps, 0, scratch)
}

/// Inner driver that scores every subframe type at the given
/// (already-wasted-shifted) bps and tags the chosen plan with the
/// wasted-bits count so the writer can prepend the header bits.
/// Scratch-buffered inner driver. Threads `scratch` into the FIXED and
/// LPC candidate searches so the per-plan residual / partition tables
/// live in one set of reusable buffers instead of one fresh `Vec` per
/// `(method, order)` candidate.
fn best_subframe_with_wasted_s(
    samples: &[i32],
    bps: u32,
    wasted: u32,
    scratch: &mut SubframeScratch,
) -> Result<SubframePlan> {
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
        if let Some(plan) = encode_fixed_plan_s(samples, bps, order, wasted, scratch) {
            if plan.bits < best.bits {
                best = plan;
            }
        }
    }

    let max_lpc = MAX_LPC_ORDER.min(n.saturating_sub(1));
    for order in 1..=max_lpc {
        if let Some(plan) = encode_lpc_plan_s(samples, bps, order, wasted, scratch) {
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

#[cfg(test)]
fn encode_fixed_plan(samples: &[i32], bps: u32, order: usize, wasted: u32) -> Option<SubframePlan> {
    let mut scratch = SubframeScratch::default();
    encode_fixed_plan_s(samples, bps, order, wasted, &mut scratch)
}

/// Scratch-buffered FIXED plan. The residual vector + every downstream
/// partitioned-Rice scratch table live in `scratch` and survive the
/// candidate sweep instead of being re-allocated per candidate.
fn encode_fixed_plan_s(
    samples: &[i32],
    bps: u32,
    order: usize,
    wasted: u32,
    scratch: &mut SubframeScratch,
) -> Option<SubframePlan> {
    let n = samples.len();
    if n <= order {
        return None;
    }
    // Fixed-predictor coefficients (FLAC spec §SUBFRAME_FIXED).
    const COEFFS: [&[i64]; 5] = [&[], &[1], &[2, -1], &[3, -3, 1], &[4, -6, 4, -1]];
    let c = COEFFS[order];
    scratch.residuals.clear();
    scratch.residuals.reserve(n - order);
    for i in order..n {
        let mut pred: i64 = 0;
        for (j, &cj) in c.iter().enumerate() {
            pred += cj * samples[i - 1 - j] as i64;
        }
        let r = samples[i] as i64 - pred;
        if !(i32::MIN as i64..=i32::MAX as i64).contains(&r) {
            return None;
        }
        scratch.residuals.push(r as i32);
    }

    let mut w = BitWriter::new();
    let header_bits = write_subframe_header(&mut w, 0b001000 | (order as u32 & 0x07), wasted);
    for i in 0..order {
        w.write_i32(samples[i], bps);
    }
    // Detach `residuals` so `encode_rice_residual_s` can borrow `scratch`
    // for its zigzag / prefix / raw_bits tables without aliasing the
    // residual slice.
    let residuals = std::mem::take(&mut scratch.residuals);
    let residual_bits = encode_rice_residual_s(&mut w, &residuals, n, order, scratch);
    scratch.residuals = residuals;
    Some(bits_snapshot(
        w,
        header_bits + (bps * order as u32) + residual_bits,
    ))
}

/// Build the smallest LPC subframe for `samples` at the given `order`,
/// searching both the analysis window (Welch / Hann / Tukey) **and** the
/// coefficient precision and keeping whichever combination encodes fewest
/// bits.
///
/// The window is an encoder-only analysis choice with no decoder-side
/// footprint (RFC 9639 ties down only the quantised coefficients,
/// precision, and shift that reach the bitstream), so each window yields
/// its own Levinson-Durbin solution; a window that better matches the
/// signal's spectrum produces coefficients that predict more tightly and
/// shrink the residual. Precision is then searched over a small window
/// beneath the block-size heuristic: higher precision tracks the
/// floating-point ideals more closely (smaller residual) but spends
/// `precision` extra bits per coefficient, and the flip point is
/// signal-dependent, so the only reliable choice is to encode the
/// candidates and compare actual sizes. The Levinson-Durbin solve runs
/// once per window and is reused across every candidate precision for
/// that window — only the quantise + residual + write passes repeat.
/// Ties break toward the earlier window in `APODIZATION_WINDOWS`, so the
/// historical Welch result is preserved whenever nothing strictly beats
/// it. RFC 9639 §9.2.6.
#[cfg(test)]
fn encode_lpc_plan(samples: &[i32], bps: u32, order: usize, wasted: u32) -> Option<SubframePlan> {
    let mut scratch = SubframeScratch::default();
    encode_lpc_plan_s(samples, bps, order, wasted, &mut scratch)
}

/// Scratch-buffered LPC plan. The Levinson-Durbin scratch (windowed
/// signal, autocorrelation, in-place state) and the candidate-precision
/// quantisation buffer all live in `scratch`. The final coefficient
/// vector `lpc_coeffs` is borrowed by the precision sweep — no `Vec`
/// clone per `(window, precision)` pair.
fn encode_lpc_plan_s(
    samples: &[i32],
    bps: u32,
    order: usize,
    wasted: u32,
    scratch: &mut SubframeScratch,
) -> Option<SubframePlan> {
    let n = samples.len();
    if n <= order {
        return None;
    }
    let ceiling = lpc_precision_for(n);
    let floor = ceiling.saturating_sub(LPC_PRECISION_SEARCH_SPAN);
    let floor = floor.max(MIN_LPC_QLP_PRECISION);

    let mut best: Option<SubframePlan> = None;
    for &window in APODIZATION_WINDOWS.iter() {
        // `levinson_durbin_s` writes its FLAC-convention coefficients
        // into `scratch.lpc_coeffs`. Returning `Ok` lets the precision
        // sweep below read `scratch.lpc_coeffs` directly; on `Err` the
        // buffer's contents are undefined and skipped.
        if !levinson_durbin_s(samples, order, window, scratch) {
            continue;
        }
        // Detach the coefficient vector so the per-precision call can
        // borrow `scratch` mutably for its own residual / partition
        // tables without aliasing the read-only coefficient slice.
        let coeffs = std::mem::take(&mut scratch.lpc_coeffs);
        for precision in floor..=ceiling {
            if let Some(plan) =
                encode_lpc_plan_at_s(samples, bps, order, wasted, &coeffs, precision, scratch)
            {
                let better = match &best {
                    Some(b) => plan.bits < b.bits,
                    None => true,
                };
                if better {
                    best = Some(plan);
                }
            }
        }
        scratch.lpc_coeffs = coeffs;
    }
    best
}

/// Build one LPC subframe at a fixed coefficient `precision` from
/// already-computed floating-point coefficients `coeffs_f`. Returns
/// `None` if the coefficients can't be quantised at this precision or a
/// residual overflows the 32-bit signed window required by RFC 9639
/// §9.2.7.
#[cfg(test)]
fn encode_lpc_plan_at(
    samples: &[i32],
    bps: u32,
    order: usize,
    wasted: u32,
    coeffs_f: &[f64],
    precision: u32,
) -> Option<SubframePlan> {
    let mut scratch = SubframeScratch::default();
    encode_lpc_plan_at_s(
        samples,
        bps,
        order,
        wasted,
        coeffs_f,
        precision,
        &mut scratch,
    )
}

/// Scratch-buffered variant of [`encode_lpc_plan_at`]. Quantised LPC
/// coefficients, residuals, and the partitioned-Rice search tables all
/// reuse `scratch`'s buffers across the candidate sweep.
#[allow(clippy::too_many_arguments)]
fn encode_lpc_plan_at_s(
    samples: &[i32],
    bps: u32,
    order: usize,
    wasted: u32,
    coeffs_f: &[f64],
    precision: u32,
    scratch: &mut SubframeScratch,
) -> Option<SubframePlan> {
    let n = samples.len();
    let qlp_shift = quantize_lpc_into(coeffs_f, precision, &mut scratch.qcoeffs)?;

    scratch.residuals.clear();
    scratch.residuals.reserve(n - order);
    for i in order..n {
        let mut pred: i64 = 0;
        for (j, &q) in scratch.qcoeffs.iter().enumerate() {
            pred += q as i64 * samples[i - 1 - j] as i64;
        }
        let predicted = pred >> qlp_shift;
        let r = samples[i] as i64 - predicted;
        if !(i32::MIN as i64..=i32::MAX as i64).contains(&r) {
            return None;
        }
        scratch.residuals.push(r as i32);
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
    for &q in &scratch.qcoeffs {
        w.write_i32(q, precision);
    }
    // Detach residuals so the Rice residual call can borrow `scratch`
    // mutably for its zigzag / prefix / raw_bits tables.
    let residuals = std::mem::take(&mut scratch.residuals);
    let residual_bits = encode_rice_residual_s(&mut w, &residuals, n, order, scratch);
    scratch.residuals = residuals;
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
///
/// Retained as the reference path against which [`best_partition_z`] is
/// regression-tested in the unit-test module. The production encoder
/// path (`encode_rice_residual`) uses the zigzag-amortised variant.
#[cfg(test)]
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

/// Variant of [`best_partition`] that takes a pre-computed zigzag slice
/// alongside the original residuals (the escape-path raw-bits scan still
/// needs the signed input). Avoids the per-partition `Vec<u32>` alloc
/// + per-residual zigzag re-pass that the standalone path runs.
///
/// Retained as the round-176 reference path. The production encoder
/// (`encode_rice_residual`) now uses [`best_partition_zp`] which
/// additionally amortises the `sum_u` derivation and the `raw_bits`
/// scan across the search via subframe-scoped prefix/per-element tables.
#[cfg(test)]
fn best_partition_z(residuals: &[i32], zigzag: &[u32], param_bits: u32, k_max: u32) -> PartChoice {
    debug_assert_eq!(residuals.len(), zigzag.len());
    let (rice_bits, k) = best_rice_params_z(zigzag, k_max);
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

/// Per-element bit width required to store `residuals[i]` raw as a
/// two's-complement signed integer; matches the per-element body of
/// [`raw_bits_needed`] without its outer max-and-clamp. Always in
/// `1..=32` for any `i32` value. Used to seed [`raw_bits_table`] so
/// the per-partition escape-cost test reduces to a max over a
/// precomputed `u8` slice instead of re-derived `leading_zeros` per
/// partition per `(method, partition_order)` pair.
#[inline(always)]
fn raw_bits_for_sample(r: i32) -> u8 {
    let needed = if r >= 0 {
        33 - (r as u32).leading_zeros()
    } else {
        33 - (!r as u32).leading_zeros()
    };
    // The per-element value is bounded by 32 by construction (33 minus
    // at least 1 leading zero across both arms of the branch), so the
    // cast to u8 cannot truncate.
    needed as u8
}

/// Build the subframe-scoped tables the partitioned-Rice search reuses
/// across every `(method, partition_order)` pair. Two arrays of length
/// `residuals.len() + 1` (prefix sums) and `residuals.len()` (per-sample
/// raw-bit widths) replace one full O(n) sum + one full O(n) leading-zero
/// scan per partition per pair the search visits.
///
/// * `prefix[i] = sum(zigzag[0..i])`, so a partition's `sum_u` is
///   `prefix[end] - prefix[start]` in O(1). Stored as `u64` because the
///   sum of 4096 u32 zigzag values can reach 2^32 × 4096 = 2^44, well
///   inside `u64::MAX` even at the full 4 KiB block.
/// * `raw_bits[i] = raw_bits_for_sample(residuals[i])`. A partition's
///   escape-cost test reads `max(raw_bits[start..end])` (each element is
///   already clamped to `[1, 32]`) instead of running `leading_zeros`
///   inside the inner loop.
///
/// Allocates twice per subframe (the two tables) — both fit in L1 for
/// any realistic block size: the 32 KiB `prefix` + 4 KiB `raw_bits`
/// pair stays under 36 KiB for a 4096-sample residual.
#[cfg(test)]
fn build_partition_tables(residuals: &[i32], zigzag: &[u32]) -> (Vec<u64>, Vec<u8>) {
    let mut prefix = Vec::new();
    let mut raw_bits = Vec::new();
    build_partition_tables_into(residuals, zigzag, &mut prefix, &mut raw_bits);
    (prefix, raw_bits)
}

/// Scratch-buffered table builder. Writes the prefix-sum table into
/// `prefix` and the per-sample raw widths into `raw_bits`, clearing
/// each first so capacity carries across subframes. Same numeric
/// output as [`build_partition_tables`].
fn build_partition_tables_into(
    residuals: &[i32],
    zigzag: &[u32],
    prefix: &mut Vec<u64>,
    raw_bits: &mut Vec<u8>,
) {
    debug_assert_eq!(residuals.len(), zigzag.len());
    let n = residuals.len();
    prefix.clear();
    prefix.reserve(n + 1);
    prefix.push(0u64);
    let mut acc: u64 = 0;
    for &u in zigzag {
        acc += u as u64;
        prefix.push(acc);
    }
    raw_bits.clear();
    raw_bits.reserve(n);
    for &r in residuals {
        raw_bits.push(raw_bits_for_sample(r));
    }
}

/// Pre-computed-stats variant of [`best_partition_z`]. The chosen
/// `(rice_cost, k)` plus the escape-cost comparison are derived from
/// subframe-scoped prefix-sum + per-sample raw-bits tables; both shared
/// across every `(method, partition_order)` pair in the partition-order
/// search. The walk-based `sum(u >> k)` inside [`rice_cost_for_k`] still
/// runs per candidate `k` — that's the inherent cost of scoring a Rice
/// parameter — but the `sum_u` pass (the closed-form `k_est`'s input) is
/// folded into a single O(1) prefix-sum subtraction, and the escape-path
/// `raw_bits_needed` scan is replaced by a `max(u8 slice)` over the
/// precomputed widths.
fn best_partition_zp(
    zigzag: &[u32],
    prefix: &[u64],
    raw_bits: &[u8],
    start: usize,
    end: usize,
    param_bits: u32,
    k_max: u32,
) -> PartChoice {
    debug_assert!(end <= zigzag.len());
    debug_assert_eq!(prefix.len(), zigzag.len() + 1);
    debug_assert_eq!(raw_bits.len(), zigzag.len());

    let z_slice = &zigzag[start..end];
    let sum_u = prefix[end] - prefix[start];
    let (rice_bits, k) = best_rice_params_zp(z_slice, sum_u, k_max);
    let rice_cost = param_bits as u64 + rice_bits;

    // Max width over the precomputed per-sample u8 table. The empty
    // case is impossible here — partition_layout_cost_zp short-circuits
    // out before reaching this helper when partition_size would be
    // zero — but we still guard against it to keep the function
    // self-consistent.
    let needed_bits = {
        let mut m: u8 = 1;
        for &b in &raw_bits[start..end] {
            if b > m {
                m = b;
            }
        }
        m as u32
    };
    if needed_bits <= 31 {
        let n = (end - start) as u64;
        let escape_cost = param_bits as u64 + 5 + n * needed_bits as u64;
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
///
/// Retained as the reference path. The production encoder uses
/// [`partition_layout_cost_z`] which shares one zigzag buffer across
/// the entire search.
#[cfg(test)]
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

/// Same as [`partition_layout_cost`] but operates on a pre-zigzagged
/// view of the same residual span. The two slices are parallel (same
/// length and ordering) so the per-partition indices match.
///
/// Retained as the round-176 reference path. The production encoder
/// uses [`partition_layout_cost_zp`] which additionally folds the
/// per-partition `sum_u` and `raw_bits` scans into subframe-scoped
/// prefix/per-element tables.
#[cfg(test)]
fn partition_layout_cost_z(
    residuals: &[i32],
    zigzag: &[u32],
    block_size: usize,
    predictor_order: usize,
    partition_order: u32,
    param_bits: u32,
    k_max: u32,
) -> Option<u64> {
    debug_assert_eq!(residuals.len(), zigzag.len());
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
        total += best_partition_z(
            &residuals[idx..idx + count],
            &zigzag[idx..idx + count],
            param_bits,
            k_max,
        )
        .cost;
        idx += count;
    }
    Some(total)
}

/// Production partition-order cost helper. Same per-partition cost
/// the `_z` reference path would compute — every layout decision
/// (which Rice `k` per partition, which partitions take the escape
/// path) is byte-identical — but the per-partition `sum_u` and
/// `raw_bits` scans read from subframe-scoped tables instead of
/// re-deriving from the slice on every visit. Each
/// `(method, partition_order)` pair pays only the `rice_cost_for_k`
/// walks the search inherently needs.
///
/// Eight arguments because each table comes from a different stage of
/// the per-subframe pipeline (zigzag conversion, prefix sum, per-sample
/// raw widths, then the search loop's geometric and method parameters);
/// folding them into a struct would push the allocation onto every call
/// site without buying any clarity over the call shape `_z` already
/// uses with seven arguments.
#[allow(clippy::too_many_arguments)]
fn partition_layout_cost_zp(
    zigzag: &[u32],
    prefix: &[u64],
    raw_bits: &[u8],
    block_size: usize,
    predictor_order: usize,
    partition_order: u32,
    param_bits: u32,
    k_max: u32,
) -> Option<u64> {
    debug_assert_eq!(prefix.len(), zigzag.len() + 1);
    debug_assert_eq!(raw_bits.len(), zigzag.len());
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
        total += best_partition_zp(
            zigzag,
            prefix,
            raw_bits,
            idx,
            idx + count,
            param_bits,
            k_max,
        )
        .cost;
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
#[cfg(test)]
fn encode_rice_residual(
    w: &mut BitWriter,
    residuals: &[i32],
    block_size: usize,
    predictor_order: usize,
) -> u32 {
    let mut scratch = SubframeScratch::default();
    encode_rice_residual_s(w, residuals, block_size, predictor_order, &mut scratch)
}

/// Scratch-buffered residual emitter. The zigzag span, prefix-sum table,
/// and per-sample raw-width table are written into `scratch`'s reusable
/// buffers instead of being allocated fresh per call — eliminating the
/// per-subframe `Vec<u32>` / `Vec<u64>` / `Vec<u8>` triple the
/// pre-round-191 code paid on every Fixed / LPC candidate.
fn encode_rice_residual_s(
    w: &mut BitWriter,
    residuals: &[i32],
    block_size: usize,
    predictor_order: usize,
    scratch: &mut SubframeScratch,
) -> u32 {
    // Zigzag the entire residual span **once**. Both the partition-order
    // search and the emit loop reuse this single buffer instead of
    // re-zigzagging each partition for every `(method, partition_order)`
    // pair the search visits (2 methods × up to 9 orders × N partitions).
    // The buffer stays in cache for the rest of the call: the typical
    // 4096-sample residual is 16 KiB of `u32`, comfortably inside L1.
    //
    // Pre-round-176 the standalone `best_rice_params` path allocated a
    // fresh `Vec<u32>` per partition per partition-order; profile-147
    // showed `best_partition` at ~61 % of encode self-samples. Folding
    // the zigzag conversion up to subframe scope removed that redundant
    // work entirely. Round 186 took the same shape one step further: a
    // subframe-scoped prefix-sum table (so `sum_u` for the closed-form
    // `k_est` is O(1) per partition) and a per-sample raw-bit-width
    // table (so the escape-cost path's max-of-leading-zeros is a `u8`
    // max instead of `leading_zeros` per element) both shared across
    // every `(method, partition_order)` pair the search visits. Round
    // 191 carries that shape one layer further still: the zigzag /
    // prefix / raw-bits triple itself now lives in subframe-scope
    // scratch reused across every Fixed / LPC candidate.
    scratch.zigzag.clear();
    scratch.zigzag.reserve(residuals.len());
    for &r in residuals {
        scratch.zigzag.push(zigzag_encode(r));
    }
    // Detach the parallel buffers so `build_partition_tables_into` can
    // borrow `prefix` and `raw_bits` mutably without aliasing `zigzag`.
    let zigzag = std::mem::take(&mut scratch.zigzag);
    let mut prefix = std::mem::take(&mut scratch.prefix);
    let mut raw_bits = std::mem::take(&mut scratch.raw_bits);
    build_partition_tables_into(residuals, &zigzag, &mut prefix, &mut raw_bits);

    // Find the (method, partition_order) pair with the smallest payload.
    let mut best: Option<(u32, u32, u32, u32, u64)> = None; // method, param_bits, k_max, p, cost
    for (method, param_bits, k_max) in [(0u32, 4u32, 14u32), (1u32, 5u32, 30u32)] {
        for p in 0..=MAX_PARTITION_ORDER {
            let Some(cost) = partition_layout_cost_zp(
                &zigzag,
                &prefix,
                &raw_bits,
                block_size,
                predictor_order,
                p,
                param_bits,
                k_max,
            ) else {
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
        let z_slice = &zigzag[idx..idx + count];
        let choice = best_partition_zp(
            &zigzag,
            &prefix,
            &raw_bits,
            idx,
            idx + count,
            param_bits,
            k_max,
        );
        idx += count;
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
                // Reuse the cached zigzag rather than re-zigzagging
                // each residual in the unary/remainder split.
                for &u in z_slice {
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
    // Restore the parallel buffers so the next candidate sees the same
    // (now-grown) capacity.
    scratch.zigzag = zigzag;
    scratch.prefix = prefix;
    scratch.raw_bits = raw_bits;
    total_bits
}

fn zigzag_encode(s: i32) -> u32 {
    ((s << 1) ^ (s >> 31)) as u32
}

/// Maximum bit width needed to store every residual in `residuals` raw
/// as a two's-complement signed integer. Retained as the round-176
/// reference path; production reads from the per-sample [`raw_bits`]
/// table that [`build_partition_tables`] seeds via
/// [`raw_bits_for_sample`].
#[cfg(test)]
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

/// Score one Rice parameter `k` against `residuals` (already converted
/// to their unsigned zigzag form by the caller). Returns the total bit
/// count for the entire partition under that `k`. Aborts the inner sum
/// as soon as it exceeds `best_bits`, matching the original brute-force
/// implementation's early-exit shape so the closed-form estimate path
/// never picks a `k` worse than the brute-force path would have.
#[inline(always)]
fn rice_cost_for_k(zigzag: &[u32], k: u32, best_bits: u64) -> u64 {
    let n = zigzag.len() as u64;
    let mut total: u64 = n * (1 + k as u64);
    if total >= best_bits {
        return total;
    }
    for &u in zigzag {
        total += (u >> k) as u64;
        if total >= best_bits {
            return total;
        }
    }
    total
}

/// Pick the Rice parameter `k` in `0..=k_max` that minimises the total
/// bit cost of coding `residuals` as `unary(zigzag(r) >> k) || raw(k)`.
/// RFC 9639 §9.2.5.
///
/// For residuals approximately Laplace-distributed (the working
/// assumption for predictor-residual coding), the bit-optimal Rice
/// parameter is `k ≈ floor(log2(E[|r|]))`, derivable from the partition
/// payload `N * (1 + k) + (sum_u >> k)`: differentiating with respect to
/// `k` and setting to zero gives `2^k ≈ sum_u / N` (within the floor /
/// ceil rounding ambiguity). So instead of scoring every `k` in
/// `0..=k_max` (up to 31 passes per partition), this implementation
/// computes that estimate once from the unsigned sum, scores a small
/// `[k_est-2 .. k_est+3]` window around it, and only extends past either
/// end when the window's chosen edge could plausibly still improve.
///
/// Safety net: if the estimate-window's choice is at the lower boundary
/// (k_est-2, where the brute-force scan might have found a strictly
/// smaller k=0 cost on a degenerate, heavily-clipped residual block),
/// we extend downward to 0 — and likewise upward toward `k_max` when
/// the window's pick lands at the top. The aggregate score function is
/// strictly unimodal in `k` for any non-empty residual block (cost is
/// linear in `k` plus a sum-of-shifts term that is monotonically
/// non-increasing), so the local minimum within the window plus
/// directional extension is also the global minimum — the same `k` the
/// brute-force loop would have returned, just at a fraction of the work.
#[cfg(test)]
fn best_rice_params(residuals: &[i32], k_max: u32) -> (u64, u32) {
    if residuals.is_empty() {
        return (0, 0);
    }
    // Zigzag once instead of `k_max + 1` times inside the inner loop.
    // For the typical partition (16..=1024 i32 residuals) the temporary
    // Vec stays well inside cache, and removing the redundant work
    // dominates the small allocation overhead.
    let zigzag: Vec<u32> = residuals.iter().map(|&r| zigzag_encode(r)).collect();
    best_rice_params_z(&zigzag, k_max)
}

/// Pre-zigzagged variant of [`best_rice_params`]. Skips the per-call
/// `Vec<u32>` allocation by accepting a caller-owned zigzag slice; the
/// caller (typically [`encode_rice_residual`]) computes the zigzag once
/// per subframe and re-slices it across every partition the search
/// visits. Output is bit-identical to [`best_rice_params`] for the
/// equivalent input.
///
/// Retained as the round-176 reference path. The production encoder
/// uses [`best_rice_params_zp`] which additionally accepts a
/// pre-computed `sum_u` derived from a subframe-scoped prefix-sum so
/// the `sum_u` pass over the slice doesn't repeat per
/// `(method, partition_order)` pair.
#[cfg(test)]
fn best_rice_params_z(zigzag: &[u32], k_max: u32) -> (u64, u32) {
    if zigzag.is_empty() {
        return (0, 0);
    }
    let n = zigzag.len() as u64;
    let sum_u: u64 = zigzag.iter().map(|&u| u as u64).sum();

    // Closed-form estimate: `k_est = floor(log2(sum_u / n))`. The `sum_u
    // == 0` case is the all-zero (post-predictor) partition where the
    // optimal `k` is 0 — short-circuit so we don't run msb(0).
    let k_est: u32 = if sum_u == 0 {
        0
    } else {
        let mean_u = sum_u / n;
        if mean_u == 0 {
            0
        } else {
            // msb(mean_u) = 63 - leading_zeros; convert to u32.
            63u32.saturating_sub(mean_u.leading_zeros())
        }
    };

    // Centre the search window on the estimate, clamped to the legal
    // `[0, k_max]` range. ±2 around the estimate is enough to absorb
    // the floor / ceil rounding ambiguity in the closed-form derivation
    // for the realistic Laplace assumption; the directional extension
    // below recovers the remaining edge cases. When `k_est` falls
    // outside `[0, k_max]` entirely (huge mean for the method-0
    // `k_max = 14` partition, say) we still want the loop body to run
    // at least once so `best_bits` becomes finite before the
    // extension fires.
    let lo = k_est.saturating_sub(2).min(k_max);
    let hi = (k_est + 3).min(k_max);

    let mut best_k = lo;
    let mut best_bits = u64::MAX;
    for k in lo..=hi {
        let cost = rice_cost_for_k(zigzag, k, best_bits);
        if cost < best_bits {
            best_bits = cost;
            best_k = k;
        }
    }

    // The cost as a function of `k` is unimodal (linear `k` term plus a
    // monotone-decreasing sum-of-shifts term). If the chosen `k` sits at
    // the window edge, the true optimum could be further in that
    // direction — walk outward while we keep improving. We never step
    // past `k_max`, which is the spec-imposed parameter ceiling.
    if best_k == lo && lo > 0 {
        for k in (0..lo).rev() {
            let cost = rice_cost_for_k(zigzag, k, best_bits);
            if cost < best_bits {
                best_bits = cost;
                best_k = k;
            } else {
                break;
            }
        }
    }
    if best_k == hi && hi < k_max {
        for k in (hi + 1)..=k_max {
            let cost = rice_cost_for_k(zigzag, k, best_bits);
            if cost < best_bits {
                best_bits = cost;
                best_k = k;
            } else {
                break;
            }
        }
    }

    (best_bits, best_k)
}

/// Production variant of [`best_rice_params_z`]. Accepts a pre-computed
/// `sum_u` (derived from the subframe-scoped prefix-sum table) instead
/// of running an O(n) sum pass per partition per
/// `(method, partition_order)` pair. The `rice_cost_for_k` walks still
/// dominate the per-partition cost — they have to: scoring a Rice
/// parameter inherently needs the shift-and-sum over the slice — but the
/// `sum_u` reduction the closed-form `k_est` derivation needs is now
/// O(1) per call. Output is bit-identical to [`best_rice_params_z`] for
/// the equivalent input.
fn best_rice_params_zp(zigzag: &[u32], sum_u: u64, k_max: u32) -> (u64, u32) {
    if zigzag.is_empty() {
        return (0, 0);
    }
    let n = zigzag.len() as u64;

    // Closed-form estimate: `k_est = floor(log2(sum_u / n))`. The `sum_u
    // == 0` case is the all-zero (post-predictor) partition where the
    // optimal `k` is 0 — short-circuit so we don't run msb(0).
    let k_est: u32 = if sum_u == 0 {
        0
    } else {
        let mean_u = sum_u / n;
        if mean_u == 0 {
            0
        } else {
            // msb(mean_u) = 63 - leading_zeros; convert to u32.
            63u32.saturating_sub(mean_u.leading_zeros())
        }
    };

    let lo = k_est.saturating_sub(2).min(k_max);
    let hi = (k_est + 3).min(k_max);

    let mut best_k = lo;
    let mut best_bits = u64::MAX;
    for k in lo..=hi {
        let cost = rice_cost_for_k(zigzag, k, best_bits);
        if cost < best_bits {
            best_bits = cost;
            best_k = k;
        }
    }

    if best_k == lo && lo > 0 {
        for k in (0..lo).rev() {
            let cost = rice_cost_for_k(zigzag, k, best_bits);
            if cost < best_bits {
                best_bits = cost;
                best_k = k;
            } else {
                break;
            }
        }
    }
    if best_k == hi && hi < k_max {
        for k in (hi + 1)..=k_max {
            let cost = rice_cost_for_k(zigzag, k, best_bits);
            if cost < best_bits {
                best_bits = cost;
                best_k = k;
            } else {
                break;
            }
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
#[cfg(test)]
fn levinson_durbin(samples: &[i32], order: usize, window: ApodizationWindow) -> Option<Vec<f64>> {
    let mut scratch = SubframeScratch::default();
    if !levinson_durbin_s(samples, order, window, &mut scratch) {
        return None;
    }
    Some(scratch.lpc_coeffs)
}

/// Scratch-buffered Levinson-Durbin. Writes the FLAC-convention LPC
/// coefficients into `scratch.lpc_coeffs` and returns `true`; returns
/// `false` (with `lpc_coeffs` cleared) for degenerate input. The
/// `windowed`, `autoc`, and `lpc_state` work buffers are reused
/// across windows and across frames; only the contents matter to the
/// next call.
fn levinson_durbin_s(
    samples: &[i32],
    order: usize,
    window: ApodizationWindow,
    scratch: &mut SubframeScratch,
) -> bool {
    scratch.lpc_coeffs.clear();
    let n = samples.len();
    if n <= order || order == 0 {
        return false;
    }

    // Taper the block with the requested analysis window before computing
    // the autocorrelation. The window only re-weights the signal — it has
    // no decoder-side footprint (the decoder reads quantised coefficients,
    // never the window) — so trying several and keeping the smallest
    // result is a pure rate optimisation. RFC 9639 leaves the
    // autocorrelation-to-coefficient step to the encoder.
    scratch.windowed.clear();
    scratch.windowed.reserve(n);
    for (i, &s) in samples.iter().enumerate() {
        scratch.windowed.push(s as f64 * window.weight(i, n));
    }

    scratch.autoc.clear();
    scratch.autoc.resize(order + 1, 0.0f64);
    for lag in 0..=order {
        let mut s = 0.0f64;
        for i in lag..n {
            s += scratch.windowed[i] * scratch.windowed[i - lag];
        }
        scratch.autoc[lag] = s;
    }
    if scratch.autoc[0] <= 0.0 || !scratch.autoc[0].is_finite() {
        return false;
    }

    scratch.lpc_state.clear();
    scratch.lpc_state.resize(order, 0.0f64);
    let mut error = scratch.autoc[0];
    for i in 0..order {
        let mut r = -scratch.autoc[i + 1];
        for j in 0..i {
            r -= scratch.lpc_state[j] * scratch.autoc[i - j];
        }
        if error.abs() < 1e-12 {
            return false;
        }
        let k = r / error;
        // Symmetric update in place. The recurrence
        //   new_lpc[j]     = lpc[j]     + k * lpc[i-1-j]
        //   new_lpc[i-1-j] = lpc[i-1-j] + k * lpc[j]
        // depends only on the OLD `lpc[j]` and `lpc[i-1-j]`, so by
        // processing the pair `(j, i-1-j)` together we can write both
        // updates without disturbing any value the rest of the sweep
        // still needs. When `i` is odd the middle index satisfies
        // `j == i-1-j`, where the pair collapses to
        // `new_lpc[j] = lpc[j] + k * lpc[j] = lpc[j] * (1 + k)`.
        // Floating-point operands and order are bit-identical to the
        // previous `new_lpc[j] = lpc[j] + k * lpc[i-1-j]` formulation,
        // so the quantised coefficient stream stays byte-stable.
        //
        // Pre-round-182 this loop allocated + memcpy'd a fresh
        // `new_lpc` of length `order` on every outer-loop iteration
        // (order extra Vec allocations per Levinson-Durbin call); with
        // 3 apodisation windows × 12 LPC orders per subframe, those
        // allocations were a measurable fraction of encode time on the
        // r147 flamegraph's allocator paths (`_xzm_free`, `xzm_malloc`,
        // `__bzero`). The in-place sweep eliminates them. Round 191
        // additionally pulls the `lpc_state` / `windowed` / `autoc` work
        // buffers up into [`SubframeScratch`] so they live across the
        // 3-window × 12-order outer search instead of being re-allocated
        // per `(window, order)` pair.
        let mid = i / 2;
        for j in 0..mid {
            let opp = i - 1 - j;
            let a = scratch.lpc_state[j] + k * scratch.lpc_state[opp];
            let b = scratch.lpc_state[opp] + k * scratch.lpc_state[j];
            scratch.lpc_state[j] = a;
            scratch.lpc_state[opp] = b;
        }
        if i % 2 == 1 {
            // Self-paired middle element of an odd-length sweep.
            scratch.lpc_state[mid] = scratch.lpc_state[mid] + k * scratch.lpc_state[mid];
        }
        scratch.lpc_state[i] = k;
        error *= 1.0 - k * k;
        if !error.is_finite() || error <= 0.0 {
            return false;
        }
    }

    // FLAC's prediction is `s[n] ≈ sum_j coeff[j] * s[n-1-j]`, i.e.
    // `coeff[0]` applies to the most-recent sample. Standard
    // Levinson-Durbin (predicting `s[n] = -sum a[j] * s[n-j]`) yields
    // `a[j]`; FLAC's coefficient is `-a[j+1]` with index-0 being the
    // nearest neighbour.
    scratch.lpc_coeffs.reserve(order);
    for &v in scratch.lpc_state.iter() {
        scratch.lpc_coeffs.push(-v);
    }
    true
}

/// Quantise floating-point LPC coefficients into `precision`-bit signed
/// integers plus a non-negative shift. Returns `None` if the magnitudes
/// are too small to represent meaningfully (would overflow the shift
/// range). The shift is clamped to 0..=15: RFC 9639 §9.2.6 writes the
/// shift as a 5-bit two's-complement quantity that MUST NOT be negative
/// (Appendix B.4), so the legal positive range is exactly 0..=15.
#[cfg(test)]
fn quantize_lpc(coeffs: &[f64], precision: u32) -> Option<(Vec<i32>, u32)> {
    let mut out = Vec::new();
    let shift = quantize_lpc_into(coeffs, precision, &mut out)?;
    Some((out, shift))
}

/// Scratch-buffered variant of [`quantize_lpc`] that writes the
/// quantised coefficients into the caller-provided `out` (cleared,
/// then pushed in order). Returns the chosen `qlp_shift`; `None`
/// signals a degenerate coefficient vector with `out` left in a
/// caller-defined state (typically empty).
fn quantize_lpc_into(coeffs: &[f64], precision: u32, out: &mut Vec<i32>) -> Option<u32> {
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

    out.clear();
    out.reserve(coeffs.len());
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
    Some(shift)
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
        let mut scratch = SubframeScratch::default();
        while i < total {
            let take = ((total - i) as u32).min(block_size) as usize;
            let per_ch: Vec<Vec<i32>> = (0..n_ch)
                .map(|c| channels_pcm[c][i..i + take].to_vec())
                .collect();
            let data = encode_frame(
                frame_num,
                take as u32,
                sample_rate,
                bps,
                &per_ch,
                &mut scratch,
            )
            .unwrap();
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
            true,
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
            true,
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
        let mut scratch = SubframeScratch::default();
        let (code, plans) =
            choose_channel_assignment(&[l.clone(), r.clone()], 16, &mut scratch).unwrap();
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
        let mut scratch = SubframeScratch::default();
        let (code, plans) = choose_channel_assignment(&[l, r], 16, &mut scratch).unwrap();
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

    /// `best_rice_params` is a hot-path estimate-window optimisation
    /// over an originally-brute-force `for k in 0..=k_max` scan. Its
    /// safety net (extend outward when the chosen `k` sits at a window
    /// edge) plus the convexity of `cost(k)` guarantee it picks the
    /// same `(best_bits, best_k)` pair the brute-force scan would
    /// return for any input. This test exercises both the estimate
    /// window's centre and its directional extensions against a
    /// hand-rolled brute-force reference across a deliberately diverse
    /// set of residual buffers — degenerate (all zero, all huge,
    /// single-spike), Laplace-shaped (the design point), and
    /// adversarially bi-modal (small-then-large, large-then-small) so
    /// the estimate is misaligned and the safety net must fire.
    #[test]
    fn best_rice_params_matches_brute_force_reference() {
        // Brute-force scan kept inline so the reference stays in this
        // test file rather than in `src/`, and so future tweaks to the
        // optimised path can't accidentally also tweak the reference.
        fn brute_force(residuals: &[i32], k_max: u32) -> (u64, u32) {
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
                }
                if total < best_bits {
                    best_bits = total;
                    best_k = k;
                }
            }
            (best_bits, best_k)
        }

        // xorshift32 inline so the test stays self-contained.
        fn xorshift32(state: &mut u32) -> u32 {
            *state ^= *state << 13;
            *state ^= *state >> 17;
            *state ^= *state << 5;
            *state
        }

        // Generate Laplace-shaped residuals at a target mean magnitude
        // by sign-flipping geometrically-thinned uniform variates. Not
        // a strict Laplace, but close enough to exercise every k in
        // the realistic range.
        let laplace = |mean_abs: u32, n: usize, seed: u32| -> Vec<i32> {
            let mut state = seed;
            let mut out = Vec::with_capacity(n);
            for _ in 0..n {
                let u = xorshift32(&mut state);
                let mag = (u % (mean_abs * 4 + 1)) as i32;
                let sign = if (u >> 31) & 1 == 1 { -1 } else { 1 };
                out.push(sign * mag);
            }
            out
        };

        let mut cases: Vec<(String, Vec<i32>)> = Vec::new();
        // Degenerate inputs.
        cases.push(("all_zero_16".into(), vec![0; 16]));
        cases.push(("all_zero_512".into(), vec![0; 512]));
        cases.push(("single_spike".into(), {
            let mut v = vec![0; 64];
            v[7] = 1_000_000;
            v
        }));
        cases.push(("all_ones".into(), vec![1; 128]));
        cases.push(("all_neg_ones".into(), vec![-1; 128]));
        // Laplace at a range of means so the estimate lands at low,
        // medium, and high `k`.
        for &mean in &[1u32, 4, 16, 64, 256, 1024, 8192, 100_000] {
            for &n in &[16usize, 64, 256, 1024] {
                let r = laplace(mean, n, 0xDEAD_C0DE ^ (mean << 1) ^ (n as u32));
                cases.push((format!("laplace_mean{mean}_n{n}"), r));
            }
        }
        // Adversarial bi-modal: mean is far from per-sample magnitude,
        // forcing the safety net to fire.
        cases.push(("bimodal_small_then_large".into(), {
            let mut v: Vec<i32> = (0..256).map(|i| if i < 240 { 0 } else { 50_000 }).collect();
            v[0] = 1;
            v
        }));
        cases.push(("bimodal_large_then_small".into(), {
            let mut v: Vec<i32> = (0..256).map(|i| if i < 16 { 50_000 } else { 0 }).collect();
            v[255] = 1;
            v
        }));

        // Method-0 (`k_max = 14`) and method-1 (`k_max = 30`) cover
        // every partitioned-Rice configuration the encoder emits.
        for &k_max in &[14u32, 30] {
            for (label, residuals) in &cases {
                let (got_bits, got_k) = best_rice_params(residuals, k_max);
                let (ref_bits, ref_k) = brute_force(residuals, k_max);
                assert_eq!(
                    got_bits, ref_bits,
                    "{label}/k_max={k_max}: optimised bits {got_bits} \
                     != brute-force {ref_bits} (got_k={got_k}, ref_k={ref_k})"
                );
                assert_eq!(
                    got_k, ref_k,
                    "{label}/k_max={k_max}: optimised k {got_k} \
                     != brute-force k {ref_k} (both should yield {ref_bits} bits)"
                );
            }
        }
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

    /// The per-subframe precision search must never produce a larger LPC
    /// plan than the previous fixed-heuristic-precision behaviour: the
    /// heuristic precision is the search window's ceiling and is always
    /// one of the candidates, so the search can only tie it or beat it.
    /// We assert this for every LPC order across a handful of signals.
    #[test]
    fn precision_search_never_regresses_against_fixed_heuristic() {
        let sr = 48_000u32;
        let n = 4096usize;
        let mk = |a: f64, fa: f64, b: f64, fb: f64| -> Vec<i32> {
            (0..n)
                .map(|i| {
                    let t = i as f64 / sr as f64;
                    let v = (t * fa * 2.0 * std::f64::consts::PI).sin() * a
                        + (t * fb * 2.0 * std::f64::consts::PI).sin() * b;
                    v as i32
                })
                .collect()
        };
        let signals = [
            mk(20_000.0, 440.0, 0.0, 0.0),
            mk(18_000.0, 523.25, 4_000.0, 1567.98),
            mk(9_000.0, 110.0, 9_000.0, 6_000.0),
        ];
        let ceiling = lpc_precision_for(n);
        for samples in &signals {
            for order in 1..=MAX_LPC_ORDER.min(n - 1) {
                // Old behaviour: a single plan at the heuristic precision
                // under the historical Welch window.
                let coeffs = match levinson_durbin(samples, order, ApodizationWindow::Welch) {
                    Some(c) => c,
                    None => continue,
                };
                let fixed = encode_lpc_plan_at(samples, 16, order, 0, &coeffs, ceiling);
                // New behaviour: the precision + window search. Because
                // Welch is one of the searched windows and the heuristic
                // ceiling is one of the searched precisions, the new plan
                // can only ever tie or beat the Welch-at-ceiling plan.
                let searched = encode_lpc_plan(samples, 16, order, 0);
                match (fixed, searched) {
                    (Some(f), Some(s)) => assert!(
                        s.bits <= f.bits,
                        "order {order}: search {} bits > fixed-heuristic {} bits",
                        s.bits,
                        f.bits
                    ),
                    (None, _) => {} // heuristic plan itself failed; nothing to compare
                    (Some(_), None) => {
                        panic!("search returned None where the heuristic plan exists")
                    }
                }
            }
        }
    }

    /// A signal whose LPC residual is dominated by quantisation noise
    /// (low-amplitude content where the coefficient field is a large
    /// fraction of the subframe) lets a *narrower* precision win: the
    /// few residual bits saved by the extra precision don't cover the
    /// `order` extra coefficient bits per step. The search must find that
    /// narrower precision and beat the heuristic-ceiling plan strictly.
    #[test]
    fn precision_search_beats_ceiling_on_coefficient_dominated_subframe() {
        let sr = 48_000u32;
        let n = 4096usize;
        // Low-amplitude noisy-ish content: a quiet tone plus a small
        // deterministic perturbation so the residual floor is set by
        // quantisation rather than by a clean predictor fit. With the
        // residual nearly incompressible, fatter coefficients are pure
        // overhead.
        let mut samples: Vec<i32> = Vec::with_capacity(n);
        let mut state: u32 = 0x1234_5678;
        for i in 0..n {
            let t = i as f64 / sr as f64;
            let tone = (t * 300.0 * 2.0 * std::f64::consts::PI).sin() * 40.0;
            // xorshift LCG perturbation, ±3 LSB.
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let noise = (state % 7) as i32 - 3;
            samples.push(tone as i32 + noise);
        }
        let order = 12usize;
        // Pin this analysis to the historical Welch window so the
        // precision-search assertion is independent of the window search;
        // the driver may additionally pick a different window, which can
        // only shrink the result further (asserted below).
        let coeffs = levinson_durbin(&samples, order, ApodizationWindow::Welch).expect("coeffs");
        let ceiling = lpc_precision_for(n);
        let floor = ceiling
            .saturating_sub(LPC_PRECISION_SEARCH_SPAN)
            .max(MIN_LPC_QLP_PRECISION);

        let ceiling_plan = encode_lpc_plan_at(&samples, 16, order, 0, &coeffs, ceiling)
            .expect("ceiling plan")
            .bits;
        // The minimum across the precision window for the Welch coeffs.
        let mut window_min = u64::MAX;
        let mut window_argmin = ceiling;
        for p in floor..=ceiling {
            if let Some(plan) = encode_lpc_plan_at(&samples, 16, order, 0, &coeffs, p) {
                if plan.bits < window_min {
                    window_min = plan.bits;
                    window_argmin = p;
                }
            }
        }
        assert!(
            window_argmin < ceiling,
            "expected a sub-ceiling precision to win; argmin was {window_argmin} (ceiling {ceiling})"
        );
        assert!(
            window_min < ceiling_plan,
            "search min {window_min} bits should beat the ceiling plan {ceiling_plan} bits"
        );
        // The driver searches windows too, so its result is at most the
        // Welch-window precision minimum (a different window may beat it).
        let driven = encode_lpc_plan(&samples, 16, order, 0)
            .expect("driven plan")
            .bits;
        assert!(
            driven <= window_min,
            "encode_lpc_plan {driven} bits should be <= the Welch-window minimum {window_min}"
        );
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
        let coeffs = levinson_durbin(&samples, order, ApodizationWindow::Welch).unwrap();

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

    /// An LPC subframe encoded for a 4096-sample block must declare a
    /// precision inside the legal search window on the wire, read its
    /// coefficients at that width, and still decode bit-exactly. The
    /// precision is no longer fixed to the heuristic ceiling — the
    /// per-subframe search may settle lower — so we assert it lands in
    /// the search band and never the forbidden `0b1111`. This guards the
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
        // Confirm the chosen subframe is actually LPC and declares a
        // precision the search is allowed to pick.
        let plan = best_subframe(&samples, 16).unwrap();
        // type byte: bit0=pad, bits1..7 = type code. LPC is 0b1xxxxx.
        let type_code = (plan.payload[0] >> 1) & 0x3F;
        assert!(
            type_code & 0b100000 != 0,
            "expected an LPC subframe for a predictable tone, got type {type_code:#08b}"
        );
        // The on-wire precision must sit in the legal search band:
        // [ceiling - span, ceiling], clamped to MIN, never 0b1111.
        let ceiling = lpc_precision_for(n);
        let floor = ceiling
            .saturating_sub(LPC_PRECISION_SEARCH_SPAN)
            .max(MIN_LPC_QLP_PRECISION);
        let declared = declared_lpc_precision(&plan, 16, 0);
        assert!(
            (floor..=ceiling).contains(&declared),
            "declared precision {declared} outside search band [{floor}, {ceiling}]"
        );
        assert!(declared < 16, "declared the forbidden precision 0b1111");
        roundtrip(vec![samples], sr, 16);
    }

    /// Decode the 4-bit `qlp_precis` field (stored as precision-1) from a
    /// finished LPC `SubframePlan`. Walks the same prelude the encoder
    /// wrote: 1 pad bit, 6 type bits, the wasted-bits unary group, then
    /// `bps * order` warm-up bits, landing on the 4-bit precision field.
    fn declared_lpc_precision(plan: &SubframePlan, bps: u32, wasted: u32) -> u32 {
        let bytes = &plan.payload;
        let read_bit = |pos: usize| -> u32 { (bytes[pos / 8] as u32 >> (7 - (pos % 8))) & 1 };
        let read_bits = |start: usize, count: u32| -> u32 {
            let mut v = 0u32;
            for i in 0..count {
                v = (v << 1) | read_bit(start + i as usize);
            }
            v
        };
        // 1 pad + 6 type bits.
        let type_code = read_bits(1, 6);
        // LPC: type is 0b1xxxxx, order-1 in the low 5 bits.
        let order = (type_code & 0x1F) + 1;
        // Wasted-bits group: 1 flag, plus `wasted` unary bits if nonzero.
        let mut pos = 7;
        let flag = read_bit(pos);
        pos += 1;
        if flag == 1 {
            pos += wasted as usize; // unary group is exactly `wasted` bits.
        }
        // Warm-up samples, then the 4-bit precision field.
        pos += (bps * order) as usize;
        read_bits(pos, 4) + 1
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

        // Isolate the order-cap question from the apodization-window
        // search by pinning both order ranges to a single window (Welch).
        // This is the same baseline this test used before the encoder
        // grew a window search, so the AR(11) signal continues to
        // demonstrate that orders 9..=12 strictly beat orders 1..=8 on
        // content engineered to need a longer impulse response. The
        // window search runs in addition on top in the production path,
        // and can only shrink either side further (verified separately).
        let welch_min_over = |orders: std::ops::RangeInclusive<usize>| -> u64 {
            let mut best = u64::MAX;
            for order in orders {
                let Some(coeffs) = levinson_durbin(&samples, order, ApodizationWindow::Welch)
                else {
                    continue;
                };
                let ceiling = lpc_precision_for(n);
                let floor = ceiling
                    .saturating_sub(LPC_PRECISION_SEARCH_SPAN)
                    .max(MIN_LPC_QLP_PRECISION);
                for p in floor..=ceiling {
                    if let Some(plan) = encode_lpc_plan_at(&samples, 16, order, 0, &coeffs, p) {
                        best = best.min(plan.bits);
                    }
                }
            }
            best
        };
        let best_low = welch_min_over(1..=8);
        let best_high = welch_min_over(9..=MAX_LPC_ORDER);
        assert!(
            best_high < best_low,
            "expected order 9..=12 to beat order 1..=8 on an AR(11) signal \
             (Welch-pinned baseline), got best_high={best_high} best_low={best_low}"
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

    // ------------------------------------------------------------------
    // Apodization-window search.
    // ------------------------------------------------------------------

    /// Welch / Hann / Tukey weight curves are symmetric, peak at 1.0 in
    /// the centre (or near it), and are non-negative on `[0, N-1]`. A
    /// regression here would skew the autocorrelation and silently
    /// distort the LPC coefficients.
    #[test]
    fn apodization_windows_are_well_formed() {
        let n = 1024usize;
        for window in APODIZATION_WINDOWS.iter().copied() {
            for i in 0..n {
                let w = window.weight(i, n);
                assert!(
                    w.is_finite() && (-1e-9..=1.0 + 1e-9).contains(&w),
                    "{window:?} weight at {i}/{n} out of range: {w}"
                );
                // Symmetry: w[i] == w[N-1-i].
                let mirror = window.weight(n - 1 - i, n);
                assert!(
                    (w - mirror).abs() < 1e-12,
                    "{window:?} not symmetric at {i}: {w} vs {mirror}"
                );
            }
            // Centre weight: Hann's `1 - cos(pi)` lands exactly on 1.0
            // when N is even; Tukey's flat top is exactly 1.0; Welch
            // peaks at `1 - (0.5 / (half + 1))^2` for an even-length
            // block (the formula's denominator is `half + 1`, not
            // `half`, to avoid zeroing the endpoints), which can sit a
            // few ulp below 1.0 on a large block. The actual constraint
            // is just "very near 1.0".
            let centre = window.weight(n / 2, n);
            assert!(
                (centre - 1.0).abs() < 1e-3,
                "{window:?} centre weight {centre} should be ~1.0"
            );
        }
    }

    /// Adding the window search to `encode_lpc_plan` must never inflate
    /// the LPC subframe: Welch is in the search set and the historical
    /// behaviour was Welch-only, so the new path can only tie or beat
    /// the old per-order plan. Cross-checked across a variety of
    /// signals and every LPC order.
    #[test]
    fn window_search_never_regresses_against_welch_only() {
        let sr = 48_000u32;
        let n = 4096usize;
        let mk = |a: f64, fa: f64, b: f64, fb: f64| -> Vec<i32> {
            (0..n)
                .map(|i| {
                    let t = i as f64 / sr as f64;
                    let v = (t * fa * 2.0 * std::f64::consts::PI).sin() * a
                        + (t * fb * 2.0 * std::f64::consts::PI).sin() * b;
                    v as i32
                })
                .collect()
        };
        let signals = [
            mk(20_000.0, 440.0, 0.0, 0.0),
            mk(18_000.0, 523.25, 4_000.0, 1567.98),
            mk(9_000.0, 110.0, 9_000.0, 6_000.0),
            mk(15_000.0, 220.0, 8_000.0, 660.0),
        ];

        let welch_only_min = |samples: &[i32], order: usize| -> Option<u64> {
            let coeffs = levinson_durbin(samples, order, ApodizationWindow::Welch)?;
            let ceiling = lpc_precision_for(samples.len());
            let floor = ceiling
                .saturating_sub(LPC_PRECISION_SEARCH_SPAN)
                .max(MIN_LPC_QLP_PRECISION);
            let mut best = u64::MAX;
            for p in floor..=ceiling {
                if let Some(plan) = encode_lpc_plan_at(samples, 16, order, 0, &coeffs, p) {
                    best = best.min(plan.bits);
                }
            }
            (best != u64::MAX).then_some(best)
        };

        for samples in &signals {
            for order in 1..=MAX_LPC_ORDER.min(n - 1) {
                let Some(welch) = welch_only_min(samples, order) else {
                    continue;
                };
                let searched = encode_lpc_plan(samples, 16, order, 0)
                    .expect("driver plan exists where Welch plan does")
                    .bits;
                assert!(
                    searched <= welch,
                    "order {order}: window search {searched} bits > Welch-only {welch} bits"
                );
            }
        }
    }

    /// A signal whose spectral leakage hurts the Welch autocorrelation
    /// the most — closely-spaced partials near the block-rate
    /// fundamental — should benefit from the lower-leakage Hann window.
    /// We construct such a signal and assert the full window search
    /// strictly beats the Welch-only baseline for at least one LPC
    /// order, demonstrating the window search isn't a no-op.
    #[test]
    fn window_search_beats_welch_on_leakage_sensitive_signal() {
        let sr = 48_000u32;
        let n = 4096usize;
        // Two partials so close in frequency they bleed into each
        // other's autocorrelation bins under a centre-heavy Welch
        // window. With a flatter-passband Hann window the leakage
        // shrinks and the LPC fit tightens.
        let mut samples: Vec<i32> = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f64 / sr as f64;
            let v = (t * 4000.0 * 2.0 * std::f64::consts::PI).sin() * 12_000.0
                + (t * 4050.0 * 2.0 * std::f64::consts::PI).sin() * 12_000.0
                + (t * 11_017.0 * 2.0 * std::f64::consts::PI).sin() * 4_000.0;
            samples.push(v as i32);
        }

        let welch_only_min = |order: usize| -> Option<u64> {
            let coeffs = levinson_durbin(&samples, order, ApodizationWindow::Welch)?;
            let ceiling = lpc_precision_for(samples.len());
            let floor = ceiling
                .saturating_sub(LPC_PRECISION_SEARCH_SPAN)
                .max(MIN_LPC_QLP_PRECISION);
            let mut best = u64::MAX;
            for p in floor..=ceiling {
                if let Some(plan) = encode_lpc_plan_at(&samples, 16, order, 0, &coeffs, p) {
                    best = best.min(plan.bits);
                }
            }
            (best != u64::MAX).then_some(best)
        };

        // For each order, compare the window-search result to the
        // Welch-only minimum; at least one order must strictly win.
        let mut any_strict_win = false;
        let mut welch_total: u64 = 0;
        let mut search_total: u64 = 0;
        for order in 1..=MAX_LPC_ORDER {
            let Some(welch) = welch_only_min(order) else {
                continue;
            };
            let searched = encode_lpc_plan(&samples, 16, order, 0)
                .expect("driver plan exists")
                .bits;
            welch_total += welch;
            search_total += searched;
            if searched < welch {
                any_strict_win = true;
            }
        }
        assert!(
            any_strict_win,
            "window search did not beat Welch-only on any order \
             (welch_total={welch_total}, search_total={search_total}) — \
             apodization search isn't being exercised by this signal"
        );
        assert!(
            search_total < welch_total,
            "window search summed across orders ({search_total}) did not beat \
             Welch-only ({welch_total}) on leakage-sensitive content"
        );
    }

    /// `best_subframe` integrates the window search through the full
    /// per-channel-assignment pipeline. Encoding a stereo signal whose
    /// frame-level encode benefits from the window search must still
    /// round-trip bit-exactly through this crate's own decoder — the
    /// search is a pure rate optimisation, never a correctness change.
    #[test]
    fn apodization_search_roundtrip_full_frame() {
        let sr = 48_000u32;
        let n = 4096usize;
        // Same leakage-sensitive content, stereo with a slight L/R
        // amplitude differential so channel decorrelation is also in
        // play. The point is end-to-end bit-exactness.
        let mut l: Vec<i32> = Vec::with_capacity(n);
        let mut r: Vec<i32> = Vec::with_capacity(n);
        for i in 0..n {
            let t = i as f64 / sr as f64;
            let v = (t * 4000.0 * 2.0 * std::f64::consts::PI).sin() * 12_000.0
                + (t * 4050.0 * 2.0 * std::f64::consts::PI).sin() * 12_000.0;
            l.push(v as i32);
            r.push((v * 0.87) as i32);
        }
        roundtrip(vec![l, r], sr, 16);
    }

    // --- PADDING-aware encoder options (round 158) -----------------------
    //
    // `FlacEncoderOptions::padding_bytes = Some(n)` instructs the
    // encoder to mark its STREAMINFO non-last and append a `n`-byte
    // PADDING block as the last metadata in the chain. The default
    // `None` preserves the historical "STREAMINFO is the only and last
    // block" extradata. These tests pin both:
    //   * that the default factory still produces the exact 38-byte
    //     STREAMINFO-only chain (no silent regression for existing
    //     callers);
    //   * that the opt-in chain is well-formed (STREAMINFO non-last
    //     + last-flagged PADDING + correct payload length + all-zero
    //     payload) and walks back through the demuxer's block parser;
    //   * that the post-flush rebuild keeps the PADDING block and only
    //     rewrites the STREAMINFO payload (MD5 + frame-size + total
    //     sample count);
    //   * that an oversize padding request fails at construction time
    //     rather than panicking inside `flush` or producing a header
    //     no conformant reader will accept.

    fn build_audio_params(channels: u16, sample_rate: u32, fmt: SampleFormat) -> CodecParameters {
        let mut p = CodecParameters::audio(CodecId::new("flac"));
        p.channels = Some(channels);
        p.sample_rate = Some(sample_rate);
        p.sample_format = Some(fmt);
        p
    }

    #[test]
    fn make_encoder_default_emits_streaminfo_only_extradata() {
        let p = build_audio_params(2, 48_000, SampleFormat::S16);
        let enc = make_encoder(&p).unwrap();
        let extradata = &enc.output_params().extradata;
        // STREAMINFO is 4-byte header + 34-byte payload = 38 bytes.
        // No trailing PADDING block in the default case.
        assert_eq!(extradata.len(), 38);
        // Block-header byte 0: high bit (last) set, low 7 bits = 0
        // (STREAMINFO).
        assert_eq!(extradata[0], 0x80);
    }

    #[test]
    fn make_encoder_with_padding_appends_last_padding_block() {
        let p = build_audio_params(2, 48_000, SampleFormat::S16);
        let opts = FlacEncoderOptions {
            padding_bytes: Some(8192),
        };
        let enc = make_encoder_with_options(&p, opts).unwrap();
        let extradata = &enc.output_params().extradata;
        // STREAMINFO header (4) + payload (34) + PADDING header (4) +
        // PADDING payload (8192).
        assert_eq!(extradata.len(), 4 + 34 + 4 + 8192);
        // STREAMINFO is now non-last (high bit clear).
        assert_eq!(extradata[0], 0x00);
        // STREAMINFO header: parse it back through BlockHeader.
        let si_hdr = BlockHeader::parse(&extradata[..4]).unwrap();
        assert!(!si_hdr.last);
        assert_eq!(si_hdr.block_type, BlockType::StreamInfo);
        assert_eq!(si_hdr.length, 34);
        // Walk to PADDING header.
        let pad_off = 4 + 34;
        let pad_hdr = BlockHeader::parse(&extradata[pad_off..pad_off + 4]).unwrap();
        assert!(pad_hdr.last);
        assert_eq!(pad_hdr.block_type, BlockType::Padding);
        assert_eq!(pad_hdr.length, 8192);
        // PADDING payload must be all zeros per RFC 9639 §8.6.
        assert!(extradata[pad_off + 4..].iter().all(|&b| b == 0));
    }

    #[test]
    fn make_encoder_with_zero_padding_emits_empty_padding_block() {
        // A zero-byte PADDING block is spec-legal (length=0 is a
        // perfectly representable 24-bit value). The chain becomes
        // STREAMINFO(non-last) + PADDING(last, length=0) = 38 + 4 = 42 bytes.
        let p = build_audio_params(1, 44_100, SampleFormat::S16);
        let opts = FlacEncoderOptions {
            padding_bytes: Some(0),
        };
        let enc = make_encoder_with_options(&p, opts).unwrap();
        let extradata = &enc.output_params().extradata;
        assert_eq!(extradata.len(), 4 + 34 + 4);
        let si_hdr = BlockHeader::parse(&extradata[..4]).unwrap();
        assert!(!si_hdr.last);
        let pad_hdr = BlockHeader::parse(&extradata[38..42]).unwrap();
        assert!(pad_hdr.last);
        assert_eq!(pad_hdr.block_type, BlockType::Padding);
        assert_eq!(pad_hdr.length, 0);
    }

    #[test]
    fn make_encoder_rejects_oversize_padding_request() {
        let p = build_audio_params(2, 48_000, SampleFormat::S16);
        let opts = FlacEncoderOptions {
            // 2^24 = MAX_LENGTH + 1; just over the wire limit.
            padding_bytes: Some(BlockHeader::MAX_LENGTH as usize + 1),
        };
        let result = make_encoder_with_options(&p, opts);
        let err = match result {
            Err(e) => e,
            Ok(_) => {
                panic!("padding_bytes above the 24-bit length max must fail at construction time")
            }
        };
        let msg = format!("{err}");
        assert!(
            msg.contains("padding_bytes") && msg.contains("24-bit"),
            "error message should mention padding_bytes and the 24-bit cap: {msg}"
        );
    }

    #[test]
    fn make_encoder_with_padding_flush_preserves_padding_block() {
        // End-to-end: feed real PCM, flush, and verify the final
        // extradata still carries the PADDING tail with the STREAMINFO
        // payload now populated with live MD5 / min/max frame size /
        // total samples. The post-flush rebuild must not drop or
        // corrupt the PADDING block.
        let sr = 44_100u32;
        let n = 4096usize;
        let mut pcm = Vec::with_capacity(n * 2);
        for i in 0..n {
            let v = ((i as f64 / sr as f64 * 440.0 * 2.0 * std::f64::consts::PI).sin() * 20_000.0)
                as i16;
            pcm.extend_from_slice(&v.to_le_bytes());
        }
        let p = build_audio_params(1, sr, SampleFormat::S16);
        let opts = FlacEncoderOptions {
            padding_bytes: Some(1024),
        };
        let mut enc = make_encoder_with_options(&p, opts).unwrap();
        let af = AudioFrame {
            samples: n as u32,
            pts: Some(0),
            data: vec![pcm],
        };
        enc.send_frame(&Frame::Audio(af)).unwrap();
        enc.flush().unwrap();
        // Drain pending packets (we don't need to inspect them; the
        // STREAMINFO rebuild only happens after `flush` is called).
        while enc.receive_packet().is_ok() {}

        let extradata = &enc.output_params().extradata;
        assert_eq!(extradata.len(), 4 + 34 + 4 + 1024);
        // STREAMINFO is still non-last.
        let si_hdr = BlockHeader::parse(&extradata[..4]).unwrap();
        assert!(!si_hdr.last);
        assert_eq!(si_hdr.length, 34);
        // STREAMINFO payload now carries real stats. MD5 (last 16
        // bytes of the 34-byte payload) is non-zero on real PCM
        // input — the post-flush rebuild populated it.
        let md5_off = 4 + 34 - 16;
        let md5 = &extradata[md5_off..md5_off + 16];
        assert!(md5.iter().any(|&b| b != 0), "MD5 should be populated");
        // PADDING tail intact: last-flagged, length=1024, all zeros.
        let pad_hdr = BlockHeader::parse(&extradata[4 + 34..4 + 34 + 4]).unwrap();
        assert!(pad_hdr.last);
        assert_eq!(pad_hdr.block_type, BlockType::Padding);
        assert_eq!(pad_hdr.length, 1024);
        assert!(extradata[4 + 34 + 4..].iter().all(|&b| b == 0));
    }

    #[test]
    fn extradata_chain_with_padding_walks_through_block_parser() {
        // The chain produced by the encoder must be walkable by the
        // exact same BlockHeader::parse loop the demuxer uses to read
        // metadata from a .flac file. This is the regression guard for
        // future changes to `build_extradata`.
        let p = build_audio_params(2, 48_000, SampleFormat::S16);
        let opts = FlacEncoderOptions {
            padding_bytes: Some(256),
        };
        let enc = make_encoder_with_options(&p, opts).unwrap();
        let extradata = &enc.output_params().extradata;
        let mut cursor = 0usize;
        let mut seen_streaminfo = false;
        let mut seen_padding = false;
        loop {
            let hdr = BlockHeader::parse(&extradata[cursor..cursor + 4]).unwrap();
            cursor += 4;
            match hdr.block_type {
                BlockType::StreamInfo => {
                    seen_streaminfo = true;
                    assert!(
                        !hdr.last,
                        "STREAMINFO must not be last when padding follows"
                    );
                }
                BlockType::Padding => {
                    seen_padding = true;
                    assert!(hdr.last, "PADDING is the chain's last block");
                }
                other => panic!("unexpected block type in chain: {other:?}"),
            }
            cursor += hdr.length as usize;
            if hdr.last {
                break;
            }
        }
        assert!(seen_streaminfo);
        assert!(seen_padding);
        assert_eq!(cursor, extradata.len(), "chain must be fully consumed");
    }

    /// Round-176 hoisted the partition-search zigzag conversion from
    /// per-partition-per-(method, partition_order) scope up to a single
    /// per-subframe `Vec<u32>`. Round-186 took the same shape one step
    /// further: subframe-scoped prefix-sum and per-sample raw-bit tables
    /// so the `sum_u` reduction (closed-form `k_est` input) is O(1) per
    /// partition and the escape-cost path's `raw_bits_needed` max is a
    /// `u8` slice scan instead of `leading_zeros` per element. Both
    /// optimised variants — the round-176 `_z` path and the round-186
    /// `_zp` path — must produce **bit-identical** layout costs to the
    /// standalone pre-round-176 path for every residual the encoder
    /// might submit; otherwise the search would pick a different
    /// `(method, partition_order)` pair and the on-wire output would
    /// silently drift.
    ///
    /// This regression test crosswalks both optimised paths against the
    /// retained reference path across the same diverse residual buffers
    /// the `best_rice_params_matches_brute_force_reference` test uses
    /// (degenerate, Laplace-shaped, split-statistics, all-zero,
    /// adversarial single-spike), plus every legal partition order for a
    /// 4096-sample block under both methods. A mismatch fails the test
    /// before it can reach a fixture round-trip.
    #[test]
    fn partition_layout_cost_z_matches_reference() {
        fn xorshift32(state: &mut u32) -> u32 {
            *state ^= *state << 13;
            *state ^= *state >> 17;
            *state ^= *state << 5;
            *state
        }

        let block = 4096usize;
        let mut cases: Vec<(String, Vec<i32>)> = Vec::new();

        // Degenerate: all zero residuals — every partition collapses to
        // k=0 Rice across the board.
        cases.push(("all-zero".into(), vec![0i32; block]));

        // Degenerate: single-spike — one huge residual hidden in
        // otherwise-tiny content; tests that an escape partition
        // wins for the partition that contains the spike.
        let mut spike = vec![1i32; block];
        spike[block / 2] = 1_000_000;
        cases.push(("single-spike".into(), spike));

        // Laplace-shaped at a few mean magnitudes spanning the realistic
        // span the encoder sees.
        for (mean, seed) in [(2u32, 0xDEAD_BEEFu32), (32, 1), (512, 2)] {
            let mut state = seed;
            let mut buf = Vec::with_capacity(block);
            for _ in 0..block {
                let u = xorshift32(&mut state);
                let mag = (u % (mean * 4 + 1)) as i32;
                let sign = if (u >> 31) & 1 == 1 { -1 } else { 1 };
                buf.push(sign * mag);
            }
            cases.push((format!("laplace(mean={mean})"), buf));
        }

        // Split-statistics: tiny first half, huge second half — the
        // canonical case higher partition orders exist to exploit.
        let mut split = Vec::with_capacity(block);
        for i in 0..block {
            if i < block / 2 {
                split.push((i % 3) as i32 - 1);
            } else {
                split.push(((i * 2654435761usize) % 20000) as i32 - 10000);
            }
        }
        cases.push(("split-stats".into(), split));

        // Extreme: residuals at the i32::MIN / i32::MAX edges. Exercises
        // the round-186 `raw_bits_for_sample` against the limits of the
        // `33 - leading_zeros` derivation (which must clamp to 32 for
        // i32::MIN since !(i32::MIN) leading_zeros = 1, giving needed = 32).
        let mut edge = vec![0i32; block];
        edge[0] = i32::MIN;
        edge[block / 4] = i32::MAX;
        edge[block / 2] = -1;
        edge[3 * block / 4] = 1;
        cases.push(("edge-i32-bounds".into(), edge));

        for (name, residuals) in &cases {
            let zigzag: Vec<u32> = residuals.iter().map(|&r| zigzag_encode(r)).collect();
            let (prefix, raw_bits) = build_partition_tables(residuals, &zigzag);
            for predictor_order in [0usize, 1, 4, 8, 12] {
                for (param_bits, k_max) in [(4u32, 14u32), (5u32, 30u32)] {
                    for partition_order in 0..=MAX_PARTITION_ORDER {
                        let ref_cost = partition_layout_cost(
                            residuals,
                            block,
                            predictor_order,
                            partition_order,
                            param_bits,
                            k_max,
                        );
                        let z_cost = partition_layout_cost_z(
                            residuals,
                            &zigzag,
                            block,
                            predictor_order,
                            partition_order,
                            param_bits,
                            k_max,
                        );
                        let zp_cost = partition_layout_cost_zp(
                            &zigzag,
                            &prefix,
                            &raw_bits,
                            block,
                            predictor_order,
                            partition_order,
                            param_bits,
                            k_max,
                        );
                        assert_eq!(
                            ref_cost, z_cost,
                            "{name}: _z mismatch for predictor_order={predictor_order} \
                             param_bits={param_bits} partition_order={partition_order}: \
                             ref={ref_cost:?} z={z_cost:?}",
                        );
                        assert_eq!(
                            ref_cost, zp_cost,
                            "{name}: _zp mismatch for predictor_order={predictor_order} \
                             param_bits={param_bits} partition_order={partition_order}: \
                             ref={ref_cost:?} zp={zp_cost:?}",
                        );
                    }
                }
            }
        }
    }

    /// Reference implementation of the Levinson-Durbin coefficient
    /// update kept in test scope only, mirroring the pre-round-182 path
    /// that allocated a fresh `new_lpc` vector per outer iteration. Used
    /// solely as a regression oracle: the production [`levinson_durbin`]
    /// performs the same recurrence in place by processing the symmetric
    /// pair `(j, i-1-j)` simultaneously. This reference exists so the
    /// crosswalk test [`levinson_durbin_in_place_matches_reference`] can
    /// catch any future drift in the in-place sweep at the bit level.
    #[cfg(test)]
    fn levinson_durbin_reference(
        samples: &[i32],
        order: usize,
        window: ApodizationWindow,
    ) -> Option<Vec<f64>> {
        let n = samples.len();
        if n <= order || order == 0 {
            return None;
        }
        let mut windowed: Vec<f64> = Vec::with_capacity(n);
        for (i, &s) in samples.iter().enumerate() {
            windowed.push(s as f64 * window.weight(i, n));
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
            // Allocate + copy form: exactly what the pre-round-182
            // production path did.
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
        let mut out = Vec::with_capacity(order);
        for &v in lpc.iter() {
            out.push(-v);
        }
        Some(out)
    }

    /// Round-182 replaced the cloning Levinson-Durbin inner loop with a
    /// symmetric-pair in-place sweep that eliminates ~order Vec
    /// allocations per call (called 3 windows × 12 LPC orders = 36 times
    /// per encoded subframe). The recurrence is provably arithmetic-
    /// identical to the previous formulation — both update positions `j`
    /// and `i-1-j` from the **old** values of `lpc[j]` and `lpc[i-1-j]`
    /// using the same `+` and `*` operands in the same evaluation order,
    /// so the f64 result is bit-identical. This crosswalk verifies that
    /// claim empirically across diverse signals and across every LPC
    /// order the production encoder will ever submit, so that any future
    /// refactor of the in-place sweep cannot silently drift the on-wire
    /// coefficient stream.
    #[test]
    fn levinson_durbin_in_place_matches_reference() {
        fn xorshift32(state: &mut u32) -> u32 {
            *state ^= *state << 13;
            *state ^= *state >> 17;
            *state ^= *state << 5;
            *state
        }

        let mut cases: Vec<(String, Vec<i32>)> = Vec::new();

        // Sine sweep — produces tightly-correlated samples for LPC.
        let sr = 44_100u32;
        for &freq in &[110.0f64, 440.0, 1_000.0, 4_410.0] {
            let mut buf = Vec::with_capacity(4096);
            for i in 0..4096 {
                let t = i as f64 / sr as f64;
                buf.push(((t * freq * 2.0 * std::f64::consts::PI).sin() * 20_000.0) as i32);
            }
            cases.push((format!("sine_{}hz", freq as u32), buf));
        }

        // Stacked partials at non-harmonic spacings — the case the
        // multi-window search was added to handle. LPC coefficients
        // here are particularly sensitive to autocorrelation precision.
        let mut stacked = Vec::with_capacity(4096);
        for i in 0..4096 {
            let t = i as f64 / sr as f64;
            let v = (t * 4_000.0 * 2.0 * std::f64::consts::PI).sin() * 8_000.0
                + (t * 4_050.0 * 2.0 * std::f64::consts::PI).sin() * 8_000.0
                + (t * 11_017.0 * 2.0 * std::f64::consts::PI).sin() * 4_000.0;
            stacked.push(v as i32);
        }
        cases.push(("stacked_partials".into(), stacked));

        // Pseudo-random walk — broad-spectrum input where LPC fits less
        // tightly; the recurrence sees a wide dynamic range of `k`
        // reflection coefficients.
        let mut state = 0xC0FFEEu32;
        let mut walk = Vec::with_capacity(4096);
        let mut acc: i32 = 0;
        for _ in 0..4096 {
            let u = xorshift32(&mut state);
            acc = acc.wrapping_add(((u >> 16) as i16 / 8) as i32);
            walk.push(acc.clamp(-30_000, 30_000));
        }
        cases.push(("walk".into(), walk));

        // Constant-tail PCM ending in zeros — the windowed autocorrelation
        // gets a strong DC term that the recurrence handles via the
        // reflection step.
        let mut tail = Vec::with_capacity(4096);
        for i in 0..2048 {
            let t = i as f64 / sr as f64;
            tail.push(((t * 1_000.0 * 2.0 * std::f64::consts::PI).sin() * 15_000.0) as i32);
        }
        tail.extend(std::iter::repeat(0i32).take(2048));
        cases.push(("tonal_then_silence".into(), tail));

        for (name, samples) in &cases {
            for window in APODIZATION_WINDOWS.iter().copied() {
                for order in 1..=MAX_LPC_ORDER {
                    let opt = levinson_durbin(samples, order, window);
                    let refc = levinson_durbin_reference(samples, order, window);
                    assert_eq!(
                        opt.is_some(),
                        refc.is_some(),
                        "{name}: convergence disagreement at order={order} window={window:?}",
                    );
                    if let (Some(opt), Some(refc)) = (opt, refc) {
                        assert_eq!(
                            opt.len(),
                            refc.len(),
                            "{name}: length disagreement at order={order} window={window:?}",
                        );
                        for (idx, (a, b)) in opt.iter().zip(refc.iter()).enumerate() {
                            // Strict bit-for-bit f64 equality — see the
                            // arithmetic-identity argument in
                            // `levinson_durbin`'s body comment.
                            assert_eq!(
                                a.to_bits(),
                                b.to_bits(),
                                "{name}: coefficient [{idx}] drift at order={order} \
                                 window={window:?}: in-place={a} reference={b}",
                            );
                        }
                    }
                }
            }
        }
    }
}
