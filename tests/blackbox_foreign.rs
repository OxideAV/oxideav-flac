//! Foreign → ours: streams written by outside encoders, decoded by this
//! crate and compared sample for sample with the PCM that went in, with
//! the black-box decoder's output, and with the STREAMINFO MD5.
//!
//! Every option of the outside encoders that changes the bitstream is
//! swept: bit depths 4..=32, 1..=8 channels, the stereo layouts, sample
//! rates in each of the frame-header encodings, block sizes, compression
//! levels, predictor orders and precisions, residual partition orders,
//! analysis windows, wasted bits, and signals that force constant,
//! verbatim and full-range subframes.
//!
//! The tools are opaque processes (PCM in, `.flac` out). Each family
//! skips when its tool is not installed. A case the outside encoder
//! itself refuses to write is counted as skipped, not passed.

mod common;

use std::sync::Mutex;

use common::*;
use oxideav_flac::verify::{VerifyOutcome, verify_stream};

static CENSUS: Mutex<Option<Census>> = Mutex::new(None);

#[derive(Clone, Copy, PartialEq)]
enum Tool {
    Flac,
    Ffmpeg,
    Sox,
}

struct Case {
    name: String,
    tool: Tool,
    bps: u32,
    channels: usize,
    rate: u32,
    samples: usize,
    signal: Signal,
    args: Vec<String>,
}

fn case(tool: Tool, bps: u32, channels: usize, rate: u32, samples: usize, signal: Signal) -> Case {
    Case {
        name: String::new(),
        tool,
        bps,
        channels,
        rate,
        samples,
        signal,
        args: Vec::new(),
    }
}

impl Case {
    fn args(mut self, args: &[&str]) -> Self {
        self.args.extend(args.iter().map(|a| a.to_string()));
        self
    }

    fn named(mut self, family: &str) -> Self {
        let tool = match self.tool {
            Tool::Flac => "flac",
            Tool::Ffmpeg => "ffmpeg",
            Tool::Sox => "sox",
        };
        self.name = format!(
            "{family} {tool} {}bit {}ch {}Hz {}smp {:?} [{}]",
            self.bps,
            self.channels,
            self.rate,
            self.samples,
            self.signal,
            self.args.join(" ")
        );
        self
    }
}

enum Outcome {
    Pass,
    /// The outside encoder would not write this stream.
    Refused(String),
    Fail(String),
}

/// Write the outside encoder's output for `case` to `out`.
fn foreign_encode(
    scratch: &Scratch,
    case: &Case,
    planes: &[Vec<i32>],
    out: &std::path::Path,
) -> Result<(), String> {
    let wav = scratch.path("in.wav");
    std::fs::write(&wav, wav_bytes(planes, case.bps, case.rate, &[])).map_err(|e| e.to_string())?;
    let mut args: Vec<String> = Vec::new();
    match case.tool {
        Tool::Flac => {
            args.extend(["-s", "-f", "--lax", "--channel-map=none"].map(String::from));
            args.extend(case.args.iter().cloned());
            args.extend(["-o".to_string(), s(out), s(&wav)]);
            run("flac", &args).map(|_| ())
        }
        Tool::Ffmpeg => {
            args.extend(["-v", "error", "-y", "-i"].map(String::from));
            args.push(s(&wav));
            args.extend(["-c:a", "flac", "-strict", "experimental"].map(String::from));
            args.extend(case.args.iter().cloned());
            args.extend(["-f".to_string(), "flac".to_string(), s(out)]);
            run("ffmpeg", &args).map(|_| ())
        }
        Tool::Sox => {
            args.push(s(&wav));
            args.extend(case.args.iter().cloned());
            args.push(s(out));
            run("sox", &args).map(|_| ())
        }
    }
}

fn first_difference(ours: &[u8], want: &[u8], stride: usize) -> String {
    if ours.len() != want.len() {
        return format!("decoded {} PCM bytes, expected {}", ours.len(), want.len());
    }
    let at = ours.iter().zip(want).position(|(a, b)| a != b).unwrap_or(0);
    format!("first PCM difference at sample frame {}", at / stride)
}

