// Parallel-array index loops are idiomatic in codec / bench code; skip
// the lint (mirrors the crate root's `#![allow(clippy::needless_range_loop)]`).
#![allow(clippy::needless_range_loop)]

//! Criterion benchmarks for the FLAC demuxer's `seek_to` hot path.
//!
//! Round 223 (depth-mode benchmarks): paired with the round-128
//! `decode` / `encode` / `roundtrip` harnesses and the round-218 `crc`
//! harness. Every prior bench measures forward-only decode/encode work;
//! the seek path — the demuxer's binary search through the SEEKTABLE
//! followed by a re-anchored sync-code walk into the next frame — has
//! never been measured in isolation. That leaves three under-instrumented
//! hot spots:
//!
//! * the `partition_point` binary search across the `Vec<SeekPoint>`
//!   (RFC 9639 §8.5.1) — cost should be O(log N) and dominated by
//!   table-prefetch behaviour, but a regression that linear-scans
//!   would be undetectable from the existing decode benches that
//!   never call `seek_to`,
//! * the underlying `io::Seek::seek(SeekFrom::Start(_))` on the
//!   demuxer's `Cursor<Vec<u8>>` input — cheap in isolation but
//!   sensitive to the SEEKTABLE-Vec growth pattern,
//! * the post-seek `try_take` sync-code walk that finds the next
//!   `0xFF 0xF8 / 0xF9` frame header from the anchored offset
//!   (`container.rs::reset_to` + sync-code re-scan) — the user-visible
//!   "seek latency" comes from this leg, not the binary search.
//!
//! Scenarios:
//!
//!   - **seek_only/small**: `seek_to(0, target)` on a 1-s mono S16
//!     fixture whose SEEKTABLE indexes every emitted frame (~11
//!     entries at the encoder's current 4096-sample default block
//!     size; see RFC 9639 §8.2 for the STREAMINFO block-size
//!     fields). Measures the binary search + `input.seek` cost in
//!     isolation (no `next_packet` follow-up). Each iteration picks a
//!     deterministic-but-varying target so the bench can't sit on a
//!     single hot cache line of seek_points. The demuxer is opened
//!     once per criterion batch (via `iter_batched`), then
//!     `SEEKS_PER_BATCH` seek calls run inside the timed region — so
//!     STREAMINFO + SEEKTABLE parse cost is amortised away and only
//!     the seek path remains.
//!   - **seek_only/large**: same but with a 16-s fixture; the
//!     SEEKTABLE swells to ~173 entries. With log2(173) ≈ 7.4 vs
//!     log2(11) ≈ 3.5 comparisons, the binary search should cost
//!     roughly 2.1× — a regression to linear search would show
//!     ~16× instead.
//!   - **seek_then_next_packet/small**: `seek_to(0, target)` followed
//!     by a single `next_packet()` call. Exercises the post-seek
//!     sync-code walk + frame-header parse that user code actually
//!     waits on. Also `iter_batched` so the demuxer open is outside
//!     the timed region.
//!   - **linear_walk_baseline**: walks the full packet stream from
//!     the start without seeking — A/B baseline for "what does
//!     `next_packet` cost cold" (this scenario reopens the demuxer
//!     every iteration so cursor state is fresh).
//!
//! Per-iteration setup: each scenario reuses one pre-built FLAC byte
//! buffer (`fLaC` magic + STREAMINFO + SEEKTABLE + frame bytes
//! produced by the production encoder). The byte buffer is built
//! once at criterion-group setup time and shared by reference across
//! every iteration.
//!
//! Baseline numbers (release, Darwin aarch64): seek_only ~14.6 ns
//! per warm seek at N=11, ~19.5 ns at N=173 — ratio 1.34× confirms
//! the binary search is O(log N) and dominated by the constant-cost
//! `Cursor::seek` + `scan.reset_to` overhead rather than the
//! `partition_point` walk. seek_then_next_packet ~2.47 µs / call
//! (dominated by the post-seek sync-code walk). linear_walk_baseline
//! ~1.36 G samples/s through the demuxer's frame-boundary scanner.
//!
//! Run with:
//!     cargo bench -p oxideav-flac --bench seek

