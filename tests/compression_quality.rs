//! Compression-quality regression guards for the FLAC encoder.
//!
//! The encoder's residual-coding pipeline is a minimum-bit
//! rate-distortion search: for every subframe it evaluates CONSTANT,
//! VERBATIM, FIXED orders 0..=4, and LPC orders 1..=`max_lpc_order`
//! under each configured apodization window and a sweep of coefficient
//! precisions, then partitioned-Rice-codes the chosen residual under
//! both Rice parameter widths over every legal partition order, letting
//! each partition independently fall back to an escape (raw) coding when
//! that is cheaper (RFC 9639 §9.2.5–§9.2.7). Because the driver always
//! keeps the smallest plan, the search has two load-bearing invariants:
//!
//! 1. **Losslessness.** Every plan, whatever it picks, must reproduce
//!    the input PCM bit-exactly through the decoder (RFC 9639 §1: "FLAC
//!    ... is a lossless codec"). This file re-encodes the project's real
//!    WAV fixtures and decodes them back through *this crate's own*
//!    decoder (no external tool in the loop), asserting byte-exact
//!    recovery.
//!
//! 2. **Monotone search.** Widening the search space — adding an
//!    apodization window, raising the LPC-order ceiling, raising the
//!    partition-order ceiling — can only ever *shrink or tie* the
//!    encoded size, never inflate it, because the extra candidates are
//!    compared on actual bit cost and discarded when they don't win.
//!    A regression that made a "more thorough" setting emit *more* bits
//!    would mean the comparison is broken; the property battery below
//!    catches it across tonal, transient, noisy, and autoregressive
//!    signal classes.
//!
//! On top of those two correctness invariants the per-fixture cases pin
//! a conservative **compression-ratio ceiling** for each real fixture so
//! a future change that quietly de-tuned the RDO search (e.g. dropping a
//! window from the default set, or shrinking a search range) would trip a
//! size regression here rather than passing silently.

use oxideav_core::{AudioFrame, CodecId, CodecParameters, Error, Frame, Packet, SampleFormat};
use oxideav_flac::encoder::{make_encoder_with_options, Apodization, FlacEncoderOptions};

/// Minimal RIFF/WAVE reader: returns `(channels, sample_rate,
/// bits_per_sample, data_bytes)` for the PCM fixtures under
/// `docs/audio/flac/fixtures/`. Only the integer-PCM path is needed —
/// every fixture this file touches is integer PCM.
fn parse_wav(bytes: &[u8]) -> Option<(u16, u32, u16, Vec<u8>)> {
    if bytes.len() < 12 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let (mut ch, mut sr, mut bits) = (0u16, 0u32, 0u16);
    let mut data: Vec<u8> = Vec::new();
    let mut i = 12usize;
    while i + 8 <= bytes.len() {
        let id = &bytes[i..i + 4];
        let sz =
            u32::from_le_bytes([bytes[i + 4], bytes[i + 5], bytes[i + 6], bytes[i + 7]]) as usize;
        let bs = i + 8;
        if bs + sz > bytes.len() {
            break;
        }
        match id {
            b"fmt " if sz >= 16 => {
                ch = u16::from_le_bytes([bytes[bs + 2], bytes[bs + 3]]);
                sr = u32::from_le_bytes([
                    bytes[bs + 4],
                    bytes[bs + 5],
                    bytes[bs + 6],
                    bytes[bs + 7],
                ]);
                bits = u16::from_le_bytes([bytes[bs + 14], bytes[bs + 15]]);
            }
            b"data" => {
                data = bytes[bs..bs + sz].to_vec();
                break;
            }
            _ => {}
        }
        i = bs + sz + (sz & 1);
    }
    if ch == 0 || sr == 0 || bits == 0 || data.is_empty() {
        return None;
    }
    Some((ch, sr, bits, data))
}

fn fixtures_root() -> String {
    concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../docs/audio/flac/fixtures/"
    )
    .to_string()
}

