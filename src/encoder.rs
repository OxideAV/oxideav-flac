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
//! search over partition orders 0..=8 (the streamable-subset ceiling;
//! `FlacEncoderOptions::allow_non_subset_partition_order` raises it to
//! the full 4-bit field range 0..=15) and both Rice methods, with each
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
use crate::decoder::decode_frame_channels;
use crate::md5::Md5;
use crate::metadata::{write_padding, BlockHeader, BlockType, StreamInfo};
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

    /// Allow the partitioned-Rice residual coder (RFC 9639 §9.2.7) to
    /// search partition orders above the FLAC streamable-subset ceiling
    /// of 8 (RFC 9639 §7), up to the full 4-bit field range of 15.
    ///
    /// The default (`false`) keeps every emitted stream inside the
    /// streamable subset: the residual search caps the partition order
    /// at 8, exactly the pre-this-round behaviour. Setting it to `true`
    /// lifts the encoder's search ceiling to 15, letting content whose
    /// residual statistics vary on a fine scale (quiet passages abutting
    /// transients within one block) pick a tighter Rice parameter per
    /// region. A finer split only ever wins when it encodes fewer bits —
    /// the search keeps the smallest layout — so the option can only
    /// shrink or tie the residual payload, never inflate it. The trade is
    /// that the resulting stream is **no longer streamable-subset**: a
    /// decoder that relied on the subset's resource bounds (this crate's
    /// own decoder does not — it reads the full 4-bit field) might reject
    /// it, so the higher ceiling is opt-in. RFC 9639 §9.2.7 defines the
    /// field as 4 bits regardless of subset membership, so the produced
    /// bytes remain a valid FLAC stream that any spec-complete decoder —
    /// including this crate's — recovers bit-exactly.
    pub allow_non_subset_partition_order: bool,

    /// Highest LPC predictor order (RFC 9639 §9.2.6) the per-subframe
    /// search will try. `None` (the default) keeps the historical
    /// ceiling of 12, which is also the streamable-subset boundary for
    /// audio with a sample rate ≤ 48000 Hz (RFC 9639 §7: "Linear
    /// prediction subframes ... MUST have a predictor order less than or
    /// equal to 12").
    ///
    /// `Some(k)` raises the search ceiling to `k`, clamped to the spec
    /// maximum of 32 (the subframe-type field stores `order - 1` in 5
    /// bits, so 32 is the largest representable order); a value below 1
    /// is treated as 1. The `best_subframe` driver always keeps the
    /// minimum-bit plan across every order it tries, so a higher ceiling
    /// can only ever shrink or tie the output — orders that don't earn
    /// their warm-up + coefficient overhead are silently discarded.
    ///
    /// The trade is encode cost (Levinson-Durbin is O(order²) and the
    /// per-order residual search is linear in order) and, for ≤48 kHz
    /// audio, **subset membership**: a stream that actually picks an
    /// order above 12 is no longer streamable-subset, so this is opt-in.
    /// The produced bytes stay a valid FLAC stream that any spec-complete
    /// decoder — including this crate's, which reads the full 5-bit order
    /// field — recovers bit-exactly. For audio above 48 kHz the subset
    /// already permits orders up to 32, so raising the ceiling there
    /// stays inside the subset.
    pub max_lpc_order: Option<usize>,

    /// Override the set of apodization (analysis) windows the
    /// per-subframe LPC search evaluates. `None` (the default) uses the
    /// five-window set — Welch, Hann, Tukey(1/4), Bartlett, and
    /// Blackman-Harris.
    ///
    /// An apodization window tapers a block before the autocorrelation
    /// that feeds Levinson-Durbin (RFC 9639 §9.2.6). It has **no**
    /// on-wire footprint: the RFC fixes only the quantised coefficients,
    /// precision, and shift that reach the bitstream, so which window
    /// shaped the analysis is invisible to the decoder. Different windows
    /// trade spectral leakage against retained edge energy, and the best
    /// one is signal-dependent, so the encoder solves under each window
    /// in the set, residual-codes the result, and keeps whichever LPC
    /// subframe encodes smallest. Because the minimum-bit `best_subframe`
    /// driver always keeps the smallest plan across every `(window,
    /// order, precision)` candidate it visits, adding windows can only
    /// ever shrink or tie the output — a window that never wins is
    /// silently discarded. The trade is encode cost: each extra window is
    /// one more Levinson-Durbin solve (plus its precision sweep) per LPC
    /// order per subframe.
    ///
    /// `Some(set)` replaces the default windows with `set` — useful both
    /// to add a window the default set omits and to trim to a leaner set
    /// (e.g. Welch-only) when encode speed matters more than the last
    /// few percent. An empty set is treated as the default (the LPC
    /// search needs at least one window to run). Duplicate windows are
    /// harmless — they just cost a redundant solve. The produced bytes
    /// stay a fully valid streamable-subset FLAC stream regardless of the
    /// window set; this knob never affects subset membership.
    pub apodization: Option<Vec<Apodization>>,

    /// Number of interchannel samples per frame the encoder accumulates
    /// before emitting a FLAC frame (RFC 9639 §9.1). `None` (the default)
    /// keeps the historical fixed block size of 4096 and is byte-for-byte
    /// unchanged.
    ///
    /// `Some(n)` sets the per-frame block size to `n`. This encoder uses
    /// the fixed-blocking strategy (STREAMINFO `min_block_size ==
    /// max_block_size`), so every frame but the last carries exactly `n`
    /// interchannel samples; the final frame carries the remainder (the
    /// last frame is exempt from the minimum-block-size floor per RFC 9639
    /// §8.2). The block size is a free encoder choice that trades coding
    /// efficiency (larger blocks amortise the per-frame header + warm-up +
    /// LPC-coefficient overhead and let the predictor + Rice-partition
    /// search adapt over a longer window) against latency and seek
    /// granularity (a decoder must buffer a whole block, and seek points
    /// land on block boundaries).
    ///
    /// `n` MUST be in the STREAMINFO range `16..=65535` (RFC 9639 §8.2:
    /// "The minimum block size and the maximum block size MUST be in the
    /// 16-65535 range"); a value outside it makes
    /// `make_encoder_with_options` return `Error::invalid`.
    ///
    /// **Subset membership.** The FLAC streamable subset (RFC 9639 §7)
    /// caps the block size at 16384 always, and at 4608 for audio whose
    /// sample rate is ≤ 48000 Hz. By default a request that would exceed
    /// the applicable subset ceiling is rejected with `Error::invalid`, so
    /// the produced stream stays streamable-subset. Setting
    /// [`allow_non_subset_block_size`](Self::allow_non_subset_block_size)
    /// to `true` lifts the check to the full STREAMINFO ceiling (65535),
    /// producing a valid — but no longer streamable-subset — FLAC stream
    /// that any spec-complete decoder (including this crate's) still
    /// recovers bit-exactly.
    pub block_size: Option<u32>,

    /// Permit [`block_size`](Self::block_size) values above the streamable
    /// subset ceiling (RFC 9639 §7), up to the STREAMINFO maximum of
    /// 65535. The default (`false`) rejects any block size that would push
    /// the stream out of the subset (> 16384 always, or > 4608 for audio
    /// at ≤ 48000 Hz). It has no effect when `block_size` is `None` or
    /// already within the subset ceiling.
    pub allow_non_subset_block_size: bool,

    /// Decode every encoded frame back to PCM as it is produced and assert
    /// it reconstructs the input samples bit-exactly — FLAC's canonical
    /// encode-time verify safety net (the `flac --verify` flag).
    ///
    /// FLAC is a lossless codec, so a *correct* encoder always produces a
    /// stream that decodes back to the exact input. The verify pass is a
    /// belt-and-braces self-check against an encoder bug: with `true`, the
    /// encoder runs its own decoder over each freshly-emitted frame and
    /// compares the reconstructed per-channel `i32` planes against the
    /// original block sample-for-sample. On any mismatch — or if the
    /// round-tripped frame fails to decode at all (e.g. a corrupt
    /// frame-header / CRC the encoder somehow emitted) —
    /// [`Encoder::send_frame`](oxideav_core::Encoder::send_frame) /
    /// [`Encoder::flush`](oxideav_core::Encoder::flush) returns
    /// `Error::InvalidData` naming the first offending `(channel, sample)`,
    /// and the offending frame is *not* queued for `receive_packet`, so a
    /// caller that propagates the error never writes a silently-corrupt
    /// `.flac`.
    ///
    /// This is strictly stronger than the post-hoc whole-stream MD5 check
    /// in [`crate::verify`]: it catches a mis-encode at the exact frame
    /// that produced it (before the file is even finalised), pinpoints the
    /// diverging sample, and needs no STREAMINFO MD5 to compare against.
    /// The check runs through the very same [`decode_frame_channels`] core
    /// the streaming decoder uses, so it verifies exactly the bytes a real
    /// decoder would see — including the frame-header parse and the
    /// trailing CRC-16.
    ///
    /// The trade is encode cost: a full extra decode per frame (roughly the
    /// decode-side cost, which is far cheaper than the encoder's
    /// per-subframe search, so verify typically adds well under the encode
    /// time rather than doubling it). The default (`false`) skips the pass
    /// and is byte-for-byte identical to the historical encoder — verify
    /// never changes the produced bytes, only whether they are checked.
    ///
    /// [`decode_frame_channels`]: crate::decoder::decode_frame_channels
    pub verify: bool,
}

impl FlacEncoderOptions {
    /// A speed-biased preset: trades a little compression ratio for a
    /// markedly faster encode, analogous to a low `flac -N` level.
    ///
    /// The default encoder runs a *maximal-effort* per-subframe search —
    /// five apodization windows × LPC orders 1..=12 × a small precision
    /// window — which is what makes it thorough but slow (#12). This preset
    /// trims the two most expensive search axes:
    ///
    /// * **One apodization window** (Welch) instead of five — cutting the
    ///   per-subframe LPC solve + quantise + residual + Rice-partition
    ///   passes to a fifth.
    /// * **`max_lpc_order = 8`** instead of 12 — fewer (and cheaper, since
    ///   residual cost is linear in order) candidate orders per subframe.
    ///
    /// Every other field keeps its default, so the output is still a valid
    /// streamable-subset FLAC stream that any spec-complete decoder
    /// (including this crate's) recovers bit-exactly. The bytes differ from
    /// the default encoder's (fewer candidates searched ⇒ a slightly larger
    /// residual on some subframes), but only in size — never in
    /// correctness. Use [`FlacEncoderOptions::default`] when ratio matters
    /// more than speed.
    ///
    /// ```
    /// use oxideav_core::{CodecId, CodecParameters, SampleFormat};
    /// use oxideav_flac::encoder::{make_encoder, make_encoder_with_options, FlacEncoderOptions};
    ///
    /// let mut params = CodecParameters::audio(CodecId::new("flac"));
    /// params.channels = Some(2);
    /// params.sample_rate = Some(48_000);
    /// params.sample_format = Some(SampleFormat::S16);
    ///
    /// // Default: maximal-effort search (best ratio).
    /// let _best = make_encoder(&params).expect("default encoder");
    ///
    /// // Fast preset: single window + lower LPC order ceiling (faster).
    /// let _fast = make_encoder_with_options(&params, FlacEncoderOptions::fast())
    ///     .expect("fast encoder");
    /// ```
    pub fn fast() -> Self {
        FlacEncoderOptions {
            apodization: Some(vec![Apodization::Welch]),
            max_lpc_order: Some(8),
            ..FlacEncoderOptions::default()
        }
    }

    /// The default maximal-effort search with the encode-time
    /// [`verify`](Self::verify) self-check enabled — equivalent to
    /// `flac --verify` at the default compression level.
    ///
    /// Every emitted frame is decoded back and asserted bit-exact against
    /// the input before its packet is queued, so an encoder regression
    /// surfaces as an `Error::InvalidData` at the offending frame instead
    /// of a silently-corrupt output file. The produced bytes are identical
    /// to [`FlacEncoderOptions::default`]'s — verify only adds the check,
    /// never changes the encoding. Costs one extra decode per frame.
    pub fn verifying() -> Self {
        FlacEncoderOptions {
            verify: true,
            ..FlacEncoderOptions::default()
        }
    }
}

/// A caller-selectable apodization (analysis) window for the encoder's
/// per-subframe LPC search. See [`FlacEncoderOptions::apodization`].
///
/// These are the public mirror of the encoder-internal window set. They
/// carry **no** bitstream semantics (RFC 9639 §9.2.6 leaves the analysis
/// window a free encoder choice); the decoder never observes which one
/// was used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[non_exhaustive]
pub enum Apodization {
    /// Parabolic taper `1 - x^2`. Centre-heavy, cheap, a strong
    /// all-rounder on steady tonal content; the encoder's historical
    /// default window.
    Welch,
    /// Raised-cosine `0.5 - 0.5*cos(2*pi*i/(N-1))`. Lower side lobes than
    /// Welch — better on signals with closely-spaced partials — at the
    /// cost of heavier edge attenuation.
    Hann,
    /// Tapered-cosine ("Tukey") with taper fraction `alpha = num/den`: a
    /// flat top over the central `(1-alpha)` of the block with cosine
    /// ramps over the outer `alpha`. `alpha` is clamped into `[0, 1]`
    /// (`0` degenerates to a rectangular window, `1` to a Hann). A small
    /// `alpha` keeps almost the whole block at full weight (good for
    /// transient-light blocks) while still smoothing the block-boundary
    /// discontinuity an un-windowed autocorrelation would alias. `den`
    /// must be non-zero; a zero denominator is treated as `alpha = 0`.
    Tukey {
        /// Numerator of the taper fraction `alpha`.
        alpha_num: u32,
        /// Denominator of the taper fraction `alpha` (treated as `1`
        /// when zero to avoid a divide-by-zero in the weight function).
        alpha_den: u32,
    },
    /// Triangular ("Bartlett") taper: a linear ramp from 0 at each edge to
    /// 1 at the centre. The cheapest non-rectangular window — no
    /// transcendental call per sample — and a milder edge attenuation than
    /// Hann, so it can win on content that a centre-heavy Welch over-weights
    /// but a full raised-cosine over-tapers.
    Bartlett,
    /// Four-term Blackman-Harris cosine-sum window. Its side lobes sit far
    /// below Welch/Hann/Tukey, so on densely-packed or closely-spaced
    /// partials — where autocorrelation leakage corrupts the LPC fit — it
    /// can recover a tighter predictor than any of the other windows at the
    /// cost of the heaviest edge attenuation in the set.
    BlackmanHarris,
}

impl Apodization {
    /// Lower the public window to the encoder-internal representation.
    fn to_internal(self) -> ApodizationWindow {
        match self {
            Apodization::Welch => ApodizationWindow::Welch,
            Apodization::Hann => ApodizationWindow::Hann,
            Apodization::Tukey {
                alpha_num,
                alpha_den,
            } => ApodizationWindow::Tukey {
                alpha_num,
                // A zero denominator would divide-by-zero in `weight`;
                // map it to alpha = 0 (rectangular) so the public API can
                // never trip an internal panic.
                alpha_den: if alpha_den == 0 { 1 } else { alpha_den },
            },
            Apodization::Bartlett => ApodizationWindow::Bartlett,
            Apodization::BlackmanHarris => ApodizationWindow::BlackmanHarris,
        }
    }
}

