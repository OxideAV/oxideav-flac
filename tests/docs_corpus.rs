//! Integration tests against the `docs/audio/flac/fixtures/` corpus.
//!
//! Each fixture under `../../docs/audio/flac/fixtures/<name>/` ships:
//! * `input.flac` — native FLAC stream (`fLaC` magic + metadata blocks
//!   + frames).
//! * `expected.wav` — reference PCM produced by FFmpeg's native FLAC
//!   decoder. The bit depth varies (8 / 16 / 24 / 32 bps); the
//!   container is RIFF/WAVE with either WAVEFORMAT (tag 0x0001) or
//!   WAVEFORMATEXTENSIBLE (tag 0xFFFE) carrying the real sample
//!   format in a sub-format GUID.
//! * `notes.md` + `trace.txt` — implementor notes (not consumed by
//!   this driver).
//!
//! Pipeline per fixture:
//! 1. Open `input.flac` through the public container registry, so the
//!    demuxer parses STREAMINFO into `params.extradata` + emits one
//!    packet per FLAC frame.
//! 2. Construct the FLAC decoder from those params and feed every
//!    packet, accumulating PCM samples per channel.
//! 3. Parse `expected.wav` (handles WAVEFORMAT + WAVEFORMATEXTENSIBLE
//!    + IEEE float subformat).
//! 4. Compare per channel: exact match count, max |diff|, RMS error,
//!    PSNR (in LSBs of the reference's bit depth).
//!
//! Tiering:
//! * `Tier::BitExact` — must decode bit-exactly. CI fails on any
//!   divergence. FLAC is lossless so any clean fixture should land
//!   here once it's been confirmed clean in `ReportOnly` mode.
//! * `Tier::ReportOnly` — log deltas, never gate CI. All fixtures
//!   start here per the brief and graduate after one CI round.
//!
//! The test logs `skip <name>: ...` and returns success when fixtures
//! aren't present (standalone-crate CI checkout has no `docs/`).

use std::fs;
use std::io::Cursor;
use std::path::PathBuf;

use oxideav_core::{ContainerRegistry, Error, Frame, NullCodecResolver, ReadSeek};
// `Box<dyn Decoder>` / `Box<dyn Demuxer>` resolve their trait methods
// through the dyn-vtable, so the `Decoder` / `Demuxer` traits don't
// need to be in scope at the call site here.

/// Locate `docs/audio/flac/fixtures/<name>/`. Tests run with CWD set
/// to the crate root, so we walk two levels up to reach the workspace
/// root and then into `docs/`.
fn fixture_dir(name: &str) -> PathBuf {
    PathBuf::from("../../docs/audio/flac/fixtures").join(name)
}

#[derive(Clone, Copy, Debug)]
#[allow(dead_code)]
enum Tier {
    /// Must produce sample-for-sample identical PCM. Test fails on
    /// any divergence. FLAC is lossless so this is the eventual
    /// landing zone for every well-formed fixture.
    BitExact,
    /// Decode is permitted to diverge from the FFmpeg reference;
    /// per-channel deltas are logged but not asserted. All fixtures
    /// start here per the brief.
    ReportOnly,
}

struct CorpusCase {
    name: &'static str,
    /// Expected channels (sanity-check vs STREAMINFO). None to skip.
    channels: Option<u16>,
    /// Expected sample rate. None to skip.
    sample_rate: Option<u32>,
    /// Expected bit depth. None to skip.
    bits_per_sample: Option<u16>,
    tier: Tier,
}

/// Decoded PCM expressed as i32 samples per channel (frame-major), so
/// the comparator can handle 8 / 16 / 24 / 32 bps without per-bps
/// branching.
struct DecodedPcm {
    /// Interleaved i32 samples (one entry per sample-per-channel).
    /// 8/16/24-bit decoder output is sign-extended into i32; 32-bit
    /// decoder output is the raw i32.
    samples: Vec<i32>,
    channels: u16,
    sample_rate: u32,
    bits_per_sample: u16,
}

