//! Ours → foreign: every option of this crate's encoder and muxer, with
//! the output handed to outside decoders.
//!
//! For each case the black-box `flac` tool must test the file clean with
//! warnings treated as errors (which includes its MD5 comparison) and
//! decode it to the PCM that went in; a second outside decoder, when
//! installed, must decode it to the same PCM; and this crate's own
//! decoder and MD5 check must agree. Skips when `flac` is not installed.

mod common;

use std::sync::Mutex;

use common::*;
use oxideav_flac::container::FlacMuxerOptions;
use oxideav_flac::encoder::{Apodization, FlacEncoderOptions};
use oxideav_flac::verify::{VerifyOutcome, verify_stream};

struct Case {
    name: String,
    bps: u32,
    channels: usize,
    rate: u32,
    samples: usize,
    signal: Signal,
    options: FlacEncoderOptions,
    /// Samples handed to the encoder per `send_frame`.
    chunk: usize,
    /// Mux through the container muxer with these options instead of
    /// concatenating the packets.
    mux: Option<FlacMuxerOptions>,
}

fn case(bps: u32, channels: usize, rate: u32, samples: usize, signal: Signal) -> Case {
    Case {
        name: String::new(),
        bps,
        channels,
        rate,
        samples,
        signal,
        options: FlacEncoderOptions::new(),
        chunk: 4096,
        mux: None,
    }
}

impl Case {
    fn with(mut self, label: &str, options: FlacEncoderOptions) -> Self {
        self.name = label.to_string();
        self.options = options;
        self
    }

    fn chunk(mut self, chunk: usize) -> Self {
        self.chunk = chunk;
        self
    }

    fn mux(mut self, options: FlacMuxerOptions) -> Self {
        self.mux = Some(options);
        self
    }

    fn title(&self, family: &str) -> String {
        format!(
            "{family} {}bit {}ch {}Hz {}smp {:?} chunk {} [{}]",
            self.bps, self.channels, self.rate, self.samples, self.signal, self.chunk, self.name
        )
    }
}

/// Depth-specific options: a stream narrower than its sample format
/// says so.
fn depth_options(options: FlacEncoderOptions, bps: u32) -> FlacEncoderOptions {
    if bps.is_multiple_of(8) {
        options
    } else {
        options.with_bits_per_sample(bps as u8)
    }
}

fn check(scratch: &Scratch, case: &Case, ffmpeg: bool) -> Result<(), String> {
    let planes = signal(
        case.signal,
        case.bps,
        case.channels,
        case.samples,
        case.bps as u64 * 977 + case.channels as u64,
    );
    let options = depth_options(case.options.clone(), case.bps);
    let md5_stored = !options.skip_md5;
    let (extradata, packets) = encode_packets(&planes, case.bps, case.rate, options, case.chunk)?;
    let file = match &case.mux {
        Some(mux) => mux_file(
            &extradata,
            case.channels,
            case.bps,
            case.rate,
            &packets,
            mux.clone(),
        )?,
        None => {
            let mut out = b"fLaC".to_vec();
            out.extend_from_slice(&extradata);
            for p in &packets {
                out.extend_from_slice(&p.data);
            }
            out
        }
    };
    let want = pack(&planes, case.bps);

    CENSUS
        .lock()
        .unwrap()
        .get_or_insert_with(Census::default)
        .take(&file)?;

    // This crate reads its own output.
    let ours = decode_file(&file)?;
    if ours.pcm != want {
        return Err("own decoder does not return the input".into());
    }
    match verify_stream(&file) {
        Ok(VerifyOutcome::Match) if md5_stored => {}
        Ok(VerifyOutcome::NoSignature) if !md5_stored => {}
        other => return Err(format!("own MD5 check: {other:?}")),
    }

    // The black-box tool tests it clean and decodes it to the input.
    let path = scratch.path("ours.flac");
    std::fs::write(&path, &file).map_err(|e| e.to_string())?;
    if md5_stored {
        tool_test(&path)?;
        if tool_decode(scratch, &path)? != want {
            return Err("black-box decoder returns other samples".into());
        }
    } else {
        // Without a stored MD5 the tool warns that it cannot compare
        // one; everything else must still hold.
        let wav = scratch.path("nomd5.wav");
        run(
            "flac",
            &[
                "-s".into(),
                "-d".into(),
                "-f".into(),
                "-o".into(),
                s(&wav),
                s(&path),
            ],
        )?;
        let bytes = std::fs::read(&wav).map_err(|e| e.to_string())?;
        if wav_data(&bytes) != Some(&want[..]) {
            return Err("black-box decoder returns other samples".into());
        }
    }

    // A second outside decoder. Where it disagrees, the same PCM coded
    // by the black-box encoder at the same block size is the control: a
    // stream shape that decoder cannot read from anyone says nothing
    // about this crate.
    if ffmpeg && !planes[0].is_empty() && second_decode(scratch, &path, case.bps)? != want {
        let wav = scratch.path("control.wav");
        let control = scratch.path("control.flac");
        std::fs::write(&wav, wav_bytes(&planes, case.bps, case.rate, &[]))
            .map_err(|e| e.to_string())?;
        let block = case.options.block_size.unwrap_or(4096).to_string();
        let args = [
            "-s",
            "-f",
            "--lax",
            "--channel-map=none",
            "-b",
            &block,
            "-o",
        ];
        let mut args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
        args.extend([s(&control), s(&wav)]);
        run("flac", &args)?;
        if second_decode(scratch, &control, case.bps)? == want {
            return Err("second outside decoder returns other samples".into());
        }
        eprintln!(
            "note: the second outside decoder cannot read this stream shape from the \
             black-box encoder either ({} bits, {} channels, block {block})",
            case.bps, case.channels
        );
    }
    Ok(())
}