fn check(scratch: &Scratch, case: &Case) -> Outcome {
    let planes = signal(
        case.signal,
        case.bps,
        case.channels,
        case.samples,
        case.bps as u64 * 131 + case.channels as u64,
    );
    let out = scratch.path("foreign.flac");
    let _ = std::fs::remove_file(&out);
    if let Err(e) = foreign_encode(scratch, case, &planes, &out) {
        return Outcome::Refused(e);
    }
    let file = match std::fs::read(&out) {
        Ok(f) => f,
        Err(e) => return Outcome::Refused(format!("no output: {e}")),
    };
    let want = pack(&planes, case.bps);
    let stride = container_bytes(case.bps) * case.channels;

    // The black-box decoder must agree with the input first: a case the
    // outside tools do not carry losslessly proves nothing about us.
    match tool_decode(scratch, &out) {
        Ok(theirs) if theirs == want => {}
        Ok(theirs) => {
            return Outcome::Refused(format!(
                "outside tools are not lossless here: {}",
                first_difference(&theirs, &want, stride)
            ));
        }
        Err(e) => return Outcome::Refused(format!("black-box decoder: {e}")),
    }

    let ours = match decode_file(&file) {
        Ok(d) => d,
        Err(e) => return Outcome::Fail(e),
    };
    if ours.format != format_for(case.bps) {
        return Outcome::Fail(format!("sample format {:?}", ours.format));
    }
    if ours.channels != case.channels || ours.rate != case.rate {
        return Outcome::Fail(format!(
            "stream reported as {} channels at {} Hz",
            ours.channels, ours.rate
        ));
    }
    if ours.pcm != want {
        return Outcome::Fail(first_difference(&ours.pcm, &want, stride));
    }
    let counted = {
        let mut guard = CENSUS.lock().unwrap();
        guard.get_or_insert_with(Census::default).take(&file)
    };
    if let Err(e) = counted {
        return Outcome::Fail(format!("census: {e}"));
    }
    match verify_stream(&file) {
        Ok(VerifyOutcome::Match) => Outcome::Pass,
        Ok(other) => Outcome::Fail(format!("MD5 check: {other:?}")),
        Err(e) => Outcome::Fail(format!("MD5 check: {e}")),
    }
}

/// Cases run, and the failures among them.
type FamilyResult = (usize, Vec<String>);

fn run_family(family: &str, mut cases: Vec<Case>) -> FamilyResult {
    let (ffmpeg, sox) = (have_ffmpeg(), have_sox());
    cases.retain(|c| match c.tool {
        Tool::Flac => true,
        Tool::Ffmpeg => ffmpeg,
        Tool::Sox => sox,
    });
    let scratch = Scratch::new("foreign");
    let mut tally = Tally::default();
    let mut refused = Vec::new();
    for case in cases {
        let case = case.named(family);
        match check(&scratch, &case) {
            Outcome::Pass => tally.record(&case.name, Ok(())),
            Outcome::Refused(why) => refused.push(format!("{}: {why}", case.name)),
            Outcome::Fail(why) => tally.record(&case.name, Err(why)),
        }
    }
    if !refused.is_empty() {
        eprintln!(
            "{family}: {} case(s) the outside encoder would not write",
            refused.len()
        );
        if std::env::var_os("OXIDEAV_FLAC_MATRIX_VERBOSE").is_some() {
            for r in &refused {
                eprintln!("  refused: {r}");
            }
        }
    }
    eprintln!(
        "{family}: {}/{} cases pass",
        tally.run - tally.failures.len(),
        tally.run
    );
    (tally.run, tally.failures)
}

const N: usize = 4096 * 2 + 1234;

fn every_bit_depth_and_channel_count() -> FamilyResult {
    let mut cases = Vec::new();
    for bps in 4..=32 {
        for channels in [1, 2] {
            for sig in [Signal::Tone, Signal::Extremes, Signal::Noise] {
                cases.push(case(Tool::Flac, bps, channels, 44_100, 5000, sig));
            }
        }
    }
    for bps in [8, 12, 16, 20, 24, 32] {
        for channels in 3..=8 {
            cases.push(case(Tool::Flac, bps, channels, 48_000, 5000, Signal::Tone));
        }
    }
    for bps in [5, 13, 17, 23, 27, 31] {
        cases.push(case(Tool::Flac, bps, 8, 96_000, 3000, Signal::Tone).args(&["-8"]));
    }
    run_family("depth x channels", cases)
}

