# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [Unreleased]

### Added

- tests: **cross-implementation encode oracle** — a new
  `reference_decode_oracle` suite encodes PCM with this crate's encoder,
  assembles a complete `.flac` stream (`fLaC` magic + finalised
  STREAMINFO/PADDING chain + frame packets) and decodes it with an
  *independent* black-box reference `flac` decoder, asserting sample-exact
  recovery. This closes a blind spot the existing self-round-trip cannot:
  an encoder + decoder that share a bitstream bug which cancels out would
  pass `roundtrip.rs` but be caught here. 13 cases span every supported
  bit depth (U8/S16/S24/S32), mono → 7.1, CONSTANT silence, 6-bit
  wasted-bits shift, 256-sample blocks (last-frame remainder path),
  PADDING, the fast preset, a non-subset config (LPC order 32 +
  partition order 15), and each public apodization window driven alone
  (Welch / Hann / Tukey / Bartlett / Blackman-Harris) so a window with a
  broken weight function would surface as a foreign-decoder mismatch. The
  reference binary is invoked purely as an opaque process (its source is
  never read); the suite skips cleanly when the binary is absent, so it
  never reddens CI on a host without it.

- tests: **stereo-decorrelation selection completeness.** Two new unit
  tests pin the inverse of the existing correlated-stereo cases:
  uncorrelated stereo (two independent random walks) must keep the
  independent L/R assignment (code 1) rather than pay the side-channel
  penalty, and a cheap-left / expensive-right pair must never select
  right-side (code 9) — left-side or mid-side stores the cheap channel
  verbatim and always wins. Together with the prior tests, all four
  channel assignments (independent / left-side / right-side / mid-side)
  now have their selection logic exercised in both the win and lose
  directions.

- encoder: **encode-time verify (`--verify`)** — opt-in
  `FlacEncoderOptions { verify: true, .. }` (and the
  `FlacEncoderOptions::verifying()` preset) decode each frame back to PCM
  as it is produced and assert it reconstructs the input block
  bit-exactly, through the same `decoder::decode_frame_channels` core the
  streaming decoder uses (so the check sees the real frame-header parse +
  trailing CRC-16). On the first diverging sample — or a frame that fails
  to decode — `send_frame` / `flush` returns `Error::InvalidData` naming
  the offending `(frame, channel, sample)`, and the bad frame is not
  queued for `receive_packet`, so a caller that propagates the error never
  emits a silently-corrupt `.flac`. This is FLAC's canonical encoder
  safety net: strictly stronger than the post-hoc whole-stream MD5 check
  (it pinpoints the mis-encode at the producing frame, before the file is
  finalised, and needs no STREAMINFO signature). Verify has **zero on-wire
  effect** — emitted bytes are byte-for-byte identical to the default
  encoder; it only decides whether they are checked, at the cost of one
  extra decode per frame. Default `verify: false` is byte-stable. Tests
  pin: byte-identical output vs. default (both decoded PCM and raw frame
  bytes), a pass across every supported depth/layout (8/16/24/32-bit,
  1/2/6-channel), and the failure path (mismatching input → error naming
  the channel/sample; positive control accepts the true input).

- tests: **foreign-encoder decode oracle** — `reference_encode_decode`
  encodes raw PCM with a black-box reference `flac` encoder and decodes
  the result through this crate's container + decoder, asserting
  sample-exact recovery across 8/16/24/32-bit and mono/stereo. The
  mirror image of the encode oracle: that proves our bytes decode
  elsewhere; this proves we decode bytes produced elsewhere. The headline
  case is genuine 32-bit audio, which carries bit-depth code `0b111` in
  every frame header (see the Fixed entry below).

### Fixed

- encoder: **32-bit frames now self-describe their bit depth.** The
  frame-header sample-size field emitted code `0b000` ("from STREAMINFO")
  for 32-bit input — every other supported depth (8/12/16/20/24) already
  wrote its explicit RFC 9639 §9.1.4 Table 17 code. 32-bit now writes
  `0b111` (code 7) like the others, so a 32-bit frame carries its own
  depth and decodes correctly even if separated from its STREAMINFO. The
  field is 3 bits wide for every code, so frame size is unchanged; the
  emitted 32-bit bytes differ only in that one header field, and remain a
  valid stream that decodes sample-exact (verified through the
  independent-decoder oracle).

- decoder: **accept the 32-bit frame-header bit-depth code (`0b111`).**
  RFC 9639 §9.1.4 Table 17 assigns sample-size code `0b111` to 32 bits
  per sample, but the frame-header parser treated it as reserved and
  rejected the frame. A reference encoder emits exactly this code in
  every frame of a genuine 32-bit FLAC file, so before this fix the
  decoder could not read any real 32-bit stream (it could only fall back
  to the STREAMINFO-supplied depth via code `0b000`). The parser now maps
  `0b111 → 32` per the table; covered by a new exhaustive Table-17 unit
  test plus the foreign-encoder 32-bit decode oracle.

### Changed

- encoder: **apodization weight rows are memoised per `(window, n)`
  instead of recomputed per subframe.** The maximal-effort LPC search
  tapers every subframe with each analysis window before autocorrelating,
  and the taper calls `window.weight(i, n)` — a transcendental (`cos`) for
  Hann / Tukey / Blackman-Harris — once per sample. With a fixed block
  size every subframe of every frame has the same length `n`, so the same
  `(window, n)` weight vector was rebuilt from scratch for each of them.
  The encoder now caches each `(window, n)` weight row on the reused
  per-subframe scratch and multiplies samples by the cached row. **Output
  is byte-identical**: `window.weight(i, n)` is a pure function of
  `(window, i, n)`, so a cached weight is the same `f64` a fresh call
  returns, and `sample as f64 * weight` is the same product — the windowed
  signal, autocorrelation, LPC fit, and emitted bytes are unchanged. A new
  `cached_window_weights_match_inline_weight` unit test pins the cache
  against direct `weight` calls across every window and several block
  sizes. Measured additional encode speedup ~3–5% on the stereo/6-channel
  S16 benches (atop the finest-level shift-sum win).

- encoder: **finest-level shift-sum accumulation skips the all-zero
  high-`k` tail per residual.** Building the partitioned-Rice ladder's
  finest level sums `zigzag[i] >> k` into `stride = k_max + 1` rows for
  every residual. But `zigzag[i] >> k` is `0` for every
  `k >= 64 - leading_zeros(zigzag[i])`, so the high rows only ever receive
  `+ 0`. The inner loop now walks exactly the low
  `min(64 - leading_zeros(u), stride)` rows and stops, leaving the
  zero-add tail untouched — which dominates for the small residuals a good
  LPC fit produces. **Output is byte-identical**: skipped rows would have
  added `0`, so every shift-sum (and the derived partition-order cost
  search and winning Rice selection) is unchanged. A new
  `build_finest_level_matches_naive_full_stride` unit test pins the
  optimized loop against a naive full-`stride` reference across partition
  orders, `k_max` values, and residual magnitudes spanning 0 to 32
  significant bits (including the `nbits >= stride` clamp). Measured
  encode speedup ~4–7% on the stereo/6-channel S16 benches.