/// Decode with the second outside decoder and return the samples in the
/// WAVE layout of [`pack`]. Its raw output formats are 16 and 32 bits
/// wide, left-justified.
fn second_decode(scratch: &Scratch, path: &std::path::Path, bps: u32) -> Result<Vec<u8>, String> {
    let (fmt, bits) = if bps <= 16 {
        ("s16le", 16)
    } else {
        ("s32le", 32)
    };
    let raw = scratch.path("second.raw");
    let args = ["-v", "error", "-xerror", "-y", "-i"];
    let mut args: Vec<String> = args.iter().map(|a| a.to_string()).collect();
    args.extend([s(path), "-f".into(), fmt.into(), s(&raw)]);
    let diag = run("ffmpeg", &args)?;
    if !diag.is_empty() {
        return Err(format!(
            "second decoder complained: {}",
            String::from_utf8_lossy(&diag).trim()
        ));
    }
    let got = std::fs::read(&raw).map_err(|e| e.to_string())?;
    let width = container_bytes(bps);
    let mut out = Vec::with_capacity(got.len() / (bits / 8) * width);
    for sample in got.chunks_exact(bits / 8) {
        if width == 1 {
            // 16-bit signed, left-justified → one unsigned byte.
            out.push(sample[1].wrapping_add(128));
        } else {
            out.extend_from_slice(&sample[bits / 8 - width..]);
        }
    }
    Ok(out)
}

static CENSUS: Mutex<Option<Census>> = Mutex::new(None);

type FamilyResult = (usize, Vec<String>);

fn run_family(family: &str, cases: Vec<Case>) -> FamilyResult {
    let scratch = Scratch::new("ours");
    let ffmpeg = have_ffmpeg();
    let mut tally = Tally::default();
    for case in cases {
        let title = case.title(family);
        tally.record(&title, check(&scratch, &case, ffmpeg));
    }
    eprintln!(
        "{family}: {}/{} cases pass",
        tally.run - tally.failures.len(),
        tally.run
    );
    (tally.run, tally.failures)
}

const N: usize = 4096 * 2 + 777;

