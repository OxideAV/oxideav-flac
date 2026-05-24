# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
- `fuzz/` cargo-fuzz scaffold with three targets:
  - `panic_free_decode` (panic-freedom of the demuxer + decoder pipeline)
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
