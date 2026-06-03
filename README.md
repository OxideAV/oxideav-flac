# oxideav-flac

Pure-Rust **FLAC** lossless audio codec + native container — decoder,
encoder, demuxer, muxer. Zero C dependencies, no FFI, no `*-sys` crates.

Part of the [oxideav](https://github.com/OxideAV/oxideav-workspace)
framework but usable standalone.

## Installation

```toml
[dependencies]
oxideav-core = "0.1"
oxideav-codec = "0.1"
oxideav-container = "0.1"
oxideav-flac = "0.0"
```

## Quick use

Open a `.flac` file through the container, then decode packets to PCM:

```rust
use oxideav_codec::CodecRegistry;
use oxideav_container::ContainerRegistry;
use oxideav_core::Frame;

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

Fully implemented, covering the FLAC format specification (Subset and
non-Subset):

- **Bit depths**: 8, 12, 16, 20, 24, 32 bits per sample. Output format
  is `U8` for 8 bps, `S16` for 9..=16 (covers 12 bps), `S24` for
  17..=24 (covers 20 bps), `S32` for 25..=32.
- **Sample rates**: full spec range (1 Hz up to 655_350 Hz), including
  the 11 fixed rate codes and the three variable-rate escapes
  (`code 12`: multiples of 1000 Hz; `code 13`: 16-bit Hz; `code 14`:
  multiples of 10 Hz).
- **Channels**: 1..=8 independent, plus the three decorrelated stereo
  layouts (left/side, right/side, mid/side).
- **Blocking strategy**: fixed and variable block size, any block size
  the spec allows (via the 8-bit and 16-bit "extended" block-size codes).
- **Subframe types**: CONSTANT, VERBATIM, FIXED predictor (orders 0–4),
  LPC (orders 1–32) with full QLP precision / shift handling.
- **Residual coding**: both partitioned Rice methods (method 0 with 4-bit
  parameters, method 1 with 5-bit parameters), all 16 partition orders,
  and the "escape" partitions that store raw unencoded residuals.
- **Wasted bits per sample** (unary-coded count applied to each subframe).
- **Frame CRC**: header CRC-8 and frame CRC-16 are both verified; bad
  frames are rejected.

## Encode support

- **Bit depths**: 8 (`U8`), 16 (`S16`), 24 (`S24`), 32 (`S32`) bps.
- **Sample rates**: any rate the STREAMINFO format can hold
  (up to 655_350 Hz).
- **Channels**: 1..=8 independent. For stereo (2-channel) inputs the
  encoder evaluates all four spec-defined channel assignments
  (independent L/R, left-side, right-side, mid-side) per frame and
  picks the one that produces the smallest total subframe size.
- **Predictors**: per subframe the encoder tries CONSTANT, FIXED
  orders 0..=4, LPC orders 1..=12 and VERBATIM, and keeps the smallest.
  RFC 9639 §9.2.6 permits orders up to 32; the gain curve flattens past
  order ~12 while per-frame encode cost grows quadratically, so we cap
  at 12 as the conventional moderate-quality ceiling. Orders 9..=12 are
  silently discarded when shorter taps already win, so the lift only
  helps content with broader autocorrelation support. For each LPC
  order the encoder additionally **searches three analysis windows**
  before Levinson-Durbin: Welch (centre-heavy parabola, the historical
  default; strong on steady tonal content), Hann (low-side-lobe raised
  cosine; better on signals with closely-spaced partials where Welch
  leakage smears the autocorrelation) and a shallow Tukey (1/4)
  (mostly-flat passband with light cosine edge ramps). The window has
  **no on-wire footprint** — RFC 9639 only ties down the quantised
  coefficients, precision, and shift, leaving the autocorrelation-to-
  coefficient step (Acknowledgments) to the encoder — so trying several
  windows and keeping the smallest result is a pure rate decision. LPC
  coefficient precision is also searched per subframe: a block-size
  heuristic sets the search ceiling (rising with `log2(block_len)`,
  clamped to the legal `[5, 15]` window; RFC 9639 §9.2.6 forbids the
  16-bit `0b1111` encoding), and each (window, order) pair is quantised
  and residual-coded at every precision in a small window beneath the
  ceiling, keeping whichever encodes fewest bits. Higher precision
  keeps coefficients closer to their floating-point ideals and shrinks
  the residual, but costs `precision` extra bits per coefficient; the
  flip point depends on the signal, so the only reliable choice is to
  encode the candidates and compare. The previous Welch-only behaviour
  is always one of the candidates, so the search can only tie or beat
  it. The output remains fully valid and fully lossless — any compliant
  decoder (including this crate's own) will recover the original PCM
  bit-exactly.
- **Wasted bits per sample**: detected per subframe (largest `k`
  such that every sample is divisible by `2^k`) and folded into the
  spec's wasted-bits unary header. This shaves the trailing-zero
  payload off upsampled or low-amplitude content (e.g. a 16-bit
  stream that only ever uses its top 8 bits).
- **Residual coding**: partitioned Rice with a full search over
  partition orders 0..=8 (every order legal for the block size and
  predictor order is scored, RFC 9639 §9.2.5) under both Rice methods
  (4-bit and 5-bit parameters); the cheapest layout wins, and each
  partition independently falls back to an escape (raw fixed-width)
  coding when that beats Rice. Splitting the residual lets locally
  varying content (quiet passages next to transients) pick a tighter
  Rice parameter per region instead of one global compromise.
- **Block size**: 4096 samples per frame (fixed-blocking strategy).
- **Metadata**: emits a STREAMINFO metadata header in `extradata`. An
  MD5 signature of the PCM input is computed during encode and written
  into the block at `flush()` time, along with the observed min/max
  frame size and the total sample count. Fetch the final
  `enc.output_params().extradata` after flushing to feed a muxer.
- **PADDING reservation**: `encoder::make_encoder_with_options(params,
  FlacEncoderOptions { padding_bytes: Some(n), .. })` chains a
  `n`-byte all-zero PADDING block (RFC 9639 §8.6, last-flagged) after
  STREAMINFO in the produced `extradata`. The default factory
  (`make_encoder`) keeps the historical "STREAMINFO is the only block"
  output. Reserved PADDING lets downstream tools rewrite metadata
  (tag edits, cuesheet edits) in place without having to shift the
  audio frame bytes. The bound check happens at construction time —
  requesting more than `BlockHeader::MAX_LENGTH` (2²⁴ − 1) returns
  `Error::invalid` before any encode work runs.

## Native container

- **Demuxer**: parses `fLaC` magic + metadata block chain
  (STREAMINFO, VORBIS_COMMENT, SEEKTABLE, PICTURE, CUESHEET, plus
  ID3v2 tags prepended by some taggers), emits one packet per frame
  via a CRC-verified sync-code scanner.
- **Muxer**: writes `fLaC` + preserved metadata blocks + frame packets.
  Packets produced by the encoder pass straight through.
- **Seeking**: SEEKTABLE-driven byte-offset seek (`seek_to(pts)` lands
  on the nearest prior seek point). Files without a SEEKTABLE return
  `Error::Unsupported` on seek.
- **Metadata surfaces**: Vorbis-comment key/value pairs and attached
  pictures (FLAC PICTURE block + ID3v2 APIC fallback).
- **Chapters**: CUESHEET tracks are surfaced through the standard
  `Demuxer::chapters()` API. Each non-lead-out CUESHEET track becomes
  one `Chapter` whose `start` / `end` are the track's sample-offset
  range (taken from the next track's offset for the closing bound).
  Chapter `id` carries the CUESHEET track number (1..=99 for CD-DA
  audio). The full parsed CUESHEET — including the 128-byte media
  catalog number, lead-in sample count, per-track ISRC and INDEX
  point list — is also retained on the demuxer for callers needing
  CD-DA fidelity. A malformed CUESHEET is non-fatal: the demuxer
  drops it and returns an empty chapter list instead of erroring out.
- **CUESHEET writer**: `metadata::write_cuesheet` serialises a
  `CueSheet` back into a RFC 9639 §8.7-conformant block payload.
  Round-trips through `metadata::parse_cuesheet` bit-for-bit; reserved
  spans are zero-padded so the strict reader accepts the output.
  Rejects structurally illegal cuesheets (empty track list, track
  number 0, zero indices on a non-lead-out track, non-printable bytes
  in the media catalog) before emitting any bytes.
- **SEEKTABLE writer**: `metadata::write_seektable(real_points,
  placeholder_count)` serialises a slice of `SeekPoint`s back into a
  RFC 9639 §8.5.1-conformant block payload, optionally padding the
  tail with placeholder entries (sentinel sample number
  `0xFFFF_FFFF_FFFF_FFFF`, exposed as `SEEK_POINT_PLACEHOLDER`) so a
  caller can pre-reserve space for future seek-point insertions
  without rewriting the surrounding metadata chain. Rejects real
  seek points that are not strictly ascending by sample number
  (covers both the spec's "MUST be sorted in ascending order" and
  "MUST be unique by sample number" rules) and refuses to emit a
  real point that shadows the placeholder sentinel — both would
  silently corrupt the table on the next parse pass. Round-trips
  through `parse_seektable` byte-for-byte.
- **PICTURE typed accessor + writer**: `metadata::parse_picture` and
  `metadata::write_picture` round-trip a RFC 9639 §8.8 PICTURE block
  payload through a typed `Picture` struct that carries every field
  the spec defines — `picture_type`, `mime_type`, `description`, the
  four informational fields (`width`, `height`, `depth`,
  `colour_count`), and the raw `data` bytes. The container demuxer
  internally projects onto the framework's narrower `AttachedPicture`
  (which has no pixel-metadata slots) via the typed parser; callers
  who need width / height / depth / colour_count read the block
  payload from the demuxer's preserved metadata-chain extradata and
  feed it to `parse_picture` directly. The pre-round-163 inline
  parser silently dropped the 16 bytes of pixel metadata and used
  `from_utf8(...).unwrap_or("")` on the mime / description fields so
  a payload with non-UTF-8 bytes round-tripped through an empty
  string; the typed parser enforces both the §8.8 printable-ASCII
  rule on mime and strict UTF-8 on description. The writer accepts
  the spec-mandated zero-fields shape for vector images, preserves
  the full 32-bit `picture_type` (so a producer that smuggled a
  non-APIC code through gets it back on the next read) and rejects
  the `-->` URI sentinel only when its bytes happen to be
  non-printable (which they aren't).
- **PADDING writer + composable block-header serialiser**:
  `metadata::write_padding(len)` returns a `len`-byte all-zero
  payload per RFC 9639 §8.6, capped at the 24-bit block-header
  length ceiling. The companion `BlockType::to_byte()` +
  `BlockHeader::write_into` / `BlockHeader::to_bytes` helpers
  serialise the 4-byte metadata-block prefix (1-bit `last` flag +
  7-bit type code + 24-bit big-endian length per RFC 9639 §8.1), so
  callers can wrap a CUESHEET / SEEKTABLE / PADDING payload into a
  full on-wire block in one chained `Vec` append. The header
  serialiser refuses to emit the spec's `Invalid` sentinel (code
  127) and rejects payload lengths above `BlockHeader::MAX_LENGTH`
  (`2^24 - 1`).

### CRC validators

- **`crc`** *(r218)* — eight scenarios measuring the streaming `Crc8`
  / `Crc16` validators (`oxideav_flac::crc`, added r212) and the
  one-shot `crc8` / `crc16` slice helpers against deterministic
  xorshift-synthesised byte buffers sized to typical FLAC frame
  footprints. Three CRC-8 scenarios on an 8 KiB buffer (one-shot
  slice, chunked streaming via `update(&[u8])` with off-aligned
  splits, byte-at-a-time via `update_byte`) and five CRC-16 scenarios
  (the same three at 8 KiB plus one-shot + chunked at 64 KiB for a
  worst-case multichannel high-bit-depth frame). Every iteration
  asserts the produced value matches the one-shot helper so a
  regression that broke the streaming path silently shows up as a
  benchmark panic instead of a quiet 1.1x speedup that was actually
  walking different bytes. Baseline numbers (release, Darwin aarch64,
  `--quick --warm-up-time 1 --measurement-time 1`): CRC-8 ~775 MiB/s
  one-shot, ~735 MiB/s chunked, ~725 MiB/s byte-at-a-time; CRC-16
  ~500 MiB/s across all three 8 KiB shapes; the 64 KiB CRC-16 scenarios
  hold the same ~500 MiB/s, confirming the inner loop is
  table-lookup-bound and bandwidth-flat past the L1 working set.
  A future SIMD `pclmulqdq`-driven CRC-16, a slice-by-16 table, or an
  inline-attribute removal would A/B against these numbers.

## Fuzzing

`fuzz/` carries six `cargo-fuzz` targets that all share the same
panic-freedom contract — every public surface must return a `Result`
on malformed input, never panic, abort, or OOM:

- **`panic_free_decode`** — feeds arbitrary bytes through the
  container demuxer and, separately, through the raw decoder with a
  synthetic STREAMINFO header (so the decoder is configured but the
  packet bytes are attacker-controlled).
- **`decode`** — sibling-shape (mirrors `oxideav-tta` /
  `oxideav-bmp` / `oxideav-qoi`): one entry that funnels each input
  through every pure-`Result` metadata parser
  (`metadata::BlockHeader::parse`, `metadata::StreamInfo::parse`,
  `metadata::parse_seektable`, `metadata::parse_cuesheet`,
  `frame::parse_frame_header`) and the full demux+decode pipeline.
  Cheap per iteration; seeded from the 18 `docs/audio/flac/fixtures/`
  files so libFuzzer starts with real-coverage input.
- **`roundtrip`** — random PCM → encoder → decoder, asserting
  bit-exact recovery (FLAC is lossless).
- **`flac_oracle_decode`** — uses the encoder to synthesise a valid
  FLAC stream, then cross-decodes through both `oxideav-flac` and
  `libavcodec` (loaded via `libloading`); requires no `*-sys`
  dependency and skips silently when the system library is absent.
- **`metadata_walker`** — walks a `fLaC`-magic-led METADATA_BLOCK
  chain through `BlockHeader::parse` → typed parser
  (StreamInfo / SeekTable / CueSheet / Picture / PADDING-length) →
  matching writer → re-parse, asserting parser ∘ writer ∘ parser
  == parser at every step. Covers every metadata writer
  (`write_seektable`, `write_cuesheet`, `write_padding`,
  `write_picture`) plus `BlockHeader::write_into` in writer
  direction.
- **`encoder_options`** — `make_encoder_with_options` over a wide
  bps / channels / sample-rate / `padding_bytes` matrix.
  Verifies the produced `extradata` parses as a STREAMINFO+(optional
  PADDING) metadata chain with the expected shape (STREAMINFO's
  `last` flag reflects PADDING presence, PADDING block length
  matches the requested `padding_bytes`, PADDING payload is all
  zero) and that the encoded packets self-decode bit-exact under
  the **post-flush** extradata (the rebuild path that embeds real
  min/max/total/MD5). Over-MAX padding values must be rejected at
  construction with `Err(Error::InvalidData(_))`.

Daily 30-minute split run in `.github/workflows/fuzz.yml`. The
historical wins from the harnesses include the
`subframe::decode_subframe` bps-bound fix (`bps == 0 / bps > 32` now
returns `Error::Unsupported` instead of panicking inside
`BitReader::read_i32`).

## Benchmarks

`benches/` carries five `criterion` harnesses that drive the public
`oxideav_flac::encoder::make_encoder` / `decoder::make_decoder` trait
objects against deterministic xorshift-synthesised PCM. They exist so
future encoder rounds (partition-order early-exit, apodization-window
picker, LPC-precision search heuristic) have a stable baseline to
A/B against without rerunning the full docs corpus:

- **`encode`** — seven scenarios. The original four (round 128) cover
  the heaviest encoder path against tone-plus-noise content: mono S16
  44.1 kHz 1 s, stereo S16 44.1 kHz 1 s, stereo S24 48 kHz 0.5 s,
  6-channel S16 48 kHz 0.25 s; each exercises CONSTANT + FIXED 0..=4 +
  LPC 1..=12 + VERBATIM with per-subframe LPC-precision and apodization-
  window search plus 0..=8 partition-order Rice search. Round 150 added
  three further scenarios that target code paths the original four
  bypass: `mono/s24/48k/500ms` (single-channel wide-sample without the
  stereo decorrelation search), `mono/wasted_bits/s16/44k1/1s` (16-bit
  container with an 8-bit effective range, exercising the wasted-bits
  detector + narrowed-bps subframe path) and `mono/constant/s16/44k1/1s`
  (DC content, short-circuiting each subframe to CONSTANT — a
  regression guard for per-frame setup work).
- **`decode`** — same four scenarios on the decode-only side (the
  encode step is outside the timed region; only `send_packet ->
  receive_frame` is measured).
- **`roundtrip`** — encode + decode back-to-back, with each iteration
  asserting that the recovered byte count equals the input so a
  state-machine drift would surface as a `panic!` in the bench
  output rather than silent miscompression. Seven scenarios: the
  original four (mono S16, stereo S16, stereo S24, 6-channel S16) and
  three round-207 additions that bring roundtrip coverage in line
  with the matching `encode` / `decode` scenarios — `mono/s24/48k/500ms`
  (wide-sample single-channel pipeline without the stereo decorrelation
  search), `mono/noise/s16/44k1/1s` (uniform-noise content that pushes
  per-partition Rice `k` toward the §9.2.7 escape marker in both
  directions — the recovered-bytes invariant doubles as a regression
  guard that the escape branch never silently drops or duplicates
  samples), and `stereo/s32/96k/250ms` (full-width 32-bit residual +
  LPC + S32 little-endian interleave, the only path that reaches the
  encoder's and decoder's 32-bit branches end-to-end).

No committed fixture files; PCM is synthesised in-bench. Run with
`cargo bench -p oxideav-flac --bench {encode,decode,roundtrip,crc,seek}`.
The shape mirrors the cinepak / tta bench harnesses so numbers are
directly comparable across the workspace.

### Seek-path benchmark

- **`seek`** *(r223)* — four scenarios measuring the demuxer's
  `seek_to` hot path (binary search across the parsed `Vec<SeekPoint>`
  per RFC 9639 §8.5.1 + `Cursor::seek` + `scan.reset_to` + post-seek
  sync-code re-walk). The decode/encode/roundtrip harnesses all
  measure forward-only work; the seek path had never been
  benchmarked in isolation, so a regression to linear-scan or a
  slow `Cursor::seek` would be invisible. Two warm-table scenarios
  use `iter_batched` so STREAMINFO + SEEKTABLE parse cost is amortised
  out: `seek_only/N=11` (1 s of audio → 11 SEEKPOINTs at the
  encoder's default 4096-sample block size) and `seek_only/N=173`
  (16 s → 173 SEEKPOINTs). A `seek_then_next_packet` scenario
  measures the user-visible seek + sync-walk cost. A
  `linear_walk_baseline` scenario walks every packet without
  seeking — A/B baseline for the per-packet demux throughput.
  Baseline numbers (release, Darwin aarch64, `--quick`):
  seek_only ~14.6 ns / call at N=11, ~19.5 ns at N=173 (ratio
  1.34× — well below the linear-scan worst case of ~16×, confirming
  the binary search is fast enough that the constant-cost
  `Cursor::seek` + `reset_to` overhead dominates the comparison
  loop); seek_then_next_packet ~2.47 µs / call (the post-seek
  sync-code walk into the next frame dominates); linear_walk_baseline
  ~1.36 G samples/s. Fixtures synthesised via the production encoder
  with a SEEKTABLE injected through
  `oxideav_flac::metadata::write_seektable` +
  `BlockHeader::write_into` — no committed `.flac` files. Gives a
  future seek-path tweak (a zero-copy Cursor-replacement, a packed
  16-byte SeekPoint, an "already at offset" fast-path) a stable A/B
  baseline.

## Profiling

`examples/profile_flac.rs` is a flat measure-this-thing driver suited
for `samply`, `perf record`, and `cargo flamegraph`. Criterion's
warm-up + sampling layers + estimator math show up in a sampling
profile and bury the real codec hot paths; the example is one
`Instant::now()` / `elapsed()` pair around a fixed-iteration loop so
the profile is dominated by encoder / decoder cost itself. Same four
scenarios as the benches (mono S16 44.1 kHz 1 s, stereo S16 44.1 kHz
1 s, stereo S24 48 kHz 0.5 s, 6-channel S16 48 kHz 0.25 s) so the
profile output and bench numbers correspond.

Usage:

```text
cargo run --example profile_flac --release -- <mode> [<iters>]
# modes: encode | decode | roundtrip | all (default)
```

Driving it under `samply`:

```text
samply record -- ./target/release/examples/profile_flac encode 30
samply record -- ./target/release/examples/profile_flac decode 100
```

`profile/` carries committed SVG flamegraphs (`encode.svg`,
`decode.svg`, `roundtrip.svg`) plus the matching folded-stack
inputs from a round-147 capture on macOS arm64 — see
`profile/README.md` for the full recipe + reproduction notes. The
captured profile pins `oxideav_flac::encoder::best_partition`
(the per-subframe Rice partition-order search introduced in round
129) at ~61 % of encode self-samples; the obvious next encoder
optimisation handle is reusing a single residual `Vec<i32>`
buffer across subframes / candidate plans (allocator paths
contribute another ~10 % to encode wall time).

Reference numbers on an M-series laptop (release build, default
iteration count, single run — absolute MiB/s will vary by host but
the encode-vs-decode shape is stable):

| Path / scenario                          |  ms/iter |   MiB/s | Realtime |
| ---------------------------------------- | -------: | ------: | -------: |
| `decode    mono/s16/44k1/1s`             |     0.45 |   ~190  |  ~2230x  |
| `decode    stereo/s16/44k1/1s`           |     1.10 |   ~150  |   ~910x  |
| `decode    stereo/s24/48k/500ms`         |     0.67 |   ~205  |   ~745x  |
| `decode    6ch/s16/48k/250ms`            |     0.74 |   ~190  |   ~340x  |
| `encode    mono/s16/44k1/1s`             |      252 |   ~0.33 |    3.9x  |
| `encode    stereo/s16/44k1/1s`           |     1005 |   ~0.17 |    1.0x  |
| `encode    stereo/s24/48k/500ms`         |      491 |   ~0.28 |    1.0x  |
| `encode    6ch/s16/48k/250ms`            |      388 |   ~0.36 |    0.6x  |
| `encode    mono/s24/48k/500ms`           |      115 |   ~0.61 |    4.3x  |
| `encode    mono/wasted/s16/44k1/1s`      |      261 |   ~0.32 |    3.8x  |
| `encode    mono/constant/s16/44k1/1s`    |    0.263 |   ~324  |   ~3800x |

The decode path is comfortably realtime by orders of magnitude. The
encode path is now mono-realtime (3.8x) and within a small factor of
realtime for stereo / multichannel — round 143's closed-form Rice
parameter estimate (see `best_rice_params` in `src/encoder.rs`) cut
the per-partition cost by ~4.6×–6× over the previous brute-force
`for k in 0..=k_max` scan with byte-identical output (regression-
tested in `encoder::tests::best_rice_params_matches_brute_force_reference`).
The remaining encode cost lives in the per-subframe LPC + apodization
window search, with each surviving LPC subframe still iterating
0..=8 partition orders × 2 Rice methods. Round 176 cut a further
~12–23 % off the encode wall time on the existing benches by hoisting
the residual zigzag conversion from per-partition-per-(method,
partition_order) scope up to **once per subframe** in
`encode_rice_residual` (see CHANGELOG `Round 176`). Round 186 took the
same shape one step further: a subframe-scoped prefix-sum table (so
the closed-form `k_est` input `sum_u` is O(1) per partition) and a
per-sample raw-bit-width `u8` table (so the escape-cost test's max-bits
scan is a `u8`-slice max instead of `leading_zeros` per element) both
shared across every `(method, partition_order)` pair the search visits;
the wider-sample scenarios benefited most (`mono/s24/48k/500ms` 160 ms
→ 128 ms, `stereo/s24/48k/500ms` 622 ms → 511 ms, `6ch/s16/48k/250ms`
474 ms → 410 ms — see CHANGELOG `Round 186` for the full table). Round
191 promoted the per-subframe scratch tables themselves to **encoder-
owned reusable buffers** — `residuals`, `zigzag`, `prefix`, `raw_bits`,
plus the LPC `windowed`, `autoc`, `lpc_state`, `lpc_coeffs`, `qcoeffs`
buffers — threaded through the candidate sweep so they grow once to
peak capacity on the first frame and then reuse across every Fixed /
LPC / precision candidate and across every subsequent frame. The
biggest beneficiary is `6ch/s16/48k/250ms` (~7 % faster) where six
independent subframes per frame amortise the scratch-reuse savings;
the remaining encode benches gain 1–2.5 % each. Round 150 had
attempted the same shape against the post-r147 hot path and measured
a ~10 % regression because that path's per-partition allocations were
already small enough for libmalloc's freelist; the post-r186 hot path
allocates a different set of subframe-scoped buffers (the prefix-sum
and raw-bits tables added in r186) whose reuse now does pay off. See
`CHANGELOG.md` `[Unreleased]` for the bench A/B and the r150 note for
the historical contrast.

## Codec / container IDs

- Codec: `"flac"`; accepted sample formats `U8`, `S16`, `S24`, `S32`.
- Container: `"flac"`, matches `.flac` / `.fla` by extension and the
  `fLaC` magic bytes (including files prefixed with an ID3v2 tag).

## License

MIT — see [LICENSE](LICENSE).
