# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- bench: round-218 `crc` harness — eight scenarios measuring the
  streaming `Crc8` / `Crc16` validators (round-212) and the
  one-shot `crc8` / `crc16` slice helpers against deterministic
  xorshift-synthesised byte buffers sized to typical FLAC frame
  footprints (8 KiB) and a worst-case multichannel high-bit-depth
  frame (64 KiB). Three CRC-8 scenarios (one-shot, chunked streaming
  via `update(&[u8])` with off-aligned splits to mimic Ogg-FLAC
  segment arrival, byte-at-a-time via `update_byte`) at 8 KiB; the
  same three CRC-16 scenarios at 8 KiB plus one-shot + chunked
  CRC-16 at 64 KiB. CRC-8 is skipped at 64 KiB because RFC 9639 §6.1
  caps the header it covers at ≤ 16 bytes. Every bench iteration
  asserts the produced value matches the one-shot helper so a
  regression that broke the streaming path silently surfaces as a
  benchmark panic instead of a 1.1x speedup walking different bytes.
  Baseline numbers (release, Darwin aarch64): CRC-8 ~775 MiB/s
  one-shot, ~735 MiB/s chunked, ~725 MiB/s byte-at-a-time; CRC-16
  ~500 MiB/s across every 8 KiB and 64 KiB shape, confirming the
  inner loop is table-lookup-bound and bandwidth-flat past the L1
  working set. The bench gives future CRC tweaks (a slice-by-16
  table, a SIMD `pclmulqdq`-driven inner loop, an
  inline-attribute audit) a stable A/B baseline; sized per
  RFC 9639 §6.1 (header CRC-8) and §7 (frame footer CRC-16).
- crc: round-212 streaming CRC-8 / CRC-16 validators (`Crc8` /
  `Crc16` in `oxideav_flac::crc`) for callers that consume FLAC bytes
  in pieces — Ogg-FLAC demuxers fetching one Ogg segment at a time, a
  network-fed decoder fronted by a ring buffer, an in-place verifier
  that wants to early-fail before allocating subframe sample vectors.
  Both expose `new()` / `update_byte(b)` / `update(&[u8])` / `value()`
  and share the same lookup tables as the existing one-shot `crc8` /
  `crc16` slice helpers (which are now thin wrappers around the
  streaming types — single byte-loop either way, no extra cost per
  decode). The frame-CRC check site in `decoder::decode_one_frame` is
  rewired to fold through `Crc16` in production so a regression that
  broke `Crc16::update` would surface on every decode call instead of
  only on the unit-test path. Six unit tests pin the streaming /
  one-shot equivalence: byte-at-a-time and chunked variants for each
  width plus an empty-update no-op test (Ogg-FLAC EOS / zero-byte
  network reads must not perturb the running state) and a
  chunk-boundary independence test that sweeps every split point
  1..n-1 and confirms each pair-split agrees with the one-shot value.

- bench: round-207 roundtrip coverage for the three scenarios that
  encode (r150) and decode (r195) gained but roundtrip never picked
  up. `roundtrip_mono_s24_48k_500ms` (wide-sample single-channel
  end-to-end pipeline, no stereo decorrelation search), `roundtrip
  _mono_noise_s16_44k1_1s` (uniform-noise content that pushes the
  per-partition Rice parameter `k` toward / past the RFC 9639 §9.2.7
  escape marker; the bench-body `recovered_bytes == input` invariant
  doubles as a regression guard that the escape branch never silently
  drops or duplicates samples on either direction), and
  `roundtrip_stereo_s32_96k_250ms` (full-width 32-bit residual + LPC
  + S32 little-endian interleave — the only roundtrip path that
  reaches the encoder's and decoder's 32-bit branches end-to-end).
  Baseline numbers (release, Darwin aarch64, `--quick --measurement-
  time 1 --warm-up-time 1`): `roundtrip_mono_s24_48k_500ms` ≈
  120.7 ms, `roundtrip_mono_noise_s16_44k1_1s` ≈ 267 ms,
  `roundtrip_stereo_s32_96k_250ms` ≈ 277 ms — all within ~1 ms of the
  encode-only number for the same shape (the decode pass is
  effectively free at the criterion timer's resolution, which matches
  the per-bench `decode` table where every scenario is ≤ 1 ms).
- fuzz: round-200 `metadata_walker` target — walks a `fLaC`-magic-led
  METADATA_BLOCK chain through `BlockHeader::parse` → typed parser
  (StreamInfo / SeekTable / CueSheet / Picture / PADDING) → matching
  writer (`write_seektable` / `write_cuesheet` / `write_picture` /
  `write_padding`) → re-parse, asserting parser ∘ writer ∘ parser
  == parser at every step. First fuzz coverage for `parse_picture`,
  `write_picture`, and `BlockHeader::write_into`, and the first
  writer-direction round-trip fuzz for any FLAC metadata block.
  Bounded loop (≤ 16 blocks per iteration, ≤ 64 KiB per payload)
  keeps each libFuzzer iteration cheap. Auto-discovered by the
  filesystem walk in the org-level `crate-fuzz.yml` workflow, so no
  caller-side wiring is needed.
- bench: round-195 decoder coverage for mono-S24, uniform-noise Rice
  hot path (escape-partition branch per RFC 9639 §9.2.7), and full-
  width S32 sample path. Pairs the existing `encode_mono_s24` with a
  matching decode scenario, exercises `subframe::decode_residual`
  when `k` is forced toward the escape marker, and adds the only
  decode scenario that drives the 32-bit residual + S32 interleave
  output. Baseline numbers (release, Darwin aarch64):
  `decode_mono_s24_48k_500ms` ≈ 306 µs (~224 MiB/s),
  `decode_mono_noise_s16_44k1_1s` ≈ 635 µs (~132 MiB/s — the
  slowest decode scenario, confirming residual reader is on the hot
  path rather than VERBATIM-short-circuited),
  `decode_stereo_s32_96k_250ms` ≈ 657 µs (~279 MiB/s).

## [0.0.10](https://github.com/OxideAV/oxideav-flac/compare/v0.0.9...v0.0.10) - 2026-05-30

### Other

