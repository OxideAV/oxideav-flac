# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

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
