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
  25..=32. Every RFC 9639 §9.1.4 Table 17 frame-header bit-depth code is
  honoured, including `0b111` (32 bits per sample) — the code a reference
  encoder writes in every frame of a genuine 32-bit file. Streams an
  outside encoder produced decode sample-exact (see the
  `reference_encode_decode` oracle).
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
- **Verify-decoder MD5** (RFC 9639 §8.2): the `verify` module
  recomputes the STREAMINFO MD5 from the decoded PCM and checks it,
  confirming the whole-stream decode is lossless — the canonical FLAC
  integrity check, stronger than the per-frame CRC-16 (which only
  proves a single frame's bytes are intact, not that the stream decoded
  to the original audio). `verify::verify_stream(&flac_bytes)` walks the
  metadata chain, decodes every frame, and returns `Match` /
  `NoSignature` (all-zero signature = "MD5 not computed") or an error on
  mismatch. `Md5Verifier` exposes the same check incrementally for
  streaming callers (feed each frame's decoded planes, then `verify`).
  The PCM is serialised in FLAC's canonical order — interchannel samples
  interleaved, each signed two's-complement little-endian, sign-extended
  to a whole byte — validated against the RFC 9639 Appendix D.1 worked
  example and every reference `input.flac` fixture (all 18 verify
  `Match`).

## Encode support

- **Bit depths**: 8 (`U8`), 16 (`S16`), 24 (`S24`), 32 (`S32`) bps. Each
  frame header carries the explicit RFC 9639 §9.1.4 Table 17 bit-depth
  code (including `0b111` for 32 bps), so every frame self-describes its
  depth without relying on STREAMINFO.
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
  five analysis windows (Welch, Hann, shallow Tukey, Bartlett,
  Blackman-Harris) before Levinson-Durbin and keeps the smallest; the
  window choice has no on-wire footprint, so the wider default set is
  subset-neutral and recovers a few percent on real multichannel /
  wideband content that the leaner three-window set left on the table.
  LPC coefficient precision is also searched per
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
  five-window analysis set (Welch, Hann, Tukey(1/4), Bartlett,
  Blackman-Harris) with a caller-chosen set — useful both to add a window
  the default omits and to trim to a leaner set (e.g. Welch-only) when
  encode speed matters more than the last few percent. An apodization
  window only tapers a block before the autocorrelation that feeds
  Levinson-Durbin — it has **no** on-wire footprint (RFC 9639 §9.2.6
  fixes only the quantised coefficients, precision, and shift that reach
  the bitstream), so the decoder never observes it and output stays a
  valid streamable-subset stream regardless of the set. The encoder
  solves under every window in the set and the minimum-bit driver keeps
  the smallest plan, so more windows can only shrink or tie the output at
  the cost of one extra Levinson-Durbin solve per window per order. The
  public `Apodization` enum offers five window shapes — `Welch`, `Hann`,
  `Tukey { alpha_num, alpha_den }`, `Bartlett` (triangular, the cheapest
  non-rectangular taper), and `BlackmanHarris` (4-term cosine-sum with
  the lowest side lobes / strongest leakage suppression in the set).
  `Apodization::Tukey` takes an `alpha = num/den` taper fraction (clamped
  to `[0, 1]`; a zero denominator is treated as `0`). An empty set or the
  default `None` uses the five-window default set.
- **Fast preset (opt-in)**: `encoder::make_encoder_with_options(params,
  FlacEncoderOptions::fast())` selects a speed-biased configuration —
  a single Welch analysis window and `max_lpc_order = 8` — analogous to a
  low `flac -N` level for callers who want a markedly faster encode over
  the last few percent of ratio. On a 1 s stereo S16/44.1 kHz buffer it
  encodes roughly **6.8× faster** than the default maximal-effort search
  (five windows × LPC 1..=12). The output bytes differ from the default
  encoder's (fewer search candidates) but stay a valid streamable-subset
  FLAC stream that decodes bit-exactly; `FlacEncoderOptions::default()` is
  unchanged and still byte-stable. Note that the default search itself was
  also made faster (#12): the per-subframe LPC analysis now windows +
  autocorrelates + runs Levinson-Durbin once **per analysis window**
  (snapshotting every order's coefficients) instead of once per
  `(window, order)` pair — a byte-identical restructure.
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
- **Encode-time verify (opt-in)**:
  `make_encoder_with_options(params, FlacEncoderOptions { verify: true,
  .. })` — or the `FlacEncoderOptions::verifying()` preset — turns on
  FLAC's `--verify` self-check. As each frame is produced the encoder
  runs its own decoder over the freshly-emitted bytes (through the very
  same `decoder::decode_frame_channels` core the streaming decoder uses,
  so it sees exactly what a real decoder would — frame-header parse and
  trailing CRC-16 included) and asserts the reconstructed per-channel
  `i32` planes equal the input block sample-for-sample. On the first
  diverging sample — or if the round-tripped frame fails to decode at
  all — `send_frame` / `flush` returns `Error::InvalidData` naming the
  offending `(frame, channel, sample)`, and the bad frame is **not**
  queued for `receive_packet`, so a caller that propagates the error
  never writes a silently-corrupt `.flac`. This is strictly stronger
  than the post-hoc whole-stream MD5 check: it catches a mis-encode at
  the exact frame that produced it, before the file is even finalised,
  and needs no STREAMINFO signature to compare against. Verify has
  **zero on-wire effect** — the emitted bytes are byte-for-byte
  identical to the default encoder's; it only decides whether they are
  checked. The trade is one extra (cheap) decode per frame. The default
  `verify: false` skips the pass and is byte-stable.

### Compression-quality guarantees

The encoder's residual/predictor/window search is a minimum-bit
rate-distortion search, and two invariants of that search are pinned by
the `compression_quality` test suite:

- **Lossless on real fixtures.** Every PCM fixture under
  `docs/audio/flac/fixtures/` (mono/stereo/7.1, 8/16/24-bit) is
  re-encoded and decoded back through this crate's own decoder, asserting
  byte-exact recovery — no external tool in the loop.
- **Monotone search.** Widening the search space — adding apodization
  windows, raising the LPC-order ceiling to 32, raising the
  partition-order ceiling to 15 — can only ever *shrink or tie* the
  encoded size, never inflate it, because the extra candidates are
  compared on actual bit cost and discarded when they don't win. The
  property battery checks this across tonal, transient, near-white-noise,
  and autoregressive signal classes.
- **Compression-ratio ceilings.** Each fixture carries a locked
  compressed/PCM ratio ceiling, so a change that quietly de-tuned the RDO
  search (e.g. dropping a default window or narrowing a search range)
  trips a size regression rather than passing silently.
- **Independent-decoder oracle.** The `reference_decode_oracle` suite
  assembles a complete `.flac` stream from the encoder's output and decodes
  it with a **black-box** reference decoder (invoked as an opaque process —
  its source is never read), asserting sample-exact recovery across every
  bit depth (U8/S16/S24/S32), channel layout (mono → 7.1), and the
  CONSTANT, wasted-bits, small-block, PADDING, fast-preset, and non-subset
  (LPC 32 / partition 15) paths. Self-round-trip proves the encoder and
  decoder agree with each other; the oracle proves the *bitstream* is
  correct against an outside implementation. The suite skips when no
  reference decoder is on `PATH`.

## Native container

- **Demuxer**: parses `fLaC` magic + metadata block chain
  (STREAMINFO, VORBIS_COMMENT, SEEKTABLE, PICTURE, CUESHEET, plus
  ID3v2 tags prepended by some taggers), emits one packet per frame
  via a CRC-verified sync-code scanner.
- **Muxer**: writes `fLaC` + metadata blocks + frame packets. Packets
  produced by the encoder pass straight through.
- **SEEKTABLE generation** (RFC 9639 §8.5): by default the muxer builds
  a SEEKTABLE from the frames it writes. As each frame is muxed it
  records that frame's `(first_sample, offset_from_first_frame,
  frame_samples)` seek anchor — read from the frame header itself, not
  the packet timestamp — and at `write_trailer` time inserts a single
  SEEKTABLE block (right after STREAMINFO) at a configurable point
  density. The seek-point offset is "from the first byte of the first
  frame header to the first byte of the target frame's header" (§8.5.1),
  exactly what the demuxer's `seek_to` consumes, so a muxer→demuxer
  round-trip seeks land on the right frame without the slow
  header-scanning fallback. The default density places roughly one point
  per second of audio, capped at 2048 points (so even a multi-hour
  stream's table stays under ~36 KiB). An existing SEEKTABLE in the
  source extradata is preserved untouched and never duplicated (§8.5
  forbids more than one SEEKTABLE per stream). `container::open_muxer_with_options(output,
  streams, FlacMuxerOptions { generate_seektable, seek_point_interval,
  max_seek_points })` exposes the density control plus
  `FlacMuxerOptions::no_seektable()`, which restores the historical
  zero-buffering pass-through that streams frames straight to the output
  and writes the metadata chain byte-for-byte. (Generating a table buffers
  the frame packets until the trailer, since the SEEKTABLE must precede
  the frames it indexes.)
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