- encoder-owned scratch reused across candidate sweep
- subframe-scoped prefix-sum + raw-bits tables (RFC 9639 §9.2.5)
- in-place symmetric-pair Levinson-Durbin update (RFC 9639 §9.2.6)
- hoist residual zigzag to once-per-subframe (RFC 9639 §9.2.5)
- add typed PICTURE accessor + writer (RFC 9639 §8.8)
- wire write_padding into make_encoder_with_options
- add PADDING writer + composable block-header serialiser
- round-150 encoder coverage for wide-mono / wasted-bits / CONSTANT
- capture round-147 SVG flamegraphs for encode/decode/roundtrip
- closed-form Rice parameter estimate cuts encode 4.6x-6.0x
- add profile_flac.rs driver for samply / perf / flamegraph
- add criterion benches for encode / decode / roundtrip
- add sibling-shape decode target
- add SEEKTABLE writer + property-style sine sweep test
- search Welch/Hann/Tukey apodization windows per LPC subframe
- search LPC coefficient precision per subframe
- raise MAX_LPC_ORDER from 8 to 12 (per RFC 9639 §9.2.6)
- adaptive LPC coefficient precision from block size
- search partition orders 0..=8 for residual Rice coding
- add write_cuesheet for round-trip CUESHEET emission
- rewrite library-citation comments to remove external-library references
- parse CUESHEET block + surface as Demuxer::chapters()
- reject bps>32 to fix panic the fuzz harness found
- add cargo-fuzz scaffold + libavcodec FLAC oracle

### Changed

- **Encoder: encoder-owned reusable scratch for the per-subframe
  candidate sweep** (round 191, RFC 9639 §9.2). The per-subframe driver
  evaluates CONSTANT + FIXED 0..=4 + LPC 1..=12 + VERBATIM with
  per-LPC-order apodisation × precision search, then partitioned-Rice
  codes the residual. Pre-round-191 each surviving candidate allocated
  a fresh `Vec<i32>` residual, the post-r186
  `encode_rice_residual` then allocated a `Vec<u32>` zigzag plus
  `Vec<u64>` prefix-sum plus `Vec<u8>` raw-bits triple per call, and
  every LPC `(window, order)` pair allocated three `Vec<f64>` work
  buffers (`windowed`, `autoc`, `lpc_state`) plus the output `Vec<f64>`
  coefficient vector. Round 191 lifts every one of those into a single
  `SubframeScratch` struct owned by the `FlacEncoder` itself; the
  candidate sweep clears + reuses each buffer in place via
  `std::mem::take`, the encoder reuses them across every frame in the
  stream, and capacities grow to peak on the first block and stay
  there. The numeric output is byte-identical (every roundtrip test
  passes unchanged; the `sine_sweep_bit_exact_and_compresses_below_verbatim`
  integration test verifies the encoded bytes against the deterministic
  sweep). Bench A/B against the pre-r191 baseline on Apple M-class
  hardware (criterion `--sample-size 10 --warm-up-time 2
  --measurement-time 8`, two-run mean of the lower/upper change-CI
  bounds):
  - `mono/s16/44k1/1s`: 254 → 252 ms (~1.1 % faster, p < 0.01)
  - `stereo/s16/44k1/1s`: 1013 → 1005 ms (~0.9 % faster, p < 0.01)
  - `stereo/s24/48k/500ms`: 495 → 491 ms (~0.8 % faster, p < 0.05)
  - `6ch/s16/48k/250ms`: 420 → 388 ms (~7.4 % faster, p ≈ 0.06)
  - `mono/s24/48k/500ms`: 116 → 115 ms (~1.4 % faster, p < 0.01)
  - `mono/wasted_bits/s16/44k1/1s`: 261 → 261 ms (~1.0 % faster, p < 0.05)
  - `mono/constant/s16/44k1/1s`: 266 → 263 µs (~2.1 % faster, p < 0.01)

  The 6-channel scenario benefits the most: six independent subframes
  per frame mean the scratch-reuse savings compound across more
  candidates per block, while the per-allocation overhead the reuse
  replaces is essentially fixed regardless of channel count. The other
  scenarios all show statistically-significant but smaller wins
  (1–2 %), consistent with the residual-allocation paths the round
  targets accounting for a small but non-zero slice of the per-subframe
  cost (the Rice candidate scoring still dominates per-subframe).

  Round 150 had attempted the same "reuse residual scratch across
  plans" follow-up against the post-r147 hot path and measured a
  ~10 % *regression* — that hot path's per-plan allocations were small
  enough for libmalloc's small-bin freelist that the threading and
  `std::mem::take`/restore overhead exceeded the savings. The
  post-r186 hot path allocates a wider set of subframe-scoped
  buffers (the new prefix-sum + raw-bits tables) on top of the
  residual + zigzag pair, and the wider allocation surface is what
  tips the trade-off the other way at r191. The legacy
  `best_subframe`, `encode_fixed_plan`, `encode_lpc_plan`,
  `encode_lpc_plan_at`, `encode_rice_residual`, `levinson_durbin`,
  `quantize_lpc`, and `build_partition_tables` helpers are retained
  behind `#[cfg(test)]` as the wrap-each-call-in-a-fresh-scratch
  reference path the regression suite still drives directly.

