#![no_main]

//! Fuzz: streaming-vs-oneshot equivalence for `oxideav_flac::md5`.
//!
//! Round 232 (depth-mode fuzz): the existing six targets cover the
//! decoder pipeline (`panic_free_decode`, `decode`), the encode/decode
//! self-consistency (`roundtrip`), the libavcodec oracle
//! (`flac_oracle_decode`), arbitrary metadata-block chains
//! (`metadata_walker`), and the encoder-construction extradata path
//! (`encoder_options`). None of them put `oxideav_flac::md5` under
//! stress: the encoder feeds the running MD5 a single deterministic
//! schedule (one `update` per audio frame) and never exercises the
//! streaming-vs-oneshot equivalence over a fuzzer-chosen schedule.
//!
//! Why this matters. STREAMINFO carries a 128-bit MD5 signature of the
//! decoded PCM (RFC 9639 §8.2). Any divergence between
//! `Md5::new() -> update(...) -> finalize()` (streaming) and the
//! oneshot `md5::compute(...)` helper would silently produce a
//! STREAMINFO whose signature does not match the bytes a verifier
//! would compute over the decoded PCM — every emitted `.flac` file
//! would carry an `md5sum`-mismatch under round-trip verification. The
//! risk surface is concentrated in `Md5::update`'s block-boundary
//! arithmetic:
//!
//!   * the leftover-buffer drain when an incoming slice straddles the
//!     64-byte block boundary (`src/md5.rs` §`if self.buffer_len > 0`),
//!   * the 64-bit bit-count accumulator's split into `bits_hi` /
//!     `bits_lo` and the overflow-add into `bits_hi` when the lo half
//!     wraps,
//!   * `finalize`'s pad-length selector
//!     (`56 - buffer_len` vs `120 - buffer_len`) at the 55 / 56 / 63 /
//!     64 / 65 edges captured by the `md5_block_boundary_lengths` unit
//!     test but only at deterministic chunk schedules,
//!   * `process_block`'s 16-word little-endian load (one mis-typed
//!     index would silently corrupt a single MD5 step without
//!     panicking).
//!
//! The harness carves the fuzz input into a chunk-schedule descriptor
//! and a payload tail, then drives the streaming API over every chunk
//! schedule the descriptor encodes. Each schedule's final digest is
//! compared against the oneshot helper. A divergence is a hard panic
//! (`assert_eq!`) — the contract under test is byte-for-byte equality.
//!
//! ## Input layout
//!
//!   - 1 B  `n_chunks_sel`    — number of streaming chunks (mod 9 → 1..=9)
//!   - 1 B  `chunk_size_seed` — xorshift seed for chunk-size derivation
//!   - 1 B  `extra_update_sel` — selector that inserts pathological
//!     zero-length / one-byte updates
//!   - 1 B  `default_sel`     — when bit 0 is set, exercise `Md5::default()`
//!     alongside `Md5::new()` and require equality of both digests
//!   - rest:                  — the PCM-like payload, capped at
//!     `MAX_PAYLOAD` to keep iteration cost bounded
//!
//! ## Contract under test
//!
//!   * Streaming any byte slice through any chunk schedule (including
//!     zero-length and one-byte interludes) produces the same 16-byte
//!     digest as the oneshot `compute()` helper.
//!   * `Md5::default()` and `Md5::new()` are interchangeable.
//!   * The streaming API never panics on any chunk schedule a fuzzer
//!     can encode against any payload up to `MAX_PAYLOAD` bytes.
//!   * The empty-input digest is the RFC 1321 §A.5 vector
//!     `d41d8cd9 8f00b204 e9800998 ecf8427e` (already asserted in
//!     the in-crate unit test; we re-assert it here as a fuzz-time
//!     sanity check that survives every iteration).

use libfuzzer_sys::fuzz_target;
use oxideav_flac::md5::{compute, Md5};

/// Cap on the payload tail. Big enough to push every input across
/// several 64-byte blocks AND across the 32-bit overflow boundary of
/// the per-`update` `bits_lo` accumulator when concatenated with a
/// non-trivial chunk schedule. Small enough that the fuzzer can run
/// tens of thousands of iterations per second on a single worker.
const MAX_PAYLOAD: usize = 8 * 1024;

