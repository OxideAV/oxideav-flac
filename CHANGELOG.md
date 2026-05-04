# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

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