- **Encoder: subframe-scoped prefix-sum + raw-bits tables for the
  partitioned-Rice search** (round 186, RFC 9639 §9.2.5). Pre-round-186
  the per-partition `sum_u` reduction inside `best_rice_params_z` (the
  input to the closed-form `k_est` derivation) and the per-partition
  `raw_bits_needed` scan inside `best_partition_z` (the escape-cost
  test) both re-walked the partition slice on every
  `(method, partition_order)` pair the search visited — for a 4096-sample
  subframe, that's 2 methods × up to 9 orders × N partitions = up to
  ~1000 redundant linear scans per subframe. Round 186 folds both
  scans up to **once per subframe**: a `zigzag_prefix[i] = sum(zigzag[0..i])`
  table reduces any partition's `sum_u` to a single subtraction
  (`prefix[end] - prefix[start]`), and a per-sample `raw_bits[i]: u8`
  table replaces the escape path's `leading_zeros`-on-i32 inner with
  a max over a precomputed `u8` slice. The `rice_cost_for_k` walks
  still run per candidate `k` — that's the inherent cost of scoring
  a Rice parameter — but every other per-partition O(n) pass is
  removed from the hot path. The pre-round-186 functions
  (`partition_layout_cost_z`, `best_partition_z`, `best_rice_params_z`,
  `raw_bits_needed`) are retained behind `#[cfg(test)]` as the
  reference path; the existing `partition_layout_cost_z_matches_reference`
  regression test now crosswalks **both** the round-176 `_z` path and
  the round-186 `_zp` path against the standalone `partition_layout_cost`
  reference across all five existing residual cases (all-zero,
  single-spike, three Laplace means, split-statistics) plus a new
  `edge-i32-bounds` case that probes `raw_bits_for_sample` at the
  `i32::MIN` / `i32::MAX` extremes; the new `_zp` path is bit-identical
  to both. Measured wins on the existing encode benches (M-series
  laptop, `--quick --measurement-time 3`): `mono/s24/48k/500ms`
  160 ms → 128 ms (~20 %), `stereo/s24/48k/500ms` 622 ms → 511 ms
  (~18 %), `6ch/s16/48k/250ms` 474 ms → 410 ms (~13 %),
  `stereo/s16/44k1/1s` 1095 ms → 1055 ms (~4 %). The wider-sample
  scenarios benefit the most: `raw_bits_for_sample` is invoked once
  per residual element regardless of bit depth, so removing its
  hot-path repetition shows up first on subframes whose residuals
  carry more bits. Decoder, encoder on-wire output, and every
  existing encode-decode round-trip pass byte-identically.

- **Encoder: in-place symmetric-pair Levinson-Durbin update**
  (round 182, RFC 9639 §9.2.6). The autocorrelation-to-coefficient
  step at the heart of LPC analysis ran a textbook Levinson-Durbin
  recurrence whose inner update allocated a fresh `new_lpc =
  lpc.clone()` `Vec<f64>` on every outer-loop iteration. With LPC
  order capped at 12 (`MAX_LPC_ORDER`) that meant up to 12 redundant
  Vec allocations plus 12 length-12 element copies *per* call —
  multiplied by the 3 apodisation windows × 12 LPC orders that
  `encode_lpc_plan` evaluates per subframe (36 calls), the per-frame
  cost added up to several hundred small heap traffic events
  exclusively to hold a 1-step lookback the recurrence already had in
  registers. The new sweep rewrites both halves of each symmetric
  index pair `(j, i-1-j)` in place from cached scalars (`a`, `b`),
  needing no scratch buffer. Floating-point operands and evaluation
  order are unchanged — both writes read the **old** `lpc[j]` and
  `lpc[i-1-j]` via locals before either lane is mutated — so the
  produced f64 coefficients are bit-identical to the cloning path
  and the quantised on-wire bytes don't drift. A new regression test
  (`levinson_durbin_in_place_matches_reference`) keeps a `#[cfg(test)]`
  reference implementation in scope and crosswalks the production
  path against it across four signal classes (sine sweep at four
  frequencies, stacked partials, pseudo-random walk, tonal-then-
  silence), every apodisation window (Welch / Hann / shallow Tukey)
  and every LPC order 1..=12, comparing coefficient outputs at
  `f64::to_bits()` granularity so any future drift in the in-place
  sweep surfaces before bytes change. Full test suite passes
  unchanged.

- **Encoder: amortise residual zigzag across the partition-order search**
  (round 176, RFC 9639 §9.2.5). Pre-round-176 the partitioned-Rice
  search re-zigzagged every residual partition under every
  `(method, partition_order)` pair it visited — for a 4096-sample
  subframe that meant `2 methods × up to 9 orders × N partitions`
  fresh `Vec<u32>` allocations + linear scans, with `best_partition`
  pinned at ~61 % of encode self-samples in the round-147 captured
  flamegraph. The hot path now zigzags the entire residual span once
  per subframe in `encode_rice_residual` and re-slices that single
  buffer across every layout the search probes
  (`partition_layout_cost_z` → `best_partition_z` → `best_rice_params_z`).
  Both the search and the emit loop (Rice quotient / remainder split)
  read from the cached buffer. The pre-round-176 functions
  (`partition_layout_cost`, `best_partition`, `best_rice_params`) are
  retained behind `#[cfg(test)]` as the reference path; a new regression
  test (`partition_layout_cost_z_matches_reference`) crosswalks the
  optimised path against the reference across every
  `(predictor_order, param_bits, partition_order)` combination on
  five hand-tuned residual buffers (all-zero, single-spike,
  Laplace(mean=2/32/512), split-statistics) so any future drift
  surfaces before the on-wire output can change. Measured wins on the
  existing encode benches (M-series laptop, `--quick --measurement-time 3`):
  `mono/s16/44k1/1s` 316 ms → 272 ms (~14 %), `stereo/s16/44k1/1s`
  1245 ms → 1096 ms (~12 %), `mono/wasted/s16/44k1/1s` 368 ms → 284 ms
  (~23 %). Decoder and on-wire output are unchanged: every existing
  encode-decode round-trip + the partitioned-Rice search test pass
  byte-identically.

### Added

- **PICTURE typed accessor + writer** (round 163, RFC 9639 §8.8):
  `metadata::parse_picture` and `metadata::write_picture` round-trip a
  PICTURE block payload through a typed `Picture` struct that carries
  every field RFC 9639 §8.8 / Table 12 defines —
  `picture_type: u32`, `mime_type: String`, `description: String`,
  the four informational fields `width` / `height` / `depth` /
  `colour_count` (all `u32`), and `data: Vec<u8>`. The container's
  pre-round-163 inline parser had two surface bugs that the typed
  parser fixes: (a) it silently dropped 16 bytes of pixel metadata
  (skipping past `width / height / depth / colour_count` with
  `i += 16`) so callers could never inspect those fields, and (b) it
  used `from_utf8(...).unwrap_or("")` on the mime / description
  fields so an attacker-controlled non-UTF-8 byte sequence would
  round-trip through an empty string. The typed parser enforces the
  §8.8 printable-ASCII rule on `mime_type` (rejects any byte outside
  `0x20..=0x7E`, including the special-cased `-->` URI sentinel
  which passes by construction), enforces strict UTF-8 on
  `description`, and rejects every truncation site (declared mime /
  description / data lengths that overshoot the payload). The
  container's `parse_flac_picture_block` now delegates to the typed
  parser before projecting onto the framework's narrower
  `AttachedPicture` accessor, so the demuxer inherits all of those
  fixes without changing its public surface. The writer round-trips
  through `parse_picture` byte-for-byte, accepts the spec-mandated
  all-zero pixel-fields shape for vector images, preserves the full
  32-bit `picture_type` (so a producer that smuggled a non-APIC code
  through gets it back unchanged on the next read), and refuses
  invalid mime bytes / oversized strings before emitting any output.
  Twelve new unit tests in `metadata::tests` pin the round-trip
  behaviour (JPEG front cover, indexed-GIF colour palette, vector
  SVG with zero pixel fields, `-->` URI sentinel), the strict
  parser checks (non-printable mime byte, invalid-UTF-8 description,
  truncation at every internal field boundary, oversize mime /
  data lengths, random-input panic-freedom) and the full-u32
  picture-type preservation. Lib test count grows from 80 to 92.

