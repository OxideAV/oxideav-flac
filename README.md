# oxideav-flac

[![CI](https://github.com/OxideAV/oxideav-flac/actions/workflows/ci.yml/badge.svg)](https://github.com/OxideAV/oxideav-flac/actions/workflows/ci.yml) [![crates.io](https://img.shields.io/crates/v/oxideav-flac.svg)](https://crates.io/crates/oxideav-flac) [![docs.rs](https://docs.rs/oxideav-flac/badge.svg)](https://docs.rs/oxideav-flac) [![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

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
use oxideav_core::{Frame, RuntimeContext};

let mut ctx = RuntimeContext::new();
oxideav_flac::register(&mut ctx); // codec "flac" + the native FLAC container

let input: Box<dyn oxideav_core::ReadSeek> = Box::new(
    std::io::Cursor::new(std::fs::read("song.flac")?),
);
let mut dmx = ctx.containers.open_demuxer("flac", input, &ctx.codecs)?;
let params = dmx.streams()[0].params.clone();
let mut dec = ctx.codecs.first_decoder(&params)?;

loop {
    match dmx.next_packet() {
        Ok(pkt) => {
            dec.send_packet(&pkt)?;
            while let Ok(Frame::Audio(af)) = dec.receive_frame() {
                // params.sample_format is one of S16 / S24 / S32 / U8 per
                // the STREAMINFO bit depth. af.data[0] is interleaved PCM.
                let _ = af;
            }
        }
        Err(oxideav_core::Error::Eof) => break,
        Err(e) => return Err(e.into()),
    }
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

`first_decoder` picks the first registered implementation; applications
that register several implementations of a codec and want
priority/preference-aware selection use
`oxideav_pipeline::make_decoder_with` instead.

### Encoder

```rust
use oxideav_core::{AudioFrame, CodecId, CodecParameters, Frame, RuntimeContext, SampleFormat};

let mut ctx = RuntimeContext::new();
oxideav_flac::register(&mut ctx);

let mut params = CodecParameters::audio(CodecId::new("flac"));
params.channels = Some(2);
params.sample_rate = Some(48_000);
params.sample_format = Some(SampleFormat::S16);
let mut enc = ctx.codecs.first_encoder(&params)?;

// 1024 stereo S16 samples of silence, interleaved.
let pcm_frame = AudioFrame { samples: 1024, pts: Some(0), data: vec![vec![0u8; 1024 * 2 * 2]] };
enc.send_frame(&Frame::Audio(pcm_frame))?;
enc.flush()?;
while let Ok(pkt) = enc.receive_packet() {
    // hand `pkt` to a muxer (e.g. the FLAC muxer via ctx.containers.open_muxer)
    let _ = pkt;
}
# Ok::<(), Box<dyn std::error::Error>>(())
```

The encoder emits a STREAMINFO metadata block into
`enc.output_params().extradata`, so you can feed that straight into the
FLAC muxer (or any other FLAC-aware muxer) for a complete `.flac` file.
STREAMINFO's total-sample count and MD5 are final only after `flush()`,
so open the muxer after flushing (or rewrite the header) if you want
`flac -t` to verify the MD5.

A complete WAV → FLAC converter showing the intended API (chunked
`send_frame`, `flush`, draining packets, muxing with the final
STREAMINFO, choosing a level) is in
[`examples/encode_wav.rs`](examples/encode_wav.rs):

```sh
cargo run --release --example encode_wav -- input.wav output.flac --level 5
```

## Choosing a compression level

`FlacEncoderOptions::level(n)` picks one of nine presets, `0..=8`;
`make_encoder` and `FlacEncoderOptions::default()` use level **5**.
Every level produces a valid streamable-subset FLAC stream that decodes
bit-exactly in any decoder; the levels differ only in how hard the
encoder searches.

```rust
use oxideav_core::{CodecId, CodecParameters, SampleFormat};
use oxideav_flac::encoder::{make_encoder_with_options, FlacEncoderOptions};

let mut params = CodecParameters::audio(CodecId::new("flac"));
params.channels = Some(2);
params.sample_rate = Some(44_100);
params.sample_format = Some(SampleFormat::S16);
let _enc = make_encoder_with_options(&params, FlacEncoderOptions::level(8))?;
# Ok::<(), oxideav_core::Error>(())
```

Measured on a 3-minute 44.1 kHz 16-bit stereo test signal (tone sweeps,
tremolo, echoes and gated noise bursts; `examples/encode_wav.rs`,
`--release`, Apple Silicon, single thread, CPU time including WAV
parsing, min of 3 runs), next to the `flac` 1.5 command-line encoder and
ffmpeg's FLAC encoder run as black boxes on the same file:

| encoder | CPU time | file size | vs. `flac -5` |
|---|---:|---:|---:|
| level 0 | 0.17 s | 19.89 MB | +24.2 % |
| level 2 | 0.16 s | 19.88 MB | +24.1 % |
| level 3 | 0.29 s | 16.44 MB | +2.6 % |
| level 4 (`fast()`) | 0.32 s | 16.02 MB | +0.03 % |
| **level 5 (default)** | **0.41 s** | **15.57 MB** | **−2.8 %** |
| level 6 | 0.59 s | 15.57 MB | −2.8 % |
| level 7 | 0.75 s | 15.56 MB | −2.8 % |
| level 8 | 1.75 s | 15.54 MB | −3.0 % |
| `flac -0` | 0.13 s | 20.42 MB | +27.4 % |
| `flac -5` | 0.21 s | 16.02 MB | — |
| `flac -8` | 0.47 s | 15.50 MB | −3.3 % |
| `ffmpeg -compression_level 5` | 0.33 s | 16.26 MB | +1.5 % |
| `ffmpeg -compression_level 8` | 0.49 s | 15.85 MB | −1.1 % |

On a steady two-tone signal level 5 came out 9.6 % smaller than
`flac -5`, and on pink/brown-noise-heavy content 0.15 % smaller, at
1.7–1.9× its CPU time.

- **Level 5** is the right choice almost everywhere.
- **Levels 0–2** use only the fixed predictors. Pick them when CPU
  matters more than about 25 % of file size, such as live capture on
  small devices.
- **Levels 3–4** add a single, shorter linear predictor. They are about
  25 % faster than level 5.
- **Levels 6–8** search more analysis windows, coefficient precisions
  and (at 8) all four stereo layouts in full. They gain a few tenths of
  a percent on real program material, and more on synthetic pure tones.
- **`FlacEncoderOptions::exhaustive()`** is the encoder's historical
  exact-cost search. It writes out every candidate and keeps the
  smallest: typically 0.2–1 % below level 8, at roughly 100× its time.
  It is meant for archival one-offs.

Individual axes can be overridden on top of any level:
`with_max_lpc_order`, `with_apodization`,
`with_lpc_precision_search_span`, `with_max_partition_order`,
`with_block_size`.

If encoding seems slow, check that you are building with `--release`.
A debug build of the encoder is about 20× slower.

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
  layouts (left/side, right/side, mid/side). At the 32-bit depth ceiling
  the decorrelated layouts carry a **33-bit side channel** (RFC 9639 §4.2
  / Appendix A.2 — the side signal `L - R` needs one extra bit of depth);
  that one channel is decoded through a wider `i64` path and the recovered
  left/right pair narrows back to 32-bit losslessly. The ≤ 32-bit path is
  unchanged.
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
  smallest total subframe size. At the 32-bit depth ceiling it emits
  independent L/R only (the decorrelated layouts would need a 33-bit
  side subframe, which the subframe encoder does not yet write — the
  *decoder* reads such frames from other encoders; see Decode support).
- **Predictors**: CONSTANT, VERBATIM, FIXED orders 0..=4 and LPC orders
  up to 12 (the streamable-subset ceiling for ≤ 48 kHz, RFC 9639 §7).
  At every compression level the encoder ranks candidates by estimate
  and encodes only the winners:
  - **FIXED**: one pass measures the residual magnitude of all five
    fixed predictors, and only the cheapest is coded.
  - **LPC**: per analysis window, one autocorrelation and one
    Levinson-Durbin recursion yield every order's coefficients and
    prediction error. That error, plus a coefficient-quantisation noise
    term, ranks the orders, and the best one or two per window are
    quantised and coded.

  `FlacEncoderOptions::exhaustive()` instead writes out every candidate
  and keeps the smallest: five windows × every order × five
  coefficient precisions. Output is always fully valid and lossless;
  any compliant decoder recovers the original PCM bit-exactly.
- **Stereo decorrelation**: for stereo input the encoder chooses among
  independent L/R, left-side, side-right and mid-side (RFC 9639 §4.2)
  from per-channel size estimates. It then searches only the two
  channels the chosen layout codes; level 8 and the exhaustive search
  code all four and compare. At the 32-bit depth ceiling the encoder
  emits independent L/R only, since the decorrelated layouts would need
  a 33-bit side subframe that the subframe encoder does not yet write.
  The *decoder* reads such frames from other encoders; see Decode
  support.
- **Higher LPC orders (opt-in)**: `FlacEncoderOptions::default()
  .with_max_lpc_order(k)` lifts the LPC ceiling above 12, up to the
  §9.2.6 maximum of 32 (requests clamp into `1..=32`). That helps content
  with a long impulse response. For ≤ 48 kHz audio that actually selects
  an order above 12, the stream is no longer **streamable-subset**
  (RFC 9639 §7).
- **Custom apodization windows (opt-in)**: `.with_apodization(vec![
  Apodization::Hann, Apodization::Tukey { alpha_num: 1, alpha_den: 8 },
  ..])` replaces the level's analysis windows. The public `Apodization`
  enum offers `Welch`, `Hann`, `Tukey { alpha_num, alpha_den }`,
  `Bartlett` and `BlackmanHarris`. A window only tapers the block
  before the autocorrelation; it has **no** on-wire footprint (RFC 9639
  §9.2.6 fixes only the quantised coefficients, precision and shift), so
  output stays a valid streamable-subset stream regardless of the set.
  Each extra window costs one more analysis plus its candidates' residual
  passes. `Apodization::Tukey` takes an `alpha = num/den` taper fraction,
  clamped to `[0, 1]`; a zero denominator is treated as `0`.
- **Fast preset**: `FlacEncoderOptions::fast()` is level 4.
- **Wasted bits per sample**: detected per subframe (largest `k` such
  that every sample is divisible by `2^k`) and folded into the spec's
  wasted-bits unary header.
- **Residual coding**: partitioned Rice (RFC 9639 §9.2.7) under both Rice
  methods (4-bit and 5-bit parameters).
  - Every legal partition order up to the level's ceiling is scored from
    per-partition sums: 8 (the subset bound) from level 3 up, and 3/4/6
    at levels 0/1/2.
  - The winning layout's Rice parameters are then chosen exactly, and
    each partition independently falls back to escape (raw fixed-width)
    coding when that is cheaper.
  - Residuals are kept strictly inside the §9.2.7.3 range (|r| < 2³¹,
    so −2³¹ itself is excluded); a predictor that would leave it is
    dropped, falling back to VERBATIM.
  - `.with_max_partition_order(p)` narrows the ceiling.
- **Non-subset partition orders**:
  `.with_allow_non_subset_partition_order(true)` lifts the residual
  search ceiling to the full field range (0..=15). Output stays a valid
  (non-subset) FLAC stream.
- **Block size**: 4096 samples per frame (fixed-blocking strategy) by
  default. `.with_block_size(n)` sets the per-frame block size to any `n` in
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
- **PADDING reservation**: `.with_padding_bytes(n)` chains an
  `n`-byte all-zero PADDING block after STREAMINFO so downstream
  tools can rewrite metadata in place. Requesting more than
  `BlockHeader::MAX_LENGTH` (2²⁴ − 1) returns `Error::invalid`.
- **Encode-time verify (opt-in)**: `.with_verify(true)`, or the
  `FlacEncoderOptions::verifying()` preset, turns on
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
- **Monotone search (exhaustive mode).** Under
  `FlacEncoderOptions::exhaustive()`, widening the search space can only
  ever *shrink or tie* the encoded size, never inflate it. Widening means
  adding apodization windows, raising the LPC-order ceiling to 32, or
  raising the partition-order ceiling to 15. The extra candidates are
  compared on actual bit cost and discarded when they don't win. The
  property battery checks this across tonal, transient, near-white-noise
  and autoregressive signal classes. The estimate-driven levels rank
  candidates by estimate, so there the guarantee is validity and
  losslessness, not monotonicity.
- **Compression-ratio ceilings.** Each fixture carries a locked
  compressed/PCM ratio ceiling, once for the exhaustive search and once
  for the default level. A change that quietly de-tuned either search
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

## Codec identification

Registration claims the WAVEFORMATEX tag `0xF1AC` (FLAC-in-WAV/AVI)
plus two payload magic prefixes for tag-less carriage, resolved via
`CodecRegistry::resolve_payload_magic_ref` / the `CodecResolver`
trait:

- `\x7fFLAC` — first packet of a FLAC-in-Ogg logical bitstream
  (RFC 9639 §10.1 Table 24); Ogg demuxers identify the mapped codec
  by this prefix alone.
- `fLaC` — the raw-stream signature heading every container-less
  FLAC stream.

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
optional system black-box validator loaded via `dlsym`, skipped
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