fn levels_depths_layouts() -> FamilyResult {
    let mut cases = Vec::new();
    for level in 0..=8u8 {
        let label = format!("level {level}");
        for (bps, channels) in [
            (8, 1),
            (8, 2),
            (16, 1),
            (16, 2),
            (24, 1),
            (24, 2),
            (32, 1),
            (32, 2),
            (16, 6),
            (24, 8),
            (12, 2),
            (20, 2),
        ] {
            cases.push(
                case(bps, channels, 44_100, N, Signal::Tone)
                    .with(&label, FlacEncoderOptions::level(level)),
            );
        }
        for channels in 3..=8 {
            cases.push(
                case(16, channels, 48_000, 5000, Signal::Clicks)
                    .with(&label, FlacEncoderOptions::level(level)),
            );
        }
    }
    for bps in 4..=32 {
        for channels in [1, 2] {
            for level in [0, 5, 8] {
                cases.push(
                    case(bps, channels, 44_100, 5000, Signal::Tone)
                        .with(&format!("level {level}"), FlacEncoderOptions::level(level)),
                );
            }
            cases.push(
                case(bps, channels, 44_100, 3000, Signal::Extremes)
                    .with("level 5", FlacEncoderOptions::level(5)),
            );
        }
    }
    run_family("levels x depths x layouts", cases)
}

fn exhaustive_search() -> FamilyResult {
    let mut cases = Vec::new();
    for (bps, channels) in [(8, 1), (16, 2), (24, 2), (32, 2), (12, 2), (16, 5)] {
        for sig in [Signal::Tone, Signal::Clicks, Signal::WideSide] {
            cases.push(
                case(bps, channels, 44_100, 3000, sig)
                    .with(
                        "exhaustive",
                        FlacEncoderOptions::exhaustive().with_block_size(1024),
                    )
                    .chunk(1000),
            );
        }
    }
    run_family("exhaustive", cases)
}

fn signals() -> FamilyResult {
    let all = [
        Signal::Tone,
        Signal::Noise,
        Signal::Silence,
        Signal::Dc,
        Signal::Extremes,
        Signal::Same,
        Signal::Inverted,
        Signal::LeftOnly,
        Signal::RightOnly,
        Signal::WideSide,
        Signal::Clicks,
        Signal::Ramp,
    ];
    let mut cases = Vec::new();
    for sig in all {
        for bps in [4, 8, 12, 16, 20, 24, 31, 32] {
            for level in [0, 3, 5, 8] {
                cases.push(
                    case(bps, 2, 44_100, 6000, sig)
                        .with(&format!("level {level}"), FlacEncoderOptions::level(level)),
                );
            }
            cases.push(case(bps, 1, 44_100, 6000, sig).with("default", FlacEncoderOptions::new()));
        }
    }
    for bps in [8, 16, 24, 32] {
        for k in 1..bps {
            cases.push(
                case(bps, 2, 44_100, 5000, Signal::Wasted(k))
                    .with("default", FlacEncoderOptions::new()),
            );
        }
    }
    run_family("signals", cases)
}

fn block_sizes_and_lengths() -> FamilyResult {
    let mut cases = Vec::new();
    let sizes = [
        16u32, 17, 100, 192, 255, 256, 257, 576, 1000, 1152, 4096, 4608, 4609, 8192, 16_384,
        32_768, 65_535,
    ];
    for b in sizes {
        let options = FlacEncoderOptions::new()
            .with_block_size(b)
            .with_allow_non_subset_block_size(true);
        let b = b as usize;
        for samples in [b * 3, b * 2 + 1, b * 2 - 1, b / 2 + 1] {
            cases.push(
                case(16, 2, 44_100, samples.min(140_000), Signal::Tone)
                    .with(&format!("block {b}"), options.clone())
                    .chunk(977),
            );
        }
        cases.push(
            case(24, 3, 96_000, b + 7, Signal::Clicks)
                .with(&format!("block {b}"), options.clone())
                .chunk(usize::MAX),
        );
    }
    for samples in [1, 2, 3, 15, 16, 17, 4095, 4096, 4097] {
        for (bps, channels) in [(16, 1), (16, 2), (32, 2), (8, 8)] {
            cases.push(
                case(bps, channels, 44_100, samples, Signal::Tone)
                    .with("default", FlacEncoderOptions::new())
                    .chunk(1),
            );
        }
    }
    run_family("block sizes", cases)
}