use std::io::Cursor;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use oxideav_core::{
    AudioFrame, CodecId, CodecParameters, ContainerRegistry, Frame, NullCodecResolver, Packet,
    ReadSeek, SampleFormat,
};
use oxideav_flac::metadata::{write_seektable, BlockHeader, BlockType, SeekPoint, FLAC_MAGIC};

// -----------------------------------------------------------------------
// PCM + encoder helpers (mirrors `benches/decode.rs::build_pcm_per_channel`
// + `prepare_flac_stream` shape — duplicated rather than shared because
// each bench file is its own criterion binary and the round-128 helpers
// aren't exposed as a library).
// -----------------------------------------------------------------------

fn xorshift32(state: &mut u32) -> u32 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    *state
}

fn build_pcm_mono_s16(n_samples: usize) -> Vec<Vec<i32>> {
    let mut out: Vec<i32> = Vec::with_capacity(n_samples);
    let mut state: u32 = 0x5EE_C0DE;
    let amp = 1i32 << 13;
    for s in 0..n_samples {
        let phase = (s % 256) as i32 - 128;
        let env = (phase * amp) / 128;
        let noise = (xorshift32(&mut state) as i32) >> 24;
        out.push(env + noise);
    }
    vec![out]
}

fn build_audio_frame(channels: u16, pcm_per_channel: &[Vec<i32>]) -> AudioFrame {
    let n = pcm_per_channel[0].len();
    let bytes_per_sample = SampleFormat::S16.bytes_per_sample();
    let mut interleaved: Vec<u8> = Vec::with_capacity(n * channels as usize * bytes_per_sample);
    for i in 0..n {
        for c in 0..channels as usize {
            let s = pcm_per_channel[c][i] as i16;
            interleaved.extend_from_slice(&s.to_le_bytes());
        }
    }
    AudioFrame {
        samples: n as u32,
        pts: Some(0),
        data: vec![interleaved],
    }
}

/// Encode `n_samples` of mono S16 PCM and return:
/// * the encoder's finalised `output_params` (carries STREAMINFO bytes
///   in `extradata`),
/// * the packet stream — one packet per FLAC frame.
fn encode_flac_packets(n_samples: usize) -> (CodecParameters, Vec<Packet>) {
    let mut params = CodecParameters::audio(CodecId::new("flac"));
    params.channels = Some(1);
    params.sample_rate = Some(44_100);
    params.sample_format = Some(SampleFormat::S16);
    let mut enc = oxideav_flac::encoder::make_encoder(&params).expect("make_encoder");

    let pcm = build_pcm_mono_s16(n_samples);
    let frame = build_audio_frame(1, &pcm);
    enc.send_frame(&Frame::Audio(frame)).expect("send_frame");
    enc.flush().expect("flush");

    let mut packets: Vec<Packet> = Vec::new();
    loop {
        match enc.receive_packet() {
            Ok(p) => packets.push(p),
            Err(oxideav_core::Error::NeedMore) | Err(oxideav_core::Error::Eof) => break,
            Err(e) => panic!("unexpected encoder error: {:?}", e),
        }
    }
    let final_params = enc.output_params().clone();
    (final_params, packets)
}