/// Reference PCM extracted from `expected.wav`. Same shape as
/// `DecodedPcm`. Float subformat WAVs are mapped onto the FLAC i32
/// integer scale (multiplied by 2^31 then clamped).
struct RefPcm {
    samples: Vec<i32>,
    channels: u16,
    sample_rate: u32,
    bits_per_sample: u16,
    /// True when the WAV used IEEE-float subformat. Float-vs-int
    /// fixtures get a softer comparison floor (the FLAC bitstream is
    /// integer, so the reference is what's been reinterpreted).
    is_float: bool,
}

/// Per-channel diff numbers + aggregate match percentage and PSNR.
struct ChannelStat {
    rms_ref: f64,
    rms_ours: f64,
    rms_err: f64, // sum-of-squares until psnr_db converts back to MSE
    exact: usize,
    near: usize, // |delta| <= 1 LSB
    total: usize,
    max_abs_err: i64,
}

impl ChannelStat {
    fn new() -> Self {
        Self {
            rms_ref: 0.0,
            rms_ours: 0.0,
            rms_err: 0.0,
            exact: 0,
            near: 0,
            total: 0,
            max_abs_err: 0,
        }
    }

    fn match_pct(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.exact as f64 / self.total as f64 * 100.0
        }
    }

    fn near_pct(&self) -> f64 {
        if self.total == 0 {
            0.0
        } else {
            self.near as f64 / self.total as f64 * 100.0
        }
    }

    /// PSNR over the i32 full-scale (peak = 2^31 - 1). Returns
    /// `f64::INFINITY` on perfect match.
    fn psnr_db(&self, peak: f64) -> f64 {
        if self.total == 0 || self.rms_err == 0.0 {
            return f64::INFINITY;
        }
        let mse = self.rms_err / self.total as f64;
        10.0 * (peak * peak / mse).log10()
    }
}