fn sample_rates() -> FamilyResult {
    let rates = [
        1, 7, 4000, 8000, 11_025, 12_000, 16_000, 22_050, 24_000, 32_000, 37_800, 44_100, 44_101,
        48_000, 50_000, 64_000, 65_535, 65_536, 65_540, 88_200, 96_000, 100_000, 123_450, 176_400,
        192_000, 255_000, 256_000, 352_800, 384_000, 655_349, 655_350, 655_351, 768_000, 1_048_575,
    ];
    let mut cases = Vec::new();
    for rate in rates {
        cases
            .push(case(16, 2, rate, 6000, Signal::Tone).with("default", FlacEncoderOptions::new()));
        cases.push(case(24, 1, rate, 6000, Signal::Tone).with(
            "level 8",
            FlacEncoderOptions::level(8).with_block_size(1024),
        ));
    }
    run_family("sample rates", cases)
}

fn search_axes() -> FamilyResult {
    let mut cases = Vec::new();
    for order in 1..=32usize {
        for level in [5u8, 8] {
            cases.push(case(16, 2, 44_100, N, Signal::Tone).with(
                &format!("level {level} lpc<={order}"),
                FlacEncoderOptions::level(level).with_max_lpc_order(order),
            ));
        }
        cases.push(
            case(24, 1, 96_000, 3000, Signal::Tone)
                .with(
                    &format!("exhaustive lpc<={order}"),
                    FlacEncoderOptions::exhaustive()
                        .with_max_lpc_order(order)
                        .with_block_size(1024),
                )
                .chunk(3000),
        );
    }
    for part in 0..=15u32 {
        for sig in [Signal::Tone, Signal::Clicks] {
            cases.push(
                case(16, 2, 44_100, 70_000, sig).with(
                    &format!("partition<={part}"),
                    FlacEncoderOptions::new()
                        .with_block_size(32_768)
                        .with_allow_non_subset_block_size(true)
                        .with_allow_non_subset_partition_order(true)
                        .with_max_partition_order(part),
                ),
            );
        }
    }
    for span in 0..=8u32 {
        cases.push(case(24, 2, 48_000, N, Signal::Tone).with(
            &format!("precision span {span}"),
            FlacEncoderOptions::level(6).with_lpc_precision_search_span(span),
        ));
    }
    let windows = [
        Apodization::Welch,
        Apodization::Hann,
        Apodization::Bartlett,
        Apodization::BlackmanHarris,
        Apodization::Tukey {
            alpha_num: 0,
            alpha_den: 1,
        },
        Apodization::Tukey {
            alpha_num: 1,
            alpha_den: 2,
        },
        Apodization::Tukey {
            alpha_num: 1,
            alpha_den: 1,
        },
    ];
    for w in windows {
        cases.push(case(16, 2, 44_100, N, Signal::Tone).with(
            &format!("{w:?}"),
            FlacEncoderOptions::new().with_apodization(vec![w]),
        ));
    }
    cases.push(case(16, 2, 44_100, N, Signal::Tone).with(
        "all windows",
        FlacEncoderOptions::level(8).with_apodization(windows.to_vec()),
    ));
    run_family("search axes", cases)
}

fn stream_options() -> FamilyResult {
    let mut cases = Vec::new();
    for padding in [0usize, 1, 4, 4096, 1 << 20, (1 << 24) - 1] {
        cases.push(case(16, 2, 44_100, N, Signal::Tone).with(
            &format!("padding {padding}"),
            FlacEncoderOptions::new().with_padding_bytes(padding),
        ));
    }
    for (bps, channels) in [(8, 1), (16, 2), (24, 6), (32, 2), (20, 2)] {
        cases.push(
            case(bps, channels, 44_100, N, Signal::Tone)
                .with("no md5", FlacEncoderOptions::new().with_md5(false)),
        );
        cases.push(
            case(bps, channels, 44_100, N, Signal::Tone)
                .with("verify", FlacEncoderOptions::verifying()),
        );
        cases.push(
            case(bps, channels, 44_100, N, Signal::Tone).with("fast", FlacEncoderOptions::fast()),
        );
        for threads in [2, 3, 8] {
            for level in [0, 5, 8] {
                cases.push(case(bps, channels, 44_100, 40_000, Signal::Tone).with(
                    &format!("level {level}, {threads} threads"),
                    FlacEncoderOptions::level(level).with_threads(threads),
                ));
            }
        }
        for chunk in [1, 7, 4095, 4097, usize::MAX] {
            cases.push(
                case(bps, channels, 44_100, N, Signal::Clicks)
                    .with("default", FlacEncoderOptions::new())
                    .chunk(chunk),
            );
        }
    }
    run_family("stream options", cases)
}

