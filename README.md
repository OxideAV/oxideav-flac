# oxideav-flac

Pure-Rust **FLAC** lossless audio codec + native container — decoder,
encoder, demuxer, muxer. Zero C dependencies, no FFI, no `*-sys` crates.

Part of the [oxideav](https://github.com/OxideAV/oxideav-workspace)
framework but usable standalone.

## Installation

```toml
[dependencies]
oxideav-core = "0.1"
oxideav-flac = "0.0"
```

## Quick use

Open a `.flac` file through the container, then decode packets to PCM:

```rust
use oxideav_core::{CodecRegistry, ContainerRegistry, Frame};

let mut codecs = CodecRegistry::new();
let mut containers = ContainerRegistry::new();
oxideav_flac::register_codecs(&mut codecs);
oxideav_flac::register_containers(&mut containers);

let input: Box<dyn oxideav_container::ReadSeek> = Box::new(
    std::io::Cursor::new(std::fs::read("song.flac")?),
);
let mut dmx = containers.open("flac", input)?;
let stream = &dmx.streams()[0];
let mut dec = codecs.make_decoder(&stream.params)?;

loop {
    match dmx.next_packet() {
        Ok(pkt) => {
            dec.send_packet(&pkt)?;
            while let Ok(Frame::Audio(af)) = dec.receive_frame() {
                // af.format is one of S16 / S24 / S32 / U8 per the
                // STREAMINFO bit depth. af.data[0] is interleaved PCM.
            }
        }
        Err(oxideav_core::Error::Eof) => break,
        Err(e) => return Err(e.into()),
    }
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

### Encoder

```rust
use oxideav_core::{CodecId, CodecParameters, Frame, SampleFormat};

let mut params = CodecParameters::audio(CodecId::new("flac"));
params.channels = Some(2);
params.sample_rate = Some(48_000);
params.sample_format = Some(SampleFormat::S16);
let mut enc = codecs.make_encoder(&params)?;
enc.send_frame(&Frame::Audio(pcm_frame))?;
enc.flush()?;
while let Ok(pkt) = enc.receive_packet() {
    muxer.write_packet(&pkt)?;
}
```

The encoder emits a STREAMINFO metadata block into
`enc.output_params().extradata`, so you can feed that straight into the
FLAC muxer (or any other FLAC-aware muxer) for a complete `.flac` file.

## Decode support

Spec-complete, covering the FLAC format (Subset and non-Subset):

- **Bit depths**: 8, 12, 16, 20, 24, 32 bits per sample. Output format
  is `U8` for 8 bps, `S16` for 9..=16, `S24` for 17..=24, `S32` for
  25..=32.
- **Sample rates**: full spec range (1 Hz up to 655_350 Hz), including
  the 11 fixed rate codes and the three variable-rate escapes.
- **Channels**: 1..=8 independent, plus the three decorrelated stereo
  layouts (left/side, right/side, mid/side).
- **Blocking strategy**: fixed and variable block size, any block size
  the spec allows.
- **Subframe types**: CONSTANT, VERBATIM, FIXED predictor (orders 0–4),
  LPC (orders 1–32) with full QLP precision / shift handling.
- **Residual coding**: both partitioned Rice methods (4-bit and 5-bit
  parameters), all 16 partition orders, and the escape partitions that
  store raw unencoded residuals.
- **Wasted bits per sample** (unary-coded count per subframe).
- **Frame CRC**: header CRC-8 and frame CRC-16 are both verified; bad
  frames are rejected.

## Encode support

- **Bit depths**: 8 (`U8`), 16 (`S16`), 24 (`S24`), 32 (`S32`) bps.
- **Sample rates**: any rate the STREAMINFO format can hold
  (up to 655_350 Hz).
- **Channels**: 1..=8 independent. For stereo inputs the encoder
  evaluates all four spec-defined channel assignments (independent
  L/R, left-side, right-side, mid-side) per frame and picks the
  smallest total subframe size.
- **Predictors**: per subframe the encoder tries CONSTANT, FIXED
  orders 0..=4, LPC orders 1..=12 and VERBATIM, and keeps the
  smallest. The default LPC ceiling is 12 — which is also the
  streamable-subset boundary for sample rates ≤ 48 kHz (RFC 9639 §7)
  and the point past which the gain curve flattens while per-frame
  cost grows quadratically. For each LPC order the encoder searches
  three analysis windows (Welch, Hann, shallow Tukey) before
  Levinson-Durbin and keeps the smallest; the window choice has no
  on-wire footprint. LPC coefficient precision is also searched per
  subframe within the legal `[5, 15]` range. Output is always fully
  valid and lossless — any compliant decoder recovers the original
  PCM bit-exactly.
- **Higher LPC orders (opt-in)**:
  `encoder::make_encoder_with_options(params, FlacEncoderOptions {
  max_lpc_order: Some(k), .. })` lifts the per-subframe LPC search
  ceiling above the default 12 up to the §9.2.6 maximum of 32
  (requests are clamped into `1..=32`). Content with a long impulse
  response — strong echoes, AR processes whose autocorrelation does
  not decay before lag 12 — can pick an order the default search never
  reaches. The `best_subframe` driver always keeps the minimum-bit
  plan, so a higher ceiling can only shrink or tie the output; the
  trade is encode cost (Levinson-Durbin is O(order²)) and, for ≤ 48
  kHz audio that actually selects an order above 12, **subset
  membership** (RFC 9639 §7). The default `None` is byte-for-byte
  identical to the historical encoder and stays subset-compliant.
- **Custom apodization windows (opt-in)**:
  `encoder::make_encoder_with_options(params, FlacEncoderOptions {
  apodization: Some(vec![Apodization::Hann, Apodization::Tukey {
  alpha_num: 1, alpha_den: 8 }, ..]), .. })` replaces the default
  three-window analysis set (Welch, Hann, Tukey(1/4)) with a
  caller-chosen set. An apodization window only tapers a block before
  the autocorrelation that feeds Levinson-Durbin — it has **no**
  on-wire footprint (RFC 9639 §9.2.6 fixes only the quantised
  coefficients, precision, and shift that reach the bitstream), so the
  decoder never observes it and output stays a valid streamable-subset
  stream regardless of the set. The encoder solves under every window
  in the set and the minimum-bit driver keeps the smallest plan, so
  more windows can only shrink or tie the output at the cost of one
  extra Levinson-Durbin solve per window per order. The public
  `Apodization` enum offers five window shapes — `Welch`, `Hann`,
  `Tukey { alpha_num, alpha_den }`, `Bartlett` (triangular, the cheapest
  non-rectangular taper), and `BlackmanHarris` (4-term cosine-sum with
  the lowest side lobes / strongest leakage suppression in the set).
  `Apodization::Tukey` takes an `alpha = num/den` taper fraction (clamped
  to `[0, 1]`; a zero denominator is treated as `0`). An empty set or the
  default `None` keeps the historical three-window (Welch / Hann /
  Tukey(1/4)) search byte-for-byte.
- **Wasted bits per sample**: detected per subframe (largest `k` such
  that every sample is divisible by `2^k`) and folded into the spec's
  wasted-bits unary header.
- **Residual coding**: partitioned Rice with a full search over
  partition orders 0..=8 under both Rice methods (4-bit and 5-bit
  parameters); the cheapest layout wins, and each partition
  independently falls back to escape (raw fixed-width) coding when
  that beats Rice. The default ceiling of 8 keeps every emitted
  stream inside the FLAC streamable subset (RFC 9639 §7).
- **Non-subset partition orders**:
  `encoder::make_encoder_with_options(params, FlacEncoderOptions {
  allow_non_subset_partition_order: true, .. })` lifts the
  residual-search ceiling to the full §9.2.7 field range (0..=15) for
  content whose residual statistics vary finer than the order-8
  partition size. The default factory keeps the subset-compliant
  ceiling of 8 and is byte-for-byte unaffected. Output stays a valid
  (non-subset) FLAC stream.
- **Block size**: 4096 samples per frame (fixed-blocking strategy) by
  default. `make_encoder_with_options(params, FlacEncoderOptions {
  block_size: Some(n), .. })` sets the per-frame block size to any `n` in
  the STREAMINFO range `16..=65535` (RFC 9639 §8.2); every frame but the
  last carries exactly `n` interchannel samples and the last carries the
  remainder (the §8.2 minimum-block floor exempts the last frame). A
  larger block amortises the per-frame header + LPC warm-up/coefficient
  overhead and lets the predictor + Rice-partition search adapt over a
  longer window; a smaller block trades that for lower latency and finer
  seek granularity. By default the size is held inside the streamable
  subset (RFC 9639 §7: ≤ 4608 for sample rates ≤ 48 kHz, ≤ 16384
  otherwise) and a request beyond the applicable ceiling is rejected;
  `allow_non_subset_block_size: true` lifts the check to the full
  STREAMINFO ceiling, producing a valid (non-subset) stream any
  spec-complete decoder still recovers bit-exactly. `None` keeps the
  historical 4096 byte-for-byte.
- **Metadata**: emits a STREAMINFO header in `extradata`. An MD5
  signature of the PCM input is computed during encode and written
  into the block at `flush()` time, along with the observed
  min/max frame size and total sample count.
- **PADDING reservation**: `make_encoder_with_options(params,
  FlacEncoderOptions { padding_bytes: Some(n), .. })` chains an
  `n`-byte all-zero PADDING block after STREAMINFO so downstream
  tools can rewrite metadata in place. Requesting more than
  `BlockHeader::MAX_LENGTH` (2²⁴ − 1) returns `Error::invalid`.

## Native container

- **Demuxer**: parses `fLaC` magic + metadata block chain
  (STREAMINFO, VORBIS_COMMENT, SEEKTABLE, PICTURE, CUESHEET, plus
  ID3v2 tags prepended by some taggers), emits one packet per frame
  via a CRC-verified sync-code scanner.
- **Muxer**: writes `fLaC` + preserved metadata blocks + frame
  packets. Packets produced by the encoder pass straight through.
- **Seeking**: SEEKTABLE-driven byte-offset seek (`seek_to(pts)`
  lands on the nearest prior seek point). Files **without** a SEEKTABLE
  seek via a frame-header scanning fallback (RFC 9639 §8.5 — "it is
  possible to seek to any given sample in a FLAC stream without a seek
  table, but the delay can be unpredictable"): the demuxer scans
  CRC-8-verified frame headers forward from the start of the frame
  stream and lands on the last frame whose first sample is ≤ the
  target, exhausting to the final frame for targets past the end and
  clamping to sample 0 for negative targets. The scan applies the
  §11 anti-DoS plausibility checks — each accepted coded number must
  be CRC-verified, strictly increasing, and within the STREAMINFO
  total-sample bound — so a stream with non-monotonic or out-of-range
  coded numbers can never loop the scanner.
- **Metadata surfaces**: Vorbis-comment key/value pairs and attached
  pictures (FLAC PICTURE block + ID3v2 APIC fallback).
- **Chapters**: CUESHEET tracks are surfaced through
  `Demuxer::chapters()`. Each non-lead-out track becomes one
  `Chapter` carrying its sample-offset range and the track number
  as `id`. The full parsed CUESHEET (media catalog number, lead-in
  sample count, per-track ISRC, INDEX point list) is also retained
  on the demuxer for CD-DA fidelity. A malformed CUESHEET is
  non-fatal: it is dropped and an empty chapter list returned.

### Typed metadata block API

Every FLAC metadata block has a typed parser + writer pair under
`metadata::`, each enforcing the RFC 9639 wire-format invariants and
round-tripping byte-for-byte:

- **STREAMINFO** — `StreamInfo::parse` / `StreamInfo::write`.
- **PADDING** — `write_padding(len)`.
- **APPLICATION** — `parse_application` / `write_application`.
- **SEEKTABLE** — `parse_seektable` / `write_seektable(points,
  placeholder_count)` (rejects non-ascending / non-unique points,
  supports the placeholder sentinel `SEEK_POINT_PLACEHOLDER`).
- **VORBIS_COMMENT** — `parse_vorbis_comment` / `write_vorbis_comment`
  (preserves field order/casing/whitespace; little-endian length
  fields; case-insensitive `VorbisComment::get`).
- **CUESHEET** — `parse_cuesheet` / `write_cuesheet` with typed
  sub-field accessors (`CueSheet::media_catalog`,
  `CueSheetTrack::isrc_str`, lead-out sentinels + helpers).
- **PICTURE** — `parse_picture` / `write_picture` (full §8.8 field
  set: type, mime, description, width/height/depth/colour_count,
  data).
- **Whole chain** — `MetadataChain::parse` / `::write` round-trips the
  entire block chain through a typed `MetadataBlock` enum (including
  an opaque `Reserved` variant for forward-compat block codes),
  enforcing the chain-level structural MUSTs (STREAMINFO first,
  singleton STREAMINFO/SEEKTABLE/VORBIS_COMMENT, `last`-flag
  placement, forbidden block-type 127). `MetadataChain::validate`
  checks a hand-built chain without serialising.

Helpers `BlockType::to_byte`, `BlockHeader::write_into` /
`to_bytes` serialise the 4-byte metadata-block prefix per RFC 9639
§8.1, and the streaming `crc::{Crc8, Crc16}` / one-shot `crc8` /
`crc16` validators are exposed for callers building blocks by hand.

## Fuzzing

`fuzz/` carries thirteen `cargo-fuzz` targets sharing one
panic-freedom contract — every public surface must return a `Result`
on malformed input, never panic, abort, or OOM: `panic_free_decode`,
`decode`, `roundtrip`, `flac_oracle_decode` (cross-decode against an
optional system black-box validator loaded via `libloading`, skipped
silently when absent), `metadata_walker`, `encoder_options`,
`md5_streaming`, `utf8_varint`, `frame_header`, `metadata_chain`,
`subframe_decode`, `application`, and `seek` (demuxer `seek_to` +
post-seek packet drain against unsorted / placeholder / out-of-range
SEEKTABLE points the §8.5.1-enforcing writer never emits). Daily split
run in `.github/workflows/fuzz.yml`.

## Benchmarks

`benches/` carries six `criterion` harnesses (`encode`, `decode`,
`roundtrip`, `crc`, `seek`, `md5`) driving the public
`encoder::make_encoder` / `decoder::make_decoder` trait objects
against deterministic xorshift-synthesised PCM. No committed fixture
files; PCM is synthesised in-bench. Run with
`cargo bench -p oxideav-flac --bench {encode,decode,roundtrip,crc,seek,md5}`.

## Profiling

`examples/profile_flac.rs` is a flat measure-this-thing driver suited
for `samply`, `perf record`, and `cargo flamegraph` — one
`Instant::now()` / `elapsed()` pair around a fixed-iteration loop so
the profile is dominated by codec cost itself.

```text
cargo run --example profile_flac --release -- <mode> [<iters>]
# modes: encode | decode | roundtrip | all (default)
```

`profile/` carries committed SVG flamegraphs (`encode.svg`,
`decode.svg`, `roundtrip.svg`) plus folded-stack inputs — see
`profile/README.md` for the recipe. The decode path is realtime by
orders of magnitude; the encode path is mono-realtime and within a
small factor of realtime for stereo / multichannel, with the
remaining cost in the per-subframe LPC + apodization window search
and the Rice partition-order search. See `CHANGELOG.md` for the
encoder optimisation history.

## Codec / container IDs

- Codec: `"flac"`; accepted sample formats `U8`, `S16`, `S24`, `S32`.
- Container: `"flac"`, matches `.flac` / `.fla` by extension and the
  `fLaC` magic bytes (including files prefixed with an ID3v2 tag).

## License

MIT — see [LICENSE](LICENSE).