- **PADDING reservation in the encoder** (round 158, RFC 9639 §8.6):
  new `encoder::FlacEncoderOptions` (`#[non_exhaustive]`) with a
  `padding_bytes: Option<usize>` field and a new factory
  `encoder::make_encoder_with_options(params, options)` that wires
  the round-155 `metadata::write_padding` payload writer + the
  composable `BlockHeader::write_into` header serialiser into the
  encoder. When `padding_bytes` is `Some(n)`, the produced
  `extradata` is `STREAMINFO(non-last) + PADDING(last, length=n,
  all-zero)` instead of the previous `STREAMINFO(last)` chain;
  reserved PADDING lets downstream tools rewrite tags / cuesheets in
  place without shifting the audio frame bytes. The bound check
  happens at construction time — a request above
  `BlockHeader::MAX_LENGTH` (2²⁴ − 1) returns `Error::invalid`
  immediately rather than panicking inside `flush`. The default
  `make_encoder(params)` factory now delegates to
  `make_encoder_with_options(params, FlacEncoderOptions::default())`,
  which is observationally identical to the pre-round-158 behaviour
  (the default has `padding_bytes: None`, so the 38-byte
  STREAMINFO-only chain is preserved byte-for-byte). Six new tests
  in `encoder::tests` pin:
    * the default factory still emits the exact 38-byte
      STREAMINFO-only chain (no silent regression for existing
      callers);
    * the opt-in chain is well-formed (STREAMINFO non-last header,
      last-flagged PADDING header, payload length matches
      `padding_bytes`, payload is all-zero) — both for an 8 KiB
      reservation (FFmpeg's documented default) and a 0-byte
      reservation (still a legal PADDING block);
    * the post-flush STREAMINFO rebuild after real PCM input
      preserves the PADDING tail and only rewrites the STREAMINFO
      payload (MD5, min/max frame size, total sample count);
    * the chain walks back through the same `BlockHeader::parse`
      loop the demuxer uses to read metadata from a `.flac` file —
      so an encoder-emitted PADDING block round-trips through the
      crate's own demuxer;
    * an oversize PADDING request fails at construction time with
      a clear "padding_bytes / 24-bit max" error message rather
      than panicking later.
  Encoder test count grows from 28 to 34 (lib test total 74 → 80).

- **PADDING metadata-block writer** (round 155, RFC 9639 §8.6):
  `metadata::write_padding(len)` produces a `len`-byte all-zero payload
  for inclusion in a FLAC metadata chain, capped at the 24-bit block-
  header length ceiling so the writer cannot generate a payload no
  conformant reader will accept. Paired with two new helpers that make
  every metadata-payload writer (CUESHEET, SEEKTABLE, PADDING)
  composable into a real on-wire chain:
  - `BlockType::to_byte()` — encodes the type into the low 7 bits of
    the metadata-block header byte; returns `None` for the spec's
    `Invalid` sentinel (code 127) and for `Reserved` codes that would
    collide with the `last` flag bit.
  - `BlockHeader::write_into(&mut Vec<u8>)` / `BlockHeader::to_bytes()`
    — serialises the 4-byte block-header prefix (`last` flag + 7-bit
    type code + 24-bit big-endian length). Rejects payload lengths
    above `BlockHeader::MAX_LENGTH` (2²⁴ − 1) and refuses to emit
    the `Invalid` block type.
  Nine new unit tests pin the byte-for-byte wire layout, parser
  round-trips for every defined block-type code, and a smallest-
  realistic STREAMINFO + last-PADDING metadata-chain walk-back. The
  metadata module's test count grows from 25 to 34.

- **Three new Criterion encode-bench scenarios** (round 150) that cover
  encoder code paths the existing four bench scenarios under-exercise.
  The earlier suite (mono/stereo S16, stereo S24, 6-channel S16) all
  feed tone-plus-noise content into the default 4096-sample block path,
  which means the LPC + partition search dominates every measurement
  and any change to the wasted-bits / CONSTANT-subframe / single-channel-
  wide-sample code paths is invisible. The additions:
  - `encode_mono_s24_48k_500ms` — 0.5 s of mono S24 at 48 kHz. Isolates
    single-channel wide-sample cost without the stereo decorrelation
    search of the existing `stereo_s24` scenario. Round-150 measurement:
    160 ms/iter (~430 KiB/s) — roughly half the stereo S24 cost, as
    expected for one channel without the four-way assignment search.
  - `encode_mono_wasted_bits_s16_44k1_1s` — 1 s of mono S16 whose
    samples carry only an 8-bit effective range (every low 8 bits are
    zero). Exercises the wasted-bits detector + narrowed-bps subframe
    path. Round-150 measurement: 368 ms/iter (~234 KiB/s); slower than
    the regular mono S16 scenario because the narrowed bps still leaves
    the full LPC + partition search in play.
  - `encode_mono_constant_s16_44k1_1s` — 1 s of mono DC content.
    Short-circuits each block to a CONSTANT subframe, so the bench
    measures per-block driver overhead (de-interleave, MD5 feed,
    frame header walk) without any LPC / partition cost. Round-150
    measurement: 246 µs/iter (~340 MiB/s) — a four-order-of-magnitude
    delta vs the LPC-heavy scenarios that flags the CONSTANT
    short-circuit as a regression-test target should a future round
    inadvertently lose it.

### Notes

- **Round 150 investigated the round-147 follow-up suggestion** to
  reuse a single residual `Vec<i32>` scratch across subframes /
  candidate plans in `FlacEncoder::encode_ready_frames`. The
  refactor (thread a per-encoder `Scratch { residuals, zigzag, … }`
  through `encode_frame`, `best_subframe`, `encode_fixed_plan`,
  `encode_lpc_plan_at`, `encode_rice_residual`, `best_partition`)
  was implemented end-to-end with byte-for-byte identical output
  verification across all four `examples/profile_flac.rs` scenarios
  (`profile_flac encode 3` shows the same packet byte count and
  fnv64 hash before vs after). The bench result was a **~10 %
  regression** on every scenario (mono S16 290 → 322 ms, stereo S16
  1150 → 1290 ms, stereo S24 593 → 666 ms, 6-channel 443 → 499 ms).
  Two simpler variants — a partition-choice cache that skipped the
  duplicate `best_partition` work in the emission loop, and a single-
  pass `zigzag + sum_u` fold in `best_rice_params` — were also tried
  and produced the same direction of regression. The proximate
  cause: macOS libmalloc's small-bin caching makes the existing
  `Vec::with_capacity(small)` calls effectively a freelist pop, and
  the threading overhead (`std::mem::take` + restore + capacity-
  checked `push` in place of `iter().map().collect()`) costs more
  than the allocator paths the flamegraph attributed to the inner
  loops. The two-pass `collect` + `iter().sum()` shape is also
  auto-vectorised by rustc 1.95, which a hand-rolled single-pass
  loop appears to defeat. None of these changes shipped — the
  encoder is unchanged structurally. The r147 finding about
  allocator self-samples remains, but the eliminate-the-allocations
  cure is empirically worse than the disease on macOS arm64 with
  the current rustc; a future Linux-target run may reverse this
  conclusion (different allocator, different cost shape).

- **Committed CPU flamegraphs** under `profile/` (round 147 depth-mode
  profile run): `encode.svg`, `decode.svg`, `roundtrip.svg` plus the
  matching `*.folded` folded-stack text inputs so future
  rounds can A/B their changes against a stable visual baseline
  without re-discovering the capture recipe. The driver is the
  existing `examples/profile_flac.rs` (round 137); the new artefacts
  are an `samply --unstable-presymbolicate` capture on macOS arm64
  converted to folded stacks via the committed
  `profile/samply_to_folded.py` helper, then rendered to SVG with
  `inferno-flamegraph`. Headline finding:
  `oxideav_flac::encoder::best_partition` (the per-subframe Rice
  partition-order search introduced in round 129; `src/encoder.rs:976`)
  is **~61 % of encode self-samples** (83 717 / 137 548), with
  allocator paths (`_xzm_free`, `xzm_malloc`, `__bzero`)
  contributing another ~7 %. Reusing a single residual `Vec<i32>`
  buffer across subframes / candidate plans is the obvious next
  encoder optimisation handle. Decode-side self-samples are
  dominated entirely by `<FlacDecoder>::receive_frame` with no
  measurable hot inner — the decode path is already
  hundreds-of-x realtime per the round 137 reference numbers and
  further gains would have to come from SIMD on the LPC restore
  loop rather than algorithmic changes. See `profile/README.md`
  for the full recipe + reproduction notes.

### Changed

- **`best_rice_params` now estimates the optimal Rice parameter from
  the partition's mean absolute residual instead of brute-forcing every
  `k` in `0..=k_max` (up to 31 passes).** RFC 9639 §9.2.5 defines the
  payload of a Rice-coded partition as `N * (1 + k) + (sum_u >> k)`
  where `sum_u` is the sum of zigzag-encoded residuals. Differentiating
  with respect to `k` and setting to zero gives `2^k ≈ sum_u / N`, so a
  single closed-form `k_est = floor(log2(sum_u / N))` plus a small
  `[k_est-2 .. k_est+3]` evaluation window — extended outward only when
  the chosen `k` lands at a window edge — provably yields the same
  `(best_bits, best_k)` pair the brute-force scan would have returned
  (cost in `k` is convex: linear `k` term + monotone-decreasing
  sum-of-shifts term). The zigzag values are also now computed once
  per partition into a small temporary `Vec<u32>` and reused across
  every candidate `k`, instead of being recomputed inside the inner
  scoring loop. Net effect on the encode benches (Apple M-class
  hardware, four-scenario coverage from r128) — output is
  byte-for-byte identical to the previous brute-force implementation,
  enforced by a new `best_rice_params_matches_brute_force_reference`
  regression test that compares the two on degenerate, Laplace-shaped,
  and adversarial-bimodal inputs across both partitioned-Rice methods:
  - mono S16 44.1 kHz 1 s: **1.456 s → 315.5 ms (4.62× faster, 273 KiB/s)**
  - stereo S16 44.1 kHz 1 s: **5.802 s → 1.245 s (4.66× faster, 138 KiB/s)**
  - stereo S24 48 kHz 0.5 s: **3.684 s → 622.0 ms (5.92× faster, 226 KiB/s)**
  - 6-ch S16 48 kHz 0.25 s: **2.263 s → 474.1 ms (4.77× faster, 297 KiB/s)**
  This is round 143's depth-mode profile delta against the round 128
  baseline; numbers from `cargo bench -p oxideav-flac --bench encode`.

### Added

- **Profiling driver** at `examples/profile_flac.rs` — a flat
  `Instant::now()` / `elapsed()` measure-this-thing harness for
  `samply` / `perf record` / `cargo flamegraph`. Criterion's warm-up
  + sampling layers + estimator math dominate sampling profiles and
  bury the real codec hot paths; this driver runs a fixed iteration
  count over the same four scenarios the Criterion benches use
  (mono S16 44.1 kHz 1 s, stereo S16 44.1 kHz 1 s, stereo S24 48 kHz
  0.5 s, 6-channel S16 48 kHz 0.25 s), prints per-iteration ms +
  MiB/s + realtime ratio, and (for encode) the achieved compression
  ratio. Modes: `encode` / `decode` / `roundtrip` / `all`. Confirms
  the workspace-wide pattern from the bench output: decode is
  hundreds-of-x realtime (~190 MiB/s on a modern laptop), encode is
  sub-realtime stereo (~0.04 MiB/s) — the per-subframe LPC +
  apodization + partition-order search is the obvious next
  optimisation target. PCM is synthesised in-driver from the same
  deterministic xorshift32 seed the benches use; no fixture files.
- **Criterion benchmarks** under `benches/{decode,encode,roundtrip}.rs`
  for the FLAC encoder, decoder and full encode + decode roundtrip
  paths. Each harness drives the public
  `oxideav_flac::encoder::make_encoder` / `decoder::make_decoder` trait
  objects (no internal-helper access) against deterministic xorshift-
  synthesised PCM across four scenarios — mono S16 44.1 kHz 1 s, stereo
  S16 44.1 kHz 1 s, stereo S24 48 kHz 0.5 s, and 6-channel S16 48 kHz
  0.25 s — so future encoder rounds (e.g. partition-order early-exit,
  apodization-window picker, LPC precision search heuristic) have a
  baseline to A/B against without rerunning the entire docs corpus.
  No committed fixture files; each scenario is self-contained and the
  roundtrip bench additionally asserts the recovered byte count
  matches the input so a state-machine drift would surface as a panic
  in the bench output rather than silent miscompression. Run with
  `cargo bench -p oxideav-flac --bench {decode,encode,roundtrip}`.
  Pattern mirrors the cinepak (r126) and tta (r127) bench harnesses
  so the per-codec numbers are directly comparable across the
  workspace.

### Changed

- Encoder now searches multiple **apodization windows** per LPC subframe
  instead of always tapering with a Welch window before Levinson-Durbin.
  Three windows are evaluated for each LPC order: Welch (centre-heavy
  parabola, the historical default), Hann (low-side-lobe raised cosine,
  better on signals with closely-spaced partials where Welch leakage
  smears the autocorrelation) and a shallow Tukey(1/4) (mostly-flat
  passband with light cosine edge ramps). Each window yields its own
  Levinson-Durbin solution, which is then carried through the existing
  per-order precision search and residual-coded; the smallest plan
  across all (window × order × precision) combinations wins, ties
  breaking toward Welch so the historical result is preserved whenever
  nothing strictly beats it. The window has no on-wire footprint
  whatsoever: RFC 9639 only pins down the quantised coefficients,
  precision, and shift, and explicitly notes (Acknowledgments) that
  computing the LPC coefficients from the autocorrelation is an
  encoder concern, so the decoder cannot tell which window the encoder
  used. This is therefore a pure lossless rate optimisation — the
  Welch-only plan is always one of the candidates, so the search can
  only ever tie or beat it. On a leakage-sensitive 4096-sample signal
  (two closely-spaced 4 kHz / 4.05 kHz partials plus an 11 kHz tone)
  the summed best-plan size across LPC orders 1..=12 drops from 355 227
  bits Welch-only to 352 054 bits with the window search (3 173 bits
  saved, ~0.9 % on that block). The 18 docs-corpus fixtures still
  decode bit-exactly via this crate's own decoder. Four new encoder
  unit tests cover window well-formedness (symmetric, non-negative,
  centre weight ≈ 1.0 for every window), the no-regression invariant
  against the Welch-only path across several signals and every LPC
  order, an explicit demonstration that the window search strictly
  beats Welch-only on the leakage-sensitive signal, and an end-to-end
  bit-exact stereo round-trip through the same content via
  `best_subframe` to confirm correctness through the full encoder
  pipeline.
- Encoder now searches the LPC coefficient precision per subframe instead
  of committing to the block-size heuristic value. The heuristic precision
  becomes the *ceiling* of a small search window (down to
  `ceiling - 4`, clamped to the legal `[5, 15]` floor); for each LPC order
  the encoder quantises + residual-codes at every precision in the window
  and keeps whichever encodes fewest bits. RFC 9639 §9.2.6 leaves the
  precision a free encoder parameter (it is written into `qlp_precis` and
  read back at that width), so this is a pure rate decision: a fatter
  coefficient field that buys more residual reduction than its
  `order`-extra-bits-per-step cost is kept, otherwise a narrower field
  wins. The old ceiling-only behaviour is always one of the candidates,
  so the search can only ever tie or beat it — on a three-tone stereo
  4096-sample frame it shaved 70 bits (22048 → 21978) off the two
  subframes versus the fixed-ceiling encode, and on a
  coefficient-dominated low-amplitude subframe a sub-ceiling precision
  wins outright. The Levinson-Durbin solve is computed once per order and
  reused across the precision sweep, so only the lightweight
  quantise+residual passes repeat. Output stays fully lossless and decodes
  bit-exactly via this crate's own decoder (all 18 docs-corpus fixtures
  still match per channel). Two new encoder unit tests cover the
  no-regression-against-the-fixed-heuristic invariant across several
  signals and orders, and an explicit demonstration that a narrower
  precision strictly beats the ceiling on coefficient-dominated content;
  the full-block adaptive-precision round-trip test now asserts the
  declared on-wire precision lands inside the legal search band.