/// Encode an interleaved-PCM frame, then decode it back through this
/// crate's own decoder and return `(payload_byte_len, recovered_pcm)`.
/// The recovered PCM is interleaved i32 (one element per channel per
/// frame) so the caller can compare it sample-exact against the input.
fn roundtrip(
    data: &[u8],
    ch: u16,
    sr: u32,
    fmt: SampleFormat,
    opts: FlacEncoderOptions,
) -> (usize, Vec<i32>) {
    let mut p = CodecParameters::audio(CodecId::new("flac"));
    p.channels = Some(ch);
    p.sample_rate = Some(sr);
    p.sample_format = Some(fmt);
    let bytes_per = fmt.bytes_per_sample();
    let nsamp = data.len() / (bytes_per * ch as usize);

    let mut enc = make_encoder_with_options(&p, opts).expect("make_encoder_with_options");
    let dec_params = {
        // Prime the encoder so output_params carries the live STREAMINFO.
        enc.send_frame(&Frame::Audio(AudioFrame {
            samples: nsamp as u32,
            pts: Some(0),
            data: vec![data.to_vec()],
        }))
        .expect("send_frame");
        enc.flush().expect("flush");
        enc.output_params().clone()
    };
    assert!(
        !dec_params.extradata.is_empty(),
        "encoder must emit STREAMINFO extradata"
    );

    let mut packets: Vec<Packet> = Vec::new();
    loop {
        match enc.receive_packet() {
            Ok(p) => packets.push(p),
            Err(Error::NeedMore) | Err(Error::Eof) => break,
            Err(e) => panic!("unexpected encoder error: {e:?}"),
        }
    }
    let payload: usize = packets.iter().map(|p| p.data.len()).sum();

    let dec_format = match bytes_per {
        1 => SampleFormat::U8,
        2 => SampleFormat::S16,
        3 => SampleFormat::S24,
        _ => SampleFormat::S32,
    };
    let mut dec = oxideav_flac::decoder::make_decoder(&dec_params).expect("make_decoder");
    let mut recovered: Vec<i32> = Vec::new();
    let n_ch = ch as usize;
    for pkt in packets {
        dec.send_packet(&pkt).expect("send_packet");
        loop {
            match dec.receive_frame() {
                Ok(Frame::Audio(a)) => {
                    let bps = dec_format.bytes_per_sample();
                    for chunk in a.data[0].chunks_exact(bps * n_ch) {
                        for c in 0..n_ch {
                            let off = c * bps;
                            let v = match dec_format {
                                SampleFormat::U8 => (chunk[off] as i32) - 128,
                                SampleFormat::S16 => {
                                    i16::from_le_bytes([chunk[off], chunk[off + 1]]) as i32
                                }
                                SampleFormat::S24 => {
                                    let mut v = (chunk[off] as i32)
                                        | ((chunk[off + 1] as i32) << 8)
                                        | ((chunk[off + 2] as i32) << 16);
                                    if v & 0x0080_0000 != 0 {
                                        v |= 0xFF00_0000_u32 as i32;
                                    }
                                    v
                                }
                                _ => i32::from_le_bytes([
                                    chunk[off],
                                    chunk[off + 1],
                                    chunk[off + 2],
                                    chunk[off + 3],
                                ]),
                            };
                            recovered.push(v);
                        }
                    }
                }
                Ok(_) => panic!("expected audio frame"),
                Err(Error::NeedMore) | Err(Error::Eof) => break,
                Err(e) => panic!("unexpected decoder error: {e:?}"),
            }
        }
    }
    (payload, recovered)
}