fn muxer_options() -> FamilyResult {
    let mut cases = Vec::new();
    let small = FlacEncoderOptions::new().with_block_size(256);
    for (label, mux) in [
        ("default seek table", FlacMuxerOptions::default()),
        ("no seek table", FlacMuxerOptions::no_seektable()),
        ("for 44100", FlacMuxerOptions::for_sample_rate(44_100)),
        (
            "every frame",
            FlacMuxerOptions::default().with_seek_point_interval(0),
        ),
        (
            "every 1000 samples",
            FlacMuxerOptions::default().with_seek_point_interval(1000),
        ),
        (
            "at most 3 points",
            FlacMuxerOptions::default()
                .with_seek_point_interval(1)
                .with_max_seek_points(3),
        ),
        (
            "one point",
            FlacMuxerOptions::default().with_max_seek_points(1),
        ),
    ] {
        for (bps, channels, samples) in [(16, 2, 60_000), (24, 6, 9000), (16, 1, 1), (8, 2, 300)] {
            cases.push(
                case(bps, channels, 44_100, samples, Signal::Tone)
                    .with(label, small.clone())
                    .mux(mux.clone()),
            );
            cases.push(
                case(bps, channels, 44_100, samples, Signal::Tone)
                    .with(
                        &format!("{label} + padding"),
                        small.clone().with_padding_bytes(8192),
                    )
                    .mux(mux.clone()),
            );
        }
    }
    run_family("muxer", cases)
}

/// Threads change nothing in the output, and neither does how the input
/// is cut into `send_frame` calls.
fn byte_identity() -> FamilyResult {
    let mut tally = Tally::default();
    for (bps, channels) in [(16, 2), (24, 6), (32, 2), (12, 1)] {
        for level in [0u8, 5, 8] {
            let planes = signal(Signal::Tone, bps, channels, 50_000, 5);
            let base = depth_options(FlacEncoderOptions::level(level), bps);
            let reference = encode_file(&planes, bps, 44_100, base.clone(), 4096);
            for (threads, chunk) in [(1, 1), (1, 50_000), (2, 4096), (4, 333), (16, 50_000)] {
                let other = encode_file(
                    &planes,
                    bps,
                    44_100,
                    base.clone().with_threads(threads),
                    chunk,
                );
                let name = format!(
                    "identity {bps}bit {channels}ch level {level} {threads}thr chunk {chunk}"
                );
                tally.record(
                    &name,
                    match (&reference, &other) {
                        (Ok(a), Ok(b)) if a == b => Ok(()),
                        (Ok(_), Ok(_)) => Err("output differs".into()),
                        (Err(e), _) | (_, Err(e)) => Err(e.clone()),
                    },
                );
            }
        }
    }
    eprintln!(
        "byte identity: {}/{} cases pass",
        tally.run - tally.failures.len(),
        tally.run
    );
    (tally.run, tally.failures)
}

#[test]
fn own_streams_are_accepted_outside() {
    if !have_flac() {
        eprintln!("skip: the black-box `flac` tool is not installed");
        return;
    }
    let families: [fn() -> FamilyResult; 9] = [
        levels_depths_layouts,
        exhaustive_search,
        signals,
        block_sizes_and_lengths,
        sample_rates,
        search_axes,
        stream_options,
        muxer_options,
        byte_identity,
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
        "own-stream matrix: {}/{} cases pass",
        run - failures.len(),
        run
    );
    let census = CENSUS.lock().unwrap().take().unwrap_or_default();
    eprintln!("census of this encoder's streams: {}", census.describe());
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