- Encoder now searches LPC orders 1..=12 instead of stopping at 8.
  RFC 9639 §9.2.6 permits orders 1..=32; the gain curve flattens fast,
  so most content tops out somewhere between order 6 and 12, but the
  long tail of harmonic / vocal content with broader autocorrelation
  support genuinely benefits from a longer impulse response. The
  per-subframe search keeps the smallest plan, so adding the higher
  orders can only ever shrink (or tie) the output — orders that don't
  earn their coefficient overhead are silently discarded. On a 4096-
  sample AR(11) test signal — engineered so the autocorrelation
  doesn't decay before lag 11 — the best plan in orders 9..=12 came in
  at 50209 bits versus 50233 bits for the best in 1..=8. Output stays
  fully lossless and every existing test (including the 18 docs-corpus
  fixtures and the LPC-orders-1..=32 decoder coverage) decodes
  bit-exactly. Three new encoder unit tests cover the
  no-regression-on-low-order-friendly-content invariant, the
  AR(11)-win demonstration, and an explicit bit-exact round-trip of
  every order in 9..=12 through this crate's own subframe decoder.
- Encoder LPC coefficient precision is now chosen per subframe from the
  block size instead of a fixed 12 bits. A larger block amortises the
  per-coefficient storage cost over more residual samples, so it can
  afford higher precision; the heuristic rises with `log2(block_len)`
  and clamps to the legal `[5, 15]` window (RFC 9639 §9.2.6 forbids the
  4-bit `qlp_precis` all-ones encoding `0b1111`, capping on-wire
  precision at 15). The default 4096-sample frame now quantises at the
  15-bit ceiling, keeping the coefficients closer to their
  floating-point ideals and shrinking the LPC residual; on a
  multi-tone stereo frame this trimmed the encoded frame from 2995 to
  2972 bytes. The LPC shift clamp was also lifted from 0..=14 to the
  spec's full non-negative 5-bit range 0..=15. Output stays fully
  lossless and decodes bit-exactly via this crate's own decoder (all 18
  docs-corpus fixtures still match per channel). Three new encoder unit
  tests cover the heuristic's legal-window/monotonicity invariants, the
  residual-vs-coefficient trade-off, and a full-block adaptive-precision
  round-trip.