/// Open `input.flac` through the `ContainerRegistry` and decode every
/// packet. Returns the accumulated PCM as i32-per-channel-sample plus
/// the channel count + sample rate + bit depth the demuxer advertised.
fn decode_fixture_pcm(case: &CorpusCase) -> Option<DecodedPcm> {
    let dir = fixture_dir(case.name);
    let flac_path = dir.join("input.flac");
    let bytes = match fs::read(&flac_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skip {}: missing {} ({e})", case.name, flac_path.display());
            return None;
        }
    };

    let mut reg = ContainerRegistry::new();
    oxideav_flac::register_containers(&mut reg);
    let cursor: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    let mut demux = match reg.open_demuxer("flac", cursor, &NullCodecResolver) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("skip {}: flac demuxer open failed: {e}", case.name);
            return None;
        }
    };

    let streams = demux.streams();
    if streams.is_empty() {
        eprintln!("skip {}: flac has no streams", case.name);
        return None;
    }
    let stream = streams[0].clone();
    let params = stream.params.clone();
    let channels = params.channels.unwrap_or(0);
    let sample_rate = params.sample_rate.unwrap_or(0);
    // FLAC's STREAMINFO bit depth round-trips through `sample_format`
    // (the demuxer maps 8/16/24/32 bps onto `SampleFormat::U8` /
    // `S16` / `S24` / `S32` — see `container::open_demuxer`). The byte
    // width of that variant is therefore the source of truth here.
    let bps = params
        .sample_format
        .map(|f| (f.bytes_per_sample() * 8) as u16)
        .unwrap_or(0);
    if channels == 0 || sample_rate == 0 || bps == 0 {
        eprintln!(
            "skip {}: stream advertises bogus channels/rate/bps ({channels}/{sample_rate}/{bps})",
            case.name
        );
        return None;
    }
    if let Some(want) = case.channels {
        assert_eq!(
            channels, want,
            "{}: STREAMINFO says {channels} channels, expected {want}",
            case.name
        );
    }
    if let Some(want) = case.sample_rate {
        assert_eq!(
            sample_rate, want,
            "{}: STREAMINFO says {sample_rate} Hz, expected {want}",
            case.name
        );
    }
    if let Some(want) = case.bits_per_sample {
        if bps != want {
            // Don't hard-fail — the FLAC container projects STREAMINFO's
            // bps onto a `SampleFormat` enum (U8 / S16 / S24 / S32), and
            // an unusual bit depth (e.g. 12 bps) would round to a wider
            // variant. Log so a follow-up can investigate.
            eprintln!(
                "{}: WARN STREAMINFO sample_format implies {bps} bps, expected {want}",
                case.name
            );
        }
    }

    let mut decoder = match oxideav_flac::decoder::make_decoder(&params) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("skip {}: decoder ctor failed: {e}", case.name);
            return None;
        }
    };

    let stream_index = stream.index;
    let mut samples: Vec<i32> = Vec::new();
    let mut decoder_errors = 0usize;
    // Decoder-emitted byte stride per sample. Set on first audio frame
    // and used to derive the *actual* output bit depth (FLAC's
    // STREAMINFO maps via `output_format` so 1..=16 bps → S16,
    // 17..=24 → S24, 25..=32 → S32).
    let mut decoder_stride: usize = 0;
    loop {
        let pkt = match demux.next_packet() {
            Ok(p) => p,
            Err(Error::Eof) => break,
            Err(e) => {
                eprintln!(
                    "{}: demux error after {} samples: {e}",
                    case.name,
                    samples.len()
                );
                break;
            }
        };
        if pkt.stream_index != stream_index {
            continue;
        }
        if let Err(e) = decoder.send_packet(&pkt) {
            decoder_errors += 1;
            if decoder_errors <= 3 {
                eprintln!("{}: send_packet error: {e}", case.name);
            }
            continue;
        }
        match decoder.receive_frame() {
            Ok(Frame::Audio(af)) => {
                // Derive the byte stride per sample from the frame:
                // af.samples * channels * stride == af.data[0].len().
                // This is robust to FLAC's STREAMINFO-vs-output-format
                // mapping (e.g. 8 bps STREAMINFO decodes into S16 PCM).
                let total = af.samples as usize * channels as usize;
                let stride = af.data[0].len().checked_div(total).unwrap_or(0);
                if decoder_stride == 0 {
                    decoder_stride = stride;
                }
                append_pcm_samples(&af.data[0], stride, &mut samples);
            }
            Ok(other) => {
                eprintln!("{}: unexpected non-audio frame: {other:?}", case.name);
            }
            Err(Error::NeedMore) => continue,
            Err(Error::Eof) => break,
            Err(e) => {
                decoder_errors += 1;
                if decoder_errors <= 3 {
                    eprintln!("{}: receive_frame error: {e}", case.name);
                }
            }
        }
    }
    if decoder_errors > 0 {
        eprintln!(
            "{}: total decoder errors: {decoder_errors} (decoded {} samples / {} per channel)",
            case.name,
            samples.len(),
            samples.len() / channels.max(1) as usize
        );
    }

    // Prefer the stride observed from actual decoder output (drives the
    // PSNR/diff-shift math); fall back to the param-level bps if the
    // decoder produced no audio frames at all.
    let actual_bps = if decoder_stride > 0 {
        (decoder_stride * 8) as u16
    } else {
        bps
    };

    Some(DecodedPcm {
        samples,
        channels,
        sample_rate,
        bits_per_sample: actual_bps,
    })
}