fn every_compression_level() -> FamilyResult {
    let mut cases = Vec::new();
    for level in 0..=8 {
        let flag = format!("-{level}");
        for (bps, channels) in [(8, 1), (16, 2), (24, 2), (32, 2), (16, 6), (24, 8), (20, 2)] {
            cases.push(case(Tool::Flac, bps, channels, 44_100, N, Signal::Tone).args(&[&flag]));
        }
        cases.push(case(Tool::Flac, 16, 2, 44_100, N, Signal::Clicks).args(&[&flag]));
        cases.push(case(Tool::Flac, 16, 2, 44_100, N, Signal::Tone).args(&[&flag, "-e"]));
        cases.push(case(Tool::Flac, 24, 2, 96_000, N, Signal::Tone).args(&[&flag, "-p"]));
        cases.push(
            case(Tool::Flac, 16, 2, 44_100, N, Signal::Tone).args(&[&flag, "--limit-min-bitrate"]),
        );
    }
    run_family("levels", cases)
}

fn sample_rates_in_every_header_encoding() -> FamilyResult {
    // Table codes, whole kHz in 8 bits, Hz in 16 bits, tens of Hz in 16
    // bits, and rates only STREAMINFO can state.
    let rates = [
        1, 7, 4000, 8000, 11_025, 12_000, 16_000, 22_050, 24_000, 32_000, 37_800, 44_100, 44_101,
        48_000, 50_000, 64_000, 65_535, 65_536, 65_540, 88_200, 96_000, 100_000, 123_450, 176_400,
        192_000, 255_000, 256_000, 352_800, 384_000, 655_349, 655_350, 655_351, 768_000, 1_048_575,
    ];
    let mut cases = Vec::new();
    for rate in rates {
        cases.push(case(Tool::Flac, 16, 2, rate, 6000, Signal::Tone));
        cases.push(case(Tool::Flac, 24, 1, rate, 6000, Signal::Tone).args(&["-b", "1024"]));
    }
    run_family("sample rates", cases)
}

fn block_sizes() -> FamilyResult {
    let sizes = [
        16, 17, 31, 32, 100, 192, 255, 256, 257, 512, 576, 1000, 1024, 1152, 2048, 2304, 4096,
        4608, 4609, 8192, 9216, 16_384, 18_432, 32_768, 50_000, 65_534, 65_535,
    ];
    let mut cases = Vec::new();
    for b in sizes {
        let flag = b.to_string();
        // Whole blocks, one sample over, one sample short, less than a block.
        for samples in [b * 3, b * 2 + 1, b * 2 - 1, b / 2 + 1] {
            let samples = samples.min(140_000);
            cases.push(case(Tool::Flac, 16, 2, 44_100, samples, Signal::Tone).args(&["-b", &flag]));
        }
        cases.push(case(Tool::Flac, 24, 3, 96_000, b + 7, Signal::Clicks).args(&["-b", &flag]));
    }
    for samples in [1, 2, 3, 15, 16, 17] {
        for channels in [1, 2, 8] {
            cases.push(case(
                Tool::Flac,
                16,
                channels,
                44_100,
                samples,
                Signal::Tone,
            ));
            cases.push(case(
                Tool::Flac,
                32,
                channels,
                44_100,
                samples,
                Signal::Extremes,
            ));
        }
    }
    run_family("block sizes", cases)
}

fn predictor_orders_and_precisions() -> FamilyResult {
    let mut cases = Vec::new();
    for order in 0..=32 {
        let l = order.to_string();
        cases.push(case(Tool::Flac, 16, 2, 44_100, N, Signal::Tone).args(&["-l", &l, "-e"]));
        cases.push(
            case(Tool::Flac, 24, 1, 96_000, N, Signal::Tone).args(&["-l", &l, "-b", "16384"]),
        );
        cases.push(case(Tool::Flac, 32, 2, 48_000, 5000, Signal::Tone).args(&["-l", &l, "-m"]));
    }
    for q in 5..=15 {
        let q = q.to_string();
        for (bps, l) in [(16, "8"), (24, "32"), (32, "12"), (8, "4")] {
            cases
                .push(case(Tool::Flac, bps, 2, 44_100, N, Signal::Tone).args(&["-l", l, "-q", &q]));
        }
    }
    run_family("predictors", cases)
}

