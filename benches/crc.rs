//! Criterion benchmarks for the FLAC frame-CRC validators.
//!
//! Round 218 (depth-mode benchmarks): round 212 added the streaming
//! `Crc8` / `Crc16` types in `oxideav_flac::crc` for callers that
//! consume FLAC frame bytes incrementally (an Ogg-FLAC demuxer
//! receiving one Ogg segment at a time, a network-fed decoder fronted
//! by a ring buffer, an in-place verifier that wants to early-fail
//! before allocating subframe sample vectors). The one-shot `crc8` /
//! `crc16` slice helpers are now thin wrappers around the streaming
//! types — same lookup tables, same byte loop.
//!
//! Per the workspace "saturated -> fuzz/bench/profile" memo this round
//! wires up a `criterion` harness so future CRC speed work (a wider
//! slice-by-N table, a SIMD `pclmulqdq`-driven inner loop, an early-
//! exit on a known-bad byte stream, etc.) has a stable baseline to
//! A/B against. Bit-exact agreement with the one-shot slice helper is
//! asserted inside every iteration so a regression that broke the
//! streaming path silently would surface as a benchmark panic rather
//! than a quiet 1.1x speedup that was actually walking different bytes.
//!
//! Each scenario synthesises a deterministic xorshift byte buffer
//! in-bench (no `docs/` fixtures or external files are read) sized to
//! match realistic FLAC frame footprints described in RFC 9639 §7
//! (frame footer covers the entire frame including header — typical
//! 44.1 kHz / 4096-block frames land in the 4..16 KiB range; a high-
//! bit-depth multichannel block can comfortably exceed 64 KiB). Each
//! group covers both `Crc8` (frame-header coverage, ends right before
//! the 8-bit CRC field per §6.1) and `Crc16` (whole-frame coverage,
//! ends before the 16-bit footer per §7) so the relative shapes can
//! be read off side-by-side.
//!
//! Scenarios:
//!
//!   - **crc8_oneshot_8k / crc16_oneshot_8k**: the one-shot slice
//!     helper on a typical 8 KiB FLAC frame's worth of bytes. The
//!     baseline a single-frame decoder pays per frame.
//!   - **crc8_streaming_chunked_8k / crc16_streaming_chunked_8k**:
//!     the same 8 KiB buffer fed through `update(&[u8])` as ten
//!     unequal chunks (1 KiB-ish each, with a couple of off-aligned
//!     splits). Mirrors how an Ogg-FLAC demuxer hands segments to the
//!     validator.
//!   - **crc8_streaming_byte_8k / crc16_streaming_byte_8k**: the same
//!     8 KiB buffer fed through `update_byte` one byte at a time. The
//!     worst-case interface — single-byte ring-buffer feeds, in-place
//!     fuzz harnesses pushing one byte per iteration — and the
//!     scenario most exposed to per-call overhead if a future inline
//!     hint were removed.
//!   - **crc16_oneshot_64k / crc16_streaming_chunked_64k**: the same
//!     two shapes scaled up to a 64 KiB buffer (roughly a multichannel
//!     high-bit-depth FLAC frame). 8x the work — the throughput
//!     numbers should land within criterion's noise of the 8 KiB
//!     scenarios if the inner loop is bandwidth-bound. CRC-8 is
//!     skipped at this size because the FLAC header it covers is
//!     fixed at ≤ 16 bytes per §6.1.
//!
//! Run with:
//!     cargo bench -p oxideav-flac --bench crc

use criterion::{black_box, criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};

use oxideav_flac::crc::{crc16, crc8, Crc16, Crc8};

/// Cheap deterministic xorshift32 — synthesises realistic random byte
/// content for the CRC bench buffers. A pure-zero buffer would still
/// fold through every table entry (CRC-8 / CRC-16 are linear over GF(2)
/// so input-zero is no faster than input-random) but a varied buffer
/// keeps the bench shape honest about what real FLAC frame payloads
/// look like — every table entry gets touched within the first ~256
/// iterations regardless of bit pattern. Same xorshift seed as the
/// neighbouring `encode` / `decode` / `roundtrip` benches so numbers
/// stay comparable across the bench surface.
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

/// Hand back a list of slice ranges that split `len` into roughly-equal
/// chunks of `~chunk_target` bytes with two deliberately off-aligned
/// splits in the middle of the buffer. Mirrors the irregular segment
/// arrival pattern an Ogg-FLAC demuxer hands to the validator — Ogg
/// segments don't align to FLAC frame boundaries, so the chunk lengths
/// the CRC sees in practice are deliberately not powers of two.
fn build_chunk_ranges(len: usize, chunk_target: usize) -> Vec<(usize, usize)> {
    let mut ranges: Vec<(usize, usize)> = Vec::new();
    let mut cursor = 0usize;
    let mut step = 0usize;
    while cursor < len {
        // Every fifth chunk gets a 13-byte off-alignment kick so the
        // chunked-streaming path is forced through a split that doesn't
        // land on a 16-byte / 32-byte natural boundary. A future
        // slice-by-N table tweak that only works on aligned chunks
        // would surface as a measurable slowdown here.
        let bump = if step % 5 == 4 { 13 } else { 0 };
        let end = (cursor + chunk_target + bump).min(len);
        ranges.push((cursor, end));
        cursor = end;
        step += 1;
    }
    ranges
}

// ---- CRC-8 (FLAC §6.1 frame header) ---------------------------------