/// Convert the decoder's interleaved byte plane into i32 samples. The
/// FLAC decoder writes S16 / S24 (3-byte little-endian) / S32 little-
/// endian; the caller passes the stride derived from `af.samples` so
/// this function is bps-agnostic.
fn append_pcm_samples(plane: &[u8], stride: usize, out: &mut Vec<i32>) {
    if stride == 0 {
        return;
    }
    for chunk in plane.chunks_exact(stride) {
        let v: i32 = match stride {
            2 => i16::from_le_bytes([chunk[0], chunk[1]]) as i32,
            3 => {
                let mut v =
                    (chunk[0] as i32) | ((chunk[1] as i32) << 8) | ((chunk[2] as i32) << 16);
                if v & 0x0080_0000 != 0 {
                    v |= 0xFF00_0000_u32 as i32;
                }
                v
            }
            4 => i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]),
            _ => 0,
        };
        out.push(v);
    }
}

/// Parse a RIFF/WAVE header. Locates the `fmt ` chunk to read the
/// channel count + sample rate + bit depth + format tag (or the
/// SubFormat GUID's data1 for WAVEFORMATEXTENSIBLE), then returns the
/// `data` chunk decoded into i32 samples. PCM and IEEE-float are both
/// recognised; float samples are scaled into the i32 integer range so
/// the comparator can use the same code path.
fn parse_wav(bytes: &[u8]) -> Option<RefPcm> {
    if bytes.len() < 44 || &bytes[0..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return None;
    }
    let mut i = 12usize;
    let mut channels: u16 = 0;
    let mut sample_rate: u32 = 0;
    let mut bits_per_sample: u16 = 0;
    let mut format_tag: u16 = 0;
    let mut subformat_data1: u32 = 0;
    let mut have_extensible_subformat = false;
    let mut data: Option<&[u8]> = None;
    while i + 8 <= bytes.len() {
        let id = &bytes[i..i + 4];
        let sz =
            u32::from_le_bytes([bytes[i + 4], bytes[i + 5], bytes[i + 6], bytes[i + 7]]) as usize;
        let body_start = i + 8;
        let body_end = body_start + sz;
        if body_end > bytes.len() {
            break;
        }
        match id {
            b"fmt " => {
                if sz < 16 {
                    return None;
                }
                format_tag = u16::from_le_bytes([bytes[body_start], bytes[body_start + 1]]);
                channels = u16::from_le_bytes([bytes[body_start + 2], bytes[body_start + 3]]);
                sample_rate = u32::from_le_bytes([
                    bytes[body_start + 4],
                    bytes[body_start + 5],
                    bytes[body_start + 6],
                    bytes[body_start + 7],
                ]);
                bits_per_sample =
                    u16::from_le_bytes([bytes[body_start + 14], bytes[body_start + 15]]);
                if format_tag == 0xFFFE && sz >= 40 {
                    // WAVEFORMATEXTENSIBLE: the real sample format is
                    // the first u32 of the SubFormat GUID at body+24.
                    subformat_data1 = u32::from_le_bytes([
                        bytes[body_start + 24],
                        bytes[body_start + 25],
                        bytes[body_start + 26],
                        bytes[body_start + 27],
                    ]);
                    have_extensible_subformat = true;
                }
            }
            b"data" => {
                data = Some(&bytes[body_start..body_end]);
                break;
            }
            _ => {}
        }
        i = body_end + (sz & 1);
    }
    let data = data?;
    if channels == 0 || sample_rate == 0 || bits_per_sample == 0 {
        return None;
    }

    // Decide PCM vs IEEE float. WAVE_FORMAT_PCM = 1, WAVE_FORMAT_IEEE_FLOAT = 3.
    // For WAVEFORMATEXTENSIBLE we look at the SubFormat GUID's first u32.
    let effective_tag = if format_tag == 0xFFFE {
        if have_extensible_subformat {
            subformat_data1 as u16
        } else {
            // Some writers emit a 16-byte fmt chunk even with tag 0xFFFE;
            // assume PCM.
            1
        }
    } else {
        format_tag
    };
    let is_float = effective_tag == 3;
    if effective_tag != 1 && effective_tag != 3 {
        eprintln!(
            "  parse_wav: unsupported effective format tag 0x{:04x}",
            effective_tag
        );
        return None;
    }

    let mut samples: Vec<i32> =
        Vec::with_capacity(data.len() * 8 / bits_per_sample.max(1) as usize);
    if is_float {
        match bits_per_sample {
            32 => {
                for chunk in data.chunks_exact(4) {
                    let f = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
                    // Scale [-1, 1] f32 to i32 full-scale. Saturate to
                    // avoid wrap on out-of-range float values.
                    let scaled = (f as f64 * 2_147_483_648.0).round();
                    let v = if scaled >= 2_147_483_647.0 {
                        i32::MAX
                    } else if scaled <= -2_147_483_648.0 {
                        i32::MIN
                    } else {
                        scaled as i32
                    };
                    samples.push(v);
                }
            }
            64 => {
                for chunk in data.chunks_exact(8) {
                    let f = f64::from_le_bytes([
                        chunk[0], chunk[1], chunk[2], chunk[3], chunk[4], chunk[5], chunk[6],
                        chunk[7],
                    ]);
                    let scaled = (f * 2_147_483_648.0).round();
                    let v = if scaled >= 2_147_483_647.0 {
                        i32::MAX
                    } else if scaled <= -2_147_483_648.0 {
                        i32::MIN
                    } else {
                        scaled as i32
                    };
                    samples.push(v);
                }
            }
            _ => {
                eprintln!(
                    "  parse_wav: unsupported float bits_per_sample={}",
                    bits_per_sample
                );
                return None;
            }
        }
    } else {
        match bits_per_sample {
            8 => {
                // WAV 8-bit PCM is unsigned (offset 128).
                for &b in data {
                    samples.push((b as i32) - 128);
                }
            }
            16 => {
                for chunk in data.chunks_exact(2) {
                    samples.push(i16::from_le_bytes([chunk[0], chunk[1]]) as i32);
                }
            }
            24 => {
                for chunk in data.chunks_exact(3) {
                    let mut v =
                        (chunk[0] as i32) | ((chunk[1] as i32) << 8) | ((chunk[2] as i32) << 16);
                    if v & 0x0080_0000 != 0 {
                        v |= 0xFF00_0000_u32 as i32;
                    }
                    samples.push(v);
                }
            }
            32 => {
                for chunk in data.chunks_exact(4) {
                    samples.push(i32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]));
                }
            }
            other => {
                eprintln!("  parse_wav: unsupported pcm bits_per_sample={other}");
                return None;
            }
        }
    }

    Some(RefPcm {
        samples,
        channels,
        sample_rate,
        bits_per_sample,
        is_float,
    })
}