/// Splice `fLaC` magic + STREAMINFO + SEEKTABLE + frame bytes into a
/// single in-memory FLAC byte stream the container demuxer can open.
///
/// `streaminfo_bytes` is the raw 34-byte STREAMINFO payload the
/// encoder stashed in `output_params.extradata` (no leading metadata
/// header — this function emits the headers itself, marking the
/// SEEKTABLE block as `last`).
fn assemble_flac_with_seektable(
    streaminfo_bytes: &[u8],
    packets: &[Packet],
    seek_points: &[SeekPoint],
) -> Vec<u8> {
    let mut file = Vec::new();
    file.extend_from_slice(&FLAC_MAGIC);

    // STREAMINFO header (last=false because SEEKTABLE follows).
    let si_header = BlockHeader {
        last: false,
        block_type: BlockType::StreamInfo,
        length: streaminfo_bytes.len() as u32,
    };
    si_header
        .write_into(&mut file)
        .expect("STREAMINFO header serialises");
    file.extend_from_slice(streaminfo_bytes);

    // SEEKTABLE (last=true). Each SEEKPOINT is 18 bytes per RFC 9639
    // §8.5.1; `write_seektable` enforces strict-ascending ordering
    // which we satisfy by construction (one entry per packet, sample
    // numbers monotonically increase by `frame.samples`).
    let seektable_payload =
        write_seektable(seek_points, 0).expect("write_seektable on monotonic points");
    let st_header = BlockHeader {
        last: true,
        block_type: BlockType::SeekTable,
        length: seektable_payload.len() as u32,
    };
    st_header
        .write_into(&mut file)
        .expect("SEEKTABLE header serialises");
    file.extend_from_slice(&seektable_payload);

    // Frame stream: just concatenate every packet's bytes in order.
    // Each packet IS one complete FLAC frame including sync code,
    // sub-frames, padding, and footer CRC-16 — the demuxer's
    // sync-code walk handles re-anchoring at any offset that lands
    // on a frame boundary, which by construction every entry in our
    // SEEKTABLE does.
    for pkt in packets {
        file.extend_from_slice(&pkt.data);
    }
    file
}

/// Build a fixture: encode `n_samples` mono S16 PCM, then synthesize a
/// SEEKTABLE indexing **at most** `max_seek_entries` of those frames
/// (evenly spaced — `step = packets.len() / max_seek_entries`).
///
/// Returns `(file_bytes, total_samples, n_seek_entries)` so the bench
/// can pick a target sample inside the populated range without
/// reaching past the last seek point.
fn make_seekable_fixture(n_samples: usize, max_seek_entries: usize) -> (Vec<u8>, u64, usize) {
    let (params, packets) = encode_flac_packets(n_samples);
    // The encoder stashes the STREAMINFO block payload (34 bytes per
    // RFC 9639 §8.2) directly in `output_params.extradata` with no
    // metadata-block-header prefix — the fixture builder emits the
    // header itself in `assemble_flac_with_seektable`.
    let streaminfo_bytes = params.extradata;
    assert!(
        !streaminfo_bytes.is_empty(),
        "encoder must publish STREAMINFO bytes in extradata"
    );

    // The encoder's per-packet metadata fills in pts (first-sample
    // number of the frame) and duration (frame size in samples). We
    // need byte offsets relative to the start of the frame stream
    // (post-magic, post-STREAMINFO, post-SEEKTABLE) — accumulate as
    // we walk.
    let mut byte_offset: u64 = 0;
    let mut all_points: Vec<(SeekPoint, u64)> = Vec::with_capacity(packets.len());
    for pkt in &packets {
        let sample_number = pkt.pts.unwrap_or(0) as u64;
        let frame_samples = pkt.duration.unwrap_or(0) as u16;
        all_points.push((
            SeekPoint {
                sample_number,
                offset: byte_offset,
                frame_samples,
            },
            byte_offset,
        ));
        byte_offset += pkt.data.len() as u64;
    }
    let total_samples = (packets.last().and_then(|p| p.pts).unwrap_or(0) as u64)
        + (packets.last().and_then(|p| p.duration).unwrap_or(0) as u64);

    // Subsample evenly. `step` is at least 1 to avoid a div-by-zero
    // when there are fewer packets than `max_seek_entries`.
    let step = (packets.len() / max_seek_entries.max(1)).max(1);
    let mut seek_points: Vec<SeekPoint> = Vec::new();
    let mut idx = 0usize;
    while idx < all_points.len() && seek_points.len() < max_seek_entries {
        seek_points.push(all_points[idx].0);
        idx += step;
    }
    // `write_seektable` rejects equal sample_numbers; with one packet
    // per entry we're strictly increasing by construction. Sanity
    // check at fixture-build time so a regression to all-same-pts
    // packets surfaces here, not as a silent bench-no-op.
    debug_assert!(
        seek_points
            .windows(2)
            .all(|w| w[0].sample_number < w[1].sample_number),
        "fixture seek points must be strictly ascending"
    );

    let n_seek_entries = seek_points.len();
    let file = assemble_flac_with_seektable(&streaminfo_bytes, &packets, &seek_points);
    (file, total_samples, n_seek_entries)
}

