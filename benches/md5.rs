//! Criterion benchmarks for the FLAC STREAMINFO MD5 validator.
//!
//! Round 255 (depth-mode benchmarks): the encoder accumulates a
//! running MD5 over the decoded PCM bytes (per RFC 9639 §8.2, the
//! STREAMINFO 128-bit signature is computed over the
//! little-endian-packed PCM samples in interleaved channel-major
//! frame order) by calling `Md5::update` once per audio frame in
//! `feed_md5`. The decoder, the round-232 fuzz target, and the
//! standalone `compute()` oneshot helper all exercise the same RFC
//! 1321 block compression. Until this round the MD5 had no
//! standalone benchmark: every existing measurement folded the MD5
//! cost into the wider encode wall time, so a regression in the
//! 64-byte block compressor (e.g. an inadvertent layout change
//! that defeated round-1 ↔ round-4 register allocation in
//! `process_block`, or a `Md5::update` shape that started reborrowing
//! the buffer mid-stream and lost the slice-rest fast path) would
//! be measurable only as a tiny tail on the encode bench number,
//! not as a stable A/B target.
//!
//! Per the workspace "saturated -> fuzz/bench/profile" memo this
//! round wires up a `criterion` harness so future MD5 speed work
//! (a 4-message-block parallel compressor, SIMD round mixing, or a
//! tail-pad micro-optimisation) has a dedicated baseline to A/B
//! against. The CRC bench in `benches/crc.rs` (round 218) provides
//! the shape this harness mirrors: deterministic xorshift buffer
//! synthesised in-bench (no `docs/` fixtures or external files
//! read), every scenario asserts the result matches the oneshot
//! reference inside the timed iteration so a regression that
//! silently produced a different digest would surface as a panic
//! rather than a misleading speedup.
//!
//! Each scenario picks a buffer size that maps to a real call shape:
//!
//!   - **md5_oneshot_4k**: a single `compute()` on a 4 KiB buffer
//!     — roughly one stereo-S16-44.1k frame at the encoder's
//!     default 4096-sample block size (4096 samples × 2 channels ×
//!     2 bytes/sample = 16 KiB; the 4 KiB scenario mirrors the
//!     mono variant). Calls `Md5::new()` + `update` + `finalize`
//!     inside the timed region so the per-frame setup overhead
//!     stays visible.
//!   - **md5_oneshot_16k**: 16 KiB buffer, mirrors one
//!     stereo-S16-44.1k frame at the same default block size.
//!   - **md5_oneshot_64k**: 64 KiB buffer, mirrors one
//!     multichannel-S24-48k frame (6 channels × 4096 samples × 3
//!     bytes ≈ 72 KiB). 16x the work — throughput should land
//!     within criterion noise of the 4 KiB scenario if the inner
//!     `process_block` loop is bandwidth-bound rather than
//!     setup-bound.
//!   - **md5_streaming_chunked_64k**: the same 64 KiB buffer fed
//!     through `Md5::update` as the encoder's per-frame schedule
//!     would: roughly 16 chunks of ~4 KiB, with two deliberately
//!     off-aligned splits in the middle of the buffer that don't
//!     land on 64-byte block boundaries. Forces the in-buffer
//!     copy + carry-block path through every iteration so a
//!     regression that broke the partial-block accumulator would
//!     surface here even if `update(whole_buffer)` still worked.
//!   - **md5_streaming_byte_4k**: a 4 KiB buffer fed through
//!     `Md5::update(&[byte])` one byte at a time. Worst-case
//!     interface — every call refills the 64-byte carry buffer
//!     before any `process_block` fast path can fire — and the
//!     scenario most exposed to per-call overhead if the buffer
//!     accumulator shape were to regress.
//!   - **md5_empty / md5_short**: zero-byte and 3-byte one-shot
//!     paths, the smallest legal inputs (the empty-input RFC 1321
//!     §A.5 test vector lands here). Stresses the finalize-pad
//!     branch where the buffer is < 56 bytes and one pad block
//!     suffices.
//!
//! Reference numbers (release, Darwin aarch64) populate the
//! per-crate `README.md` "Round 255" entry; reproduce with:
//!
//!     cargo bench -p oxideav-flac --bench md5
//!
//! No external `md5` reference implementation is consulted — the
//! crate's own `oxideav_flac::md5::compute` slice helper is the
//! oneshot oracle the streaming scenarios validate against, and
//! it is itself anchored to the four RFC 1321 §A.5 test vectors
//! covered in `src/md5.rs`'s unit tests.

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use oxideav_flac::md5::{compute, Md5};

/// Cheap deterministic xorshift32 — synthesises realistic random
/// byte content for the MD5 bench buffers. A pure-zero buffer is
/// no faster through MD5 than a varied buffer (the round-4 mixer
/// function `c ^ (b | !d)` is non-linear so input pattern does
/// not collapse the work), but a varied buffer keeps the bench
/// shape honest about what real PCM-byte payloads look like.
/// Same xorshift seed as the neighbouring `crc` / `encode` /
/// `decode` / `roundtrip` benches so numbers stay comparable
/// across the bench surface.
fn xorshift32(state: &mut u32) -> u32 {
    *state ^= *state << 13;
    *state ^= *state >> 17;
    *state ^= *state << 5;
    *state
}

fn build_buffer(len: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(len);
    let mut state: u32 = 0xCAFE_F00D;
    while out.len() < len {
        let w = xorshift32(&mut state);
        out.extend_from_slice(&w.to_le_bytes());
    }
    out.truncate(len);
    out
}

