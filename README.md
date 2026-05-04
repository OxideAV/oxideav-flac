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
  orders 0..=4, LPC orders 1..=8 (Levinson-Durbin on a Welch-windowed
  autocorrelation with 12-bit coefficient quantisation) and VERBATIM,
  and keeps the smallest. The output remains fully valid and fully
  lossless — any compliant decoder (including this crate's own) will
  recover the original PCM bit-exactly.
- **Wasted bits per sample**: detected per subframe (largest `k`
  such that every sample is divisible by `2^k`) and folded into the
  spec's wasted-bits unary header. This shaves the trailing-zero
  payload off upsampled or low-amplitude content (e.g. a 16-bit
  stream that only ever uses its top 8 bits).
- **Residual coding**: partitioned Rice with partition order 0,
  exhaustive choice between Rice methods 0 and 1 per subframe, and
  escape partitions for samples that can't be Rice-coded cheaply.
- **Block size**: 4096 samples per frame (fixed-blocking strategy).
- **Metadata**: emits a STREAMINFO metadata header in `extradata`. An
  MD5 signature of the PCM input is computed during encode and written
  into the block at `flush()` time, along with the observed min/max
  frame size and the total sample count. Fetch the final
  `enc.output_params().extradata` after flushing to feed a muxer.

## Native container

- **Demuxer**: parses `fLaC` magic + metadata block chain
  (STREAMINFO, VORBIS_COMMENT, SEEKTABLE, PICTURE, plus ID3v2 tags
  prepended by some taggers), emits one packet per frame via a
  CRC-verified sync-code scanner.
- **Muxer**: writes `fLaC` + preserved metadata blocks + frame packets.
  Packets produced by the encoder pass straight through.
- **Seeking**: SEEKTABLE-driven byte-offset seek (`seek_to(pts)` lands
  on the nearest prior seek point). Files without a SEEKTABLE return
  `Error::Unsupported` on seek.
- **Metadata surfaces**: Vorbis-comment key/value pairs and attached
  pictures (FLAC PICTURE block + ID3v2 APIC fallback).

## Codec / container IDs

- Codec: `"flac"`; accepted sample formats `U8`, `S16`, `S24`, `S32`.
- Container: `"flac"`, matches `.flac` / `.fla` by extension and the
  `fLaC` magic bytes (including files prefixed with an ID3v2 tag).

## License

MIT — see [LICENSE](LICENSE).