fn read_reference(case: &CorpusCase) -> Option<RefPcm> {
    let dir = fixture_dir(case.name);
    let wav_path = dir.join("expected.wav");
    let bytes = match fs::read(&wav_path) {
        Ok(b) => b,
        Err(e) => {
            eprintln!("skip {}: missing {} ({e})", case.name, wav_path.display());
            return None;
        }
    };
    parse_wav(&bytes)
}

/// Compute per-channel match/PSNR statistics. Compares only the
/// overlapping prefix of decoded vs reference. If our decoder emits
/// fewer-bps samples than the reference (e.g. 24-bit ours vs 32-bit
/// reference float), the comparator left-shifts ours to land on the
/// same scale.
fn compare(ours: &DecodedPcm, refp: &RefPcm) -> Vec<ChannelStat> {
    let chs = ours.channels.min(refp.channels) as usize;
    if chs == 0 {
        return Vec::new();
    }
    let frames_ours = ours.samples.len() / ours.channels.max(1) as usize;
    let frames_ref = refp.samples.len() / refp.channels.max(1) as usize;
    let n = frames_ours.min(frames_ref);

    // Align scales so a 16 vs 16 bps comparison stays in 16-bit LSBs
    // and a 24 vs 32 bps comparison gets ours << 8 to match the
    // reference's i32-scaled samples.
    let our_shift = (refp.bits_per_sample as i32 - ours.bits_per_sample as i32).max(0);
    let ref_shift = (ours.bits_per_sample as i32 - refp.bits_per_sample as i32).max(0);

    let mut stats: Vec<ChannelStat> = (0..chs).map(|_| ChannelStat::new()).collect();
    for f in 0..n {
        for (ch, s) in stats.iter_mut().enumerate() {
            let our = (ours.samples[f * ours.channels as usize + ch] as i64) << our_shift;
            let r = (refp.samples[f * refp.channels as usize + ch] as i64) << ref_shift;
            let err = (our - r).abs();
            s.total += 1;
            if err == 0 {
                s.exact += 1;
            }
            if err <= 1 {
                s.near += 1;
            }
            if err > s.max_abs_err {
                s.max_abs_err = err;
            }
            s.rms_ref += (r as f64) * (r as f64);
            s.rms_ours += (our as f64) * (our as f64);
            s.rms_err += (err as f64) * (err as f64);
        }
    }
    for s in &mut stats {
        if s.total > 0 {
            s.rms_ref = (s.rms_ref / s.total as f64).sqrt();
            s.rms_ours = (s.rms_ours / s.total as f64).sqrt();
            // s.rms_err kept as sum-of-squares for psnr_db.
        }
    }
    stats
}