- Encoder residual coding now searches partition orders 0..=8 instead
  of always emitting a single order-0 partition (RFC 9639 §9.2.5). For
  each subframe the encoder scores every partition order legal for the
  block size and predictor order under both Rice methods (4-bit and
  5-bit parameters), keeps the layout with the smallest total payload,
  and lets each partition independently fall back to an escape (raw
  fixed-width) coding when that beats Rice. Splitting the residual lets
  locally varying content pick a tighter Rice parameter per region, so
  files with quiet passages next to transients shrink noticeably; output
  remains fully lossless and decodes bit-exactly via this crate's own
  decoder and the reference `flac` tool. Two new encoder unit tests
  (`partition_search_beats_order_zero_on_split_statistics`,
  `encode_decode_partitioned_residual_roundtrip`) cover the search and
  the partitioned round-trip.

### Added

- `metadata::write_seektable(real_points, placeholder_count)` serialises
  a slice of `SeekPoint`s back into a RFC 9639 §8.5.1-conformant
  SEEKTABLE block payload, optionally padding the tail with
  placeholder entries (`sample_number = 0xFFFF_FFFF_FFFF_FFFF`, now
  exposed as `metadata::SEEK_POINT_PLACEHOLDER`) so callers can
  pre-reserve space for future seek-point insertions without
  rewriting the surrounding metadata chain. The writer enforces every
  wire-format invariant the spec imposes on a written table: real
  points must be strictly ascending by sample number (which collapses
  the spec's separate "MUST be sorted in ascending order" and "MUST
  be unique by sample number" rules into a single check), no real
  point may shadow the placeholder sentinel (the parser uses it as
  the placeholder discriminator), and placeholders are only written
  at the tail. Each entry serialises to the fixed 18-byte shape
  (`sample_number: u64 BE`, `offset: u64 BE`, `frame_samples:
  u16 BE`); placeholders zero the two undefined trailing fields for
  byte-stable round-trips. Round-trips through `parse_seektable`
  losslessly (the parser drops the placeholder tail per spec, so a
  write→parse pair returns the input real-points slice unchanged).
  Six unit tests cover the simple three-point round-trip, the
  placeholder-tail byte layout, the empty-table and
  placeholders-only-table edge cases, both ordering-violation
  branches (equal sample numbers, descending sample numbers), the
  sentinel-as-real-point rejection, and a synthetic-bytes
  parse→write→parse equality path that pins the writer to the
  parser's exact wire-format understanding the same way the existing
  cuesheet round-trip test does.
- A property-style sweep test in `tests/roundtrip.rs`
  (`sine_sweep_bit_exact_and_compresses_below_verbatim`) encodes a
  1-second 1 kHz mono sine at 8 kHz (FLAC fixed sample-rate code
  `0b0100` per RFC 9639 §9.1.2) across 18 input variants — two bit
  depths (S16, S24) × three amplitudes (0.95 × full scale near
  clipping, 0.1 × full scale where the LPC coefficient-precision
  search lives, 0.01 × full scale where the wasted-bits fast path
  fires) × three DC offsets (0, +7, -13). Every variant asserts
  bit-exact PCM recovery (the FLAC losslessness contract per §1)
  through the public encoder→decoder trait path AND asserts the
  compressed packet is strictly smaller than the bare VERBATIM
  payload (`bps × n` bits) the encoder would emit if every search
  lost out — catching a future regression where a misconfigured
  encoder defaults to writing unencoded samples on a textbook
  LPC-friendly signal. Measured compression ratios on the 18 cases:
  S16 collapses to 0.066–0.068× verbatim across every amplitude
  (the LPC + Rice path is essentially saturated on a clean sine),
  S24 spans 0.045–0.398× depending on amplitude (the high-amplitude
  near-clipping cases carry the biggest residuals; the low-amplitude
  case benefits dramatically from wasted-bits detection because most
  of the lower 17 bits stay zero). One mismatch in any of the 18
  bit-exact recoveries, or one case slipping over the VERBATIM
  ceiling, is a hard failure.
- `metadata::write_cuesheet` serialises a `CueSheet` back into the
  RFC 9639 §8.7 CUESHEET block payload, completing the round-trip with
  `parse_cuesheet`. The writer is structural (it does not enforce CD-DA
  semantics like 588-sample alignment, since non-CD-DA cuesheets
  legitimately violate them) but refuses to emit cuesheets the parser
  would reject: empty track list, track number 0, zero indices on any
  non-lead-out track, non-printable bytes in the media catalog number,
  and any count that would overflow the on-wire `u8` fields. Reserved
  spans (258 bytes after the flag byte, 13 bytes per track, 3 bytes per
  index point) are written as zeros so the strict reserved-bit checks
  in the reader accept the result. Eight unit tests cover the
  round-trip, the synthetic-bytes equality path, and each rejection
  branch; one integration test (`tests/tag_parse.rs`) builds a FLAC
  file from a written CUESHEET and confirms the demuxer surfaces the
  same chapter list as the equivalent hand-built payload.

### Fixed

- `metadata::parse_cuesheet` doc-comment had a stale "12 + 1 + 12 + 1
  + 13 + 1 = 40" per-track byte budget; the code (and RFC 9639 §8.7.1)
  use `8 + 1 + 12 + 1 + 13 + 1 = 36`. Fixed in passing.
- `subframe::decode_subframe` now rejects `bps == 0` and `bps > 32` with
  `Error::Unsupported` rather than panicking inside `BitReader::read_i32`.
  The 33-bit side-channel that arises from a STREAMINFO declaring
  `bits_per_sample=32` plus a stereo-decorrelated frame can't be
  represented through the current 32-bit-bounded bit reader; we surface
  it as an unsupported-input error. Found by the `panic_free_decode`
  cargo-fuzz harness.

### Added

- `metadata::parse_cuesheet` parses the FLAC CUESHEET metadata block
  (RFC 9639 §8.7). Surfaces media catalog number, lead-in sample
  count, CD-DA flag, and the per-track structure including ISRC,
  audio / pre-emphasis flags and INDEX point lists. Strict reserved-
  bit / reserved-byte enforcement so malformed blocks can't smuggle
  garbage through (CD-DA decoders rely on those zeros).
- `container::FlacDemuxer` now exposes CUESHEET tracks through the
  standard `Demuxer::chapters()` accessor: one `Chapter` per non-
  lead-out track, with `start` / `end` taken from sample offsets and
  the chapter `id` set to the CUESHEET track number. A malformed
  CUESHEET is non-fatal — the demuxer drops it and returns an empty
  chapter list, so existing CUESHEET-less files keep working
  unchanged.
- `fuzz/` cargo-fuzz scaffold with four targets:
  - `panic_free_decode` (panic-freedom of the demuxer + decoder pipeline)
  - `decode` (sibling-shape, mirrors `oxideav-tta`/`oxideav-bmp`/
    `oxideav-qoi`: one entry that funnels arbitrary bytes through
    every pure-`Result` metadata parser — `BlockHeader::parse`,
    `StreamInfo::parse`, `parse_seektable`, `parse_cuesheet`,
    `parse_frame_header` — and the full demux+decode pipeline;
    seeded from the 18 `docs/audio/flac/fixtures/` files so
    libFuzzer starts with real-coverage input. 60-second local
    run hit ~60 k execs / 0 crashes / 0 OOMs with coverage
    reaching `CueSheetTrack` drop glue.)
  - `roundtrip` (random PCM through encoder→decoder, expect bit-exact)
  - `flac_oracle_decode` (cross-validate decoded PCM against
    libavcodec's FLAC decoder, loaded via `libloading`)
- `.github/workflows/fuzz.yml` — daily 30-minute fuzz run on the org-
  level reusable workflow with `ffmpeg` + `flac` + `libflac-dev` apt
  packages installed for the oracle.

## [0.0.9](https://github.com/OxideAV/oxideav-flac/compare/v0.0.8...v0.0.9) - 2026-05-06

### Other

- drop stale REGISTRARS / with_all_features intra-doc links
- drop dead `linkme` dep
- auto-register via oxideav_core::register! macro (linkme distributed slice)
- unify entry point on register(&mut RuntimeContext) ([#502](https://github.com/OxideAV/oxideav-flac/pull/502))

## [0.0.8](https://github.com/OxideAV/oxideav-flac/compare/v0.0.7...v0.0.8) - 2026-05-04

### Other

- detect + emit per-subframe wasted bits per sample
- graduate every fixture to Tier::BitExact
- emit U8 for 8 bps; container: accept 12 + 20 bps

### Fixed

- decoder: 8 bps STREAMINFO now decodes into `SampleFormat::U8`
  (offset-128 unsigned bytes), matching the demuxer's
  `params.sample_format` and the README contract. Previously the
  decoder unconditionally emitted S16 PCM for 1..=16 bps, which
  contradicted the U8 the container surfaced and forced consumers to
  carry a workaround mapping.

### Added

- container/decoder: 12 bps and 20 bps STREAMINFO files now open
  without error. They project onto `SampleFormat::S16` and
  `SampleFormat::S24` respectively, matching the next-wider standard
  variant per the spec's "no narrower container exists" convention.

### Changed

- tests/docs_corpus: all 18 fixtures graduated from `Tier::ReportOnly`
  to `Tier::BitExact`. Local + CI runs already showed 100.0000% match
  per channel, so any future divergence is a real decoder regression
  and now hard-fails CI.
- encoder: detect per-subframe "wasted bits per sample" (the largest
  `k` such that every sample in the subframe is divisible by `2^k`)
  and fold it into the spec's wasted-bits unary header. Subframes
  whose payload is naturally aligned to a power-of-two boundary
  (upsampled / low-amplitude / dithered-down content) now encode at
  the smaller effective bps. Output remains bit-exact through the
  decoder.

## [0.0.7](https://github.com/OxideAV/oxideav-flac/compare/v0.0.6...v0.0.7) - 2026-05-03

### Other

- use checked_div for sample-stride math
- rustfmt docs_corpus.rs
- wire docs/audio/flac/fixtures/ corpus into integration tests
- replace never-match regex with semver_check = false
- migrate to centralized OxideAV/.github reusable workflows
- adopt slim VideoFrame/AudioFrame shape
- pin release-plz to patch-only bumps

## [0.0.6](https://github.com/OxideAV/oxideav-flac/compare/v0.0.5...v0.0.6) - 2026-04-25

### Other

- drop oxideav-codec/oxideav-container shims, import from oxideav-core
- bump oxideav-container dep to "0.1"
- drop Cargo.lock — this crate is a library
- bump oxideav-core / oxideav-codec dep examples to "0.1"
- bump to oxideav-core 0.1.1 + codec 0.1.1
- migrate register() to CodecInfo builder
- bump oxideav-core + oxideav-codec deps to "0.1"
- thread &dyn CodecResolver through open()

## [0.0.5](https://github.com/OxideAV/oxideav-flac/compare/v0.0.4...v0.0.5) - 2026-04-19

### Other

- claim WAVEFORMATEX tag via oxideav-codec CodecTag registry
- bump to latest oxideav-* dep versions
- migrate to oxideav_core::bits + local utf8 extensions + 32bps fix
- LPC subframes + stereo decorrelation + STREAMINFO MD5

## [0.0.4](https://github.com/OxideAV/oxideav-flac/compare/v0.0.3...v0.0.4) - 2026-04-17

### Other

- satisfy rustfmt and clippy (clamp + allow needless_range_loop)
- rewrite README and module docs to reflect real codec scope
- fix subtract-with-overflow in read_unary for all-zero accumulator
- encoder full 32-bit-per-sample support (lift S32→24 clamp)
