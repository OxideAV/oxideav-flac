# oxideav-flac round-147 profile run

This directory holds CPU flamegraphs captured against the
`examples/profile_flac.rs` driver introduced in round 137. The
intent: persist a real on-disk profile artefact so future encoder /
decoder rounds have a stable visual baseline to compare their
algorithmic tweaks against, without having to re-discover the
capture recipe.

## Artefacts

| File              | Mode      | Driver iterations              | Total samples |
| ----------------- | --------- | ------------------------------ | ------------: |
| `encode.svg`      | encode    | 25 × 4 scenarios (~64 s wall)  | 137 548       |
| `decode.svg`      | decode    | 500 × 4 scenarios (~1.5 s wall)|   4 080       |
| `roundtrip.svg`   | roundtrip | 25 × 4 scenarios (~67 s wall)  | 139 962       |

The matching `.folded` files are the folded-stacks text format
flamegraph generators consume — one stack per line, frames
separated by `;` from root to leaf, followed by a single integer
sample count. The SVGs can be regenerated from them without
rerunning the sampler.

Four scenarios per mode, mirroring the criterion benches and the
profile driver:

* `mono/s16/44k1/1s`
* `stereo/s16/44k1/1s`
* `stereo/s24/48k/500ms`
* `6ch/s16/48k/250ms`

PCM input is synthesised in the driver from a deterministic
xorshift32 seed so the profile is reproducible on any host.

## Headline finding

`oxideav_flac::encoder::best_partition` accounts for roughly
**61 %** of encode self-samples (83 717 / 137 548). It is the
inner loop of the per-subframe Rice partition-order search (round
129; `src/encoder.rs:976`) — for each candidate partition order
`0..=8` and each Rice method (4-bit and 5-bit parameter), the
encoder splits the residual into `2^order` partitions and computes
the optimal Rice `k` for each. The per-call cost is dominated by
the `best_rice_params` closed-form estimate (round 143; cut the
per-partition cost ~5× already), plus a residual-bits accumulator
that the round-143 commit left as a tight scalar loop.

Secondary self-sample sinks on the encode path:

* `BitWriter::write_unary` — 8 290 (~6 %), called from
  `encode_rice_residual` per residual symbol on each surviving
  candidate plan. Variable-length per-symbol; this is the cost of
  serialising the chosen plan after the search picked it.
* `best_subframe_with_wasted` — 11 293 (~8 %), the per-subframe
  driver that iterates CONSTANT + FIXED 0..=4 + LPC 1..=12 +
  VERBATIM and keeps the smallest. The cost here is the dispatch +
  per-plan setup; the actual per-plan computation is dominated by
  the residual + partition costs above.

The remaining ~25 % is split between allocator paths
(`_xzm_free`, `xzm_malloc`, `__bzero`) — primarily the
`Vec<i32>` residual buffer being re-allocated per subframe — and
small contributions from `BitWriter::write_bits` and the LPC
matrix Levinson-Durbin solver. Pre-allocating + reusing a single
residual buffer across subframes (or across the whole-frame
candidate sweep) is the obvious next optimisation handle and was
the round-147 finding feeding into the encoder TODO list.

The decode path is dominated entirely by
`<FlacDecoder>::receive_frame` — there is no measurable hot inner
that would benefit from algorithmic changes. The decoder is
already hundreds-of-x realtime per the example driver's reference
numbers; further gains here would have to come from SIMD on the
LPC restore loop, which is not a one-round change.

## Recipe (reproduce)

These artefacts were captured on macOS arm64 (no root needed —
`samply` uses `task_for_pid` after self-signing on macOS, no
DTrace, no `perf`). On Linux substitute `perf record` (root
required) or `samply record` (perf paranoid level permitting).

```text
# 1. Build the profile driver in release with debuginfo.
cargo build --release --example profile_flac

# 2. Sample. --unstable-presymbolicate writes a sidecar syms file
#    so the JSON profile resolves to source symbols even after the
#    binary's debug-info is gone.
samply record --unstable-presymbolicate --save-only \
    -o encode.json.gz \
    -r 1997 \
    -- target/release/examples/profile_flac encode 25

samply record --unstable-presymbolicate --save-only \
    -o decode.json.gz \
    -- target/release/examples/profile_flac decode 500

samply record --unstable-presymbolicate --save-only \
    -o roundtrip.json.gz \
    -r 1997 \
    -- target/release/examples/profile_flac roundtrip 25

# 3. Convert samply's processed-profile JSON to Brendan-Gregg
#    folded stacks, then SVG. The helper is committed alongside
#    the SVGs so the conversion step survives outside the sampler.
python3 profile/samply_to_folded.py encode.json.gz encode.json.syms.json \
    > profile/encode.folded
inferno-flamegraph \
    --title "oxideav-flac encode (round 147)" \
    --subtitle "samply 1997Hz, 25 iters × 4 scenarios" \
    < profile/encode.folded > profile/encode.svg

# Repeat for decode + roundtrip.
```

The intermediate `*.json.gz` + `*.json.syms.json` files are NOT
committed — they are a samply implementation detail and the
folded-stack files are the stable interchange format. If a future
round wants a fresh profile, regenerate the SVGs from new
folded-stack outputs and overwrite the existing SVGs in this
directory.

## Wall

Captured without consulting any external library source. Samply
is a sampling profiler; it only observes the OxideAV binary at
runtime. The captured stacks reference only the project's own
modules + stdlib + macOS runtime (`libsystem_*`, `dyld`); no
third-party FLAC implementation participated in the run.