/// Decode → compare → log → tier-aware assert.
fn evaluate(case: &CorpusCase) {
    eprintln!("--- {} (tier={:?}) ---", case.name, case.tier);
    let Some(ours) = decode_fixture_pcm(case) else {
        return;
    };
    let Some(refp) = read_reference(case) else {
        eprintln!("{}: could not parse expected.wav", case.name);
        return;
    };

    eprintln!(
        "{}: decoded ch={} sr={} bps={} samples={} ({} frames); reference ch={} sr={} bps={}{} samples={} ({} frames)",
        case.name,
        ours.channels,
        ours.sample_rate,
        ours.bits_per_sample,
        ours.samples.len(),
        ours.samples.len() / ours.channels.max(1) as usize,
        refp.channels,
        refp.sample_rate,
        refp.bits_per_sample,
        if refp.is_float { " (float)" } else { "" },
        refp.samples.len(),
        refp.samples.len() / refp.channels.max(1) as usize,
    );

    if ours.channels != refp.channels {
        eprintln!(
            "{}: WARN channel count mismatch (decoded {} vs reference {})",
            case.name, ours.channels, refp.channels
        );
    }
    if ours.sample_rate != refp.sample_rate {
        eprintln!(
            "{}: WARN sample-rate mismatch (decoded {} vs reference {})",
            case.name, ours.sample_rate, refp.sample_rate
        );
    }

    let stats = compare(&ours, &refp);
    if stats.is_empty() {
        eprintln!("{}: no overlapping channels to compare", case.name);
        return;
    }

    let peak_bps = ours.bits_per_sample.max(refp.bits_per_sample);
    let peak = ((1u64 << (peak_bps - 1)) - 1) as f64;

    let mut total_exact = 0usize;
    let mut total_near = 0usize;
    let mut total_samples = 0usize;
    let mut max_err_overall: i64 = 0;
    let mut psnr_min: f64 = f64::INFINITY;
    for (i, s) in stats.iter().enumerate() {
        let psnr = s.psnr_db(peak);
        if psnr < psnr_min {
            psnr_min = psnr;
        }
        let rms_err_disp = if s.total > 0 {
            (s.rms_err / s.total as f64).sqrt()
        } else {
            0.0
        };
        eprintln!(
            "  ch{i}: rms_ref={:.1} rms_ours={:.1} rms_err={:.2} match={:.4}% near<=1LSB={:.4}% max_abs_err={} psnr={:.2} dB",
            s.rms_ref,
            s.rms_ours,
            rms_err_disp,
            s.match_pct(),
            s.near_pct(),
            s.max_abs_err,
            psnr,
        );
        total_exact += s.exact;
        total_near += s.near;
        total_samples += s.total;
        if s.max_abs_err > max_err_overall {
            max_err_overall = s.max_abs_err;
        }
    }
    let agg_pct = if total_samples > 0 {
        total_exact as f64 / total_samples as f64 * 100.0
    } else {
        0.0
    };
    let near_pct = if total_samples > 0 {
        total_near as f64 / total_samples as f64 * 100.0
    } else {
        0.0
    };
    eprintln!(
        "{}: aggregate match={:.4}% near<=1LSB={:.4}% max_abs_err={} min_psnr={:.2} dB",
        case.name, agg_pct, near_pct, max_err_overall, psnr_min,
    );

    match case.tier {
        Tier::BitExact => {
            assert_eq!(
                total_exact, total_samples,
                "{}: not bit-exact (max_abs_err={} match={:.4}%)",
                case.name, max_err_overall, agg_pct,
            );
        }
        Tier::ReportOnly => {
            // Logged; never gates CI. Fixtures graduate to BitExact
            // after one CI round confirms a 100.00% match.
        }
    }
}