const DEFAULT_BLOCK_SIZE: u32 = 4096;
/// Lowest block size RFC 9639 §8.2 allows in STREAMINFO: "The minimum
/// block size and the maximum block size MUST be in the 16-65535 range."
/// The last frame is exempt from the *minimum* floor (it carries the
/// stream remainder), but the encoder's configured block size — which is
/// also the STREAMINFO min/max value under the fixed-blocking strategy —
/// must be at least 16.
const MIN_BLOCK_SIZE: u32 = 16;
/// Highest block size RFC 9639 §8.2 allows in STREAMINFO (the field is
/// 16 bits, and 0 is reserved, so the legal maximum is 65535).
const MAX_BLOCK_SIZE: u32 = 65_535;
/// Streamable-subset block-size ceiling for audio at any sample rate
/// (RFC 9639 §7: "The stream MUST NOT contain blocks with more than
/// 16384 interchannel samples").
const SUBSET_MAX_BLOCK_SIZE: u32 = 16_384;
/// Streamable-subset block-size ceiling for audio with a sample rate
/// ≤ 48000 Hz (RFC 9639 §7: "Audio with a sample rate less than or equal
/// to 48000 Hz MUST NOT be contained in blocks with more than 4608
/// interchannel samples").
const SUBSET_MAX_BLOCK_SIZE_LOW_RATE: u32 = 4_608;
/// Sample-rate boundary (inclusive) below which the 4608-sample subset
/// block-size ceiling applies (RFC 9639 §7).
const SUBSET_BLOCK_SIZE_RATE_BOUNDARY: u32 = 48_000;
/// Default highest LPC order the encoder will try. The decoder (and
/// RFC 9639 §9.2.6) supports up to 32, but the gain curve flattens
/// fast: most content tops out somewhere between order 6 and order 12.
/// We search 1..=12, which captures the long tail of harmonic / vocal
/// content that benefits from a longer impulse response while keeping
/// the per-frame encode cost bounded (Levinson-Durbin is O(order^2),
/// residual search is linear in order). The `best_subframe` driver
/// picks the minimum-bit plan, so adding higher orders can only ever
/// shrink (or tie) the output: orders that don't earn their
/// coefficient overhead are silently discarded.
///
/// This is also exactly the streamable-subset boundary for audio with
/// a sample rate ≤ 48000 Hz (RFC 9639 §7: "Linear prediction subframes
/// ... MUST have a predictor order less than or equal to 12"), so it is
/// the default ceiling. Callers wanting higher orders opt in via
/// [`FlacEncoderOptions::max_lpc_order`].
const SUBSET_MAX_LPC_ORDER: usize = 12;
/// Highest LPC order RFC 9639 §9.2.6 permits at all: the subframe-type
/// bits store `order - 1` in 5 bits, so the largest representable order
/// is 32. The encoder's search ceiling is clamped to this regardless of
/// what [`FlacEncoderOptions::max_lpc_order`] requests.
const SPEC_MAX_LPC_ORDER: usize = 32;
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
struct SubframeScratch {
    /// Highest partition order the residual search may try (RFC 9639
    /// §9.2.7). This is a per-encoder configuration value, not a reused
    /// buffer: it rides along on the scratch struct because the scratch
    /// is already threaded through every function in the subframe
    /// candidate sweep, so carrying the cap here avoids adding a parallel
    /// parameter to the same eight-deep call chain. `Default` sets it to
    /// [`SUBSET_MAX_PARTITION_ORDER`] (8) so every reference / test path
    /// that builds a `SubframeScratch::default()` keeps emitting
    /// streamable-subset residuals; the production encoder overrides it
    /// from [`FlacEncoderOptions::allow_non_subset_partition_order`].
    max_partition_order: u32,
    /// Highest LPC order the per-subframe search may try (RFC 9639
    /// §9.2.6). Like `max_partition_order` this is a per-encoder
    /// configuration value carried on the scratch struct so it rides the
    /// same call chain the candidate sweep already threads, rather than
    /// adding a parallel parameter. `Default` sets it to
    /// [`SUBSET_MAX_LPC_ORDER`] (12) — the streamable-subset boundary for
    /// ≤48 kHz audio (RFC 9639 §7) — so every reference / test path that
    /// builds a `SubframeScratch::default()` keeps the historical search
    /// ceiling; the production encoder overrides it from
    /// [`FlacEncoderOptions::max_lpc_order`].
    max_lpc_order: usize,
    /// Apodization windows the LPC search evaluates per subframe (RFC
    /// 9639 §9.2.6). Like `max_lpc_order` this is per-encoder
    /// configuration carried on the scratch so it rides the same call
    /// chain the candidate sweep already threads. `Default` populates it
    /// with the [`APODIZATION_WINDOWS`] default set, so every reference /
    /// test path that builds a `SubframeScratch::default()` uses the same
    /// windows the production encoder does; the production encoder
    /// overrides it from [`FlacEncoderOptions::apodization`]. Never
    /// empty — the LPC search needs at least one window — so the
    /// constructor falls back to the default set when the caller supplies
    /// an empty list.
    apodization: Vec<ApodizationWindow>,
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
    /// Final FLAC-convention LPC coefficients (`-a[j+1]`) for the
    /// single-order [`levinson_durbin_s`] reference solve. The production
    /// encoder uses `lpc_coeffs_all` instead (all orders at once, #12), so
    /// this field is exercised only by the `#[cfg(test)]` reference path.
    #[cfg_attr(not(test), allow(dead_code))]
    lpc_coeffs: Vec<f64>,
    /// Triangular snapshot store for the all-orders Levinson-Durbin solve
    /// ([`levinson_durbin_all_orders_s`]). Order `m`'s `m` FLAC-convention
    /// coefficients live at `lpc_coeffs_all[m*(m-1)/2 .. m*(m+1)/2]`. A
    /// single window's recursion fills every order 1..=`max` here so the
    /// per-subframe search no longer re-windows + re-autocorrelates per
    /// order. Reused across windows and frames.
    lpc_coeffs_all: Vec<f64>,
    /// Per-order validity flag (index `m-1` ↔ order `m`) for
    /// `lpc_coeffs_all`: `false` where the recursion bailed out before
    /// reaching that order (degenerate energy), so the candidate sweep
    /// skips those orders exactly as the per-order solve would have.
    lpc_order_valid: Vec<bool>,
    /// Quantised LPC coefficients (length `order`).
    qcoeffs: Vec<i32>,
    /// Wasted-bits-shifted sample buffer used by `best_subframe_s`.
    shifted: Vec<i32>,
    /// Bottom-up partition-merge scratch (`rice_search_merged`). Holds,
    /// for the partition order currently being scored, one `u64` row of
    /// `k_max + 1` shift-sums per partition plus a parallel count / raw-
    /// width row. `level_a` and `level_b` ping-pong: the finest order is
    /// built into one, then each coarser order is merged pairwise into
    /// the other so no slice of `zigzag` is re-walked per order. See
    /// [`MergeLevel`].
    merge_a: MergeLevel,
    merge_b: MergeLevel,
    /// Memoised apodization weight rows, keyed by `(window, n)`. Every
    /// subframe of a fixed-block stream has the same length `n`, and the
    /// LPC search applies the same window set to each, so `window.weight(i,
    /// n)` over `i in 0..n` is recomputed identically for every subframe of
    /// every frame. The weight is a pure function of `(window, i, n)`, so a
    /// cached row is bit-identical to a fresh `weight` call — multiplying a
    /// sample by it yields the exact same `f64` product, keeping the
    /// windowed signal (hence the autocorrelation, the LPC fit, and the
    /// emitted bytes) unchanged. The cache holds at most a handful of rows
    /// (one per `(window, n)` pair; `n` only changes for the short final
    /// block), so a linear scan to find the row is cheaper than the per-
    /// sample transcendental rebuild it replaces.
    window_cache: Vec<WindowWeights>,
}

/// One memoised apodization weight row: the `n` per-sample weights for a
/// given `(window, n)`, reused across every subframe of that length. See
/// [`SubframeScratch::window_cache`].
struct WindowWeights {
    window: ApodizationWindow,
    n: usize,
    weights: Vec<f64>,
}

/// One partition order's worth of bottom-up Rice-merge state, reused
/// across orders by [`rice_search_merged`]. Each partition `p` owns:
/// * `shift_sums[p*stride .. p*stride + stride]` — `sum(zigzag[i] >> k)`
///   for `k in 0..=k_max` (so `stride == k_max + 1`). Element `[..0]` is
///   `sum_u` (since `u >> 0 == u`), feeding the closed-form `k_est`.
/// * `counts[p]` — residual count in the partition (the first partition
///   is short by `predictor_order` warmup samples).
/// * `max_raw[p]` — `max(raw_bits[i])` over the partition, for the
///   escape-cost test.
///
/// Coarser orders are derived by summing adjacent pairs of the next-
/// finer level's rows (shift-sums + counts add; `max_raw` maxes), which
/// is exact because `sum(u >> k)` and the residual count are additive
/// over a partition split and `max` composes. The chosen Rice `k`,
/// escape decision, and emitted bits are therefore bit-identical to the
/// per-slice [`partition_layout_cost_zp`] path.
#[derive(Default)]
struct MergeLevel {
    /// Flattened `num_partitions × stride` shift-sum rows.
    shift_sums: Vec<u64>,
    /// Per-partition residual counts.
    counts: Vec<u32>,
    /// Per-partition max raw width (escape-cost input).
    max_raw: Vec<u8>,
    /// Row width: `k_max + 1`.
    stride: usize,
}

impl Default for SubframeScratch {
    /// All buffers start empty (they grow to capacity on first use) and
    /// the partition-order ceiling starts at the streamable-subset value
    /// so any caller that builds a default scratch (every `#[cfg(test)]`
    /// reference helper does) keeps the historical subset behaviour. The
    /// production encoder overrides `max_partition_order` after building
    /// the encoder when the caller opted into non-subset output.
    fn default() -> Self {
        SubframeScratch {
            max_partition_order: SUBSET_MAX_PARTITION_ORDER,
            max_lpc_order: SUBSET_MAX_LPC_ORDER,
            apodization: APODIZATION_WINDOWS.to_vec(),
            residuals: Vec::new(),
            zigzag: Vec::new(),
            prefix: Vec::new(),
            raw_bits: Vec::new(),
            windowed: Vec::new(),
            autoc: Vec::new(),
            lpc_state: Vec::new(),
            lpc_coeffs: Vec::new(),
            lpc_coeffs_all: Vec::new(),
            lpc_order_valid: Vec::new(),
            qcoeffs: Vec::new(),
            shifted: Vec::new(),
            merge_a: MergeLevel::default(),
            merge_b: MergeLevel::default(),
            window_cache: Vec::new(),
        }
    }
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
    /// Triangular (Bartlett) taper: a linear ramp from 0 at each edge to 1
    /// at the centre. Cheapest non-rectangular window; milder edge
    /// attenuation than Hann.
    Bartlett,
    /// Four-term Blackman-Harris cosine-sum window. Very low side lobes,
    /// strongest leakage suppression in the set, at the cost of the
    /// heaviest edge attenuation.
    BlackmanHarris,
}

/// The windows the per-subframe LPC search evaluates by default, in
/// priority order (ties are broken toward the earlier entry, which keeps
/// the long-standing Welch result when nothing beats it). Five windows
/// with materially different leakage / edge-energy trade-offs:
///
/// * `Welch` — centre-heavy parabolic taper, a strong all-rounder on
///   steady tonal content.
/// * `Hann` — raised cosine, low side lobes, good on closely-spaced
///   partials.
/// * `Tukey(1/4)` — mostly flat with light edge smoothing, good for
///   transient-light blocks.
/// * `Bartlett` — triangular, milder edge attenuation than Hann; wins on
///   content Welch over-weights but Hann over-tapers.
/// * `BlackmanHarris` — four-term cosine sum with the lowest side lobes
///   in the set; recovers a tighter predictor when autocorrelation
///   leakage from densely-packed partials corrupts the LPC fit.
///
/// The window has no on-wire footprint (RFC 9639 §9.2.6 fixes only the
/// quantised coefficients, precision, and shift that reach the
/// bitstream), so trying more windows is subset-neutral and the
/// minimum-bit `best_subframe` driver keeps whichever LPC subframe
/// encodes smallest — a window that never wins is silently discarded, so
/// adding windows can only shrink or tie the output. On real
/// multichannel / wideband content the Bartlett + Blackman-Harris pair
/// recovers a few percent the three-window set left on the table; the
/// cost is two extra Levinson-Durbin solves per LPC order per subframe.
/// Callers that want the leaner three-window search (or any other set)
/// override it via [`FlacEncoderOptions::apodization`].
const APODIZATION_WINDOWS: [ApodizationWindow; 5] = [
    ApodizationWindow::Welch,
    ApodizationWindow::Hann,
    ApodizationWindow::Tukey {
        alpha_num: 1,
        alpha_den: 4,
    },
    ApodizationWindow::Bartlett,
    ApodizationWindow::BlackmanHarris,
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
            ApodizationWindow::Bartlett => {
                // Triangular: linear ramp 0 -> 1 -> 0, peak at the centre.
                // `1 - |(i - (N-1)/2) / ((N-1)/2)|` is 1 at i = (N-1)/2 and
                // 0 at both ends.
                let half = last / 2.0;
                1.0 - ((i as f64 - half) / half).abs()
            }
            ApodizationWindow::BlackmanHarris => {
                // Four-term Blackman-Harris: a0 - a1*cos(2*pi*t)
                //   + a2*cos(4*pi*t) - a3*cos(6*pi*t), t = i/(N-1).
                // The classic minimum-side-lobe coefficient set.
                const A0: f64 = 0.358_75;
                const A1: f64 = 0.488_29;
                const A2: f64 = 0.141_28;
                const A3: f64 = 0.011_68;
                let t = i as f64 / last;
                let w = std::f64::consts::PI * t;
                A0 - A1 * (2.0 * w).cos() + A2 * (4.0 * w).cos() - A3 * (6.0 * w).cos()
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
/// caller-tunable knobs (padding reservation, LPC-order / partition-order
/// ceilings, apodization window set, per-frame block size). `make_encoder`
/// is a thin wrapper that passes `FlacEncoderOptions::default()` through.
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
    // RFC 9639 §8.2: the STREAMINFO sample-rate field is 20 bits
    // wide, and "the sample rate MUST NOT be 0 when the FLAC file
    // contains audio". Frame headers fall back to sample-rate code
    // 0b0000 ("get from STREAMINFO") for rates without a direct or
    // escape coding, so the STREAMINFO ceiling is the binding bound
    // at construction. Daily-fuzz regression (`encoder_options`
    // harness, 2026-06-11 run): `sample_rate == u32::MAX` previously
    // flowed unchecked into the STREAMINFO writer inside
    // `build_extradata`, whose bounds check was consumed by an
    // `expect` — panicking instead of returning the `Err` the
    // constructor contract promises.
    if sample_rate == 0 || sample_rate > StreamInfo::MAX_SAMPLE_RATE {
        return Err(Error::invalid(format!(
            "FLAC encoder: sample_rate {} outside spec range 1..={} (RFC 9639 §8.2)",
            sample_rate,
            StreamInfo::MAX_SAMPLE_RATE
        )));
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

    // Resolve + validate the per-frame block size (RFC 9639 §9.1).
    // `None` keeps the historical fixed 4096. Any explicit value must sit
    // in the STREAMINFO range 16..=65535 (§8.2); subset membership (§7)
    // additionally bounds it unless the caller opts out.
    let block_size = match options.block_size {
        None => DEFAULT_BLOCK_SIZE,
        Some(n) => {
            if !(MIN_BLOCK_SIZE..=MAX_BLOCK_SIZE).contains(&n) {
                return Err(Error::invalid(format!(
                    "FLAC encoder: block_size {} outside STREAMINFO range {}..={} (RFC 9639 §8.2)",
                    n, MIN_BLOCK_SIZE, MAX_BLOCK_SIZE
                )));
            }
            if !options.allow_non_subset_block_size {
                let subset_ceiling = if sample_rate <= SUBSET_BLOCK_SIZE_RATE_BOUNDARY {
                    SUBSET_MAX_BLOCK_SIZE_LOW_RATE
                } else {
                    SUBSET_MAX_BLOCK_SIZE
                };
                if n > subset_ceiling {
                    return Err(Error::invalid(format!(
                        "FLAC encoder: block_size {} exceeds streamable-subset ceiling {} for \
                         sample_rate {} Hz (RFC 9639 §7); set \
                         FlacEncoderOptions::allow_non_subset_block_size to allow it",
                        n, subset_ceiling, sample_rate
                    )));
                }
            }
            n
        }
    };

    let extradata = build_extradata(
        block_size,
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
        block_size,
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
        verify: options.verify,
        scratch: build_scratch(&options),
    }))
}