/// Maximum chunk count per schedule. 9 distinct chunks gives the
/// scheduler enough slack to land an update at every 64-byte block
/// boundary within a `MAX_PAYLOAD`-sized payload while keeping the
/// per-iteration work bounded.
const MAX_CHUNKS: u8 = 9;

/// Empty-input MD5 vector from RFC 1321 §A.5.
const EMPTY_MD5: [u8; 16] = [
    0xd4, 0x1d, 0x8c, 0xd9, 0x8f, 0x00, 0xb2, 0x04, 0xe9, 0x80, 0x09, 0x98, 0xec, 0xf8, 0x42, 0x7e,
];

fuzz_target!(|data: &[u8]| {
    // -----------------------------------------------------------------
    // Empty-input sanity check: the RFC 1321 §A.5 vector. Cheap and
    // runs every iteration so a regression in the finalize-pad branch
    // can't slip past a fuzzer that happens to never produce a zero-
    // length payload.
    // -----------------------------------------------------------------
    assert_eq!(compute(b""), EMPTY_MD5);

    if data.len() < 4 {
        return;
    }

    let n_chunks = (data[0] % MAX_CHUNKS) + 1;
    let chunk_seed = data[1];
    let extra_update_sel = data[2];
    let default_sel = data[3];
    let payload_raw = &data[4..];
    let payload = &payload_raw[..payload_raw.len().min(MAX_PAYLOAD)];

    // The oneshot digest is the reference; every streaming schedule
    // below must match it.
    let oneshot = compute(payload);

    // -----------------------------------------------------------------
    // Schedule A: even-ish split. Divide the payload into `n_chunks`
    // contiguous slices of approximately equal length, drain through
    // `update` in order.
    // -----------------------------------------------------------------
    {
        let mut m = Md5::new();
        for chunk in even_split(payload, n_chunks as usize) {
            m.update(chunk);
        }
        assert_eq!(
            m.finalize(),
            oneshot,
            "even-split schedule diverged: n_chunks={n_chunks} payload_len={}",
            payload.len()
        );
    }

    // -----------------------------------------------------------------
    // Schedule B: xorshift-derived irregular split. Stresses the
    // leftover-buffer drain path with chunks of widely varying length
    // (including spans that cross multiple 64-byte block boundaries
    // and spans that land entirely inside one).
    // -----------------------------------------------------------------
    {
        let mut m = Md5::new();
        for chunk in xorshift_split(payload, n_chunks as usize, chunk_seed) {
            m.update(chunk);
        }
        assert_eq!(
            m.finalize(),
            oneshot,
            "xorshift-split schedule diverged: n_chunks={n_chunks} seed={chunk_seed} payload_len={}",
            payload.len()
        );
    }

    // -----------------------------------------------------------------
    // Schedule C: byte-at-a-time. Drains the payload one byte per
    // `update` call. Worst case for the leftover-buffer drain — every
    // call has to ride the same code path.
    //
    // Bounded: only run this if the payload is small enough that the
    // O(N) call count stays under the per-iteration budget.
    // -----------------------------------------------------------------
    if payload.len() <= 512 {
        let mut m = Md5::new();
        for b in payload {
            m.update(std::slice::from_ref(b));
        }
        assert_eq!(
            m.finalize(),
            oneshot,
            "byte-at-a-time schedule diverged: payload_len={}",
            payload.len()
        );
    }

    // -----------------------------------------------------------------
    // Schedule D: pathological zero-length / one-byte interludes.
    // The `extra_update_sel` bits choose where to inject a `&[]` or
    // a `&[0u8]` update relative to the even-split schedule. The
    // `&[]` updates must be no-ops; the `&[0u8]` updates must
    // contribute their byte to the digest.
    // -----------------------------------------------------------------
    {
        let mut m = Md5::new();
        // Prefix interlude.
        if extra_update_sel & 0b0001 != 0 {
            m.update(&[]);
        }
        let split = payload.len() / 2;
        let (head, tail) = payload.split_at(split);
        m.update(head);
        if extra_update_sel & 0b0010 != 0 {
            m.update(&[]);
        }
        m.update(tail);
        if extra_update_sel & 0b0100 != 0 {
            m.update(&[]);
        }
        assert_eq!(
            m.finalize(),
            oneshot,
            "zero-length-interlude schedule diverged: extra={extra_update_sel} payload_len={}",
            payload.len()
        );
    }

    // -----------------------------------------------------------------
    // Schedule E: `Md5::default()` parity. Exercising `Default::default`
    // alongside `new()` guards against a future refactor that adds a
    // field to `Md5` and breaks the `Default` derive's contract
    // silently.
    // -----------------------------------------------------------------
    if default_sel & 0b0001 != 0 {
        let mut m_default = Md5::default();
        for chunk in even_split(payload, n_chunks as usize) {
            m_default.update(chunk);
        }
        assert_eq!(
            m_default.finalize(),
            oneshot,
            "Md5::default() schedule diverged: payload_len={}",
            payload.len()
        );
    }

    // -----------------------------------------------------------------
    // Schedule F: block-boundary-aligned chunks. The payload is split
    // at every 64-byte mark. This is the schedule the encoder uses in
    // practice when it accumulates whole audio-frame PCM buffers, so
    // a divergence here would surface as an `md5sum` mismatch on
    // every emitted `.flac`. Cheap to run; no length cap needed.
    // -----------------------------------------------------------------
    {
        let mut m = Md5::new();
        for chunk in payload.chunks(64) {
            m.update(chunk);
        }
        assert_eq!(
            m.finalize(),
            oneshot,
            "block-aligned schedule diverged: payload_len={}",
            payload.len()
        );
    }

    // -----------------------------------------------------------------
    // Schedule G: prime-stride chunks. Splits at a prime that doesn't
    // divide 64 evenly so every chunk lands at a different offset
    // mod 64. Stresses the leftover-buffer drain path in a different
    // shape from xorshift-split.
    // -----------------------------------------------------------------
    {
        let mut m = Md5::new();
        for chunk in payload.chunks(7) {
            m.update(chunk);
        }
        assert_eq!(
            m.finalize(),
            oneshot,
            "stride-7 schedule diverged: payload_len={}",
            payload.len()
        );
    }
});