fn residual_partition_orders() -> FamilyResult {
    let mut cases = Vec::new();
    for order in 0..=15 {
        let exact = format!("{order},{order}");
        let upto = format!("0,{order}");
        for (bps, sig) in [
            (16, Signal::Tone),
            (16, Signal::Clicks),
            (24, Signal::Clicks),
            (32, Signal::Noise),
        ] {
            cases.push(
                case(Tool::Flac, bps, 2, 44_100, 70_000, sig).args(&["-b", "32768", "-r", &exact]),
            );
            cases.push(case(Tool::Flac, bps, 1, 44_100, N, sig).args(&["-r", &upto]));
        }
    }
    run_family("partition orders", cases)
}

fn analysis_windows() -> FamilyResult {
    let windows = [
        "bartlett",
        "bartlett_hann",
        "blackman",
        "blackman_harris_4term_92db",
        "connes",
        "flattop",
        "gauss(0.2)",
        "hamming",
        "hann",
        "kaiser_bessel",
        "nuttall",
        "rectangle",
        "triangle",
        "tukey(0.25)",
        "partial_tukey(2)",
        "punchout_tukey(3)",
        "subdivide_tukey(5)",
        "welch",
        "tukey(0.5);partial_tukey(2);punchout_tukey(3)",
    ];
    let mut cases = Vec::new();
    for w in windows {
        cases.push(case(Tool::Flac, 16, 2, 44_100, N, Signal::Tone).args(&["-8", "-A", w]));
        cases.push(case(Tool::Flac, 24, 1, 48_000, N, Signal::Clicks).args(&["-5", "-A", w]));
    }
    run_family("windows", cases)
}

fn stereo_layouts() -> FamilyResult {
    let signals = [
        Signal::Tone,
        Signal::Same,
        Signal::Inverted,
        Signal::LeftOnly,
        Signal::RightOnly,
        Signal::WideSide,
        Signal::Extremes,
        Signal::Noise,
    ];
    let mut cases = Vec::new();
    for sig in signals {
        for bps in [4, 8, 12, 16, 20, 24, 31, 32] {
            for mode in ["-m", "-M", "--no-mid-side"] {
                cases.push(case(Tool::Flac, bps, 2, 44_100, 9000, sig).args(&[mode, "-b", "1024"]));
            }
        }
        // The second outside encoder can be told which layout to write.
        for bps in [16, 24, 32] {
            for mode in ["indep", "left_side", "right_side", "mid_side"] {
                cases.push(case(Tool::Ffmpeg, bps, 2, 44_100, 9000, sig).args(&["-ch_mode", mode]));
            }
        }
    }
    run_family("stereo layouts", cases)
}

fn subframe_kinds_and_wasted_bits() -> FamilyResult {
    let mut cases = Vec::new();
    for bps in [4, 8, 12, 16, 20, 24, 32] {
        for channels in [1, 2, 5] {
            for sig in [
                Signal::Silence,
                Signal::Dc,
                Signal::Noise,
                Signal::Ramp,
                Signal::Extremes,
                Signal::Clicks,
            ] {
                cases.push(case(Tool::Flac, bps, channels, 44_100, 6000, sig));
            }
        }
        for k in 1..bps {
            cases.push(case(Tool::Flac, bps, 2, 44_100, 5000, Signal::Wasted(k)));
        }
    }
    for k in [1, 4, 8, 15] {
        cases.push(case(Tool::Ffmpeg, 16, 2, 44_100, 5000, Signal::Wasted(k)));
        cases.push(case(
            Tool::Ffmpeg,
            24,
            2,
            44_100,
            5000,
            Signal::Wasted(k + 8),
        ));
    }
    run_family("subframe kinds", cases)
}