/// Decode the input PCM bytes into interleaved i32 reference samples for
/// the comparison side of a bit-exact roundtrip assertion.
fn pcm_to_i32(data: &[u8], fmt: SampleFormat, ch: u16) -> Vec<i32> {
    let n_ch = ch as usize;
    let bps = fmt.bytes_per_sample();
    let mut out = Vec::with_capacity(data.len() / bps);
    for chunk in data.chunks_exact(bps * n_ch) {
        for c in 0..n_ch {
            let off = c * bps;
            let v = match fmt {
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
                _ => {
                    i32::from_le_bytes([chunk[off], chunk[off + 1], chunk[off + 2], chunk[off + 3]])
                }
            };
            out.push(v);
        }
    }
    out
}

/// One real-fixture expectation: the fixture directory name, the sample
/// format its `data` chunk should be decoded as, and a conservative
/// upper bound on the compressed-payload / input-PCM ratio. The bound is
/// set a few percent above the encoder's current achieved ratio so
/// ordinary numeric drift doesn't trip it, but a real de-tuning of the
/// RDO search (which would push the ratio up materially) does. The
/// fixtures whose `fmt ` chunk advertises 16-bit are decoded as S16; the
/// 24-bit ones as S24.
struct FixtureExpect {
    dir: &'static str,
    /// Inclusive upper bound on payload/PCM. `None` for the incompressible
    /// VERBATIM fixture, where the only guarantee is "no worse than raw +
    /// small constant overhead", checked separately.
    max_ratio: Option<f64>,
}

/// Conservative ratio ceilings, locked in against the encoder's measured
/// RDO output (each set ~5% above the current achieved ratio). A change
/// that regressed the residual / predictor / window search would push the
/// real ratio above its ceiling and fail here.
/// Cap on the interchannel-sample prefix encoded per fixture. Two full
/// default blocks (RFC 9639 §9.1, 4096 samples each) is enough to drive
/// every frame-level code path while keeping the heavy 8-channel / 24-bit
/// fixtures from dominating the suite's wall time.
const MAX_FIXTURE_SAMPLES: usize = 8192;

const FIXTURE_EXPECTATIONS: &[FixtureExpect] = &[
    FixtureExpect {
        dir: "constant-subframe-silence",
        max_ratio: Some(0.01),
    },
    FixtureExpect {
        dir: "fixed-subframe-low-order",
        max_ratio: Some(0.13),
    },
    FixtureExpect {
        dir: "left-right-channel-pair",
        max_ratio: Some(0.15),
    },
    FixtureExpect {
        dir: "left-side-channel-pair",
        max_ratio: Some(0.15),
    },
    FixtureExpect {
        dir: "lpc-subframe-typical-music",
        max_ratio: Some(0.17),
    },
    FixtureExpect {
        dir: "mid-side-channel-pair",
        max_ratio: Some(0.15),
    },
    FixtureExpect {
        dir: "mono-16bit-44100-fixed-blocksize",
        max_ratio: Some(0.15),
    },
    FixtureExpect {
        dir: "right-side-channel-pair",
        max_ratio: Some(0.15),
    },
    FixtureExpect {
        dir: "stereo-16bit-44100-fixed",
        max_ratio: Some(0.15),
    },
    FixtureExpect {
        dir: "stereo-16bit-44100-small-blocksize",
        max_ratio: Some(0.15),
    },
    FixtureExpect {
        dir: "stereo-24bit-96000",
        max_ratio: Some(0.28),
    },
    FixtureExpect {
        dir: "surround-7_1-24bit-48000",
        max_ratio: Some(0.25),
    },
    FixtureExpect {
        dir: "verbatim-subframe-noise",
        max_ratio: None,
    },
];