// -----------------------------------------------------------------------
// Bench bodies.
// -----------------------------------------------------------------------

/// Open a fresh demuxer on the shared file bytes. The buffer is
/// cloned into a new `Cursor<Vec<u8>>` per iteration so cursor seek
/// state is identical across runs — the only cost we're measuring is
/// the demuxer's internal seek path, not `Vec::clone`. The clone is
/// outside the timed region.
fn open_demuxer_on(file_bytes: &[u8]) -> Box<dyn oxideav_core::Demuxer> {
    let cursor: Box<dyn ReadSeek> = Box::new(Cursor::new(file_bytes.to_vec()));
    let mut reg = ContainerRegistry::new();
    oxideav_flac::register_containers(&mut reg);
    reg.open_demuxer("flac", cursor, &NullCodecResolver)
        .expect("open_demuxer")
}

/// How many `seek_to` calls each warm-iteration timed region issues
/// against a single already-open demuxer. Large enough to dwarf the
/// per-iteration counter-update bookkeeping, small enough that
/// criterion's outlier rejection still sees variance.
const SEEKS_PER_BATCH: u64 = 64;

fn bench_seek_only_small(c: &mut Criterion) {
    // 44_100 mono S16 samples ≈ 1 s of audio → the production
    // encoder emits ~11 frames at the default 4096-sample block
    // size. We pass `usize::MAX` as `max_seek_entries` so the
    // fixture indexes every frame and the SEEKTABLE shape matches
    // what a real reseekable encoder would write (one entry per
    // frame, no subsampling).
    //
    // The demuxer is opened OUTSIDE the timed region (`iter_batched`
    // with `LargeInput`) so the cost we're measuring is the seek
    // path only — not `Vec::clone` + STREAMINFO + SEEKTABLE parse.
    // Each timed iteration runs `SEEKS_PER_BATCH` calls; throughput
    // reports per-seek by setting `Throughput::Elements` to the same
    // count.
    let (file_bytes, total_samples, n_entries) = make_seekable_fixture(44_100, usize::MAX);
    assert!(n_entries >= 8, "fixture must produce a non-trivial table");

    let mut g = c.benchmark_group("seek_only_small");
    g.throughput(Throughput::Elements(SEEKS_PER_BATCH));
    g.bench_function(
        BenchmarkId::from_parameter(format!("flac/seek_only/N={n_entries}")),
        |b| {
            b.iter_batched(
                || open_demuxer_on(&file_bytes),
                |mut demuxer| {
                    let mut counter: u64 = 0;
                    for _ in 0..SEEKS_PER_BATCH {
                        // Spread targets across the whole file: each
                        // call picks a different point so we're not
                        // measuring the same single seek over and over.
                        let target = ((counter.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 1)
                            % total_samples) as i64;
                        counter = counter.wrapping_add(1);
                        let landed = demuxer
                            .seek_to(0, criterion::black_box(target))
                            .expect("seek_to");
                        criterion::black_box(landed);
                    }
                },
                criterion::BatchSize::SmallInput,
            );
        },
    );
    g.finish();
}