- encoder: **partitioned-Rice residual search now merges partition costs
  bottom-up instead of re-walking the residual slice per partition order**
  (GitHub #12 follow-up, after the LPC hoist). Each partition's Rice cost
  depends only on additive per-partition stats — `sum(zigzag[i] >> k)` for
  every `k`, the residual count, and `max(raw_bits[i])`. The search now
  builds those stats for the *finest* legal partition order **once** (a
  single `O(n·(k_max+1))` pass) and derives every coarser order by summing
  adjacent partition pairs (`O(num_partitions·stride)` adds), rather than
  re-scoring every `(method, partition order)` pair against the slice. A
  single shared finest-level ladder (built at the wider method-1
  `k_max = 30`) serves both Rice methods; method 0 clamps its search
  window to `k_max = 14` over the same rows. **Output is byte-identical** —
  `sum(u >> k)` and the residual count are additive over a partition split
  and `max` composes, so each merged order's per-partition cost equals the
  per-slice path's, and the winning `(method, partition order, Rice k,
  escape)` selection (including its strict-`<` tie-break) is reproduced
  exactly. The full reference / roundtrip / MD5 / compression-quality suite
  passes unchanged; a new `merge_ladder_matches_per_slice_oracle` crosswalk
  pins the ladder against the retained per-slice oracle. Measured
  encode speedup ~1.6–1.8× on the mono/stereo S16/S24 benches.

- encoder: **per-subframe LPC analysis no longer re-windows and
  re-autocorrelates the block once per LPC order** (GitHub #12 "encode is
  very slow"). The maximal-effort search visits every apodization window ×
  LPC order 1..=`max_lpc` × precision; previously each `(window, order)`
  pair re-tapered the block (O(n)) and recomputed the autocorrelation
  (O(n·order)) from scratch inside `levinson_durbin_s`, so a block was
  windowed/autocorrelated ~`max_lpc`× per window when each is needed only
  *once* per window. The loop nest is now inverted: for each window the
  encoder windows + autocorrelates (at the maximum order) + runs
  Levinson-Durbin a single time, snapshotting every order's coefficients
  (`levinson_durbin_all_orders_s`), then sweeps order × precision off the
  snapshots. **Output is byte-identical** — the autocorrelation at the
  maximum order subsumes every lower order's lags with the same f64
  summation order, and Levinson-Durbin's order-`m` solution is bit-stable
  whether reached standalone or as a prefix of the full recursion — so the
  full reference / roundtrip / MD5 suite passes unchanged. The redundant
  windowing/autocorrelation was not the dominant cost (the Rice
  partition-order search is), so the default encoder's measured win is
  modest (~2-3%); callers who want a large speedup use the new
  `FlacEncoderOptions::fast` preset below.

### Added

- encoder: **`FlacEncoderOptions::fast()` speed preset** — a
  speed-biased configuration (single Welch apodization window +
  `max_lpc_order = 8`) analogous to a low `flac -N` level, for callers who
  want a markedly faster encode over the last few percent of ratio. On a
  1 s stereo S16/44.1 kHz buffer it encodes ~6.8× faster than the default
  maximal-effort search (~232 ms vs ~1586 ms). The output bytes differ
  from the default encoder's (fewer candidates searched) but remain a
  valid streamable-subset FLAC stream that decodes bit-exactly; the
  default `FlacEncoderOptions::default()` behaviour is unchanged and still
  byte-stable. A doc-test on `FlacEncoderOptions::fast` shows both the
  default and fast-preset encode through the public API.

- encoder: the default apodization (analysis) window set the per-subframe
  LPC search evaluates grows from three windows (Welch, Hann, Tukey(1/4))
  to five — adding `Bartlett` and `BlackmanHarris`. The window has no
  on-wire footprint (RFC 9639 §9.2.6 fixes only the quantised
  coefficients, precision, and shift), so the wider set is subset-neutral
  and the minimum-bit `best_subframe` driver keeps whichever LPC subframe
  encodes smallest — a window that never wins is discarded. On real
  multichannel / wideband fixtures the extra pair recovers a few percent
  the three-window set left on the table (e.g. ~5% on a 7.1/24-bit
  fixture, ~2.6% on 24-bit stereo) while every emitted stream stays a
  valid streamable-subset FLAC stream that decodes bit-exactly. Default
  output bytes change (strictly smaller-or-equal); callers who want the
  leaner three-window search (or any other set) select it via
  `FlacEncoderOptions::apodization`. The cost is two extra
  Levinson-Durbin solves per LPC order per subframe at encode time.

### Added

- muxer: **SEEKTABLE generation** (RFC 9639 §8.5). The native FLAC muxer
  now builds a SEEKTABLE metadata block from the frames it writes,
  recording each frame's `(first_sample, offset_from_first_frame,
  frame_samples)` seek anchor from the frame header and inserting a single
  SEEKTABLE block — at a configurable point density — into the metadata
  chain at `write_trailer` time. The seek-point byte offset is relative to
  the first frame header (§8.5.1), exactly what the demuxer's `seek_to`
  consumes, so a muxer→demuxer round-trip lands seeks on the right frame
  without the slow header-scanning fallback. Generation is on by default
  (one point per ~second of audio, capped at 2048 points); an existing
  SEEKTABLE in the extradata is preserved untouched (§8.5 forbids more
  than one). The new public `container::open_muxer_with_options` +
  `FlacMuxerOptions` (`generate_seektable`, `seek_point_interval`,
  `max_seek_points`, plus `for_sample_rate` / `no_seektable` constructors)
  expose density control and an opt-out that restores the historical
  zero-buffering pass-through behaviour byte-for-byte.
- decoder/verify: **verify-decoder MD5 path** (RFC 9639 §8.2). New
  `verify` module recomputes the STREAMINFO MD5 from decoded PCM to
  confirm a whole-stream decode is lossless — the canonical FLAC
  integrity check, complementary to the per-frame CRC-16.
  `verify::verify_stream(&flac_bytes)` walks the metadata chain, decodes
  every frame, and returns `VerifyOutcome::Match` / `NoSignature` (an
  all-zero signature is "MD5 not computed" per §8.2) or an
  `Error::InvalidData` with both digests on mismatch. `Md5Verifier`
  exposes the same accumulation incrementally for streaming callers. The
  PCM is serialised in FLAC's canonical order (interchannel samples
  interleaved, each signed two's-complement little-endian, sign-extended
  to a whole byte), validated against the RFC 9639 Appendix D.1 worked
  example. `decoder::decode_frame_channels` is factored out as the shared
  decode + CRC-16 core returning post-decorrelation `i32` planes, so the
  streaming `Frame::Audio` path and the verifier check exactly the same
  samples.
- tests: `verify_md5` integration suite. Runs `verify_stream` over every
  reference `input.flac` in `docs/audio/flac/fixtures/` — all 18 verify
  `Match` against the *external* reference-encoder MD5 (constant / fixed
  / LPC / verbatim subframes; L-R / L-S / R-S / M-S layouts; 8/16/24/32-bit;
  mono..7.1). A corruption test flips a body byte and asserts
  verification no longer matches; an encode→verify roundtrip closes the
  loop through the encoder-stamped MD5 across mono / stereo / 24-bit / 7.1.
- tests: `compression_quality` suite pinning two load-bearing invariants
  of the encoder's minimum-bit rate-distortion search. (1) A real-fixture
  guard re-encodes every PCM fixture under `docs/audio/flac/fixtures/`
  (mono/stereo/7.1, 8/16/24-bit) and decodes it back through this crate's
  own decoder, asserting byte-exact recovery plus a locked
  compressed/PCM-ratio ceiling per fixture so a quietly de-tuned RDO
  search trips a size regression instead of passing silently. (2) A
  monotonicity property battery proves that widening the search space —
  adding apodization windows, raising the LPC-order ceiling to 32, raising
  the partition-order ceiling to 15 — can only ever shrink or tie the
  encoded size (never inflate it), across tonal, transient,
  near-white-noise, and autoregressive signal classes, with each variant
  also re-checked lossless. Five tests; no production code changed.
- encoder: two additional apodization (analysis) windows in the public
  `encoder::Apodization` enum — `Bartlett` (triangular: a linear ramp
  0→1→0, the cheapest non-rectangular taper, with milder edge attenuation
  than Hann) and `BlackmanHarris` (the classic four-term cosine-sum
  window `0.35875 − 0.48829·cos + 0.14128·cos2 − 0.01168·cos3`, whose far
  lower side lobes give the strongest spectral-leakage suppression in the
  set). Like the existing windows these only taper a block before the
  autocorrelation that feeds Levinson-Durbin (RFC 9639 §9.2.6) and have
  **no** on-wire footprint — the decoder never observes which window
  shaped the analysis, so output stays a valid streamable-subset stream
  regardless of the set, and the minimum-bit `best_subframe` driver keeps
  the smallest plan so adding a window can only shrink or tie the output.
  `Apodization` is `#[non_exhaustive]`, so the new variants are additive.
  New tests: `bartlett_and_blackman_harris_weights_and_lossless` (weight
  curve sanity — non-negative, symmetric about the midpoint, zero-edged,
  unit-peaked + lossless round-trip through each window as sole analysis
  window) and `new_windows_steer_lpc_fit` (each new window changes the
  per-order LPC plan vs Welch + a window superset never loses to its
  subset).