/// Hand back a list of slice ranges that split `len` into
/// roughly-equal chunks of `~chunk_target` bytes with two
/// deliberately off-aligned splits in the middle of the buffer.
/// Mirrors how the encoder's `feed_md5` hands one per-frame slice
/// to the running MD5 at a time, where the per-frame slice
/// length depends on `n_samples * n_channels * bytes_per_sample`
/// and does not land on a 64-byte MD5 block boundary in general.
/// A future tweak that only worked on aligned chunks would
/// surface as a measurable slowdown here.
fn build_chunk_ranges(len: usize, chunk_target: usize) -> Vec<(usize, usize)> {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut cursor = 0usize;
    let mut step = 0usize;
    while cursor < len {
        // Every fifth chunk gets a 13-byte off-alignment kick so
        // the chunked-streaming path is forced through a split
        // that doesn't land on a 64-byte natural boundary (the MD5
        // block size). The bench wants to catch a future change
        // whose performance is only stable on 64-byte multiples.
        let bump = if step % 5 == 4 { 13 } else { 0 };
        let end = (cursor + chunk_target + bump).min(len);
        ranges.push((cursor, end));
        cursor = end;
        step += 1;
    }
    ranges
}

// ---- Oneshot `compute()` slice helper -------------------------------

fn bench_md5_oneshot_4k(c: &mut Criterion) {
    let buf = build_buffer(4 * 1024);
    let expected = compute(&buf);
    let mut g = c.benchmark_group("md5_oneshot_4k");
    g.throughput(Throughput::Bytes(buf.len() as u64));
    g.bench_function(BenchmarkId::from_parameter("md5/oneshot/4k"), |b| {
        b.iter(|| {
            let got = compute(black_box(&buf));
            assert_eq!(got, expected);
            black_box(got);
        });
    });
    g.finish();
}

fn bench_md5_oneshot_16k(c: &mut Criterion) {
    let buf = build_buffer(16 * 1024);
    let expected = compute(&buf);
    let mut g = c.benchmark_group("md5_oneshot_16k");
    g.throughput(Throughput::Bytes(buf.len() as u64));
    g.bench_function(BenchmarkId::from_parameter("md5/oneshot/16k"), |b| {
        b.iter(|| {
            let got = compute(black_box(&buf));
            assert_eq!(got, expected);
            black_box(got);
        });
    });
    g.finish();
}

fn bench_md5_oneshot_64k(c: &mut Criterion) {
    let buf = build_buffer(64 * 1024);
    let expected = compute(&buf);
    let mut g = c.benchmark_group("md5_oneshot_64k");
    g.throughput(Throughput::Bytes(buf.len() as u64));
    g.bench_function(BenchmarkId::from_parameter("md5/oneshot/64k"), |b| {
        b.iter(|| {
            let got = compute(black_box(&buf));
            assert_eq!(got, expected);
            black_box(got);
        });
    });
    g.finish();
}

// ---- Streaming `Md5::update` paths ---------------------------------

fn bench_md5_streaming_chunked_64k(c: &mut Criterion) {
    let buf = build_buffer(64 * 1024);
    let expected = compute(&buf);
    let ranges = build_chunk_ranges(buf.len(), 4 * 1024);
    let mut g = c.benchmark_group("md5_streaming_chunked_64k");
    g.throughput(Throughput::Bytes(buf.len() as u64));
    g.bench_function(
        BenchmarkId::from_parameter("md5/streaming-chunked/64k"),
        |b| {
            b.iter(|| {
                let mut m = Md5::new();
                for &(lo, hi) in &ranges {
                    m.update(black_box(&buf[lo..hi]));
                }
                let got = m.finalize();
                assert_eq!(got, expected);
                black_box(got);
            });
        },
    );
    g.finish();
}

fn bench_md5_streaming_byte_4k(c: &mut Criterion) {
    let buf = build_buffer(4 * 1024);
    let expected = compute(&buf);
    let mut g = c.benchmark_group("md5_streaming_byte_4k");
    g.throughput(Throughput::Bytes(buf.len() as u64));
    g.bench_function(BenchmarkId::from_parameter("md5/streaming-byte/4k"), |b| {
        b.iter(|| {
            let mut m = Md5::new();
            for &byte in black_box(&buf) {
                m.update(&[byte]);
            }
            let got = m.finalize();
            assert_eq!(got, expected);
            black_box(got);
        });
    });
    g.finish();
}

// ---- Tiny-input finalize-pad coverage ------------------------------

fn bench_md5_empty(c: &mut Criterion) {
    let buf: [u8; 0] = [];
    let expected = compute(&buf);
    let mut g = c.benchmark_group("md5_empty");
    g.throughput(Throughput::Elements(1));
    g.bench_function(BenchmarkId::from_parameter("md5/oneshot/empty"), |b| {
        b.iter(|| {
            let got = compute(black_box(&buf));
            assert_eq!(got, expected);
            black_box(got);
        });
    });
    g.finish();
}

fn bench_md5_short(c: &mut Criterion) {
    let buf = build_buffer(3);
    let expected = compute(&buf);
    let mut g = c.benchmark_group("md5_short");
    g.throughput(Throughput::Bytes(buf.len() as u64));
    g.bench_function(BenchmarkId::from_parameter("md5/oneshot/3b"), |b| {
        b.iter(|| {
            let got = compute(black_box(&buf));
            assert_eq!(got, expected);
            black_box(got);
        });
    });
    g.finish();
}

criterion_group!(
    benches,
    bench_md5_oneshot_4k,
    bench_md5_oneshot_16k,
    bench_md5_oneshot_64k,
    bench_md5_streaming_chunked_64k,
    bench_md5_streaming_byte_4k,
    bench_md5_empty,
    bench_md5_short,
);
criterion_main!(benches);