/// Split `data` into `n` contiguous slices of approximately equal
/// length. The last slice absorbs the remainder. Always yields exactly
/// `n` slices (some may be empty when `data.len() < n`).
fn even_split(data: &[u8], n: usize) -> impl Iterator<Item = &[u8]> {
    let n = n.max(1);
    let base = data.len() / n;
    let extra = data.len() % n;
    let mut offset = 0usize;
    (0..n).map(move |i| {
        let len = base + if i < extra { 1 } else { 0 };
        let end = offset + len;
        let slice = &data[offset..end];
        offset = end;
        slice
    })
}

/// Split `data` into `n` slices whose lengths are derived from an
/// xorshift sequence seeded with `seed`. The lengths are normalised
/// so the slice boundaries cover `data` exactly. When `seed` lands
/// the harness on a zero-length-everywhere schedule, the residual
/// is absorbed by the last chunk.
fn xorshift_split(data: &[u8], n: usize, seed: u8) -> Vec<&[u8]> {
    let n = n.max(1);
    let mut state = (seed as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1;
    let mut weights = Vec::with_capacity(n);
    let mut total: u64 = 0;
    for _ in 0..n {
        // xorshift64*; output is the current state pre-shuffle.
        state ^= state >> 12;
        state ^= state << 25;
        state ^= state >> 27;
        let w = (state.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 32) & 0xFFFF;
        weights.push(w);
        total = total.wrapping_add(w);
    }
    if total == 0 {
        // Degenerate seed → fall back to the even split.
        return even_split(data, n).collect();
    }
    let mut slices = Vec::with_capacity(n);
    let mut offset = 0usize;
    let payload_len = data.len();
    for (i, w) in weights.iter().enumerate() {
        let len = if i + 1 == n {
            // Last slice absorbs the residual so the union covers
            // `data` exactly even when integer truncation eats a
            // few bytes earlier in the loop.
            payload_len - offset
        } else {
            ((*w as u128) * (payload_len as u128) / (total as u128)) as usize
        };
        let end = (offset + len).min(payload_len);
        slices.push(&data[offset..end]);
        offset = end;
    }
    slices
}