// ---------------------------------------------------------------------------
// Per-fixture tests — every entry maps 1:1 to a directory under
// docs/audio/flac/fixtures/. All start `Tier::ReportOnly` per the
// brief. FLAC is lossless so any well-formed fixture should round-trip
// bit-exactly through our decoder; the few that don't are real bugs in
// the decoder or container parser.
// ---------------------------------------------------------------------------

#[test]
fn corpus_constant_subframe_silence() {
    evaluate(&CorpusCase {
        name: "constant-subframe-silence",
        channels: Some(1),
        sample_rate: Some(44_100),
        bits_per_sample: Some(16),
        tier: Tier::ReportOnly,
    });
}

#[test]
fn corpus_fixed_subframe_low_order() {
    evaluate(&CorpusCase {
        name: "fixed-subframe-low-order",
        channels: Some(1),
        sample_rate: Some(44_100),
        bits_per_sample: Some(16),
        tier: Tier::ReportOnly,
    });
}

#[test]
fn corpus_left_right_channel_pair() {
    evaluate(&CorpusCase {
        name: "left-right-channel-pair",
        channels: Some(2),
        sample_rate: Some(44_100),
        bits_per_sample: Some(16),
        tier: Tier::ReportOnly,
    });
}

#[test]
fn corpus_left_side_channel_pair() {
    evaluate(&CorpusCase {
        name: "left-side-channel-pair",
        channels: Some(2),
        sample_rate: Some(44_100),
        bits_per_sample: Some(16),
        tier: Tier::ReportOnly,
    });
}

#[test]
fn corpus_lpc_subframe_typical_music() {
    evaluate(&CorpusCase {
        name: "lpc-subframe-typical-music",
        channels: Some(1),
        sample_rate: Some(44_100),
        bits_per_sample: Some(16),
        tier: Tier::ReportOnly,
    });
}

#[test]
fn corpus_mid_side_channel_pair() {
    evaluate(&CorpusCase {
        name: "mid-side-channel-pair",
        channels: Some(2),
        sample_rate: Some(44_100),
        bits_per_sample: Some(16),
        tier: Tier::ReportOnly,
    });
}

#[test]
fn corpus_mono_16bit_44100_fixed_blocksize() {
    evaluate(&CorpusCase {
        name: "mono-16bit-44100-fixed-blocksize",
        channels: Some(1),
        sample_rate: Some(44_100),
        bits_per_sample: Some(16),
        tier: Tier::ReportOnly,
    });
}