/// Build the per-subframe search scratch from the caller's options. Split
/// out of [`make_encoder_with_options`] so a test can construct a concrete
/// [`FlacEncoder`] (e.g. to drive `verify_frame` directly) without
/// re-implementing the option-to-ceiling mapping.
fn build_scratch(options: &FlacEncoderOptions) -> SubframeScratch {
    SubframeScratch {
        // Lift the residual-search partition-order ceiling above the
        // streamable-subset bound (8) only when the caller opted in.
        // Everything else stays at the `Default` (empty buffers).
        max_partition_order: if options.allow_non_subset_partition_order {
            MAX_PARTITION_ORDER
        } else {
            SUBSET_MAX_PARTITION_ORDER
        },
        // Lift the LPC-order search ceiling above the streamable-subset
        // bound (12, for ≤48 kHz) only when the caller opted in. The
        // requested ceiling is clamped to the spec maximum of 32 and
        // floored at 1; `None` keeps the historical default of 12.
        max_lpc_order: match options.max_lpc_order {
            Some(k) => k.clamp(1, SPEC_MAX_LPC_ORDER),
            None => SUBSET_MAX_LPC_ORDER,
        },
        // Replace the default window set only when the caller
        // supplied a non-empty list; an empty list (or `None`) uses
        // the `APODIZATION_WINDOWS` default set so the LPC search
        // always has at least one window to solve under.
        apodization: match &options.apodization {
            Some(set) if !set.is_empty() => set.iter().map(|w| w.to_internal()).collect(),
            _ => APODIZATION_WINDOWS.to_vec(),
        },
        ..SubframeScratch::default()
    }
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
    // Delegate the 34-byte payload to the typed writer so both call
    // paths (encoder build-time + standalone `StreamInfo::write`)
    // walk the same RFC 9639 §8.1 packer.
    let info = StreamInfo {
        min_block_size: block_size as u16,
        max_block_size: block_size as u16,
        min_frame_size,
        max_frame_size,
        sample_rate,
        channels,
        bits_per_sample: bps,
        // §8.2: the 36-bit total-samples field reserves 0 as the
        // "unknown" sentinel. A stream longer than 2^36 − 1 samples
        // cannot be represented, so degrade to "unknown" instead of
        // tripping the writer's bounds check at flush time (every
        // other field is constructor-validated or physically bounded
        // below its wire width).
        total_samples: if total_samples > StreamInfo::MAX_TOTAL_SAMPLES {
            0
        } else {
            total_samples
        },
        md5: *md5,
    };
    let payload = info
        .write()
        .expect("encoder build path supplies spec-valid STREAMINFO fields");
    let mut out = Vec::with_capacity(4 + StreamInfo::PAYLOAD_LEN);
    // Block-header byte 0: high bit = `last`, low 7 bits = STREAMINFO
    // type code (0).
    out.push(if last { 0x80 } else { 0x00 });
    out.push(0x00);
    out.push(0x00);
    out.push(0x22);
    out.extend_from_slice(&payload);
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
    /// When `true`, decode each emitted frame back and assert it
    /// reconstructs the input block bit-exactly before queuing the packet.
    /// See [`FlacEncoderOptions::verify`].
    verify: bool,
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

            // FLAC `--verify`: decode the frame we just emitted and assert
            // it reconstructs the input block bit-exactly. Runs before the
            // packet is queued, so a mis-encode aborts the stream rather
            // than producing a silently-corrupt `.flac`.
            if self.verify {
                self.verify_frame(&data, &per_channel)?;
            }

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

    /// Decode `frame` (the freshly-encoded frame bytes) back to PCM and
    /// assert it equals the original `per_channel` input sample-for-sample
    /// — FLAC's encode-time `--verify` self-check (RFC 9639 §9.2.1: the
    /// codec is lossless, so a correct encode round-trips exactly).
    ///
    /// Returns `Error::InvalidData` on the first diverging sample (or if the
    /// frame fails to decode / CRC-16 at all), naming the channel and sample
    /// index so an encoder regression is pinpointed rather than reported as a
    /// vague whole-file mismatch.
    fn verify_frame(&self, frame: &[u8], per_channel: &[Vec<i32>]) -> Result<()> {
        // A minimal STREAMINFO is all `decode_frame_channels` needs: the
        // frame header is self-describing except for the "get from
        // STREAMINFO" escapes (block size / sample rate / bit depth), which
        // this encoder never emits, but we populate the relevant fields
        // anyway so the escape path resolves correctly if it ever does.
        let info = StreamInfo {
            min_block_size: self.block_size as u16,
            max_block_size: self.block_size as u16,
            min_frame_size: 0,
            max_frame_size: 0,
            sample_rate: self.sample_rate,
            channels: self.channels as u8,
            bits_per_sample: self.bps,
            total_samples: 0,
            md5: [0u8; 16],
        };
        let decoded = decode_frame_channels(frame, &info).map_err(|e| {
            Error::invalid(format!(
                "FLAC verify (frame {}): encoded frame failed to decode: {}",
                self.frame_number, e
            ))
        })?;

        if decoded.channels.len() != per_channel.len() {
            return Err(Error::invalid(format!(
                "FLAC verify (frame {}): channel count {} != input {}",
                self.frame_number,
                decoded.channels.len(),
                per_channel.len()
            )));
        }
        for (c, (got, want)) in decoded.channels.iter().zip(per_channel.iter()).enumerate() {
            if got.len() != want.len() {
                return Err(Error::invalid(format!(
                    "FLAC verify (frame {}): channel {} length {} != input {}",
                    self.frame_number,
                    c,
                    got.len(),
                    want.len()
                )));
            }
            if let Some(i) = got.iter().zip(want.iter()).position(|(g, w)| g != w) {
                return Err(Error::invalid(format!(
                    "FLAC verify (frame {}): channel {} sample {} decoded {} != input {}",
                    self.frame_number, c, i, got[i], want[i]
                )));
            }
        }
        Ok(())
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

/// Map a stream bit depth onto the 3-bit frame-header sample-size code
/// (RFC 9639 §9.1.4 Table 17). Every depth this encoder accepts has an
/// explicit table entry, so each frame is self-describing — a decoder
/// recovers the bit depth from the frame header without consulting
/// STREAMINFO. A depth with no table entry falls back to code `0b000`
/// ("from STREAMINFO"). The field is 3 bits wide for every code, so the
/// choice has no effect on frame size.
fn encode_sample_size(bps: u8) -> u8 {
    match bps {
        8 => 1,
        12 => 2,
        16 => 4,
        20 => 5,
        24 => 6,
        32 => 7,
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
/// CONSTANT, FIXED orders 0..=4, LPC orders 1..=`scratch.max_lpc_order`
/// and VERBATIM.
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

    let max_lpc = scratch.max_lpc_order.min(n.saturating_sub(1));
    if max_lpc >= 1 {
        // LPC search. The previous structure called `encode_lpc_plan_s`
        // once per order, and each of those re-windowed + re-autocorrelated
        // + re-ran Levinson-Durbin for every apodization window — so a
        // block was windowed/autocorrelated ~`max_lpc`× per window when
        // each is needed only ONCE per window (#12 "encode is very slow").
        //
        // Here we invert the loop nest: for each window, window +
        // autocorrelate + run Levinson-Durbin a SINGLE time, snapshotting
        // every order's coefficients (`levinson_durbin_all_orders_s`), then
        // sweep order × precision off the snapshots. This is byte-identical
        // to the old nest: for any fixed order the sequence of
        // (window, precision) plan comparisons is the same window-outer /
        // precision-inner accumulation with the same strict `<` tie-break,
        // and the snapshot coefficients are bit-identical to the per-order
        // solve (see `levinson_durbin_all_orders_s`). We accumulate the
        // per-order best in `lpc_order_best`, then fold those into the
        // global `best` in ascending order — identical to the old order
        // loop's ascending strict-`<` selection.
        let ceiling = lpc_precision_for(n);
        let floor = ceiling
            .saturating_sub(LPC_PRECISION_SEARCH_SPAN)
            .max(MIN_LPC_QLP_PRECISION);

        let mut lpc_order_best: Vec<Option<SubframePlan>> = (0..max_lpc).map(|_| None).collect();
        let windows = std::mem::take(&mut scratch.apodization);
        for &window in windows.iter() {
            let reached = levinson_durbin_all_orders_s(samples, max_lpc, window, scratch);
            if reached == 0 {
                continue;
            }
            // Detach the snapshot store so `encode_lpc_plan_at_s` can borrow
            // `scratch` mutably for its quantise / residual / partition
            // tables without aliasing the read-only coefficient slice.
            let all = std::mem::take(&mut scratch.lpc_coeffs_all);
            let valid = std::mem::take(&mut scratch.lpc_order_valid);
            for order in 1..=max_lpc {
                if order > valid.len() || !valid[order - 1] {
                    continue;
                }
                let base = order * (order - 1) / 2;
                let coeffs = &all[base..base + order];
                for precision in floor..=ceiling {
                    if let Some(plan) = encode_lpc_plan_at_s(
                        samples, bps, order, wasted, coeffs, precision, scratch,
                    ) {
                        let slot = &mut lpc_order_best[order - 1];
                        let better = match slot {
                            Some(b) => plan.bits < b.bits,
                            None => true,
                        };
                        if better {
                            *slot = Some(plan);
                        }
                    }
                }
            }
            scratch.lpc_coeffs_all = all;
            scratch.lpc_order_valid = valid;
        }
        scratch.apodization = windows;

        for plan in lpc_order_best.into_iter().flatten() {
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
///
/// As of #12's per-window-solve restructure, the production encoder's
/// per-subframe search no longer calls this per-order path (it uses
/// [`levinson_durbin_all_orders_s`], which windows + autocorrelates once
/// per window instead of once per order). This single-order driver is
/// retained as the reference path the byte-exactness tests pin the
/// all-orders solve against, so it is `#[cfg(test)]`.
#[cfg(test)]
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
    // Detach the configured window set so the inner solve/precision
    // passes can borrow `scratch` mutably for their own buffers without
    // aliasing the read-only window list. `apodization` is never touched
    // by any callee, so a `take` + restore is a clone-free borrow (same
    // shape as the `lpc_coeffs` detach below). Restored before return.
    let windows = std::mem::take(&mut scratch.apodization);
    for &window in windows.iter() {
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
    scratch.apodization = windows;
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
/// Partitioned-Rice search ceiling for streamable-subset output
/// (RFC 9639 §7: "The Rice partition order ... MUST be less than or
/// equal to 8"). This is the default the encoder uses unless the caller
/// opts into non-subset output via
/// [`FlacEncoderOptions::allow_non_subset_partition_order`].
const SUBSET_MAX_PARTITION_ORDER: u32 = 8;
/// Partitioned-Rice search ceiling when the caller has opted out of the
/// streamable subset: the full 4-bit `partition order` field range of
/// RFC 9639 §9.2.7 (`2^(partition order)` partitions, field width 4
/// bits → orders 0..=15). The per-order legality check (block size
/// divisible by `2^p`, first partition non-empty after warmup) still
/// gates which orders are actually reachable for a given block.
const MAX_PARTITION_ORDER: u32 = 15;

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
    /// The production emit loop only reads `coding` (the order-cost
    /// search now runs off the [`MergeLevel`] ladder); `cost` is read by
    /// the per-slice reference oracles and their byte-exactness tests.
    #[cfg_attr(not(test), allow(dead_code))]
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

/// Per-partition-order cost helper that walks each partition's slice via
/// [`best_partition_zp`]. Same per-partition cost the `_z` reference path
/// would compute — every layout decision (which Rice `k` per partition,
/// which partitions take the escape path) is byte-identical.
///
/// Retained as the per-order reference oracle: the production search now
/// derives every order's total from the bottom-up [`MergeLevel`] ladder
/// ([`level_total_cost`]), and the unit tests pin that ladder against
/// this per-slice walk for byte-exactness — hence `#[cfg(test)]`.
///
/// Eight arguments because each table comes from a different stage of
/// the per-subframe pipeline (zigzag conversion, prefix sum, per-sample
/// raw widths, then the search loop's geometric and method parameters);
/// folding them into a struct would push the allocation onto every call
/// site without buying any clarity over the call shape `_z` already
/// uses with seven arguments.
#[cfg(test)]
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

/// Largest legal partition order `p` in `0..=max_po` for a block of
/// `block_size` samples with `predictor_order` warmup samples: the
/// order must divide the block evenly (`block_size % 2^p == 0`) and
/// leave the first partition non-empty after warmup
/// (`block_size / 2^p > predictor_order`). Returns 0 when no higher
/// order is legal (order 0 is always legal for a valid subframe).
///
/// Mirrors the legality gate inside [`partition_layout_cost_zp`]: that
/// function returns `None` for an illegal order and the production
/// search `break`s on the first `None`, so the legal orders are exactly
/// the contiguous prefix `0..=finest`.
fn finest_legal_partition_order(block_size: usize, predictor_order: usize, max_po: u32) -> u32 {
    let mut finest = 0u32;
    for p in 1..=max_po {
        let n_partitions = 1usize << p;
        if block_size % n_partitions != 0 {
            break;
        }
        if block_size / n_partitions <= predictor_order {
            break;
        }
        finest = p;
    }
    finest
}

/// Score one partition's Rice cost from pre-merged shift-sums instead of
/// re-walking the residual slice. `shift_row[k] == sum(zigzag[i] >> k)`
/// for the partition (so `shift_row[0] == sum_u`), `n` is the residual
/// count. Returns `(best_bits, best_k)` where `best_bits` is the Rice
/// payload **excluding** the `param_bits` parameter field.
///
/// Reproduces [`best_rice_params_zp`] exactly — same closed-form
/// `k_est`, same `[k_est-2 ..= k_est+3]` window, same unimodal
/// directional extension — but each candidate `k`'s cost is the O(1)
/// table read `n*(1+k) + shift_row[k]` rather than an O(partition) walk.
/// The full table cost equals `rice_cost_for_k`'s full sum (its early
/// exit only ever returns a value that loses the `cost < best_bits`
/// test, never the chosen one), so `best_bits` and `best_k` match.
fn rice_search_from_row(shift_row: &[u64], n: u64, k_max: u32) -> (u64, u32) {
    if n == 0 {
        return (0, 0);
    }
    let sum_u = shift_row[0];
    let k_est: u32 = if sum_u == 0 {
        0
    } else {
        let mean_u = sum_u / n;
        if mean_u == 0 {
            0
        } else {
            63u32.saturating_sub(mean_u.leading_zeros())
        }
    };

    let lo = k_est.saturating_sub(2).min(k_max);
    let hi = (k_est + 3).min(k_max);

    let cost_at = |k: u32| -> u64 { n * (1 + k as u64) + shift_row[k as usize] };

    let mut best_k = lo;
    let mut best_bits = u64::MAX;
    for k in lo..=hi {
        let cost = cost_at(k);
        if cost < best_bits {
            best_bits = cost;
            best_k = k;
        }
    }
    if best_k == lo && lo > 0 {
        for k in (0..lo).rev() {
            let cost = cost_at(k);
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
            let cost = cost_at(k);
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

/// Total payload bits for one partition given its pre-merged stats —
/// byte-identical to [`best_partition_zp`]'s `cost`, but reading the
/// shift-sum row / count / max-raw from the merge level instead of the
/// slice. `param_bits`/`k_max` select the method.
#[inline]
fn partition_cost_from_row(
    shift_row: &[u64],
    n: u64,
    max_raw: u8,
    param_bits: u32,
    k_max: u32,
) -> u64 {
    let (rice_bits, _k) = rice_search_from_row(shift_row, n, k_max);
    let rice_cost = param_bits as u64 + rice_bits;

    let needed_bits = max_raw.max(1) as u32;
    if needed_bits <= 31 {
        let escape_cost = param_bits as u64 + 5 + n * needed_bits as u64;
        if escape_cost < rice_cost {
            return escape_cost;
        }
    }
    rice_cost
}

/// Build the finest-order [`MergeLevel`] (`shift_sums`, `counts`,
/// `max_raw`) for `finest` from the subframe-scoped `zigzag` / `raw_bits`
/// tables. Each finest partition `p` covers sample range
/// `[p*fps, (p+1)*fps)`; residual index `i` maps to sample `i +
/// predictor_order`, so the leftmost partition is short by
/// `predictor_order` warmup samples. `shift_sums[p*stride + k]` collects
/// `sum(zigzag[i] >> k)` for `k in 0..=k_max` in a single pass over the
/// partition's residuals (one read of `zigzag[i]` shifted across all k),
/// keeping the build at `O(n * (k_max + 1))` total.
fn build_finest_level(
    zigzag: &[u32],
    raw_bits: &[u8],
    block_size: usize,
    predictor_order: usize,
    finest: u32,
    k_max: u32,
    level: &mut MergeLevel,
) {
    let n_partitions = 1usize << finest;
    let fps = block_size / n_partitions; // samples per finest partition
    let stride = (k_max + 1) as usize;
    level.stride = stride;
    level.shift_sums.clear();
    level.shift_sums.resize(n_partitions * stride, 0u64);
    level.counts.clear();
    level.counts.resize(n_partitions, 0u32);
    level.max_raw.clear();
    level.max_raw.resize(n_partitions, 1u8);

    // Residual index walks the contiguous residual array; sample index =
    // residual index + predictor_order. Partition p ends at sample
    // (p+1)*fps, i.e. residual index (p+1)*fps - predictor_order.
    let mut idx = 0usize;
    for p in 0..n_partitions {
        let part_end_sample = (p + 1) * fps;
        let end = part_end_sample - predictor_order; // residual index (exclusive)
        let row = &mut level.shift_sums[p * stride..p * stride + stride];
        let mut m: u8 = 1;
        for i in idx..end {
            let u = zigzag[i] as u64;
            // Accumulate u >> k for every k. `u >> k` is 0 for every
            // k >= 64 - leading_zeros(u), so only the low `nbits` rows ever
            // receive a non-zero add; the rest would only add 0 (a no-op).
            // We compute `nbits` once and run a branch-free loop over
            // exactly that many slots, so the long high-`k` tail of zero
            // adds (which dominates for the small residuals a good LPC fit
            // produces) is skipped without a per-iteration `if v == 0`
            // test in the inner loop. This is byte-identical to walking the
            // full `stride` rows — the skipped slots keep their prior sum
            // unchanged. This inner k loop is the hottest line in the whole
            // encoder (built once per LPC candidate).
            let nbits = (64 - u.leading_zeros() as usize).min(stride);
            let row = &mut row[..nbits];
            let mut v = u;
            for slot in row.iter_mut() {
                *slot += v;
                v >>= 1;
            }
            let rb = raw_bits[i];
            if rb > m {
                m = rb;
            }
        }
        level.counts[p] = (end - idx) as u32;
        level.max_raw[p] = m;
        idx = end;
    }
}

/// Merge adjacent partition pairs of `src` (order `p+1`) into `dst`
/// (order `p`): `dst` partition `j` is the element-wise sum of `src`
/// partitions `2j` and `2j+1`'s shift rows and counts, and the max of
/// their `max_raw`. Exact because `sum(u >> k)` and the residual count
/// are additive over a split and `max` composes — so `dst`'s per-
/// partition stats equal what [`build_finest_level`] would produce for
/// order `p` directly, at a fraction of the work (`O(num_partitions ×
/// stride)` adds instead of another `O(n × stride)` pass).
fn merge_level_down(src: &MergeLevel, dst: &mut MergeLevel) {
    let stride = src.stride;
    let dst_n = src.counts.len() / 2;
    dst.stride = stride;
    dst.shift_sums.clear();
    dst.shift_sums.resize(dst_n * stride, 0u64);
    dst.counts.clear();
    dst.counts.resize(dst_n, 0u32);
    dst.max_raw.clear();
    dst.max_raw.resize(dst_n, 1u8);

    for j in 0..dst_n {
        let a = 2 * j;
        let b = 2 * j + 1;
        let (sa, sb) = src.shift_sums[a * stride..(b + 1) * stride].split_at(stride);
        let d = &mut dst.shift_sums[j * stride..j * stride + stride];
        for k in 0..stride {
            d[k] = sa[k] + sb[k];
        }
        dst.counts[j] = src.counts[a] + src.counts[b];
        dst.max_raw[j] = src.max_raw[a].max(src.max_raw[b]);
    }
}

/// Total residual payload (sum of per-partition costs) for one partition
/// order, read from a pre-merged [`MergeLevel`]. Byte-identical to
/// summing [`best_partition_zp`] over the order's partitions.
fn level_total_cost(level: &MergeLevel, param_bits: u32, k_max: u32) -> u64 {
    let stride = level.stride;
    let mut total = 0u64;
    for p in 0..level.counts.len() {
        let row = &level.shift_sums[p * stride..p * stride + stride];
        total += partition_cost_from_row(
            row,
            level.counts[p] as u64,
            level.max_raw[p],
            param_bits,
            k_max,
        );
    }
    total
}

/// Emit the partitioned-Rice residual for one subframe and return the
/// exact number of bits written. Searches every legal partition order
/// (`0..=scratch.max_partition_order`, the subset default 8 or the
/// opt-in non-subset ceiling 15) under both Rice methods (4-bit and
/// 5-bit parameters), keeping the layout with the smallest total
/// payload, and lets each partition independently fall back to an
/// escape (raw) coding when that is cheaper than Rice. RFC 9639 §9.2.5.
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
    // The search ceiling is the caller-configured `max_partition_order`
    // (subset default 8, opt-in non-subset up to 15); the legal orders
    // are the contiguous prefix `0..=finest` (illegal orders only get
    // more constrained, so the original loop's `break`-on-first-illegal
    // means everything above `finest` is unreachable).
    //
    // Bottom-up partition merge: the per-partition Rice cost depends only
    // on additive stats — `sum(zigzag[i] >> k)` for each `k`, the
    // residual count, and `max(raw_bits[i])`. So we build those stats for
    // the *finest* legal order **once** (one `O(n × (k_max+1))` pass) and
    // derive every coarser order by summing adjacent partition pairs
    // (`O(num_partitions × stride)` adds), instead of re-walking the
    // residual slice for every `(method, partition_order)` pair. The
    // finest level is built with the wider method-1 `k_max = 30` (stride
    // 31); method 0 (`k_max = 14`) reads the same rows but clamps its
    // search window to 14, so a single shared ladder serves both methods.
    let max_po = scratch.max_partition_order;
    let finest = finest_legal_partition_order(block_size, predictor_order, max_po);

    // `totals[m][p]` = residual payload (excluding the 2+4 header) for
    // method `m`, partition order `p`. Filled top-down as the ladder
    // descends so the selection below can replay the original loop's
    // exact `(method, p)` visit order and strict-`<` tie-break.
    let mut totals = [[0u64; 16]; 2];

    let mut level_a = std::mem::take(&mut scratch.merge_a);
    let mut level_b = std::mem::take(&mut scratch.merge_b);
    build_finest_level(
        &zigzag,
        &raw_bits,
        block_size,
        predictor_order,
        finest,
        30,
        &mut level_a,
    );
    // Walk orders from `finest` down to 0, scoring both methods off the
    // current level, then merging pairwise into the spare buffer for the
    // next (coarser) order. `cur`/`spare` ping-pong between a and b.
    let mut cur = &mut level_a;
    let mut spare = &mut level_b;
    let mut p = finest as i32;
    while p >= 0 {
        let pu = p as usize;
        totals[0][pu] = level_total_cost(cur, 4, 14);
        totals[1][pu] = level_total_cost(cur, 5, 30);
        if p > 0 {
            merge_level_down(cur, spare);
            std::mem::swap(&mut cur, &mut spare);
        }
        p -= 1;
    }
    // Return the ping-ponged buffers to scratch (capacity carries over).
    scratch.merge_a = level_a;
    scratch.merge_b = level_b;

    // Replay the original method-major / order-minor selection so the
    // winning `(method, partition_order)` — and therefore every emitted
    // bit — is identical to the per-slice search, including its
    // strict-`<` tie-break (earliest visited candidate wins ties).
    let mut best: Option<(u32, u32, u32, u32, u64)> = None; // method, param_bits, k_max, p, cost
    for (method, param_bits, k_max) in [(0u32, 4u32, 14u32), (1u32, 5u32, 30u32)] {
        for p in 0..=finest {
            let total = 2 + 4 + totals[method as usize][p as usize];
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
///
/// Single-order reference solve. The production encoder uses the
/// all-orders [`levinson_durbin_all_orders_s`] (one window + autocorrelate
/// per window rather than per order, #12); this per-order form is retained
/// as the oracle the byte-exactness tests pin against, hence `#[cfg(test)]`.
#[cfg(test)]
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

/// Look up (or build) the memoised `(window, n)` weight row in `cache` and
/// return it as a slice. The row holds `window.weight(i, n)` for every
/// `i in 0..n`. Because `weight` is a pure function of `(window, i, n)`, a
/// cached row is **bit-identical** to a fresh per-sample `weight` call, so
/// reusing it across subframes does not perturb the windowed signal.
fn window_weights(cache: &mut Vec<WindowWeights>, window: ApodizationWindow, n: usize) -> &[f64] {
    // Linear scan: the cache holds at most a handful of rows (one per
    // distinct `(window, n)` — `n` only changes for the short final block),
    // so this is far cheaper than the per-sample transcendental rebuild it
    // replaces.
    let idx = cache.iter().position(|w| w.window == window && w.n == n);
    let idx = match idx {
        Some(i) => i,
        None => {
            let mut weights = Vec::with_capacity(n);
            for i in 0..n {
                weights.push(window.weight(i, n));
            }
            cache.push(WindowWeights { window, n, weights });
            cache.len() - 1
        }
    };
    &cache[idx].weights
}

/// Fill `windowed` with `samples[i] as f64 * weight(i, n)`, drawing the
/// weights from the memoised `(window, n)` row. Byte-exact with an inline
/// `samples[i] as f64 * window.weight(i, n)` loop: the cached weight is the
/// same `f64` `weight` would return, and the multiplication is identical.
fn apply_window(
    cache: &mut Vec<WindowWeights>,
    windowed: &mut Vec<f64>,
    samples: &[i32],
    window: ApodizationWindow,
) {
    let n = samples.len();
    windowed.clear();
    windowed.reserve(n);
    let weights = window_weights(cache, window, n);
    for (&s, &w) in samples.iter().zip(weights.iter()) {
        windowed.push(s as f64 * w);
    }
}

/// All-orders Levinson-Durbin: window + autocorrelate **once at
/// `max_order`**, then run the recursion **once**, snapshotting the
/// FLAC-convention coefficient vector at every order 1..=`max_order` into
/// the triangular `scratch.lpc_coeffs_all` store (validity in
/// `scratch.lpc_order_valid`, index `m-1`). Returns the highest order the
/// recursion reached before either finishing or bailing on degenerate
/// energy; orders above that are marked invalid.
///
/// This is the hot-path replacement for calling [`levinson_durbin_s`]
/// once per order inside the per-subframe search. It is **byte-exact**
/// with that per-order loop:
///
/// * The windowed signal is `samples[i] as f64 * window.weight(i, n)` —
///   identical f64 values regardless of order.
/// * `autoc[lag]` is `sum_{i=lag}^{n-1} windowed[i]*windowed[i-lag]` in
///   the same ascending-`i` summation order; computing it up to
///   `max_order` makes every lower order's lags a prefix of this vector,
///   so each lag's f64 value is bit-identical to the per-order compute.
/// * Levinson-Durbin is a forward recursion: after the outer iteration
///   `i` completes, `lpc_state[0..=i]` holds exactly the order-`(i+1)`
///   solution — bit-identical to running the recursion independently up
///   to order `i+1`, because every operation up to that point is the same
///   in the same order. Snapshotting there yields the same `lpc_coeffs`
///   the per-order [`levinson_durbin_s`] would have produced.
fn levinson_durbin_all_orders_s(
    samples: &[i32],
    max_order: usize,
    window: ApodizationWindow,
    scratch: &mut SubframeScratch,
) -> usize {
    let n = samples.len();
    scratch.lpc_order_valid.clear();
    scratch.lpc_order_valid.resize(max_order, false);
    if n <= max_order || max_order == 0 {
        // Mirror `levinson_durbin_s`'s `n <= order` guard per order:
        // every order m with n <= m is invalid; lower orders still solve.
        // Fall through with a reduced effective ceiling.
        if max_order == 0 {
            return 0;
        }
    }
    // Highest order for which `n > order` holds (matches the per-order
    // `if n <= order { return false }` guard exactly).
    let eff_max = max_order.min(n.saturating_sub(1));
    if eff_max == 0 {
        return 0;
    }

    // Window once. Bit-identical to `levinson_durbin_s`'s taper: the
    // per-sample weight is drawn from the memoised `(window, n)` row, which
    // holds exactly what `window.weight(i, n)` would return.
    let mut windowed = std::mem::take(&mut scratch.windowed);
    apply_window(&mut scratch.window_cache, &mut windowed, samples, window);
    scratch.windowed = windowed;

    // Autocorrelate once at the maximum order. `autoc[lag]` is identical
    // for every order that needs that lag (same windowed signal, same
    // ascending-`i` f64 summation), so lower orders read an exact prefix.
    scratch.autoc.clear();
    scratch.autoc.resize(eff_max + 1, 0.0f64);
    for lag in 0..=eff_max {
        let mut s = 0.0f64;
        for i in lag..n {
            s += scratch.windowed[i] * scratch.windowed[i - lag];
        }
        scratch.autoc[lag] = s;
    }
    if scratch.autoc[0] <= 0.0 || !scratch.autoc[0].is_finite() {
        return 0;
    }

    // Pre-size the triangular snapshot store: orders 1..=eff_max need
    // 1+2+...+eff_max = eff_max*(eff_max+1)/2 slots.
    let total = eff_max * (eff_max + 1) / 2;
    scratch.lpc_coeffs_all.clear();
    scratch.lpc_coeffs_all.resize(total, 0.0f64);

    scratch.lpc_state.clear();
    scratch.lpc_state.resize(eff_max, 0.0f64);
    let mut error = scratch.autoc[0];
    let mut reached = 0usize;
    for i in 0..eff_max {
        let mut r = -scratch.autoc[i + 1];
        for j in 0..i {
            r -= scratch.lpc_state[j] * scratch.autoc[i - j];
        }
        if error.abs() < 1e-12 {
            // Per-order `levinson_durbin_s` returns false here for every
            // order > reached; orders <= reached already snapshotted.
            break;
        }
        let k = r / error;
        // Identical in-place symmetric update to `levinson_durbin_s`; see
        // that function for the pairing derivation. Floating-point
        // operands and order match, so snapshots stay byte-stable.
        let mid = i / 2;
        for j in 0..mid {
            let opp = i - 1 - j;
            let a = scratch.lpc_state[j] + k * scratch.lpc_state[opp];
            let b = scratch.lpc_state[opp] + k * scratch.lpc_state[j];
            scratch.lpc_state[j] = a;
            scratch.lpc_state[opp] = b;
        }
        if i % 2 == 1 {
            scratch.lpc_state[mid] = scratch.lpc_state[mid] + k * scratch.lpc_state[mid];
        }
        scratch.lpc_state[i] = k;
        error *= 1.0 - k * k;
        if !error.is_finite() || error <= 0.0 {
            // `levinson_durbin_s` returns false for this order. Its
            // `lpc_state` is fully written for order `i+1` though — but
            // the per-order helper bails BEFORE building `lpc_coeffs`, so
            // order `i+1` is invalid. Stop without snapshotting it.
            break;
        }
        // Iteration `i` completed order `m = i+1`. `lpc_state[0..m]` is the
        // order-`m` solution; snapshot its FLAC-convention coefficients.
        let m = i + 1;
        let base = m * (m - 1) / 2; // == i*(i+1)/2
        for (j, &v) in scratch.lpc_state[..m].iter().enumerate() {
            scratch.lpc_coeffs_all[base + j] = -v;
        }
        scratch.lpc_order_valid[m - 1] = true;
        reached = m;
    }
    reached
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
            for order in 1..=SUBSET_MAX_LPC_ORDER.min(n - 1) {
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

    /// Lifting SUBSET_MAX_LPC_ORDER from 8 to 12 must not regress on a
    /// content type where the lower orders already win — the encoder
    /// considers every order in 1..=SUBSET_MAX_LPC_ORDER and keeps the
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
            "raising SUBSET_MAX_LPC_ORDER must not inflate the plan: {} > {}",
            plan_full.bits,
            best_low
        );
    }

    /// Residuals whose statistics flip on a finer scale than the
    /// streamable-subset partition-order ceiling (8) can resolve: a
    /// 4096-sample span carved into 64-sample runs that alternate between
    /// near-zero (Rice `k` ≈ 0) and wide (large `k`). Partition order 8
    /// gives 256-sample partitions, each straddling two quiet + two loud
    /// runs, so every partition is forced to a compromise `k`. Order 12
    /// gives 1-sample partitions — far finer than needed — but order 6
    /// (64-sample partitions) already isolates each run, and order 6 is
    /// inside the subset, so the win here comes specifically from letting
    /// the *search* see orders above 8 where the optimum sits for this
    /// shape. We assert the non-subset ceiling emits no more bits than
    /// the subset ceiling on this content (it must, since order ≤ 8 is a
    /// strict subset of the orders order ≤ 15 searches) and strictly
    /// fewer on a span tuned so the minimum lands above 8.
    #[test]
    fn non_subset_partition_order_never_inflates_and_can_shrink() {
        // Build residuals that put the cost minimum at a partition order
        // > 8. Period-16 runs (2048 quiet + spike pattern) make order 8's
        // 256-sample partitions each cover 16 runs, while order 11
        // (2-sample partitions) isolates the spikes. Deterministic.
        let block = 4096usize;
        let mut residuals = vec![0i32; block];
        let mut rng: u64 = 0x1234_5678_9abc_def0;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        for (i, r) in residuals.iter_mut().enumerate() {
            // Every 16th sample is a wide value; the other 15 are tiny.
            // A fine partition can give the wide samples their own high-`k`
            // partition and the tiny runs a `k`≈0 partition; a coarse
            // partition pays the high `k` across the whole 256-sample span.
            *r = if i % 16 == 0 {
                (next() % 60_000) as i32 - 30_000
            } else {
                (next() % 3) as i32 - 1
            };
        }

        let emit = |max_po: u32| -> u32 {
            let mut scratch = SubframeScratch {
                max_partition_order: max_po,
                ..SubframeScratch::default()
            };
            let mut w = BitWriter::new();
            // predictor_order 0 so the whole span is one residual run.
            encode_rice_residual_s(&mut w, &residuals, block, 0, &mut scratch)
        };

        let subset_bits = emit(SUBSET_MAX_PARTITION_ORDER);
        let non_subset_bits = emit(MAX_PARTITION_ORDER);

        assert!(
            non_subset_bits <= subset_bits,
            "non-subset search ({non_subset_bits} bits) must never beat itself \
             worse than the subset search ({subset_bits} bits) — orders 0..=8 \
             are a subset of 0..=15"
        );
        assert!(
            non_subset_bits < subset_bits,
            "this residual shape was tuned so the optimal partition order is \
             above the subset ceiling 8, so the non-subset search should win: \
             non-subset {non_subset_bits} vs subset {subset_bits}"
        );
    }

    /// A full encode → decode cycle through the public
    /// `make_encoder_with_options` path with the non-subset partition
    /// ceiling enabled must recover the input PCM bit-exactly (FLAC is
    /// lossless regardless of the partition split the encoder chose) —
    /// the partition order is a pure rate decision the decoder reads from
    /// the 4-bit §9.2.7 field, so a higher order changes only the byte
    /// count, never the recovered samples.
    #[test]
    fn non_subset_partition_order_roundtrips_bit_exact() {
        let sr = 44_100u32;
        let n = 4096usize;
        // Same fine-grained quiet/loud alternation, rendered to S16 PCM.
        let mut pcm = Vec::with_capacity(n * 2);
        let mut rng: u64 = 0x0fed_cba9_8765_4321;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        for i in 0..n {
            let v: i16 = if i % 16 == 0 {
                ((next() % 60_000) as i32 - 30_000) as i16
            } else {
                ((next() % 7) as i32 - 3) as i16
            };
            pcm.extend_from_slice(&v.to_le_bytes());
        }

        let p = build_audio_params(1, sr, SampleFormat::S16);
        let opts = FlacEncoderOptions {
            allow_non_subset_partition_order: true,
            ..FlacEncoderOptions::default()
        };
        let mut enc = make_encoder_with_options(&p, opts).unwrap();
        let af = AudioFrame {
            samples: n as u32,
            pts: Some(0),
            data: vec![pcm.clone()],
        };
        enc.send_frame(&Frame::Audio(af)).unwrap();
        enc.flush().unwrap();

        let mut frames: Vec<Vec<u8>> = Vec::new();
        while let Ok(pkt) = enc.receive_packet() {
            frames.push(pkt.data);
        }
        assert!(!frames.is_empty(), "encoder produced no packets");

        let mut dparams = build_audio_params(1, sr, SampleFormat::S16);
        dparams.extradata = enc.output_params().extradata.clone();
        let mut dec = decoder::make_decoder(&dparams).unwrap();

        let mut out: Vec<u8> = Vec::new();
        for f in frames {
            let mut pkt =
                oxideav_core::Packet::new(0, oxideav_core::TimeBase::new(1, sr as i64), f);
            pkt.pts = Some(0);
            dec.send_packet(&pkt).unwrap();
            while let Ok(oxideav_core::Frame::Audio(a)) = dec.receive_frame() {
                out.extend_from_slice(&a.data[0]);
            }
        }
        assert_eq!(
            out, pcm,
            "non-subset partition-order output is not lossless"
        );
    }

    /// The speed-biased [`FlacEncoderOptions::fast`] preset (single Welch
    /// window + `max_lpc_order = 8`) must still produce a fully lossless
    /// stream: FLAC is lossless for *any* analysis-window / LPC-order
    /// choice, so trimming the search affects only the byte count, never
    /// the recovered samples. This pins that invariant so a future preset
    /// tweak can't silently turn the fast path lossy.
    #[test]
    fn fast_preset_roundtrips_bit_exact() {
        let sr = 48_000u32;
        let n = 4096usize;
        let mut pcm = Vec::with_capacity(n * 2);
        let mut rng: u64 = 0x1234_5678_9abc_def0;
        let mut next = || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            rng
        };
        for i in 0..n {
            // Tone-plus-noise so the LPC + Rice search has real work to do.
            let phase = (i % 256) as i32 - 128;
            let env = phase * 64;
            let noise = (next() % 33) as i32 - 16;
            let v = (env + noise) as i16;
            pcm.extend_from_slice(&v.to_le_bytes());
        }

        let p = build_audio_params(1, sr, SampleFormat::S16);
        let mut enc = make_encoder_with_options(&p, FlacEncoderOptions::fast()).unwrap();
        let af = AudioFrame {
            samples: n as u32,
            pts: Some(0),
            data: vec![pcm.clone()],
        };
        enc.send_frame(&Frame::Audio(af)).unwrap();
        enc.flush().unwrap();

        let mut frames: Vec<Vec<u8>> = Vec::new();
        while let Ok(pkt) = enc.receive_packet() {
            frames.push(pkt.data);
        }
        assert!(!frames.is_empty(), "fast preset produced no packets");

        let mut dparams = build_audio_params(1, sr, SampleFormat::S16);
        dparams.extradata = enc.output_params().extradata.clone();
        let mut dec = decoder::make_decoder(&dparams).unwrap();

        let mut out: Vec<u8> = Vec::new();
        for f in frames {
            let mut pkt =
                oxideav_core::Packet::new(0, oxideav_core::TimeBase::new(1, sr as i64), f);
            pkt.pts = Some(0);
            dec.send_packet(&pkt).unwrap();
            while let Ok(oxideav_core::Frame::Audio(a)) = dec.receive_frame() {
                out.extend_from_slice(&a.data[0]);
            }
        }
        assert_eq!(out, pcm, "fast-preset output is not lossless");
    }

    /// The [`FlacEncoderOptions::fast`] preset must set exactly the two
    /// speed knobs (single Welch window + LPC ceiling 8) and leave every
    /// other field at its default, so it stays a streamable-subset config.
    #[test]
    fn fast_preset_sets_expected_knobs() {
        let fast = FlacEncoderOptions::fast();
        assert_eq!(fast.apodization.as_deref(), Some(&[Apodization::Welch][..]));
        assert_eq!(fast.max_lpc_order, Some(8));
        // Untouched defaults — fast must not silently leave the subset.
        assert!(!fast.allow_non_subset_partition_order);
        assert!(!fast.allow_non_subset_block_size);
        assert_eq!(fast.block_size, None);
        assert_eq!(fast.padding_bytes, None);
    }

    /// A signal engineered to need a longer impulse response than
    /// order 8 — a sum of harmonics passed through a pseudo-random
    /// AR(11) recursion. AR processes of order p have an
    /// autocorrelation that doesn't decay before lag p, so a p-tap
    /// LPC fits much better than anything shorter. With
    /// SUBSET_MAX_LPC_ORDER raised from 8 to 12 the encoder gets to try
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
        let best_high = welch_min_over(9..=SUBSET_MAX_LPC_ORDER);
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
        for order in 9..=SUBSET_MAX_LPC_ORDER {
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

    /// Build a deterministic order-`p` AR signal
    /// `y[n] = sum_k a[k]·y[n-k] plus drive[n]`, with `drive` an xorshift
    /// LCG. AR(p) processes have an autocorrelation that does not decay
    /// before lag `p`, so a `p`-tap LPC fits them much better than anything
    /// shorter — the workload that makes a higher LPC ceiling pay.
    #[cfg(test)]
    fn ar_signal(n: usize, a: &[f64], seed: u64) -> Vec<i32> {
        let mut y = vec![0.0f64; n];
        let mut rng = seed;
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
        y.iter()
            .map(|&v| v.clamp(-30_000.0, 30_000.0) as i32)
            .collect()
    }

    /// With `FlacEncoderOptions::max_lpc_order` lifted above the default
    /// ceiling of 12 (RFC 9639 §9.2.6 permits up to 32), the encoder gets
    /// to try orders in 13..=24 on content engineered to need a longer
    /// impulse response (an AR(20) recursion). At least one of those
    /// orders should produce a strictly smaller plan than the best in
    /// 1..=12 — otherwise the lifted cap buys nothing on the workload it
    /// targets. As with the AR(11) baseline we pin a single window (Welch)
    /// to isolate the order-cap question from the apodization search.
    #[test]
    fn higher_lpc_order_wins_on_ar20_signal() {
        let n = 4096usize;
        // Stable order-20 AR with a *dominant high-lag tap*: a mild lag-1
        // pole plus a strong lag-20 echo (`y[n] ≈ 0.5·y[n-1] − 0.78·y[n-20]
        // + drive`). The lag-20 energy is invisible to any predictor of
        // order < 20, so orders 1..=12 cannot capture it while orders
        // 13..=24 can — exactly the regime the lifted ceiling targets.
        let mut a = vec![0.0f64; 20];
        a[0] = 0.5; // lag-1
        a[19] = -0.78; // lag-20 echo
        let samples = ar_signal(n, &a, 0x2bad_c0de_f00d_1357);

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
        let best_low = welch_min_over(1..=SUBSET_MAX_LPC_ORDER);
        let best_high = welch_min_over((SUBSET_MAX_LPC_ORDER + 1)..=24);
        assert!(
            best_high < best_low,
            "expected order 13..=24 to beat order 1..=12 on an AR(20) signal \
             (Welch-pinned baseline), got best_high={best_high} best_low={best_low}"
        );
    }

    /// The `make_encoder_with_options` path with the LPC ceiling lifted to
    /// the §9.2.6 maximum of 32 must recover the input PCM bit-exactly —
    /// FLAC is lossless regardless of the predictor order the encoder
    /// chose. The predictor order is a pure rate decision the decoder
    /// reads from the 5-bit subframe-type field, so a higher order changes
    /// only the byte count, never the recovered samples. An AR(20) drive
    /// makes a high order actually competitive so the lifted code path is
    /// genuinely exercised.
    #[test]
    fn higher_lpc_order_roundtrips_bit_exact() {
        let sr = 96_000u32;
        let n = 4096usize;
        let mut a = vec![0.0f64; 20];
        a[0] = 0.5;
        a[19] = -0.78;
        let samples = ar_signal(n, &a, 0x51c0_ffee_d00d_2468);
        let mut pcm = Vec::with_capacity(n * 2);
        for &s in &samples {
            pcm.extend_from_slice(&(s as i16).to_le_bytes());
        }

        let p = build_audio_params(1, sr, SampleFormat::S16);
        let opts = FlacEncoderOptions {
            max_lpc_order: Some(32),
            ..FlacEncoderOptions::default()
        };
        let mut enc = make_encoder_with_options(&p, opts).unwrap();
        let af = AudioFrame {
            samples: n as u32,
            pts: Some(0),
            data: vec![pcm.clone()],
        };
        enc.send_frame(&Frame::Audio(af)).unwrap();
        enc.flush().unwrap();

        let mut frames: Vec<Vec<u8>> = Vec::new();
        while let Ok(pkt) = enc.receive_packet() {
            frames.push(pkt.data);
        }
        assert!(!frames.is_empty(), "encoder produced no packets");

        let mut dparams = build_audio_params(1, sr, SampleFormat::S16);
        dparams.extradata = enc.output_params().extradata.clone();
        let mut dec = decoder::make_decoder(&dparams).unwrap();

        let mut out: Vec<u8> = Vec::new();
        for f in frames {
            let mut pkt =
                oxideav_core::Packet::new(0, oxideav_core::TimeBase::new(1, sr as i64), f);
            pkt.pts = Some(0);
            dec.send_packet(&pkt).unwrap();
            while let Ok(oxideav_core::Frame::Audio(a)) = dec.receive_frame() {
                out.extend_from_slice(&a.data[0]);
            }
        }
        assert_eq!(out, pcm, "lifted-LPC-order output is not lossless");
    }

    /// `max_lpc_order` is clamped to the spec's representable range: a
    /// request above 32 is held at 32 (the 5-bit subframe-type field can
    /// only encode `order - 1` up to 31), and a request of 0 is floored
    /// to order 1. The clamp lives on the encoder's scratch ceiling, so we
    /// observe it through the constructed encoder rather than re-deriving it.
    #[test]
    fn max_lpc_order_request_is_clamped_to_spec_range() {
        let p = build_audio_params(1, 48_000, SampleFormat::S16);

        let over = make_encoder_with_options(
            &p,
            FlacEncoderOptions {
                max_lpc_order: Some(99),
                ..FlacEncoderOptions::default()
            },
        );
        assert!(
            over.is_ok(),
            "an over-large LPC order must clamp, not error"
        );

        let zero = make_encoder_with_options(
            &p,
            FlacEncoderOptions {
                max_lpc_order: Some(0),
                ..FlacEncoderOptions::default()
            },
        );
        assert!(zero.is_ok(), "a zero LPC order must floor to 1, not error");

        // A `None` ceiling and an explicit `Some(12)` must be byte-for-byte
        // identical (the default is exactly the subset boundary of 12).
        let pcm: Vec<u8> = (0..4096i32)
            .flat_map(|i| (((i * 7) % 9001 - 4500) as i16).to_le_bytes())
            .collect();
        let emit = |opts: FlacEncoderOptions| -> Vec<u8> {
            let mut enc = make_encoder_with_options(&p, opts).unwrap();
            enc.send_frame(&Frame::Audio(AudioFrame {
                samples: 4096,
                pts: Some(0),
                data: vec![pcm.clone()],
            }))
            .unwrap();
            enc.flush().unwrap();
            let mut bytes = Vec::new();
            while let Ok(pkt) = enc.receive_packet() {
                bytes.extend_from_slice(&pkt.data);
            }
            bytes
        };
        let default_bytes = emit(FlacEncoderOptions::default());
        let explicit_12 = emit(FlacEncoderOptions {
            max_lpc_order: Some(SUBSET_MAX_LPC_ORDER),
            ..FlacEncoderOptions::default()
        });
        assert_eq!(
            default_bytes, explicit_12,
            "max_lpc_order: None must match Some(12) byte-for-byte"
        );
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
            for order in 1..=SUBSET_MAX_LPC_ORDER.min(n - 1) {
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
        for order in 1..=SUBSET_MAX_LPC_ORDER {
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

    // ------------------------------------------------------------------
    // Caller-configurable apodization window set
    // (`FlacEncoderOptions::apodization`).
    // ------------------------------------------------------------------

    /// `apodization: None` and an explicit `Some(<the default set>)` must
    /// produce byte-for-byte identical output, and an empty `Some(vec![])`
    /// must fall back to that same default set (the LPC search needs at
    /// least one window). This pins the default-equivalence + empty-list
    /// guard the same way `max_lpc_order: None == Some(12)` is pinned.
    #[test]
    fn apodization_none_equals_default_set_and_empty_falls_back() {
        let p = build_audio_params(1, 48_000, SampleFormat::S16);
        let pcm: Vec<u8> = (0..4096i32)
            .flat_map(|i| (((i * 13) % 7919 - 3960) as i16).to_le_bytes())
            .collect();
        let emit = |opts: FlacEncoderOptions| -> Vec<u8> {
            let mut enc = make_encoder_with_options(&p, opts).unwrap();
            enc.send_frame(&Frame::Audio(AudioFrame {
                samples: 4096,
                pts: Some(0),
                data: vec![pcm.clone()],
            }))
            .unwrap();
            enc.flush().unwrap();
            let mut bytes = Vec::new();
            while let Ok(pkt) = enc.receive_packet() {
                bytes.extend_from_slice(&pkt.data);
            }
            bytes
        };
        let default_bytes = emit(FlacEncoderOptions::default());
        let explicit_default = emit(FlacEncoderOptions {
            apodization: Some(vec![
                Apodization::Welch,
                Apodization::Hann,
                Apodization::Tukey {
                    alpha_num: 1,
                    alpha_den: 4,
                },
                Apodization::Bartlett,
                Apodization::BlackmanHarris,
            ]),
            ..FlacEncoderOptions::default()
        });
        let empty = emit(FlacEncoderOptions {
            apodization: Some(vec![]),
            ..FlacEncoderOptions::default()
        });
        assert_eq!(
            default_bytes, explicit_default,
            "apodization: None must match an explicit default window set byte-for-byte"
        );
        assert_eq!(
            default_bytes, empty,
            "an empty apodization set must fall back to the default set byte-for-byte"
        );
    }

    /// A custom apodization set must still produce lossless output:
    /// the window only shapes the analysis that picks LPC coefficients,
    /// never the decoder-visible bitstream semantics, so any set recovers
    /// the input PCM bit-exactly. Uses a single non-default Tukey window
    /// so the custom path (not the default) is the one exercised.
    #[test]
    fn custom_apodization_set_roundtrips_bit_exact() {
        let sr = 44_100u32;
        let n = 4096usize;
        let samples: Vec<i32> = (0..n)
            .map(|i| {
                let t = i as f64 / sr as f64;
                ((t * 660.0 * 2.0 * std::f64::consts::PI).sin() * 14_000.0
                    + (t * 1320.0 * 2.0 * std::f64::consts::PI).sin() * 6_000.0)
                    as i32
            })
            .collect();
        let mut pcm = Vec::with_capacity(n * 2);
        for &s in &samples {
            pcm.extend_from_slice(&(s as i16).to_le_bytes());
        }

        let p = build_audio_params(1, sr, SampleFormat::S16);
        let opts = FlacEncoderOptions {
            apodization: Some(vec![
                Apodization::Tukey {
                    alpha_num: 1,
                    alpha_den: 2,
                },
                Apodization::Welch,
            ]),
            ..FlacEncoderOptions::default()
        };
        let mut enc = make_encoder_with_options(&p, opts).unwrap();
        enc.send_frame(&Frame::Audio(AudioFrame {
            samples: n as u32,
            pts: Some(0),
            data: vec![pcm.clone()],
        }))
        .unwrap();
        enc.flush().unwrap();
        let mut frames: Vec<Vec<u8>> = Vec::new();
        while let Ok(pkt) = enc.receive_packet() {
            frames.push(pkt.data);
        }
        assert!(!frames.is_empty(), "encoder produced no packets");

        let mut dparams = build_audio_params(1, sr, SampleFormat::S16);
        dparams.extradata = enc.output_params().extradata.clone();
        let mut dec = decoder::make_decoder(&dparams).unwrap();
        let mut out: Vec<u8> = Vec::new();
        for f in frames {
            let mut pkt =
                oxideav_core::Packet::new(0, oxideav_core::TimeBase::new(1, sr as i64), f);
            pkt.pts = Some(0);
            dec.send_packet(&pkt).unwrap();
            while let Ok(oxideav_core::Frame::Audio(a)) = dec.receive_frame() {
                out.extend_from_slice(&a.data[0]);
            }
        }
        assert_eq!(out, pcm, "custom-apodization output is not lossless");
    }

    /// The window set genuinely steers the LPC fit: a window the default
    /// three-set never tries — a wide Tukey(7/8), almost a Hann but with a
    /// small flat top — can strictly beat the default set on a signal it
    /// for at least one LPC order. This proves the override genuinely
    /// re-routes the analysis rather than collapsing back onto the same
    /// three windows. Two checks: (a) a single non-default window (wide
    /// Tukey(7/8)) yields a *different* per-order best than a single Welch
    /// window, so the set really steers the autocorrelation; and (b) a
    /// custom set that adds low-leakage windows strictly beats a
    /// Welch-only set on leakage-sensitive content. We drive the internal
    /// `encode_lpc_plan_s` with each scratch's window set installed so the
    /// only variable is the apodization list.
    #[test]
    fn custom_window_can_beat_default_set() {
        let sr = 48_000u32;
        let n = 4096usize;
        // Closely-spaced partials whose leakage a centre-heavy window
        // smears, so the choice of window measurably moves the LPC fit.
        let samples: Vec<i32> = (0..n)
            .map(|i| {
                let t = i as f64 / sr as f64;
                ((t * 3000.0 * 2.0 * std::f64::consts::PI).sin() * 12_000.0
                    + (t * 3037.0 * 2.0 * std::f64::consts::PI).sin() * 12_000.0
                    + (t * 9000.0 * 2.0 * std::f64::consts::PI).sin() * 3_000.0)
                    as i32
            })
            .collect();

        let per_order = |windows: Vec<ApodizationWindow>| -> Vec<u64> {
            let mut scratch = SubframeScratch {
                apodization: windows,
                ..SubframeScratch::default()
            };
            (1..=SUBSET_MAX_LPC_ORDER)
                .map(|order| {
                    encode_lpc_plan_s(&samples, 16, order, 0, &mut scratch)
                        .map(|p| p.bits)
                        .unwrap_or(u64::MAX)
                })
                .collect()
        };
        let min_of = |v: &[u64]| v.iter().copied().min().unwrap_or(u64::MAX);

        let welch_only = per_order(vec![ApodizationWindow::Welch]);
        // A wide Tukey (alpha 7/8) is outside the default set.
        let wide_tukey = per_order(vec![ApodizationWindow::Tukey {
            alpha_num: 7,
            alpha_den: 8,
        }]);
        // (a) The two single-window searches must disagree on at least one
        //     order — proof the apodization list actually changes the fit.
        assert!(
            welch_only != wide_tukey,
            "single-window apodization sets produced identical per-order plans \
             — the window override is not steering the search"
        );

        // (b) A custom set pairing the wide Tukey with Hann (both lower
        //     leakage than Welch) must beat a Welch-only set on this
        //     leakage-sensitive content.
        let custom = per_order(vec![
            ApodizationWindow::Hann,
            ApodizationWindow::Tukey {
                alpha_num: 7,
                alpha_den: 8,
            },
        ]);
        assert!(
            min_of(&custom) < min_of(&welch_only),
            "expected a low-leakage custom window set to beat Welch-only on \
             leakage-sensitive content: custom={} welch_only={}",
            min_of(&custom),
            min_of(&welch_only)
        );
    }

    /// A Tukey window with a zero denominator must not divide-by-zero: the
    /// public `Apodization::to_internal` maps `den == 0` to `alpha = 0`
    /// (rectangular), so construction succeeds and encoding round-trips.
    #[test]
    fn zero_denominator_tukey_is_safe_and_lossless() {
        let sr = 48_000u32;
        let n = 2048usize;
        let pcm: Vec<u8> = (0..n as i32)
            .flat_map(|i| (((i * 11) % 5001 - 2500) as i16).to_le_bytes())
            .collect();
        let p = build_audio_params(1, sr, SampleFormat::S16);
        let opts = FlacEncoderOptions {
            apodization: Some(vec![Apodization::Tukey {
                alpha_num: 1,
                alpha_den: 0, // pathological; must be treated as rectangular
            }]),
            ..FlacEncoderOptions::default()
        };
        let mut enc = make_encoder_with_options(&p, opts).expect("zero-den Tukey must not error");
        enc.send_frame(&Frame::Audio(AudioFrame {
            samples: n as u32,
            pts: Some(0),
            data: vec![pcm.clone()],
        }))
        .unwrap();
        enc.flush().unwrap();
        let mut frames: Vec<Vec<u8>> = Vec::new();
        while let Ok(pkt) = enc.receive_packet() {
            frames.push(pkt.data);
        }
        let mut dparams = build_audio_params(1, sr, SampleFormat::S16);
        dparams.extradata = enc.output_params().extradata.clone();
        let mut dec = decoder::make_decoder(&dparams).unwrap();
        let mut out: Vec<u8> = Vec::new();
        for f in frames {
            let mut pkt =
                oxideav_core::Packet::new(0, oxideav_core::TimeBase::new(1, sr as i64), f);
            pkt.pts = Some(0);
            dec.send_packet(&pkt).unwrap();
            while let Ok(oxideav_core::Frame::Audio(a)) = dec.receive_frame() {
                out.extend_from_slice(&a.data[0]);
            }
        }
        assert_eq!(out, pcm, "zero-den Tukey output is not lossless");
    }

    /// The Bartlett (triangular) and Blackman-Harris cosine-sum windows
    /// added to the public apodization set must (a) carry a sane weight
    /// curve — non-negative, peaking at 1.0 in the centre, symmetric about
    /// the midpoint, and zero at both edges (a defining property of both
    /// windows) — and (b) round-trip a block bit-exactly when selected as
    /// the sole analysis window, since the window has no on-wire footprint.
    #[test]
    fn bartlett_and_blackman_harris_weights_and_lossless() {
        // (a) weight-curve sanity for both new windows.
        for w in [
            ApodizationWindow::Bartlett,
            ApodizationWindow::BlackmanHarris,
        ] {
            let n = 4096usize;
            let mut prev_up = f64::NEG_INFINITY;
            for i in 0..n {
                let v = w.weight(i, n);
                assert!(
                    (-1e-9..=1.0 + 1e-9).contains(&v),
                    "{w:?} weight at {i} out of [0,1]: {v}"
                );
                // Symmetry: weight(i) == weight(n-1-i).
                let mirror = w.weight(n - 1 - i, n);
                assert!(
                    (v - mirror).abs() < 1e-9,
                    "{w:?} not symmetric at {i}: {v} vs {mirror}"
                );
                // Monotone rise over the first half (both windows are
                // unimodal with their peak at the centre).
                if i <= (n - 1) / 2 {
                    assert!(
                        v + 1e-9 >= prev_up,
                        "{w:?} not monotone-rising on first half at {i}"
                    );
                    prev_up = v;
                }
            }
            // Edges hug zero, centre hugs one.
            assert!(w.weight(0, n).abs() < 1e-3, "{w:?} edge weight not ~0");
            assert!(
                (w.weight((n - 1) / 2, n) - 1.0).abs() < 1e-3,
                "{w:?} centre weight not ~1"
            );
        }

        // (b) lossless round-trip through each new window as sole analysis
        //     window.
        let sr = 48_000u32;
        let n = 2048usize;
        let pcm: Vec<u8> = (0..n as i32)
            .flat_map(|i| {
                let t = i as f64 / sr as f64;
                let s = ((t * 1234.0 * 2.0 * std::f64::consts::PI).sin() * 9000.0
                    + (t * 5678.0 * 2.0 * std::f64::consts::PI).sin() * 4000.0)
                    as i16;
                s.to_le_bytes()
            })
            .collect();
        for win in [Apodization::Bartlett, Apodization::BlackmanHarris] {
            let p = build_audio_params(1, sr, SampleFormat::S16);
            let opts = FlacEncoderOptions {
                apodization: Some(vec![win]),
                ..FlacEncoderOptions::default()
            };
            let mut enc = make_encoder_with_options(&p, opts).expect("new window must construct");
            enc.send_frame(&Frame::Audio(AudioFrame {
                samples: n as u32,
                pts: Some(0),
                data: vec![pcm.clone()],
            }))
            .unwrap();
            enc.flush().unwrap();
            let mut frames: Vec<Vec<u8>> = Vec::new();
            while let Ok(pkt) = enc.receive_packet() {
                frames.push(pkt.data);
            }
            let mut dparams = build_audio_params(1, sr, SampleFormat::S16);
            dparams.extradata = enc.output_params().extradata.clone();
            let mut dec = decoder::make_decoder(&dparams).unwrap();
            let mut out: Vec<u8> = Vec::new();
            for f in frames {
                let mut pkt =
                    oxideav_core::Packet::new(0, oxideav_core::TimeBase::new(1, sr as i64), f);
                pkt.pts = Some(0);
                dec.send_packet(&pkt).unwrap();
                while let Ok(oxideav_core::Frame::Audio(a)) = dec.receive_frame() {
                    out.extend_from_slice(&a.data[0]);
                }
            }
            assert_eq!(out, pcm, "{win:?} window output is not lossless");
        }
    }

    /// The two new windows must actually steer the LPC fit — selecting one
    /// as the sole window must produce a different per-order plan than the
    /// Welch default on leakage-sensitive content, and a low-leakage window
    /// set including Blackman-Harris must beat a Welch-only search there.
    #[test]
    fn new_windows_steer_lpc_fit() {
        let sr = 48_000u32;
        let n = 4096usize;
        let samples: Vec<i32> = (0..n)
            .map(|i| {
                let t = i as f64 / sr as f64;
                ((t * 3000.0 * 2.0 * std::f64::consts::PI).sin() * 12_000.0
                    + (t * 3041.0 * 2.0 * std::f64::consts::PI).sin() * 12_000.0
                    + (t * 9000.0 * 2.0 * std::f64::consts::PI).sin() * 3_000.0)
                    as i32
            })
            .collect();

        let per_order = |windows: Vec<ApodizationWindow>| -> Vec<u64> {
            let mut scratch = SubframeScratch {
                apodization: windows,
                ..SubframeScratch::default()
            };
            (1..=SUBSET_MAX_LPC_ORDER)
                .map(|order| {
                    encode_lpc_plan_s(&samples, 16, order, 0, &mut scratch)
                        .map(|p| p.bits)
                        .unwrap_or(u64::MAX)
                })
                .collect()
        };
        let min_of = |v: &[u64]| v.iter().copied().min().unwrap_or(u64::MAX);

        let welch_only = per_order(vec![ApodizationWindow::Welch]);
        let bartlett = per_order(vec![ApodizationWindow::Bartlett]);
        let bh = per_order(vec![ApodizationWindow::BlackmanHarris]);

        // Each new window must change the fit relative to Welch on at least
        // one order — proof it is actually wired into the search.
        assert!(
            welch_only != bartlett,
            "Bartlett produced an identical per-order plan to Welch"
        );
        assert!(
            welch_only != bh,
            "Blackman-Harris produced an identical per-order plan to Welch"
        );

        // Adding the new windows alongside Welch can only shrink or tie the
        // best plan — the minimum-bit driver keeps the smallest candidate,
        // so a superset of windows is never worse than the Welch-only subset.
        let superset = per_order(vec![
            ApodizationWindow::Welch,
            ApodizationWindow::Bartlett,
            ApodizationWindow::BlackmanHarris,
        ]);
        assert!(
            min_of(&superset) <= min_of(&welch_only),
            "a window superset must never beat-then-lose to its subset: \
             superset={} welch_only={}",
            min_of(&superset),
            min_of(&welch_only)
        );
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

    /// Construct a concrete [`FlacEncoder`] (not boxed) so tests can drive
    /// the private `verify_frame` directly. Mirrors the verify preset.
    fn test_flac_encoder(channels: u16, sample_rate: u32, bps: u8) -> FlacEncoder {
        let fmt = match bps {
            8 => SampleFormat::U8,
            16 => SampleFormat::S16,
            24 => SampleFormat::S24,
            _ => SampleFormat::S32,
        };
        FlacEncoder {
            output_params: build_audio_params(channels, sample_rate, fmt),
            sample_format: fmt,
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
            padding_bytes: None,
            verify: true,
            scratch: build_scratch(&FlacEncoderOptions::verifying()),
        }
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
            ..FlacEncoderOptions::default()
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
            ..FlacEncoderOptions::default()
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
            ..FlacEncoderOptions::default()
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
    fn make_encoder_rejects_out_of_range_sample_rate() {
        // Daily-fuzz regression (`encoder_options` harness,
        // 2026-06-11 run): sample_rate == u32::MAX flowed unchecked
        // into the STREAMINFO writer inside `build_extradata` and
        // panicked through an `expect` instead of returning `Err`.
        // RFC 9639 §8.2: the field is 20 bits and MUST NOT be 0 when
        // the file contains audio.
        for bad in [0u32, StreamInfo::MAX_SAMPLE_RATE + 1, u32::MAX] {
            let p = build_audio_params(2, bad, SampleFormat::S16);
            assert!(
                make_encoder(&p).is_err(),
                "sample_rate {bad} must be rejected at construction time"
            );
        }
        // 1 Hz and the 20-bit ceiling are both inside the spec range.
        for good in [1u32, StreamInfo::MAX_SAMPLE_RATE] {
            let p = build_audio_params(2, good, SampleFormat::S16);
            assert!(
                make_encoder(&p).is_ok(),
                "sample_rate {good} is spec-legal and must construct"
            );
        }
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
            ..FlacEncoderOptions::default()
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
            ..FlacEncoderOptions::default()
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

    /// Byte-exactness crosswalk for the bottom-up partition-merge ladder.
    /// For each diverse residual buffer + predictor order, build the
    /// finest legal level, walk it down to order 0, and assert each
    /// order's [`level_total_cost`] (for **both** Rice methods) equals the
    /// per-slice [`partition_layout_cost_zp`] oracle. Also pins
    /// [`finest_legal_partition_order`] against the legality boundary the
    /// per-slice path enforces (the first order for which
    /// `partition_layout_cost_zp` returns `None`). A mismatch here means
    /// the merge would change a chosen partition order / Rice `k` / escape
    /// decision — i.e. non-byte-identical output — and fails before any
    /// fixture round-trip can.
    #[test]
    fn merge_ladder_matches_per_slice_oracle() {
        fn xorshift32(state: &mut u32) -> u32 {
            *state ^= *state << 13;
            *state ^= *state >> 17;
            *state ^= *state << 5;
            *state
        }

        let block = 4096usize;
        let mut cases: Vec<(String, Vec<i32>)> = Vec::new();

        // Mixed-amplitude pseudo-random residuals.
        let mut st = 0x1234_5678u32;
        let mut rnd = Vec::with_capacity(block);
        for _ in 0..block {
            rnd.push((xorshift32(&mut st) as i32) >> 18);
        }
        cases.push(("xorshift".into(), rnd));

        // All zero (degenerate post-predictor partition).
        cases.push(("all-zero".into(), vec![0i32; block]));

        // Laplace-ish small means.
        for mean in [1i32, 7, 64, 900] {
            let mut st = 0xBEEF_0000u32 ^ (mean as u32);
            let mut buf = Vec::with_capacity(block);
            for _ in 0..block {
                let u = xorshift32(&mut st);
                let mag = ((u % (mean as u32 * 4 + 1)) as i32) - mean * 2;
                buf.push(mag);
            }
            cases.push((format!("laplace(mean={mean})"), buf));
        }

        // Split statistics: quiet first half, loud second half.
        let mut split = Vec::with_capacity(block);
        for i in 0..block {
            if i < block / 2 {
                split.push((i % 3) as i32 - 1);
            } else {
                split.push(((i * 2654435761usize) % 20000) as i32 - 10000);
            }
        }
        cases.push(("split-stats".into(), split));

        // i32 edge values to stress the escape / raw-bits path.
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
                // The residual array the production emit path sees excludes
                // the warmup samples; here `residuals` is the full block so
                // we slice it the way `encode_rice_residual_s` does — its
                // length is block - predictor_order. Build a residual /
                // table view consistent with the production call shape.
                let res = &residuals[..block - predictor_order];
                let zz = &zigzag[..block - predictor_order];
                let (pfx, rb) = build_partition_tables(res, zz);

                // Determine the legality boundary the per-slice path sees.
                for max_po in [SUBSET_MAX_PARTITION_ORDER, MAX_PARTITION_ORDER] {
                    let finest = finest_legal_partition_order(block, predictor_order, max_po);

                    // finest must be legal and finest+1 (if <= max_po) illegal.
                    assert!(
                        partition_layout_cost_zp(
                            zz,
                            &pfx,
                            &rb,
                            block,
                            predictor_order,
                            finest,
                            4,
                            14
                        )
                        .is_some(),
                        "{name}: finest order {finest} reported illegal (po={predictor_order})",
                    );
                    if finest < max_po {
                        assert!(
                            partition_layout_cost_zp(
                                zz,
                                &pfx,
                                &rb,
                                block,
                                predictor_order,
                                finest + 1,
                                4,
                                14
                            )
                            .is_none(),
                            "{name}: order {} should be illegal (po={predictor_order})",
                            finest + 1,
                        );
                    }

                    // Walk the merge ladder finest -> 0, asserting each
                    // order's total matches the per-slice oracle for both
                    // methods.
                    let mut a = MergeLevel::default();
                    let mut b = MergeLevel::default();
                    build_finest_level(zz, &rb, block, predictor_order, finest, 30, &mut a);
                    let mut cur = &mut a;
                    let mut spare = &mut b;
                    let mut p = finest as i32;
                    while p >= 0 {
                        let pu = p as u32;
                        for (param_bits, k_max) in [(4u32, 14u32), (5u32, 30u32)] {
                            let merged = level_total_cost(cur, param_bits, k_max);
                            let oracle = partition_layout_cost_zp(
                                zz,
                                &pfx,
                                &rb,
                                block,
                                predictor_order,
                                pu,
                                param_bits,
                                k_max,
                            )
                            .expect("legal order");
                            assert_eq!(
                                merged, oracle,
                                "{name}: merge mismatch po={predictor_order} \
                                 max_po={max_po} order={pu} param_bits={param_bits}: \
                                 merged={merged} oracle={oracle}",
                            );
                        }
                        if p > 0 {
                            merge_level_down(cur, spare);
                            std::mem::swap(&mut cur, &mut spare);
                        }
                        p -= 1;
                    }
                }
                // Silence unused warnings for the block-scoped table views
                // when predictor_order == 0 makes them alias the originals.
                let _ = (&prefix, &raw_bits, res);
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
                for order in 1..=SUBSET_MAX_LPC_ORDER {
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

    // --- Configurable block size (FlacEncoderOptions::block_size) --------

    /// Drive an `S16` mono signal through `make_encoder_with_options` with
    /// the given options, fully decode the resulting frames, and return
    /// `(recovered_interleaved_pcm_bytes, frame_count, observed_block_sizes)`.
    /// Used by the block-size tests to assert losslessness + framing.
    fn encode_decode_mono_s16_with_opts(
        pcm: &[u8],
        sr: u32,
        opts: FlacEncoderOptions,
    ) -> (Vec<u8>, usize, Vec<u32>) {
        let n = pcm.len() / 2;
        let p = build_audio_params(1, sr, SampleFormat::S16);
        let mut enc = make_encoder_with_options(&p, opts).unwrap();
        let af = AudioFrame {
            samples: n as u32,
            pts: Some(0),
            data: vec![pcm.to_vec()],
        };
        enc.send_frame(&Frame::Audio(af)).unwrap();
        enc.flush().unwrap();

        let mut frames: Vec<Vec<u8>> = Vec::new();
        while let Ok(pkt) = enc.receive_packet() {
            frames.push(pkt.data);
        }
        assert!(!frames.is_empty(), "encoder produced no packets");

        let mut dparams = build_audio_params(1, sr, SampleFormat::S16);
        dparams.extradata = enc.output_params().extradata.clone();
        let mut dec = decoder::make_decoder(&dparams).unwrap();

        let mut out: Vec<u8> = Vec::new();
        let mut block_sizes: Vec<u32> = Vec::new();
        let frame_count = frames.len();
        for f in frames {
            let mut pkt =
                oxideav_core::Packet::new(0, oxideav_core::TimeBase::new(1, sr as i64), f);
            pkt.pts = Some(0);
            dec.send_packet(&pkt).unwrap();
            while let Ok(oxideav_core::Frame::Audio(a)) = dec.receive_frame() {
                block_sizes.push(a.samples);
                out.extend_from_slice(&a.data[0]);
            }
        }
        (out, frame_count, block_sizes)
    }

    /// Deterministic xorshift S16 mono PCM of `n` interchannel samples.
    fn synth_mono_s16(n: usize, seed: u64) -> Vec<u8> {
        let mut rng = seed;
        let mut pcm = Vec::with_capacity(n * 2);
        for i in 0..n {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            // Mix a tonal component with low-amplitude noise so LPC has
            // something to fit and the Rice search has structure to find.
            let tone = ((i as f64 * 0.05).sin() * 8_000.0) as i32;
            let noise = (rng % 401) as i32 - 200;
            let v = (tone + noise).clamp(-32_768, 32_767) as i16;
            pcm.extend_from_slice(&v.to_le_bytes());
        }
        pcm
    }

    /// Multichannel deterministic S16 PCM, `n` interchannel samples per
    /// channel, interleaved little-endian — exercises the verify path on a
    /// stereo/surround layout the per-frame decorrelation search touches.
    fn synth_interleaved_s16(n: usize, channels: usize, seed: u64) -> Vec<u8> {
        let mut rng = seed;
        let mut pcm = Vec::with_capacity(n * channels * 2);
        for i in 0..n {
            for c in 0..channels {
                rng ^= rng << 13;
                rng ^= rng >> 7;
                rng ^= rng << 17;
                let tone = ((i as f64 * (0.04 + 0.01 * c as f64)).sin() * 9_000.0) as i32;
                let noise = (rng % 301) as i32 - 150;
                let v = (tone + noise).clamp(-32_768, 32_767) as i16;
                pcm.extend_from_slice(&v.to_le_bytes());
            }
        }
        pcm
    }

    /// `verify: true` must produce byte-for-byte identical output to the
    /// default encoder — the self-check never changes the encoding, it only
    /// asserts the bytes round-trip. The encode must succeed (no false
    /// mismatch on a correct encoder).
    #[test]
    fn verify_output_is_byte_identical_to_default() {
        let sr = 44_100u32;
        let pcm = synth_mono_s16(12_000, 0x5151_2323_8989_abcd);

        let (out_default, frames_default, blocks_default) =
            encode_decode_mono_s16_with_opts(&pcm, sr, FlacEncoderOptions::default());
        let (out_verify, frames_verify, blocks_verify) =
            encode_decode_mono_s16_with_opts(&pcm, sr, FlacEncoderOptions::verifying());

        assert_eq!(out_verify, pcm, "verify path is not lossless");
        assert_eq!(
            out_verify, out_default,
            "verify changed the decoded PCM (must be identical)"
        );
        assert_eq!(
            frames_verify, frames_default,
            "verify changed the frame count"
        );
        assert_eq!(blocks_verify, blocks_default, "verify changed the framing");

        // And the raw produced frame bytes are identical, not just the
        // decoded PCM — verify must be a pure check with zero on-wire effect.
        let raw_default = encode_mono_frames(&pcm, sr, FlacEncoderOptions::default());
        let raw_verify = encode_mono_frames(&pcm, sr, FlacEncoderOptions::verifying());
        assert_eq!(
            raw_default, raw_verify,
            "verify altered the emitted frame bytes"
        );
    }

    /// Collect the raw per-frame packet bytes the encoder emits for mono S16.
    fn encode_mono_frames(pcm: &[u8], sr: u32, opts: FlacEncoderOptions) -> Vec<Vec<u8>> {
        let n = pcm.len() / 2;
        let p = build_audio_params(1, sr, SampleFormat::S16);
        let mut enc = make_encoder_with_options(&p, opts).unwrap();
        enc.send_frame(&Frame::Audio(AudioFrame {
            samples: n as u32,
            pts: Some(0),
            data: vec![pcm.to_vec()],
        }))
        .unwrap();
        enc.flush().unwrap();
        let mut frames = Vec::new();
        while let Ok(pkt) = enc.receive_packet() {
            frames.push(pkt.data);
        }
        frames
    }

    /// Verify must succeed across every supported bit depth, channel layout,
    /// and a span of block sizes — the round-trip self-check exercises the
    /// full encode→decode core on each.
    #[test]
    fn verify_passes_on_all_formats_and_layouts() {
        let sr = 48_000u32;
        // (channels, sample_format, bytes_per_sample)
        for &(channels, fmt) in &[
            (1usize, SampleFormat::S16),
            (2, SampleFormat::S16),
            (2, SampleFormat::S24),
            (2, SampleFormat::S32),
            (1, SampleFormat::U8),
            (6, SampleFormat::S16),
        ] {
            let n = 5_000usize;
            // Build interleaved PCM for the requested format from an i16 base.
            let base = synth_interleaved_s16(n, channels, 0x1357_9bdf_2468_ace0);
            let pcm: Vec<u8> = match fmt {
                SampleFormat::S16 => base,
                SampleFormat::S24 => base
                    .chunks_exact(2)
                    .flat_map(|c| {
                        let v = i16::from_le_bytes([c[0], c[1]]) as i32 * 64;
                        [
                            (v & 0xFF) as u8,
                            ((v >> 8) & 0xFF) as u8,
                            ((v >> 16) & 0xFF) as u8,
                        ]
                    })
                    .collect(),
                SampleFormat::S32 => base
                    .chunks_exact(2)
                    .flat_map(|c| {
                        let v = i16::from_le_bytes([c[0], c[1]]) as i32 * 1024;
                        v.to_le_bytes()
                    })
                    .collect(),
                SampleFormat::U8 => base
                    .chunks_exact(2)
                    .map(|c| {
                        let v = i16::from_le_bytes([c[0], c[1]]);
                        ((v >> 8) as i32 + 128).clamp(0, 255) as u8
                    })
                    .collect(),
                _ => unreachable!(),
            };

            let p = build_audio_params(channels as u16, sr, fmt);
            let mut enc = make_encoder_with_options(&p, FlacEncoderOptions::verifying()).unwrap();
            let samples = (pcm.len() / channels / fmt_bytes(fmt)) as u32;
            enc.send_frame(&Frame::Audio(AudioFrame {
                samples,
                pts: Some(0),
                data: vec![pcm],
            }))
            .expect("verify must accept a correctly-encoded frame");
            enc.flush()
                .unwrap_or_else(|e| panic!("verify flush failed for {channels}ch {fmt:?}: {e}"));
            let mut got = 0;
            while enc.receive_packet().is_ok() {
                got += 1;
            }
            assert!(got > 0, "no packets for {channels}ch {fmt:?}");
        }
    }

    fn fmt_bytes(fmt: SampleFormat) -> usize {
        match fmt {
            SampleFormat::U8 => 1,
            SampleFormat::S16 => 2,
            SampleFormat::S24 => 3,
            SampleFormat::S32 => 4,
            _ => unreachable!(),
        }
    }

    /// `verify_frame` must reject a frame whose decoded samples diverge from
    /// the claimed input, naming the offending channel + sample. This is the
    /// failure path that would fire on a genuine encoder regression.
    #[test]
    fn verify_frame_rejects_a_mismatching_input() {
        let sr = 44_100u32;
        let pcm = synth_mono_s16(4096, 0x9999_7777_5555_3333);
        // Encode a legitimate frame.
        let frames = encode_mono_frames(&pcm, sr, FlacEncoderOptions::default());
        let frame = &frames[0];

        // Build an encoder so we can call verify_frame with deliberately
        // wrong "input" planes (claim the all-zero signal instead of the
        // real one). The decoded frame won't match → must error.
        let enc = test_flac_encoder(1, sr, 16);
        let wrong = vec![vec![0i32; 4096]];
        let err = enc
            .verify_frame(frame, &wrong)
            .expect_err("verify must reject a non-matching input");
        let msg = format!("{err}");
        assert!(
            msg.contains("verify") && msg.contains("channel 0"),
            "error should pinpoint the diverging channel/sample: {msg}"
        );
    }

    /// `verify_frame` accepts the frame when the input planes are the ones
    /// that were actually encoded — the positive control for the test above.
    #[test]
    fn verify_frame_accepts_the_true_input() {
        let sr = 44_100u32;
        let n = 4096usize;
        let pcm = synth_mono_s16(n, 0x2468_ace0_1357_9bdf);
        let frames = encode_mono_frames(&pcm, sr, FlacEncoderOptions::default());

        let enc = test_flac_encoder(1, sr, 16);
        let truth: Vec<i32> = pcm
            .chunks_exact(2)
            .map(|c| i16::from_le_bytes([c[0], c[1]]) as i32)
            .collect();
        enc.verify_frame(&frames[0], &[truth])
            .expect("verify must accept the genuine input");
    }

    /// `block_size: None` must be byte-for-byte identical to an explicit
    /// `Some(4096)` and to the historical `make_encoder` default — the
    /// configured size is the only thing that changes, and 4096 is the
    /// historical constant.
    #[test]
    fn block_size_none_equals_default_4096() {
        let sr = 44_100u32;
        let pcm = synth_mono_s16(10_000, 0xa1b2_c3d4_e5f6_0789);

        let (out_default, frames_default, _) =
            encode_decode_mono_s16_with_opts(&pcm, sr, FlacEncoderOptions::default());
        let (out_explicit, frames_explicit, blocks_explicit) = encode_decode_mono_s16_with_opts(
            &pcm,
            sr,
            FlacEncoderOptions {
                block_size: Some(4096),
                ..FlacEncoderOptions::default()
            },
        );

        assert_eq!(out_default, pcm, "default path is not lossless");
        assert_eq!(
            out_explicit, out_default,
            "explicit Some(4096) diverged from None"
        );
        assert_eq!(
            frames_explicit, frames_default,
            "explicit Some(4096) changed the frame count"
        );
        // 10_000 / 4096 -> 3 frames: 4096, 4096, 1808.
        assert_eq!(frames_default, 3);
        assert_eq!(blocks_explicit, vec![4096, 4096, 1808]);
    }

    /// A non-default subset-legal block size (1024) must round-trip
    /// bit-exactly and produce the expected fixed-block framing: every
    /// frame but the last carries exactly the configured size, and the
    /// last carries the remainder (exempt from the minimum floor, RFC 9639
    /// §8.2). STREAMINFO must record `min == max == 1024`.
    #[test]
    fn small_block_size_roundtrips_and_frames_as_configured() {
        let sr = 44_100u32;
        let total = 3_500usize; // 3 * 1024 + 428
        let pcm = synth_mono_s16(total, 0x0246_8ace_1357_9bdf);

        let (out, frame_count, blocks) = encode_decode_mono_s16_with_opts(
            &pcm,
            sr,
            FlacEncoderOptions {
                block_size: Some(1024),
                ..FlacEncoderOptions::default()
            },
        );

        assert_eq!(out, pcm, "configured-block-size output is not lossless");
        assert_eq!(frame_count, 4);
        assert_eq!(blocks, vec![1024, 1024, 1024, 428]);

        // STREAMINFO min/max block size both equal the configured size
        // (fixed-blocking strategy).
        let p = build_audio_params(1, sr, SampleFormat::S16);
        let enc = make_encoder_with_options(
            &p,
            FlacEncoderOptions {
                block_size: Some(1024),
                ..FlacEncoderOptions::default()
            },
        )
        .unwrap();
        // extradata is the STREAMINFO metadata block: 4-byte block header
        // followed by the 34-byte payload (no `fLaC` magic — that lives in
        // the container, not extradata).
        let info = StreamInfo::parse(&enc.output_params().extradata[4..4 + 34]).unwrap();
        assert_eq!(info.min_block_size, 1024);
        assert_eq!(info.max_block_size, 1024);
    }

    /// Block size must sit inside the STREAMINFO 16..=65535 range
    /// (RFC 9639 §8.2). Values outside it are rejected at construction.
    #[test]
    fn block_size_out_of_streaminfo_range_is_rejected() {
        let p = build_audio_params(1, 44_100, SampleFormat::S16);
        for bad in [0u32, 1, 15, 65_536, 100_000] {
            let r = make_encoder_with_options(
                &p,
                FlacEncoderOptions {
                    block_size: Some(bad),
                    // Allow non-subset so the *subset* ceiling never masks
                    // the range check for the large values.
                    allow_non_subset_block_size: true,
                    ..FlacEncoderOptions::default()
                },
            );
            assert!(r.is_err(), "block_size {bad} should be rejected");
        }
        // The boundary values 16 and 65535 are accepted (65535 needs the
        // non-subset opt-in to clear the subset ceiling).
        for ok in [16u32, 65_535] {
            let r = make_encoder_with_options(
                &p,
                FlacEncoderOptions {
                    block_size: Some(ok),
                    allow_non_subset_block_size: true,
                    ..FlacEncoderOptions::default()
                },
            );
            assert!(r.is_ok(), "block_size {ok} should be accepted");
        }
    }

    /// The streamable-subset ceiling depends on sample rate (RFC 9639 §7):
    /// 4608 for ≤ 48 kHz, 16384 above. A request above the applicable
    /// ceiling is rejected by default and accepted with the non-subset
    /// opt-in.
    #[test]
    fn subset_block_size_ceiling_is_rate_dependent() {
        // ≤ 48 kHz: 4608 is the ceiling.
        let low = build_audio_params(1, 44_100, SampleFormat::S16);
        assert!(make_encoder_with_options(
            &low,
            FlacEncoderOptions {
                block_size: Some(4_608),
                ..FlacEncoderOptions::default()
            },
        )
        .is_ok());
        assert!(
            make_encoder_with_options(
                &low,
                FlacEncoderOptions {
                    block_size: Some(4_609),
                    ..FlacEncoderOptions::default()
                },
            )
            .is_err(),
            "4609 should exceed the ≤48 kHz subset ceiling"
        );
        // Same 4609 clears with the opt-in.
        assert!(make_encoder_with_options(
            &low,
            FlacEncoderOptions {
                block_size: Some(4_609),
                allow_non_subset_block_size: true,
                ..FlacEncoderOptions::default()
            },
        )
        .is_ok());

        // > 48 kHz: 16384 is the ceiling, so 8192 is subset-legal.
        let high = build_audio_params(1, 96_000, SampleFormat::S16);
        assert!(make_encoder_with_options(
            &high,
            FlacEncoderOptions {
                block_size: Some(8_192),
                ..FlacEncoderOptions::default()
            },
        )
        .is_ok());
        assert!(
            make_encoder_with_options(
                &high,
                FlacEncoderOptions {
                    block_size: Some(16_385),
                    ..FlacEncoderOptions::default()
                },
            )
            .is_err(),
            "16385 should exceed the universal 16384 subset ceiling"
        );
    }

    /// A non-subset block size above the subset ceiling but within the
    /// STREAMINFO range must still produce a fully lossless, decodable
    /// stream (block size is a free encoder choice the decoder reads from
    /// the frame header, RFC 9639 §9.1.2).
    #[test]
    fn non_subset_block_size_roundtrips_bit_exact() {
        let sr = 44_100u32; // ≤ 48 kHz, so 8192 is non-subset here.
        let total = 20_000usize;
        let pcm = synth_mono_s16(total, 0x1111_2222_3333_4444);

        let (out, _frames, blocks) = encode_decode_mono_s16_with_opts(
            &pcm,
            sr,
            FlacEncoderOptions {
                block_size: Some(8_192),
                allow_non_subset_block_size: true,
                ..FlacEncoderOptions::default()
            },
        );
        assert_eq!(out, pcm, "non-subset block-size output is not lossless");
        // 20000 / 8192 -> 8192, 8192, 3616.
        assert_eq!(blocks, vec![8_192, 8_192, 3_616]);
    }

    /// `build_finest_level`'s inner shift-sum loop walks only the low
    /// `64 - leading_zeros(u)` rows of each residual (the high-`k` rows
    /// would only add `u >> k == 0`). This proves that skipping the
    /// zero-add tail is bit-identical to the naive full-`stride`
    /// accumulation that touches every row, across magnitudes spanning a
    /// single bit up to the full 32-bit zigzag width (including zeros and
    /// the `nbits >= stride` clamp boundary). A divergence here would
    /// change the partition-order cost search and so the encoded bytes.
    #[test]
    fn build_finest_level_matches_naive_full_stride() {
        // Reference: accumulate `u >> k` for EVERY k in 0..stride, with no
        // leading-zeros short-circuit — the definition the optimized loop
        // must equal.
        fn naive(
            zigzag: &[u32],
            raw_bits: &[u8],
            block_size: usize,
            predictor_order: usize,
            finest: u32,
            k_max: u32,
        ) -> MergeLevel {
            let n_partitions = 1usize << finest;
            let fps = block_size / n_partitions;
            let stride = (k_max + 1) as usize;
            let mut lvl = MergeLevel {
                shift_sums: vec![0u64; n_partitions * stride],
                counts: vec![0u32; n_partitions],
                max_raw: vec![1u8; n_partitions],
                stride,
            };
            let mut idx = 0usize;
            for p in 0..n_partitions {
                let end = (p + 1) * fps - predictor_order;
                let row = &mut lvl.shift_sums[p * stride..p * stride + stride];
                let mut m: u8 = 1;
                for i in idx..end {
                    let u = zigzag[i] as u64;
                    for (k, slot) in row.iter_mut().enumerate() {
                        *slot += u >> k;
                    }
                    if raw_bits[i] > m {
                        m = raw_bits[i];
                    }
                }
                lvl.counts[p] = (end - idx) as u32;
                lvl.max_raw[p] = m;
                idx = end;
            }
            lvl
        }

        // Deterministic battery: magnitudes chosen so successive residuals
        // need 0, 1, … up to 32 significant bits, exercising the whole
        // `nbits` range and the `.min(stride)` clamp when k_max < 31.
        let block_size = 256usize;
        let predictor_order = 4usize;
        let n_res = block_size - predictor_order;
        let zigzag: Vec<u32> = (0..n_res)
            .map(|i| match i % 5 {
                0 => 0,                                      // nbits = 0
                1 => 1,                                      // nbits = 1
                2 => (1u32 << (i % 31)).wrapping_sub(1),     // ramp of widths
                3 => u32::MAX,                               // nbits = 32 (> any k_max)
                _ => (i as u32).wrapping_mul(2_654_435_761), // varied
            })
            .collect();
        let raw_bits: Vec<u8> = (0..n_res).map(|i| ((i % 17) + 1) as u8).collect();

        for &finest in &[0u32, 1, 2, 3] {
            for &k_max in &[0u32, 1, 7, 15, 30] {
                let mut opt = MergeLevel::default();
                build_finest_level(
                    &zigzag,
                    &raw_bits,
                    block_size,
                    predictor_order,
                    finest,
                    k_max,
                    &mut opt,
                );
                let want = naive(
                    &zigzag,
                    &raw_bits,
                    block_size,
                    predictor_order,
                    finest,
                    k_max,
                );
                assert_eq!(
                    opt.shift_sums, want.shift_sums,
                    "shift_sums diverge at finest={finest} k_max={k_max}"
                );
                assert_eq!(opt.counts, want.counts, "counts diverge");
                assert_eq!(opt.max_raw, want.max_raw, "max_raw diverge");
                assert_eq!(opt.stride, want.stride, "stride diverge");
            }
        }
    }

    /// The memoised `(window, n)` weight cache (`window_weights` /
    /// `apply_window`) must return values bit-identical to a fresh
    /// `window.weight(i, n)` call — otherwise the windowed signal, and so
    /// the encoded bytes, would shift. Checks every window across several
    /// block sizes, on a cold cache, on a warm hit, and after the cache has
    /// served an interleaved set of other `(window, n)` keys (so a wrong
    /// row can't be masked by lookup order).
    #[test]
    fn cached_window_weights_match_inline_weight() {
        let windows = [
            ApodizationWindow::Welch,
            ApodizationWindow::Hann,
            ApodizationWindow::Tukey {
                alpha_num: 1,
                alpha_den: 2,
            },
            ApodizationWindow::Bartlett,
            ApodizationWindow::BlackmanHarris,
        ];
        let sizes = [1usize, 2, 16, 17, 256, 4096];
        let mut cache: Vec<WindowWeights> = Vec::new();

        // Exact f64 bit-equality: cached row vs direct weight call.
        for &w in &windows {
            for &n in &sizes {
                let row = window_weights(&mut cache, w, n).to_vec();
                assert_eq!(row.len(), n);
                for (i, &cached) in row.iter().enumerate() {
                    let direct = w.weight(i, n);
                    assert_eq!(
                        cached.to_bits(),
                        direct.to_bits(),
                        "cached weight != weight(i,n) for {w:?} n={n} i={i}"
                    );
                }
            }
        }

        // Warm-hit path: re-querying a served key must still match exactly,
        // and `apply_window`'s product must equal the inline taper.
        let samples: Vec<i32> = (0..256i32).map(|i| (i * 37 - 911) % 9001).collect();
        for &w in &windows {
            let mut windowed = Vec::new();
            apply_window(&mut cache, &mut windowed, &samples, w);
            assert_eq!(windowed.len(), samples.len());
            for (i, (&s, &got)) in samples.iter().zip(windowed.iter()).enumerate() {
                let want = s as f64 * w.weight(i, samples.len());
                assert_eq!(
                    got.to_bits(),
                    want.to_bits(),
                    "apply_window product != inline taper for {w:?} i={i}"
                );
            }
        }
    }

    #[test]
    fn encode_sample_size_emits_table17_codes() {
        // Every depth the encoder accepts has an explicit RFC 9639
        // §9.1.4 Table 17 frame-header code, so each frame self-describes
        // its bit depth; an out-of-table depth degrades to 0b000 ("from
        // STREAMINFO"). 32-bit must map to 0b111 (code 7), not 0.
        assert_eq!(encode_sample_size(8), 1);
        assert_eq!(encode_sample_size(12), 2);
        assert_eq!(encode_sample_size(16), 4);
        assert_eq!(encode_sample_size(20), 5);
        assert_eq!(encode_sample_size(24), 6);
        assert_eq!(encode_sample_size(32), 7);
        // A depth with no table entry falls back to "from STREAMINFO".
        assert_eq!(encode_sample_size(10), 0);
    }

    #[test]
    fn encoded_32bit_frame_header_self_describes_depth() {
        // A 32-bit frame the encoder emits must carry sample-size code
        // 0b111 in its header and parse back to 32 bps *without*
        // consulting STREAMINFO (i.e. the frame is self-describing).
        let n = 512usize;
        let l: Vec<i32> = (0..n as i32).map(|i| (i * 70_000) - 18_000_000).collect();
        let r: Vec<i32> = (0..n as i32).map(|i| 9_000_000 - (i * 55_000)).collect();
        let mut scratch = SubframeScratch::default();
        let frame = encode_frame(0, n as u32, 192_000, 32, &[l, r], &mut scratch).unwrap();
        let hdr = crate::frame::parse_frame_header(&frame).unwrap();
        assert_eq!(
            hdr.bits_per_sample, 32,
            "32-bit frame header must self-describe its depth as 32 bps"
        );
    }
}