#[test]
fn real_fixtures_roundtrip_bit_exact_and_meet_ratio_floor() {
    let root = fixtures_root();
    // The `docs/` corpus is a git submodule that is not checked out in
    // the crate's CI (which builds the published-crate tree without the
    // private docs repo). When the fixtures are absent the suite's
    // synthetic property tests still cover the RDO invariants, so this
    // case logs `skip` and passes rather than failing — matching the
    // `docs_corpus` integration test's behaviour. A developer with the
    // submodule checked out gets the full real-fixture coverage.
    if !std::path::Path::new(&format!(
        "{root}{}/expected.wav",
        FIXTURE_EXPECTATIONS[0].dir
    ))
    .exists()
    {
        eprintln!("skip real_fixtures: docs/audio/flac/fixtures/ not present (submodule)");
        return;
    }
    let mut checked = 0usize;
    for exp in FIXTURE_EXPECTATIONS {
        let wp = format!("{root}{}/expected.wav", exp.dir);
        let bytes = match std::fs::read(&wp) {
            Ok(b) => b,
            Err(e) => panic!("fixture {} unreadable: {e}", exp.dir),
        };
        let (ch, sr, bits, data_full) = parse_wav(&bytes)
            .unwrap_or_else(|| panic!("fixture {} is not parseable PCM WAVE", exp.dir));
        let fmt = match bits {
            8 => SampleFormat::U8,
            16 => SampleFormat::S16,
            24 => SampleFormat::S24,
            other => panic!("fixture {} has unexpected bit depth {other}", exp.dir),
        };

        // Bound the encoded length so the heavy multichannel / 24-bit
        // fixtures don't dominate the suite's wall time: a prefix of
        // `MAX_FIXTURE_SAMPLES` interchannel samples still spans more than
        // one full default block, so the bit-exact roundtrip and the ratio
        // measurement both exercise multi-frame, multi-partition, and
        // channel-decorrelation paths. Bit-exactness and ratio are both
        // computed over the same prefix, so the assertion stays well-defined.
        let bytes_per_iframe = fmt.bytes_per_sample() * ch as usize;
        let cap_bytes = (MAX_FIXTURE_SAMPLES * bytes_per_iframe).min(data_full.len());
        let data = &data_full[..cap_bytes];

        let (payload, recovered) = roundtrip(data, ch, sr, fmt, FlacEncoderOptions::default());
        let reference = pcm_to_i32(data, fmt, ch);
        assert_eq!(
            recovered.len(),
            reference.len(),
            "fixture {}: decoded sample count {} != input {}",
            exp.dir,
            recovered.len(),
            reference.len()
        );
        assert!(
            recovered == reference,
            "fixture {}: round-trip is not bit-exact (FLAC must be lossless)",
            exp.dir
        );

        let ratio = payload as f64 / data.len() as f64;
        match exp.max_ratio {
            Some(ceiling) => assert!(
                ratio <= ceiling,
                "fixture {}: compression ratio {ratio:.4} exceeds locked ceiling {ceiling:.4} \
                 — the RDO search regressed (payload {payload} B over {} B PCM)",
                exp.dir,
                data.len()
            ),
            None => {
                // Incompressible content: the encoder must not balloon it.
                // VERBATIM plus per-frame/header overhead stays within a
                // few percent of the raw PCM size.
                assert!(
                    ratio <= 1.05,
                    "fixture {}: incompressible content inflated to ratio {ratio:.4} (>1.05)",
                    exp.dir
                );
            }
        }
        checked += 1;
    }
    assert_eq!(
        checked,
        FIXTURE_EXPECTATIONS.len(),
        "every fixture expectation must have been exercised"
    );
}

// --- RDO monotonicity property battery ---------------------------------

/// A deterministic xorshift PRNG so the property signals are reproducible
/// without pulling in a dependency.
struct XorShift(u64);
impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }
    fn noise(&mut self) -> f64 {
        ((self.next() >> 48) as i32 - 32768) as f64
    }
}