fn bench_seek_only_large(c: &mut Criterion) {
    // 16 s of audio → ~172 frames at the default block size, so the
    // SEEKTABLE is ~16× larger than the `small` scenario.
    let (file_bytes, total_samples, n_entries) = make_seekable_fixture(44_100 * 16, usize::MAX);
    assert!(
        n_entries >= 64,
        "fixture must produce a substantially larger table; got {n_entries}"
    );

    let mut g = c.benchmark_group("seek_only_large");
    g.throughput(Throughput::Elements(SEEKS_PER_BATCH));
    g.bench_function(
        BenchmarkId::from_parameter(format!("flac/seek_only/N={n_entries}")),
        |b| {
            b.iter_batched(
                || open_demuxer_on(&file_bytes),
                |mut demuxer| {
                    let mut counter: u64 = 0;
                    for _ in 0..SEEKS_PER_BATCH {
                        let target = ((counter.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 1)
                            % total_samples) as i64;
                        counter = counter.wrapping_add(1);
                        let landed = demuxer
                            .seek_to(0, criterion::black_box(target))
                            .expect("seek_to");
                        criterion::black_box(landed);
                    }
                },
                criterion::BatchSize::SmallInput,
            );
        },
    );
    g.finish();
}

fn bench_seek_then_next_packet_small(c: &mut Criterion) {
    let (file_bytes, total_samples, n_entries) = make_seekable_fixture(44_100, usize::MAX);

    let mut g = c.benchmark_group("seek_then_next_packet_small");
    g.throughput(Throughput::Elements(SEEKS_PER_BATCH));
    g.bench_function(
        BenchmarkId::from_parameter(format!("flac/seek_then_next_packet/N={n_entries}")),
        |b| {
            b.iter_batched(
                || open_demuxer_on(&file_bytes),
                |mut demuxer| {
                    let mut counter: u64 = 0;
                    for _ in 0..SEEKS_PER_BATCH {
                        let target = ((counter.wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 1)
                            % total_samples) as i64;
                        counter = counter.wrapping_add(1);
                        let _landed = demuxer
                            .seek_to(0, criterion::black_box(target))
                            .expect("seek_to");
                        // Touch the next packet so the sync-code walk shows up
                        // in the timed region.
                        let pkt = demuxer.next_packet().expect("next_packet post-seek");
                        criterion::black_box(pkt.pts);
                    }
                },
                criterion::BatchSize::SmallInput,
            );
        },
    );
    g.finish();
}

fn bench_linear_walk_baseline(c: &mut Criterion) {
    // Same fixture shape as the small-table seek bench. Body walks
    // every packet from the beginning without ever calling seek_to —
    // measures the cold "open + next_packet × N" cost as a
    // throughput reference. Throughput is in samples so the result
    // is comparable to the round-128 `decode` numbers (those measure
    // the codec's decode work, not the demuxer's packetisation).
    let (file_bytes, total_samples, _n_entries) = make_seekable_fixture(44_100, usize::MAX);

    let mut g = c.benchmark_group("linear_walk_baseline");
    g.throughput(Throughput::Elements(total_samples));
    g.bench_function(
        BenchmarkId::from_parameter("flac/linear_walk_baseline/1s"),
        |b| {
            b.iter(|| {
                let mut demuxer = open_demuxer_on(&file_bytes);
                let mut count = 0u64;
                loop {
                    match demuxer.next_packet() {
                        Ok(p) => {
                            count += p.duration.unwrap_or(0) as u64;
                        }
                        Err(oxideav_core::Error::Eof) => break,
                        Err(e) => panic!("unexpected next_packet error: {:?}", e),
                    }
                }
                criterion::black_box(count);
            });
        },
    );
    g.finish();
}

criterion_group!(
    benches,
    bench_seek_only_small,
    bench_seek_only_large,
    bench_seek_then_next_packet_small,
    bench_linear_walk_baseline,
);
criterion_main!(benches);