- demuxer: seektable-less seeking. `seek_to` on a file with no SEEKTABLE
  metadata block no longer returns `Error::Unsupported`; instead it scans
  CRC-8-verified frame headers forward from the start of the frame stream
  and lands on the last frame whose first sample is ≤ the target (RFC 9639
  §8.5 — seeking without a seek table is permitted, with an
  implementation-defined delay). Targets past the final frame exhaust the
  scan to the last decodable frame; negative targets clamp to sample 0.
  The scan enforces the RFC 9639 §11 anti-DoS plausibility checks — every
  accepted coded number must be CRC-verified, strictly increasing, and
  within the STREAMINFO total-sample bound (when known) — and the read
  cursor advances monotonically, so a malicious stream with non-monotonic
  or out-of-bounds coded numbers cannot trap the scanner in an infinite
  loop. The SEEKTABLE-driven path is unchanged.
- encoder: caller-configurable per-frame block size via
  `FlacEncoderOptions::block_size` + a subset opt-out flag
  `FlacEncoderOptions::allow_non_subset_block_size` (RFC 9639 §9.1).
  `None` (the default) keeps the historical fixed 4096-sample block and is
  byte-for-byte unchanged. `Some(n)` sets the fixed-blocking block size to
  `n` (STREAMINFO `min_block_size == max_block_size == n`); every frame but
  the last carries exactly `n` interchannel samples, the last carries the
  remainder (exempt from the §8.2 minimum-block floor). `n` must sit in the
  STREAMINFO range `16..=65535` (RFC 9639 §8.2) or construction returns
  `Error::invalid`. By default the block size is held inside the streamable
  subset (RFC 9639 §7: ≤ 4608 for sample rates ≤ 48 kHz, ≤ 16384 otherwise)
  and a request above the applicable ceiling is rejected;
  `allow_non_subset_block_size: true` lifts the bound to the full
  STREAMINFO ceiling (65535), producing a valid — but no longer
  streamable-subset — stream any spec-complete decoder (including this
  crate's) recovers bit-exactly. The block size is a free encoder choice
  the decoder reads from the §9.1.2 frame-header field, so it changes only
  framing/rate, never the recovered samples. The existing per-subframe LPC
  precision ceiling and Rice partition-order search already key off the
  live per-frame block length, so they adapt to the configured size with
  no further change. New tests:
  `block_size_none_equals_default_4096` (default / explicit-4096 byte
  equivalence + framing), `small_block_size_roundtrips_and_frames_as_configured`
  (1024-sample fixed-block lossless round-trip + STREAMINFO min==max==1024),
  `block_size_out_of_streaminfo_range_is_rejected` (16..=65535 bounds),
  `subset_block_size_ceiling_is_rate_dependent` (4608/16384 rate-keyed
  ceiling + opt-in), and `non_subset_block_size_roundtrips_bit_exact`
  (8192-sample non-subset lossless round-trip at 44.1 kHz).
- encoder: caller-configurable apodization (analysis) window set via
  `FlacEncoderOptions::apodization` + a public `encoder::Apodization`
  enum (`Welch` / `Hann` / `Tukey { alpha_num, alpha_den }`). `None`
  (the default) uses the default window set (see the **Changed** entry
  above for its current membership); an empty `Some(vec![])` falls back
  to that set so the LPC search always has at least one window.
  `Some(set)` replaces the windows the per-subframe LPC search
  evaluates (RFC 9639 §9.2.6). A window only tapers the block before the
  autocorrelation that feeds Levinson-Durbin — it has no on-wire
  footprint (the RFC fixes only the quantised coefficients, precision,
  and shift), so the decoder never observes it and the output stays a
  valid streamable-subset stream for any set. The minimum-bit
  `best_subframe` driver keeps the smallest plan across every `(window,
  order, precision)` candidate, so a larger set can only shrink or tie
  the output; the trade is one extra Levinson-Durbin solve per window
  per LPC order. `Apodization::Tukey` takes a clamped `alpha = num/den`
  taper fraction (a zero denominator is treated as `alpha = 0`,
  rectangular, so the public API can never trip the internal weight
  function's divide). New tests:
  `apodization_none_equals_default_set_and_empty_falls_back` (default /
  empty-set byte equivalence), `custom_apodization_set_roundtrips_bit_exact`
  + `zero_denominator_tukey_is_safe_and_lossless` (lossless through the
  custom path incl. the pathological denominator), and
  `custom_window_can_beat_default_set` (a non-default window set steers
  the LPC fit + beats Welch-only on leakage-sensitive content).
- fuzz: thirteenth `cargo-fuzz` target `seek` hardening the demuxer
  seek path (`<FlacDemuxer as Demuxer>::seek_to` + post-seek packet
  drain) for panic-freedom (RFC 9639 §8.5 SEEKTABLE). No prior target
  ever calls `seek_to`: the host-pipeline targets walk `next_packet`
  linearly and `benches/seek.rs` only seeks over a valid
  encoder-produced stream whose SEEKTABLE was built by `write_seektable`
  (which enforces the §8.5.1 sorted / unique / in-range MUSTs). The new
  target reaches the seek arithmetic with adversarial input two ways:
  (A) raw fuzz bytes into the demuxer plus a seek-target sweep (covers
  the magic-reject / no-SEEKTABLE / stream-index-out-of-range error
  arms); (B) a synthesised valid mono stream whose SEEKTABLE seekpoints
  are hand-assembled verbatim from fuzzer bytes (18 B each), bypassing
  the writer's guard so unsorted / placeholder
  (`sample_number == u64::MAX`, §8.5.2) / out-of-range points reach
  `seek_points.partition_point(...)`, the unchecked
  `first_frame_offset + byte_offset` (u64-overflow + past-EOF
  candidate), and `scan.reset_to(...)` followed by the CRC-verified
  post-seek frame walk. Seek targets always include the fixed boundary
  values (`i64::MIN` / `-1` / `0` / `1` / `i64::MAX`) so the
  `pts.max(0)` clamp and the partition extremes are hit every
  iteration. Validated locally panic-free over ~11 K executions.
- encoder: opt-in higher LPC predictor orders via
  `FlacEncoderOptions::max_lpc_order` (RFC 9639 §9.2.6). `None` (the
  default) keeps the historical search ceiling of 12 — exactly the
  streamable-subset boundary for ≤ 48 kHz audio (RFC 9639 §7) — and is
  byte-for-byte unchanged. `Some(k)` lifts the per-subframe LPC search
  ceiling up to the spec maximum of 32 (requests clamped into
  `1..=32`), letting content with a long impulse response (echoes,
  high-order AR signals) select an order the default search never
  reaches. The minimum-bit `best_subframe` driver guarantees a higher
  ceiling can only shrink or tie the output. New regression tests:
  `higher_lpc_order_wins_on_ar20_signal` (an AR signal with a dominant
  lag-20 tap is captured by orders 13..=24 but not 1..=12),
  `higher_lpc_order_roundtrips_bit_exact` (lossless through the lifted
  path at order ≤ 32), and `max_lpc_order_request_is_clamped_to_spec_range`
  (clamp bounds + `None` == `Some(12)` byte equivalence).

## [0.0.11](https://github.com/OxideAV/oxideav-flac/compare/v0.0.10...v0.0.11) - 2026-06-15

### Other

- opt-in non-subset partition orders (RFC 9639 §9.2.7, up to 15)
- round-307 decode wasted-bits scenario — exercise the §9.2.2 shift-back leg
- round-299 application target — direct stress on the APPLICATION metadata block (RFC 9639 §8.4)
- add subframe_decode target — structure-aware §9.2 subframe decoder stress
- reject out-of-range sample_rate at construction (daily-fuzz crash fix)
- r283 metadata_chain target — whole-chain parse/write mutation oracle + two parse/write contract fixes
- typed MetadataChain parser + writer enforcing RFC 9639 §8 chain-level rules
- flac metadata: typed PICTURE picture-type taxonomy + URI accessors (RFC 9639 §8.8 / Table 13)
- r267 typed CUESHEET sub-field accessors (RFC 9639 §8.7)
- scrub libavcodec naming from frame_header doc-comment prose
- round-259 frame_header target — focused stress on `parse_frame_header` (RFC 9639 §9.1)
- round-255 md5 harness for STREAMINFO MD5 (RFC 9639 §8.2)
- round-252 typed StreamInfo writer (RFC 9639 §8.1)
- drop release-plz.toml — use release-plz defaults across the workspace
- round-244 typed Application accessor + writer
- round-241 typed VorbisComment accessor + writer
- round-237 utf8_varint target — writer ↔ reader roundtrip + byte-length stability for the FLAC §9.1.5 varint
- round-232 md5_streaming target — streaming-vs-oneshot equivalence (RFC 9639 §8.2)
- round-228 encoder_options target — FlacEncoderOptions construction + extradata invariants
- round-223 seek harness for SEEKTABLE binary-search + sync-walk
- round-218 crc harness for streaming Crc8 / Crc16 validators
- streaming CRC-8 / CRC-16 validators for chunked-input callers
- round-207 roundtrip coverage for mono-S24 / noise / S32
- round-200 metadata_walker — writer-direction round-trip target

### Fixed

- encoder: `make_encoder` / `make_encoder_with_options` panicked
  (instead of returning `Err`) when `CodecParameters::sample_rate`
  was 0 or above the RFC 9639 §8.2 20-bit STREAMINFO ceiling
  (1_048_575) — the unvalidated rate flowed into the STREAMINFO
  writer inside `build_extradata`, whose bounds check was consumed
  by an `expect`. Found by the daily `encoder_options` fuzz run
  (2026-06-11, `sample_rate == u32::MAX` arm); the constructor now
  rejects out-of-range rates up front
  (`make_encoder_rejects_out_of_range_sample_rate` regression).
  Flush-time STREAMINFO rebuilds additionally degrade a
  total-sample count above the 36-bit field to the §8.2 "unknown"
  sentinel (0) instead of being able to trip the same writer check.

- metadata: two round-283 parse-accepts/write-refuses contract
  divergences surfaced while building the `metadata_chain` fuzz
  target, both breaking the chain rewrite (read → edit → re-emit)
  path the r279 `MetadataChain` API promises:
  - `parse_cuesheet` accepted a media catalog number whose leading
    run (up to the first NUL) contained non-printable bytes, while
    `write_cuesheet` refuses exactly those bytes — RFC 9639 §8.7
    requires printable ASCII `0x20..=0x7E`. The parser now applies
    the writer's check, so every CUESHEET it accepts can be
    re-emitted. Post-NUL right-padding bytes remain outside the
    check on both sides (§8.7 defines them as padding).
  - `MetadataChain::parse` accepted SEEKTABLE blocks whose real
    seek points violate the RFC 9639 §8.5.1 sorted-ascending /
    unique-by-sample-number MUSTs that `write_seektable` (and
    therefore `MetadataChain::write`) enforces. The chain parser
    now rejects such tables; the block-level `parse_seektable`
    keeps its lenient no-error-channel signature unchanged.

  Both pinned by unit regressions
  (`cuesheet_parser_rejects_nonprintable_catalog_byte`,
  `metadata_chain_rejects_unsorted_seektable_points`,
  `metadata_chain_rejects_cuesheet_with_nonprintable_catalog`).

### Added

- encoder: round-310 opt-in non-subset partition orders. New
  `FlacEncoderOptions::allow_non_subset_partition_order` field; when
  `true`, `make_encoder_with_options` lifts the partitioned-Rice
  residual search ceiling from the FLAC streamable-subset bound of 8
  (RFC 9639 §7) to the full 4-bit §9.2.7 `partition order` field range
  of 15. Content whose residual statistics flip on a scale finer than
  the order-8 partition size (a quiet run abutting a transient inside
  one block) can then isolate each region in its own partition with a
  tighter Rice parameter; the search keeps the smallest layout, so a
  finer split only ever shrinks or ties the residual payload. The
  default (`false`) and `make_encoder` keep the subset ceiling of 8 —
  the produced bytes are byte-for-byte identical to the pre-r310 output
  for every existing caller. The decoder already read the full 4-bit
  field, so non-subset output round-trips bit-exactly; the only
  consequence of the higher ceiling is that the stream is no longer
  streamable-subset (RFC 9639 §7). Implemented by lifting the
  hard-coded `MAX_PARTITION_ORDER` constant into a per-encoder
  `SubframeScratch::max_partition_order` (defaulting to the new
  `SUBSET_MAX_PARTITION_ORDER = 8`, with `MAX_PARTITION_ORDER` now the
  non-subset ceiling 15) so the existing eight-deep subframe call chain
  carries the cap without a parallel parameter. Two regressions pin it:
  `non_subset_partition_order_never_inflates_and_can_shrink` (a residual
  span tuned so the cost minimum sits above order 8 — the non-subset
  search emits strictly fewer bits, and never more on any shape) and
  `non_subset_partition_order_roundtrips_bit_exact` (a full
  `make_encoder_with_options` → `make_decoder` cycle recovers the input
  PCM byte-for-byte). All five existing `FlacEncoderOptions` struct
  literals in `encoder::tests` updated to `..FlacEncoderOptions::default()`.
- bench: round-307 `decode_mono_wasted_s16_44k1_1s` decode scenario —
  every sample carries 4 trailing zero bits, so the encoder tags a
  nonzero wasted-bits count on every subframe and the decoder runs its
  post-predict left-shift-back loop (`subframe::decode_subframe`'s
  `wasted > 0` branch, RFC 9639 §9.2.2) on every block. The existing
  tone-plus-noise decode scenarios bypass that leg because their
  residual noise leaves no consistent trailing zeros to waste, so the
  wasted-bits decode path was previously unmeasured on the decode-only
  side. ~389 µs/iter (release, Darwin aarch64). A profiling-guided
  rewrite of the `apply_lpc` / `apply_fixed_predictor` inner loops
  (slice-window / `iter().rev()` zip forms) was prototyped and
  measured against all eight decode scenarios but reverted — it
  regressed several scenarios 10-20% versus the existing direct-index
  form, which LLVM already lowers well.

- fuzz: round-299 `application` target (target 12) — focused stress
  on the APPLICATION metadata block (`metadata::{parse_application,
  write_application, Application}`, RFC 9639 §8.4). The typed
  APPLICATION parser / writer pair (added r244) was the last typed
  metadata-block surface with no direct fuzz coverage:
  `metadata_walker`'s `try_typed_round_trip` explicitly no-ops on
  `BlockType::Application`, and `metadata_chain` reaches
  `parse_application` only behind a structurally valid full-chain
  gate random input almost never threads. The harness drives both
  directions directly — raw-byte parse for panic-freedom (only a
  sub-4-byte payload may reject; §8.4 admits a zero-length data run),
  the positional byte-stability round-trip
  (`write_application(parse_application(x)) == x` for every accepted
  `x`, plus second-pass determinism), and a synthesised-struct writer
  drive (fuzzer-chosen `id` + bounded data run) so the writer sees
  shapes the parser never produces. The 24-bit `BlockHeader::MAX_LENGTH`
  over-ceiling reject branch is left to `metadata::tests` (reproducing
  it would force a ~16 MiB allocation per iteration). ~24 M
  executions, 0 findings (cov plateau at 76 edges — the parser /
  writer surface is small and fully explored). Closes every typed
  metadata-block parser / writer pair under direct fuzz coverage.

- fuzz: round-292 `subframe_decode` target (target 11) —
  structure-aware stress on `subframe::decode_subframe` (RFC 9639
  §9.2: CONSTANT / VERBATIM / FIXED 0..=4 / LPC 1..=32 + the §9.2.7
  residual Rice-partition coder incl. the escape partition). Every
  other target reaches the subframe path only obliquely — the
  demuxer-fed targets gate it behind a valid `fLaC` magic +
  STREAMINFO + CRC-8 header random input almost never threads, and
  the encode-side targets only feed it the narrow subframe shapes
  our own encoder emits. This harness calls `decode_subframe`
  directly with fuzzer-chosen `(block_size, bps)` from the
  production-reachable envelope (`block_size` ∈ 1..=65535 — the §9.1
  16-bit immediate cap; `bps` ∈ 0..=35 covering the rejected `0` /
  `> 32` guard incl. the 33-bit side-channel depth behind the
  2026-05-10 panic) over an attacker-controlled bitstream. Drives
  the unary wasted-bit drain, the signed `qlp_shift` range check,
  the `partition_size <= predictor_order` / `block_size %
  n_partitions` residual maths across every predictor order, and the
  escape partition's raw-bps path. ~1.7 M executions, 0 findings
  (coverage plateau — the reachable subframe surface is fully
  explored, confirming the §9.2 decoder is panic-free under
  adversarial input). Daily fuzz workflow budget re-split across the
  eleven targets.
- fuzz: round-283 `metadata_chain` target (target 10) — whole-chain
  `MetadataChain::parse` / `::write` round-trip under a
  structure-preserving mutation oracle. Three phases per iteration:
  raw bytes asserting parse-Ok ⇒ validate-Ok ⇒ write-Ok ⇒
  equal-re-parse ⇒ byte-stable-re-write; a typed-chain synthesizer
  with writer-acceptable field values but fuzzer-controlled chain
  shape, checked against an independent re-statement of the §8
  stream-level rules (invalid shapes must be refused by both writer
  and parser); and wire-image mutations with exact oracles —
  `last`-flag promotion (typed-prefix truncation), forbidden
  block-type 127 rewrite, PADDING-content corruption (§8.3
  scrub-to-zero), arbitrary single-byte XOR. Daily fuzz workflow
  budget re-split across the ten targets.
- metadata: round-279 typed metadata-chain parser + writer
  (RFC 9639 §8 / §8.1 / §8.2 / §8.5 / §8.6 / §8.8 Table 13). Every
  individual block kind already had a typed parse / write pair, but
  nothing owned the chain level: callers (and our own demuxer, muxer
  and fuzz harness) each re-implemented the header walk, and none of
  the spec's *stream-level* MUSTs had an enforcement point. The new
  `MetadataBlock` enum wraps the existing typed contents per kind —
  `StreamInfo`, `Padding(len)`, `Application`,
  `SeekTable { points, placeholder_count }`, `VorbisComment`,
  `CueSheet`, `Picture`, plus an opaque `Reserved { code, payload }`
  for table-2 codes 7..=126 (RFC 9639 §5 reserves that space for
  future format versions, so a chain editor must carry unknown
  blocks through unchanged, not drop them) — and
  `MetadataChain::parse` / `::write` (free-function aliases
  `parse_metadata_chain` / `write_metadata_chain`) round-trip the
  whole chain. Both directions enforce the chain-level rules: at
  least one block and STREAMINFO first (§8), at most one STREAMINFO
  (§8.2), at most one SEEKTABLE (§8.5), at most one VORBIS_COMMENT
  (§8.6), at most one each of picture types 1 and 2 (§8.8 Table 13 —
  closing the invariant the r274 `PictureType` taxonomy explicitly
  deferred to the chain level), and block-type 127 forbidden
  anywhere (§8.1 Table 2 / §5 forbidden-patterns list). The §8.1
  `last` flag is owned by the chain: the parser stops at the flagged
  block and reports bytes consumed (so callers know where audio
  frames begin; trailing bytes untouched, truncation surfaces as
  `Error::NeedMore` at every boundary), and the writer sets the flag
  on exactly the final block. A SEEKTABLE placeholder tail (§8.5.1)
  is carried as a count so a parse / write cycle keeps the block's
  reserved on-wire size; a (non-conformant) mid-table placeholder
  re-writes at the tail where the spec requires it, and PADDING is
  carried as its length (its content is all-zero by §8.3
  definition). `MetadataChain::validate` is public for checking a
  hand-assembled chain without serialising; `stream_info()` returns
  the mandatory first block. The encoder's `extradata` is exactly a
  marker-less chain and parses directly (regression-tested).
  `StreamInfo` additionally gains `PartialEq` / `Eq` so chains are
  comparable. 11 new unit tests: full eight-block write→parse→write
  byte-stability, last-flag placement, trailing-byte tolerance,
  empty / wrong-first-block rejection on both directions, all three
  singleton-block duplicates, picture-type-1/2 multiplicity (and
  repeated non-unique types accepted), forbidden type 127 on parse,
  a seven-boundary truncation sweep yielding `NeedMore`,
  placeholder-tail size stability, encoder-extradata parse, and
  reserved-block opacity + unwritable-code rejection (127, 128, 200).
- metadata: round-274 typed PICTURE picture-type taxonomy + URI
  accessors (RFC 9639 §8.8 / Table 13). The `Picture` struct stored
  `picture_type` only as a raw `u32`, leaving callers to memorise the
  21 ID3v2-shared type codes and to re-detect the `"-->"` URI sentinel
  by hand. The new `PictureType` enum names every defined code (0
  `Other` through 20 `PublisherLogo`, including the value-17 oddity
  Table 13 retains for compatibility) and folds all reserved /
  out-of-range values into a single `Reserved(u32)` variant that
  preserves the raw code, so `PictureType::from_code(x).code() == x`
  for every `u32` — a parsed value re-emits unchanged. `Picture`
  gains: `picture_kind()` (raw code → typed `PictureType`);
  `is_uri()` / `uri()` (test for and decode the `"-->"` sentinel
  block, returning the URI as `Option<&str>` only when the data bytes
  are valid UTF-8, else `None` with the raw bytes still reachable via
  `data`); the named sentinel constant `Picture::URI_SENTINEL = "-->"`;
  and `Picture::FILE_ICON_DIMENSION = 32` for the type-1 32×32 PNG
  file icon Table 13 fixes. `PictureType::is_unique_per_file()` flags
  the two codes (1, 2) Table 13 limits to one occurrence per file, for
  a muxer assembling a metadata block list. Pure typed-accessor
  surface — no wire-format change, no parser/writer behaviour change.
  9 new unit tests cover the full 0..=20 code round-trip, the named
  rows, reserved-value preservation, the unique-per-file split, the
  URI decode / non-UTF-8-rejection / embedded-image branches, and the
  file-icon dimension constant.
- metadata: round-267 typed CUESHEET sub-field accessors (RFC 9639
  §8.7 / §8.7.1). Until now the `CueSheet` / `CueSheetTrack` structs
  exposed the catalog and ISRC fields only as raw byte arrays
  (`[u8; 128]` / `[u8; 12]`), leaving every caller to re-implement the
  printable-leading-run decode and the all-zero "absent" detection by
  hand. The new convenience accessors fold those spec rules into typed
  methods: `CueSheet::media_catalog()` returns the catalog as
  `Option<&str>` (leading printable-ASCII run up to the first `0x00`
  terminator, `None` when empty or when the run contains a
  non-printable byte — the same character-class rule `write_cuesheet`
  enforces, so any value it returns `Some` for round-trips through a
  write/parse cycle); `CueSheetTrack::isrc_str()` returns the 12-byte
  ISRC as `Option<&str>` (`None` for the all-zero absent encoding or any
  field with embedded NULs / non-printable bytes). Two lead-out-track
  sentinels are named as associated constants
  (`CueSheetTrack::CDDA_LEAD_OUT = 170`,
  `CueSheetTrack::NON_CDDA_LEAD_OUT = 255`) with
  `CueSheetTrack::is_lead_out()` testing a track number against both;
  `CueSheet::lead_out()` returns the trailing terminator track and
  `CueSheet::playable_tracks()` iterates the selectable (non-lead-out)
  tracks, dropping the terminator. Pure typed-accessor surface — no
  wire-format change, no parser/writer behaviour change. 13 new unit
  tests cover the printable-run / absent / non-printable / embedded-NUL
  / high-bit branches plus a write→parse media-catalog round-trip and
  the lead-out / playable-tracks split for both the sentinel-terminated
  and hand-built-without-terminator shapes.
- fuzz: round-259 `frame_header` cargo-fuzz target — ninth target,
  focused stress on `oxideav_flac::frame::parse_frame_header`
  (the RFC 9639 §9.1 frame sync header: sync code + reserved bits +
  blocking strategy + block-size code + sample-rate code + channel
  assignment + sample-size code + UTF-8-shaped coded number +
  CRC-8 footer). The eight prior targets either feed full streams
  through the demuxer (`panic_free_decode` / `decode` /
  `flac_oracle_decode` / `roundtrip`) — only reaching the
  conservative subset of code-tuples the encoder actually emits —
  or exercise one field in isolation (`utf8_varint` only sees the
  coded-number varint outside any framing / CRC). The harness
  builds a syntactically valid header from fuzzer-controlled
  `(blocking, block_size_code, sample_rate_code, channel_code,
  sample_size_code, immediate-tail bytes, coded_number)` and
  asserts `parse_frame_header` recovers the exact
  `(block_size, sample_rate, channels, bps, coded_number,
  blocking_strategy, header_byte_len)` tuple the constructor
  emitted. Every iteration unconditionally sweeps all
  (block-size-code × sample-rate-code × channel-code ×
  sample-size-code) combinations at seven coded-number boundary
  values (0, 0x7F, 0x80, 0xFFFF, 0x10000, 0xFFFFFFFF, 0xF_FFFFFFFF)
  in both fixed and variable blocking — ~24 K well-formed headers
  per iter — so a regression on any code-table entry is reached
  before the fuzzer's corpus has converged. Reserved-code
  rejection is asserted for every spec-marked-reserved value
  (block_size_code 0, sample_rate_code 15, sample_size_code 3 + 7,
  channel_code 11..=15). A 14-position single-bit-flip sweep on
  the constructed header asserts CRC-8 verification stability
  under arbitrary corruption (a CRC that didn't actually verify
  the payload would let an attacker steer the decoder into a
  frame whose tuple disagrees with STREAMINFO — historically a
  class of bugs that leaked uninitialised subframe-buffer memory
  into output PCM). Finally, the raw input is fed to
  `parse_frame_header` at four sliding offsets for panic-freedom
  on hand-crafted bytes (continuation bytes, reserved `0xFF`
  lead, mis-aligned sync prefixes). All construction +
  expected-tuple logic is computed directly from the RFC 9639
  §9.1 tables (Table 11 block size, Table 12 sample rate,
  Table 13 channel assignment, Table 14 sample size) — no
  external implementation consulted.

- bench: round-255 `md5` Criterion harness — fifth depth-mode bench
  binary alongside `decode` / `encode` / `roundtrip` / `crc` / `seek`,
  exercising the FLAC STREAMINFO MD5 validator (RFC 9639 §8.2) on the
  exact `oxideav_flac::md5::{compute, Md5}` API the encoder calls
  once per audio frame in `feed_md5`. Every prior round folded the
  MD5 cost into the wider encode wall time; the fresh harness pins
  the per-byte hash throughput as a standalone A/B target so future
  speed work on `process_block` (parallel 4-message-block mixing,
  SIMD round mixing, tail-pad micro-optimisation) has a stable
  baseline. Seven scenarios: `md5_oneshot_{4k,16k,64k}` (slice
  helper, mirroring per-frame payloads — 4 KiB mono S16,
  16 KiB stereo S16, 64 KiB multichannel S24);
  `md5_streaming_chunked_64k` (16-chunk `Md5::update` schedule with
  two deliberately off-aligned splits, matching the encoder's
  per-frame call pattern); `md5_streaming_byte_4k` (worst-case
  single-byte feed); `md5_empty` + `md5_short` (RFC 1321 §A.5
  pad-branch coverage). Every scenario asserts the digest matches
  the oneshot reference inside the timed iteration so a regression
  that silently produced a different hash would surface as a panic
  rather than a misleading speedup. Buffers synthesised in-bench
  from the same `0xCAFE_F00D` xorshift32 seed as the neighbouring
  `crc` / `encode` / `decode` / `roundtrip` benches so numbers stay
  comparable across the bench surface. No `docs/` fixtures, no
  external files, no external `md5` reference implementation
  consulted. Reference numbers on an M-series laptop (release,
  Darwin aarch64): `md5/oneshot/4k` ≈ 5.2 µs (~780 MiB/s),
  `md5/oneshot/16k` ≈ 20.8 µs (~770 MiB/s), `md5/oneshot/64k` ≈
  83 µs (~775 MiB/s — confirming the inner loop is bandwidth-bound
  rather than setup-bound), `md5/streaming-chunked/64k` ≈ 83 µs
  (matches oneshot to noise — chunked feed does not regress the
  in-buffer carry path), `md5/streaming-byte/4k` ≈ 10 µs (~400
  MiB/s — about 2× the oneshot cost per byte from per-call dispatch
  overhead), `md5/oneshot/empty` ≈ 67 ns, `md5/oneshot/3b` ≈
  70 ns. Run with `cargo bench -p oxideav-flac --bench md5`.

- metadata: round-252 typed `StreamInfo` writer — `StreamInfo::write`
  (with a `metadata::write_streaminfo` free-function alias for
  symmetry with the other top-level writers) serialises a
  `StreamInfo` back into the exact 34-byte RFC 9639 §8.1 block
  payload, round-tripping through `StreamInfo::parse` bit-for-bit.
  Closes the parser-only gap noted in the round-241 entry: every
  RFC 9639 §8 block type whose payload has a spec-defined
  structural split (STREAMINFO / SEEKTABLE / VORBIS_COMMENT /
  CUESHEET / PICTURE / PADDING / APPLICATION) now has a parser +
  writer pair. The writer enforces every wire-format ceiling its
  packed-bit fields can carry: `sample_rate` against
  `MAX_SAMPLE_RATE` (20-bit, 1_048_575), `total_samples` against
  `MAX_TOTAL_SAMPLES` (36-bit), `channels` against
  `MIN_CHANNELS..=MAX_CHANNELS` (3-bit `channels − 1`, range 1..=8),
  `bits_per_sample` against `MIN_BPS..=MAX_BPS` (5-bit `bps − 1`,
  range 1..=32), and both frame-size fields against `MAX_FRAME_SIZE`
  (24-bit). New `PAYLOAD_LEN` const advertises the spec's 34-byte
  fixed length so callers walking the metadata-block chain do not
  hard-code the literal. The encoder's internal
  `build_streaminfo_metadata_block` now delegates the 34-byte
  payload pack to this typed writer so both code paths share the
  same packer; the wire bytes the encoder hands the muxer are
  byte-identical to the pre-r252 output. Eleven unit tests cover
  the round trip, the free-function alias agreement, the matching
  payload-length, MD5 preservation, every per-field boundary
  (low + high), and the canonical small fixture's packed
  sr/ch/bps/total bytes match the hand-built reference.
- metadata: round-244 typed `Application` accessor + writer —
  `metadata::parse_application` and `metadata::write_application`
  round-trip a RFC 9639 §8.4 APPLICATION block payload through a new
  `Application` struct carrying the 32-bit IANA-registered
  application ID (RFC 9639 §12.2) split from the opaque
  application-defined byte run. Completes the typed-accessor
  coverage of RFC 9639 §8 — every block type whose payload has a
  spec-defined structural split (STREAMINFO / SEEKTABLE /
  VORBIS_COMMENT / CUESHEET / PICTURE / PADDING / APPLICATION) now
  has a parser + writer pair. Parser rejects payloads shorter than
  the mandatory 4-byte ID header without slicing past the buffer;
  writer rejects payloads whose total size would overflow the
  enclosing metadata-block-header's 24-bit length ceiling
  (`BlockHeader::MAX_LENGTH`). Six unit tests cover the canonical
  round trip, the spec-legal zero-byte data run, short-buffer
  rejection across every length from 0 to 3, big-endian byte-order
  emission, the writer's ceiling check (boundary + overflow), and
  that the parser does not alias the input buffer.
- metadata: round-241 typed `VorbisComment` accessor + writer —
  `metadata::parse_vorbis_comment` and `metadata::write_vorbis_comment`
  round-trip a RFC 9639 §8.6 VORBIS_COMMENT block payload through a
  new `VorbisComment` struct (`vendor: String`,
  `comments: Vec<(String, String)>`). Fills the last typed-accessor
  gap in `metadata::` — every well-known FLAC metadata block now has a
  symmetric parser + writer pair (STREAMINFO is parser-only; the
  encoder owns its serialisation). The container demuxer's existing
  internal Vorbis-comment projection continues to populate the
  framework's narrower `Vec<(String, String)>` metadata surface
  (lowercases names, trims values, drops empty fields); the typed
  accessor adds a higher-fidelity path for callers needing the
  vendor string, original-case names, value whitespace, repeated
  identically-named fields (e.g. multi-artist), or byte-stable
  re-emission. The parser enforces every §8.6 invariant (UTF-8
  vendor, UTF-8 fields, printable-ASCII names excluding `=`,
  mandatory `=` separator per field, no trailing slack after the
  declared field count, no integer overflow on length-prefix
  arithmetic). The writer mirrors the same character-class check so
  a hand-constructed value cannot round-trip through a writer /
  parser pair that disagree. A `VorbisComment::get(name)`
  convenience performs the §8.6 case-insensitive field lookup
  (`name.eq_ignore_ascii_case(...)`). 21 unit tests cover the
  minimal payload, real-shape title/artist/album, case-insensitive
  lookup, empty-value preservation, whitespace preservation,
  arbitrary UTF-8 in values (`Été`, CJK, emoji), every parser
  rejection path (bad name byte, missing `=`, truncated vendor,
  overflowing field length, invalid UTF-8 in vendor or field,
  trailing garbage, short payload), writer rejection of bad names
  (DEL `0x7F`, embedded `=`), printable-ASCII boundary acceptance
  (`0x20` space and `0x7E` tilde), byte-stable deterministic
  writer output, and duplicate-field-name preservation
  (multi-artist albums). The `metadata_walker` fuzz harness gains
  VORBIS_COMMENT coverage on both its walker path (BlockType
  dispatch now hits all five typed parsers) and its direct typed
  surface path (Path 2's parser ∘ writer ∘ parser invariant now
  includes VORBIS_COMMENT alongside SEEKTABLE / CUESHEET /
  PICTURE / PADDING); the previous "VORBIS_COMMENT not exposed as
  typed parsers" skip note is now stale and replaced by a
  byte-length-equality assertion that mirrors the writer's
  pre-allocation math against the parser-consumed byte count.
- fuzz: round-237 `utf8_varint` target — eighth `cargo-fuzz`
  harness covering `oxideav_flac::bits_ext::{read_utf8_u64,
  write_utf8_u64}`, the FLAC UTF-8-shaped variable-length integer
  of RFC 9639 §9.1.5 (frame-header `sample_number` under
  variable-blocking, `frame_number` under fixed-blocking). Every
  host-pipeline target exercises the reader only at the value the
  encoder happened to emit (one varint per emitted frame at one
  value per frame); none of the prior seven harnesses sweep the
  value space, so a divergence between writer and reader at any
  of the seven lead-byte-range boundaries (`0xxxxxxx` /
  `110xxxxx` / `1110xxxx` / `11110xxx` / `111110xx` /
  `1111110x` / `11111110`) would silently corrupt every coded
  number near that boundary — a 16-bit `sample_number` bug would
  surface only on variable-blocking streams whose first frame
  lands after sample ~65 536 and would break seek-table
  consistency at every later frame. The harness covers four
  contracts: writer→reader round-trip equivalence
  (`write_utf8_u64(v) -> bytes -> read_utf8_u64(bytes) == Ok(v)`),
  per-bit-width byte-length stability (the emitted byte count
  must fall inside the 1/2/3/4/5/6/7-byte schedule documented in
  §9.1.5), a 14-entry boundary sweep that runs unconditionally
  every iteration (0, 0x7F, 0x80, 0x7FF, 0x800, 0xFFFF, 0x10000,
  0x1F_FFFF, 0x20_0000, 0x3FF_FFFF, 0x400_0000, 0x7FFF_FFFF,
  0x8000_0000, (1u64<<36)-1) so a regression at an edge cannot
  escape a session that never produces the exact boundary value
  through the value-driven sweep, and reader-robustness against
  arbitrary input bytes (the parser must always return a
  `Result` — never panic, never read past the slice — for any
  byte sequence the fuzzer can produce, including continuation
  bytes 0x80..=0xBF at the lead-byte position and the reserved
  0xFF sentinel that the writer never produces). Successful
  parses re-encode and re-parse; the second parse must match
  the first (canonical-form stability). Ran 2 000 000 iterations
  locally with zero crashes / panics / hangs (release, Darwin
  aarch64). With this target the eight-target rotation now
  covers: decoder pipeline (`panic_free_decode`, `decode`),
  encode/decode self-consistency (`roundtrip`), cross-decode
  validation, metadata chain walks (`metadata_walker`),
  encoder-construction extradata (`encoder_options`), MD5
  streaming (`md5_streaming`), and frame-header varint
  (`utf8_varint`).
- fuzz: round-232 `md5_streaming` target — seventh `cargo-fuzz`
  harness covering the one surface the prior six targets
  (`panic_free_decode`, `roundtrip`, `flac_oracle_decode`,
  `decode`, `metadata_walker`, `encoder_options`) only swept
  indirectly: the streaming-vs-oneshot equivalence of
  `oxideav_flac::md5::Md5`. The encoder feeds the running MD5 a
  single deterministic schedule (one `update` per audio frame);
  none of the existing targets stress the chunk-schedule surface
  of `Md5::update` / `Md5::finalize`. A divergence between the
  streaming API and the oneshot `md5::compute()` helper would
  silently produce a STREAMINFO whose 128-bit MD5 signature (RFC
  9639 §8.2) does not match the decoded PCM bytes a verifier
  would recompute over the stream — every emitted `.flac` file
  would carry an `md5sum`-mismatch under round-trip verification.
  The risk surface is concentrated in `Md5::update`'s
  block-boundary arithmetic: the leftover-buffer drain when an
  incoming slice straddles the 64-byte block boundary, the
  64-bit bit-count accumulator's `bits_hi` / `bits_lo` split with
  overflow-add into `bits_hi`, `finalize`'s pad-length selector
  (`56 - buffer_len` vs `120 - buffer_len`) at the 55 / 56 / 63
  / 64 / 65 edges, and `process_block`'s 16-word little-endian
  load. The harness carves the input into a 4-byte schedule
  descriptor (`n_chunks_sel`, `chunk_size_seed`, `extra_update_sel`,
  `default_sel`) and a payload tail capped at 8 KiB. Every fuzz
  iteration drives the streaming API over seven distinct chunk
  schedules — even split, xorshift-irregular, byte-at-a-time
  (bounded ≤ 512 B), zero-length-interlude, `Md5::default()`
  parity, 64-byte block-aligned, prime-stride-7 — and asserts
  byte-for-byte equality of every schedule's digest against the
  oneshot reference. Re-asserts the RFC 1321 §A.5 empty-input
  vector (`d41d8cd9 8f00b204 e9800998 ecf8427e`) on every
  iteration so a finalize-pad regression cannot slip past a
  fuzzer that never produces a zero-length payload. Ran 7 960 199
  iterations against the existing corpus locally with zero
  crashes / panics / hangs (release, Darwin aarch64). Together
  with the round-228 `encoder_options` target this closes the
  STREAMINFO-production surface: encoder-construction extradata
  (`encoder_options`) + the 128-bit signature field's hashing
  primitive (`md5_streaming`).
- fuzz: round-228 `encoder_options` target — sixth `cargo-fuzz`
  harness exercising `make_encoder_with_options` itself, the one
  encoder-construction surface the existing five targets
  (`panic_free_decode`, `roundtrip`, `flac_oracle_decode`,
  `decode`, `metadata_walker`) only sweep indirectly. Every fuzz
  iteration carves an 8-byte prefix into a format / channel /
  sample-rate / padding-mode / padding-seed selector tuple,
  builds a `FlacEncoderOptions` whose `padding_bytes` lands in one
  of five regimes (`None`, `Some(0)`, `Some(small)`,
  `Some(near-cap)`, `Some(over-cap)`), and calls
  `make_encoder_with_options`. The channel byte is the raw u8 so
  most values fall outside the `1..=8` guard; the sample-rate
  selector includes the pathological `0`, `1`, and `u32::MAX`
  edges. Every successful construction has its `extradata` chain
  walked via `BlockHeader::parse` and asserted against the RFC
  9639 §6 / §8.2 / §8.6 invariants — STREAMINFO is block 0,
  payload length is fixed at 34, the `last` flag flips iff
  PADDING follows, and any PADDING block carries `last=true`, the
  caller-requested length, and an all-zero payload with no
  trailing slack. When the params round-trip cleanly, the
  remaining input bytes (capped at 2 KiB) feed a single
  `send_frame` -> `flush` -> `receive_packet*` cycle and the
  post-flush extradata is re-verified — the STREAMINFO
  repopulation (MD5 / min-max frame size / total samples) must
  not change the chain length nor corrupt the PADDING tail.
  Contract under test: every reachable combination of params +
  options returns `Result` and never panics, never overflows in a
  debug build, and the extradata invariants survive the flush
  rebuild. The harness ran 50 000 iterations against an empty
  corpus locally with zero crashes / panics / hangs (release,
  Darwin aarch64). Together with the round-200 `metadata_walker`
  target this closes the metadata-block surface: parsers (decode
  direction), writers via the chain walker (`metadata_walker`),
  encoder-construction extradata production (`encoder_options`).
- bench: round-223 `seek` harness — four scenarios measuring the
  FLAC demuxer's `seek_to` hot path (binary search across the
  parsed `Vec<SeekPoint>` per RFC 9639 §8.5.1 + `Cursor::seek` +
  `scan.reset_to` + post-seek sync-code re-walk). Every prior
  bench measured forward-only decode/encode work; the seek path
  had never been measured in isolation, leaving a regression to
  linear scan or a `Cursor::seek` slowdown undetectable from the
  existing `decode` / `encode` / `roundtrip` numbers. Two
  warm-table scenarios (`iter_batched` so STREAMINFO + SEEKTABLE
  parse happens outside the timed region) cover small (1 s of
  audio → ~11 SEEKPOINTs) and large (16 s → ~173 SEEKPOINTs)
  tables; a `seek_then_next_packet` scenario measures the
  user-visible seek + frame-header walk cost; a
  `linear_walk_baseline` gives an A/B reference for the
  per-packet demuxer throughput without ever calling `seek_to`.
  Baseline numbers (release, Darwin aarch64): seek_only ~14.6 ns
  per call at N=11, ~19.5 ns at N=173 (ratio 1.34× —
  log2(173)/log2(11) ≈ 2.1, so the binary search is fast enough
  that the constant-cost `Cursor::seek` + `reset_to` overhead
  dominates the comparison loop); seek_then_next_packet
  ~2.47 µs / call (the post-seek sync-code walk into the next
  frame dominates); linear_walk_baseline ~1.36 G samples/s
  demuxer throughput. Fixtures are built via the production
  `oxideav_flac::encoder::make_encoder` with a SEEKTABLE
  injected through `oxideav_flac::metadata::write_seektable` +
  `BlockHeader::write_into`; no committed `.flac` files, no
  external library — every byte is synthesised from a
  deterministic xorshift seed. Gives the next round's seek-path
  tweaks (a zero-copy Cursor-replacement, a packed 16-byte
  SeekPoint layout, a fast-path that returns early when the
  cursor is already at the resolved offset) a stable A/B
  baseline.
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