/// Build one block of interleaved mono S16 PCM bytes for a given signal
/// class. The classes deliberately span the regimes the encoder's search
/// must handle: tonal (LPC-friendly), transient (partition-adaptivity),
/// near-white noise (Rice-parameter / escape stress, LPC useless), and an
/// autoregressive process (genuine predictive structure).
fn signal_block(kind: u32, n: usize) -> Vec<u8> {
    let mut rng = XorShift(0x9E37_79B9_7F4A_7C15 ^ (kind as u64).wrapping_mul(0x1000_0001));
    let mut pcm = Vec::with_capacity(n * 2);
    if kind == 3 {
        // AR(2): x[i] = 1.6 x[i-1] - 0.72 x[i-2] + small noise.
        let (mut x1, mut x2) = (0.0f64, 0.0f64);
        for _ in 0..n {
            let e = rng.noise() * 0.05;
            let x = (1.6 * x1 - 0.72 * x2 + e).clamp(-32768.0, 32767.0);
            pcm.extend_from_slice(&(x.round() as i16).to_le_bytes());
            x2 = x1;
            x1 = x;
        }
        return pcm;
    }
    for i in 0..n {
        let t = i as f64;
        let s = match kind {
            // closely-spaced partials — leakage-sensitive, where the
            // window choice matters most.
            0 => {
                8000.0 * (2.0 * std::f64::consts::PI * t * 0.040).sin()
                    + 6000.0 * (2.0 * std::f64::consts::PI * t * 0.041).sin()
                    + 4000.0 * (2.0 * std::f64::consts::PI * t * 0.043).sin()
                    + 2000.0 * (2.0 * std::f64::consts::PI * t * 0.210).sin()
            }
            // transient bursts: loud/quiet alternation within a block,
            // which a finer partition order can exploit.
            1 => {
                let env = if (i / 512) % 2 == 0 { 1.0 } else { 0.08 };
                env * 12000.0 * (2.0 * std::f64::consts::PI * t * 0.07).sin()
            }
            // near-white noise: LPC can't help; the residual coder's
            // Rice-parameter and escape paths carry the whole load.
            _ => rng.noise() * 0.45,
        };
        let v = s.round().clamp(-32768.0, 32767.0) as i16;
        pcm.extend_from_slice(&v.to_le_bytes());
    }
    pcm
}

// A full default-size block (4096 interchannel samples, RFC 9639 §9.1):
// large enough to exercise multi-partition residual layouts and the full
// LPC-order / precision sweep, while keeping the order-32 × partition-15
// "rich" search in the combined property test inside a sane CI budget.
const PROPERTY_BLOCK_LEN: usize = 4096;
const PROPERTY_SR: u32 = 44_100;

/// Assert that for every signal class:
/// * encoding is bit-exact lossless (the recovered PCM equals the input);
/// * the "richer" option set never produces a *larger* payload than the
///   "leaner" one (the min-bit search can only shrink-or-tie when given
///   more candidates).
///
/// `lean`/`rich` are option builders; `rich` must be a superset of the
/// search space `lean` covers for the monotonicity claim to hold.
fn assert_monotone_and_lossless_on(
    label: &str,
    kinds: &[u32],
    lean: impl Fn() -> FlacEncoderOptions,
    rich: impl Fn() -> FlacEncoderOptions,
) {
    for &kind in kinds {
        let pcm = signal_block(kind, PROPERTY_BLOCK_LEN);
        let reference = pcm_to_i32(&pcm, SampleFormat::S16, 1);

        let (lean_bytes, lean_rec) = roundtrip(&pcm, 1, PROPERTY_SR, SampleFormat::S16, lean());
        let (rich_bytes, rich_rec) = roundtrip(&pcm, 1, PROPERTY_SR, SampleFormat::S16, rich());

        assert!(
            lean_rec == reference,
            "{label}/kind={kind}: lean option set is not lossless"
        );
        assert!(
            rich_rec == reference,
            "{label}/kind={kind}: rich option set is not lossless"
        );
        assert!(
            rich_bytes <= lean_bytes,
            "{label}/kind={kind}: richer search inflated output \
             ({rich_bytes} B > {lean_bytes} B) — the min-bit comparison regressed"
        );
    }
}