fn second_encoder_options() -> FamilyResult {
    let mut cases = Vec::new();
    for level in 0..=12 {
        let l = level.to_string();
        for (bps, channels) in [(16, 1), (16, 2), (24, 2), (32, 2), (16, 6), (24, 8)] {
            cases.push(
                case(Tool::Ffmpeg, bps, channels, 48_000, N, Signal::Tone)
                    .args(&["-compression_level", &l]),
            );
        }
    }
    for lpc in ["none", "fixed", "levinson", "cholesky"] {
        for method in ["estimation", "2level", "4level", "8level", "search", "log"] {
            cases.push(case(Tool::Ffmpeg, 16, 2, 44_100, N, Signal::Tone).args(&[
                "-lpc_type",
                lpc,
                "-prediction_order_method",
                method,
            ]));
        }
    }
    for precision in 1..=15 {
        let p = precision.to_string();
        cases.push(case(Tool::Ffmpeg, 24, 2, 44_100, N, Signal::Tone).args(&[
            "-lpc_type",
            "levinson",
            "-lpc_coeff_precision",
            &p,
        ]));
    }
    for order in [1, 2, 5, 12, 13, 20, 31, 32] {
        let o = order.to_string();
        cases.push(case(Tool::Ffmpeg, 16, 2, 44_100, N, Signal::Tone).args(&[
            "-lpc_type",
            "levinson",
            "-min_prediction_order",
            &o,
            "-max_prediction_order",
            &o,
        ]));
        cases.push(case(Tool::Ffmpeg, 32, 2, 96_000, N, Signal::Tone).args(&[
            "-lpc_type",
            "cholesky",
            "-lpc_passes",
            "3",
            "-min_prediction_order",
            &o,
            "-max_prediction_order",
            &o,
        ]));
    }
    for part in 0..=8 {
        let p = part.to_string();
        cases.push(case(Tool::Ffmpeg, 16, 2, 44_100, N, Signal::Clicks).args(&[
            "-min_partition_order",
            &p,
            "-max_partition_order",
            &p,
        ]));
    }
    for frame in [16, 17, 256, 1000, 4096, 4608, 16_384, 65_535] {
        let f = frame.to_string();
        cases.push(
            case(Tool::Ffmpeg, 16, 2, 44_100, 70_000, Signal::Tone).args(&["-frame_size", &f]),
        );
    }
    cases.push(case(Tool::Ffmpeg, 16, 2, 44_100, N, Signal::Tone).args(&["-multi_dim_quant", "1"]));
    cases.push(
        case(Tool::Ffmpeg, 24, 2, 44_100, N, Signal::Clicks).args(&["-exact_rice_parameters", "1"]),
    );
    for rate in [8000, 11_025, 44_100, 96_000, 192_000, 384_000, 123_457] {
        cases.push(case(Tool::Ffmpeg, 16, 2, rate, N, Signal::Tone));
    }
    run_family("second encoder", cases)
}

fn third_encoder_options() -> FamilyResult {
    let mut cases = Vec::new();
    for level in 0..=8 {
        let l = level.to_string();
        for (bps, channels) in [(8, 1), (16, 2), (24, 2), (16, 4)] {
            cases.push(case(Tool::Sox, bps, channels, 44_100, N, Signal::Tone).args(&["-C", &l]));
        }
    }
    run_family("third encoder", cases)
}

/// The whole matrix. Needs `flac`; the families that use a second or
/// third outside encoder leave those cases out when it is missing.
#[test]
fn foreign_streams_decode_exactly() {
    if !have_flac() {
        eprintln!("skip: the black-box `flac` tool is not installed");
        return;
    }
    let families: [fn() -> FamilyResult; 11] = [
        every_bit_depth_and_channel_count,
        every_compression_level,
        sample_rates_in_every_header_encoding,
        block_sizes,
        predictor_orders_and_precisions,
        residual_partition_orders,
        analysis_windows,
        stereo_layouts,
        subframe_kinds_and_wasted_bits,
        second_encoder_options,
        third_encoder_options,
    ];
    let next = std::sync::atomic::AtomicUsize::new(0);
    let results: Mutex<Vec<FamilyResult>> = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                loop {
                    let i = next.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    let Some(family) = families.get(i) else { break };
                    let r = family();
                    results.lock().unwrap().push(r);
                }
            });
        }
    });
    let results = results.into_inner().unwrap();
    let run: usize = results.iter().map(|r| r.0).sum();
    let failures: Vec<&String> = results.iter().flat_map(|r| &r.1).collect();
    eprintln!(
        "foreign matrix: {}/{} cases pass",
        run - failures.len(),
        run
    );

    let census = CENSUS.lock().unwrap().take().unwrap_or_default();
    eprintln!(
        "census of the outside encoders' streams: {}",
        census.describe()
    );
    assert!(
        failures.is_empty(),
        "{} of {run} cases failed:\n{}",
        failures.len(),
        failures
            .iter()
            .take(60)
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n")
    );
}