fn bench_crc8_oneshot_8k(c: &mut Criterion) {
    let buf = build_buffer(8 * 1024);
    let expected = crc8(&buf);
    let mut g = c.benchmark_group("crc8_oneshot_8k");
    g.throughput(Throughput::Bytes(buf.len() as u64));
    g.bench_function(BenchmarkId::from_parameter("crc8/oneshot/8k"), |b| {
        b.iter(|| {
            let got = crc8(black_box(&buf));
            assert_eq!(got, expected);
            black_box(got);
        });
    });
    g.finish();
}

fn bench_crc8_streaming_chunked_8k(c: &mut Criterion) {
    let buf = build_buffer(8 * 1024);
    let expected = crc8(&buf);
    let ranges = build_chunk_ranges(buf.len(), 1024);
    let mut g = c.benchmark_group("crc8_streaming_chunked_8k");
    g.throughput(Throughput::Bytes(buf.len() as u64));
    g.bench_function(
        BenchmarkId::from_parameter("crc8/streaming-chunked/8k"),
        |b| {
            b.iter(|| {
                let mut c = Crc8::new();
                for &(lo, hi) in &ranges {
                    c.update(black_box(&buf[lo..hi]));
                }
                let got = c.value();
                assert_eq!(got, expected);
                black_box(got);
            });
        },
    );
    g.finish();
}

fn bench_crc8_streaming_byte_8k(c: &mut Criterion) {
    let buf = build_buffer(8 * 1024);
    let expected = crc8(&buf);
    let mut g = c.benchmark_group("crc8_streaming_byte_8k");
    g.throughput(Throughput::Bytes(buf.len() as u64));
    g.bench_function(BenchmarkId::from_parameter("crc8/streaming-byte/8k"), |b| {
        b.iter(|| {
            let mut c = Crc8::new();
            for &byte in black_box(&buf) {
                c.update_byte(byte);
            }
            let got = c.value();
            assert_eq!(got, expected);
            black_box(got);
        });
    });
    g.finish();
}

// ---- CRC-16 (FLAC §7 frame footer) ----------------------------------

fn bench_crc16_oneshot_8k(c: &mut Criterion) {
    let buf = build_buffer(8 * 1024);
    let expected = crc16(&buf);
    let mut g = c.benchmark_group("crc16_oneshot_8k");
    g.throughput(Throughput::Bytes(buf.len() as u64));
    g.bench_function(BenchmarkId::from_parameter("crc16/oneshot/8k"), |b| {
        b.iter(|| {
            let got = crc16(black_box(&buf));
            assert_eq!(got, expected);
            black_box(got);
        });
    });
    g.finish();
}

fn bench_crc16_streaming_chunked_8k(c: &mut Criterion) {
    let buf = build_buffer(8 * 1024);
    let expected = crc16(&buf);
    let ranges = build_chunk_ranges(buf.len(), 1024);
    let mut g = c.benchmark_group("crc16_streaming_chunked_8k");
    g.throughput(Throughput::Bytes(buf.len() as u64));
    g.bench_function(
        BenchmarkId::from_parameter("crc16/streaming-chunked/8k"),
        |b| {
            b.iter(|| {
                let mut c = Crc16::new();
                for &(lo, hi) in &ranges {
                    c.update(black_box(&buf[lo..hi]));
                }
                let got = c.value();
                assert_eq!(got, expected);
                black_box(got);
            });
        },
    );
    g.finish();
}

fn bench_crc16_streaming_byte_8k(c: &mut Criterion) {
    let buf = build_buffer(8 * 1024);
    let expected = crc16(&buf);
    let mut g = c.benchmark_group("crc16_streaming_byte_8k");
    g.throughput(Throughput::Bytes(buf.len() as u64));
    g.bench_function(
        BenchmarkId::from_parameter("crc16/streaming-byte/8k"),
        |b| {
            b.iter(|| {
                let mut c = Crc16::new();
                for &byte in black_box(&buf) {
                    c.update_byte(byte);
                }
                let got = c.value();
                assert_eq!(got, expected);
                black_box(got);
            });
        },
    );
    g.finish();
}

fn bench_crc16_oneshot_64k(c: &mut Criterion) {
    let buf = build_buffer(64 * 1024);
    let expected = crc16(&buf);
    let mut g = c.benchmark_group("crc16_oneshot_64k");
    g.throughput(Throughput::Bytes(buf.len() as u64));
    g.bench_function(BenchmarkId::from_parameter("crc16/oneshot/64k"), |b| {
        b.iter(|| {
            let got = crc16(black_box(&buf));
            assert_eq!(got, expected);
            black_box(got);
        });
    });
    g.finish();
}

fn bench_crc16_streaming_chunked_64k(c: &mut Criterion) {
    let buf = build_buffer(64 * 1024);
    let expected = crc16(&buf);
    let ranges = build_chunk_ranges(buf.len(), 4 * 1024);
    let mut g = c.benchmark_group("crc16_streaming_chunked_64k");
    g.throughput(Throughput::Bytes(buf.len() as u64));
    g.bench_function(
        BenchmarkId::from_parameter("crc16/streaming-chunked/64k"),
        |b| {
            b.iter(|| {
                let mut c = Crc16::new();
                for &(lo, hi) in &ranges {
                    c.update(black_box(&buf[lo..hi]));
                }
                let got = c.value();
                assert_eq!(got, expected);
                black_box(got);
            });
        },
    );
    g.finish();
}

criterion_group!(
    benches,
    bench_crc8_oneshot_8k,
    bench_crc8_streaming_chunked_8k,
    bench_crc8_streaming_byte_8k,
    bench_crc16_oneshot_8k,
    bench_crc16_streaming_chunked_8k,
    bench_crc16_streaming_byte_8k,
    bench_crc16_oneshot_64k,
    bench_crc16_streaming_chunked_64k,
);
criterion_main!(benches);