#[test]
fn corpus_mono_8bit_22050() {
    // notes.md asks ffmpeg for `-bits_per_raw_sample 8 -sample_fmt s16`,
    // and the resulting STREAMINFO actually carries 16 bps (the file
    // command also reports "16 bit"). The "8bit" name reflects the
    // *raw* sample bit depth at the encoder input.
    evaluate(&CorpusCase {
        name: "mono-8bit-22050",
        channels: Some(1),
        sample_rate: Some(22_050),
        bits_per_sample: Some(16),
        tier: Tier::ReportOnly,
    });
}

#[test]
fn corpus_right_side_channel_pair() {
    evaluate(&CorpusCase {
        name: "right-side-channel-pair",
        channels: Some(2),
        sample_rate: Some(44_100),
        bits_per_sample: Some(16),
        tier: Tier::ReportOnly,
    });
}

#[test]
fn corpus_stereo_16bit_44100_fixed() {
    evaluate(&CorpusCase {
        name: "stereo-16bit-44100-fixed",
        channels: Some(2),
        sample_rate: Some(44_100),
        bits_per_sample: Some(16),
        tier: Tier::ReportOnly,
    });
}

#[test]
fn corpus_stereo_16bit_44100_small_blocksize() {
    evaluate(&CorpusCase {
        name: "stereo-16bit-44100-small-blocksize",
        channels: Some(2),
        sample_rate: Some(44_100),
        bits_per_sample: Some(16),
        tier: Tier::ReportOnly,
    });
}

#[test]
fn corpus_stereo_24bit_96000() {
    evaluate(&CorpusCase {
        name: "stereo-24bit-96000",
        channels: Some(2),
        sample_rate: Some(96_000),
        bits_per_sample: Some(24),
        tier: Tier::ReportOnly,
    });
}

#[test]
fn corpus_stereo_32bit_192000() {
    // Despite the fixture name, ffmpeg actually muxes the FLAC stream
    // at 24 bps (the `file` command and STREAMINFO both report 24).
    // The reference WAV is float32 (KSDATAFORMAT_SUBTYPE_FLOAT) at
    // 192 kHz — parse_wav scales floats to the i32 grid so the
    // per-channel comparator stays meaningful.
    evaluate(&CorpusCase {
        name: "stereo-32bit-192000",
        channels: Some(2),
        sample_rate: Some(192_000),
        bits_per_sample: Some(24),
        tier: Tier::ReportOnly,
    });
}

#[test]
fn corpus_surround_7_1_24bit_48000() {
    evaluate(&CorpusCase {
        name: "surround-7_1-24bit-48000",
        channels: Some(8),
        sample_rate: Some(48_000),
        bits_per_sample: Some(24),
        tier: Tier::ReportOnly,
    });
}

#[test]
fn corpus_verbatim_subframe_noise() {
    evaluate(&CorpusCase {
        name: "verbatim-subframe-noise",
        channels: Some(1),
        sample_rate: Some(44_100),
        bits_per_sample: Some(16),
        tier: Tier::ReportOnly,
    });
}

#[test]
fn corpus_with_padding_block() {
    evaluate(&CorpusCase {
        name: "with-padding-block",
        channels: Some(1),
        sample_rate: Some(44_100),
        bits_per_sample: Some(16),
        tier: Tier::ReportOnly,
    });
}

#[test]
fn corpus_with_picture_block() {
    evaluate(&CorpusCase {
        name: "with-picture-block",
        channels: Some(1),
        sample_rate: Some(44_100),
        bits_per_sample: Some(16),
        tier: Tier::ReportOnly,
    });
}

#[test]
fn corpus_with_vorbis_comment() {
    evaluate(&CorpusCase {
        name: "with-vorbis-comment",
        channels: Some(1),
        sample_rate: Some(44_100),
        bits_per_sample: Some(16),
        tier: Tier::ReportOnly,
    });
}