/// Every signal class. Used by the cheap (subset-ceiling) property tests
/// where the per-block search cost is small.
const ALL_KINDS: &[u32] = &[0, 1, 2, 3];

/// A tonal + a near-white-noise class. Used by the expensive
/// (order-32 / partition-15) property tests: the monotonicity invariant
/// holds per block independent of the signal, so two representatives that
/// bracket the LPC-friendly and LPC-useless regimes are enough coverage
/// without paying the full O(order²) sweep on every class.
const BRACKET_KINDS: &[u32] = &[0, 2];

fn assert_monotone_and_lossless(
    label: &str,
    lean: impl Fn() -> FlacEncoderOptions,
    rich: impl Fn() -> FlacEncoderOptions,
) {
    assert_monotone_and_lossless_on(label, ALL_KINDS, lean, rich);
}

/// The leaner three-window set (the historical default before the
/// Bartlett + Blackman-Harris pair joined the default).
fn three_windows() -> Vec<Apodization> {
    vec![
        Apodization::Welch,
        Apodization::Hann,
        Apodization::Tukey {
            alpha_num: 1,
            alpha_den: 4,
        },
    ]
}

/// The full five-window apodization set — what the default factory now
/// uses (the three windows above plus the two minimum-side-lobe
/// additions).
fn five_windows() -> Vec<Apodization> {
    let mut w = three_windows();
    w.push(Apodization::Bartlett);
    w.push(Apodization::BlackmanHarris);
    w
}

#[test]
fn extra_apodization_windows_never_inflate() {
    // The leaner three-window set vs the five-window default. Adding the
    // Bartlett + Blackman-Harris windows can only ever shrink-or-tie: a
    // window that never wins the per-subframe min-bit comparison is
    // discarded, so the five-window output is never larger. (This is also
    // why making the five-window set the default can only improve, or
    // leave unchanged, the bytes any caller emits.)
    assert_monotone_and_lossless(
        "windows",
        || {
            let mut o = FlacEncoderOptions::default();
            o.apodization = Some(three_windows());
            o
        },
        || {
            let mut o = FlacEncoderOptions::default();
            o.apodization = Some(five_windows());
            o
        },
    );
}

#[test]
fn higher_lpc_order_ceiling_never_inflates() {
    // Raising the LPC-order ceiling from the subset default (12) to the
    // spec maximum (32) only adds candidate orders to the search; orders
    // that don't earn their warm-up + coefficient overhead are discarded,
    // so the output can only shrink-or-tie.
    assert_monotone_and_lossless_on(
        "lpc_order",
        BRACKET_KINDS,
        FlacEncoderOptions::default,
        || {
            let mut o = FlacEncoderOptions::default();
            o.max_lpc_order = Some(32);
            o
        },
    );
}

#[test]
fn higher_partition_order_ceiling_never_inflates() {
    // Lifting the partition-order ceiling from the subset default (8) to
    // the full field range (15) only lets the residual coder consider
    // finer splits; a finer split is kept only when it encodes fewer
    // bits, so the output can only shrink-or-tie.
    assert_monotone_and_lossless("partition_order", FlacEncoderOptions::default, || {
        let mut o = FlacEncoderOptions::default();
        o.allow_non_subset_partition_order = true;
        o
    });
}

#[test]
fn all_knobs_combined_never_inflate_and_stay_lossless() {
    // The leanest legal search (single window, subset LPC + partition
    // ceilings) vs the richest (five windows, order 32, partition 15).
    // Every dimension can only shrink-or-tie, so the union does too.
    assert_monotone_and_lossless_on(
        "combined",
        BRACKET_KINDS,
        || {
            let mut o = FlacEncoderOptions::default();
            o.apodization = Some(vec![Apodization::Welch]);
            o
        },
        || {
            let mut o = FlacEncoderOptions::default();
            o.apodization = Some(five_windows());
            o.max_lpc_order = Some(32);
            o.allow_non_subset_partition_order = true;
            o
        },
    );
}
