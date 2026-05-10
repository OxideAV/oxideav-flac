#![no_main]

//! Fuzz: arbitrary bytes → FLAC demuxer + decoder pipeline.
//!
//! Contract: every public API surface MUST return a `Result` for
//! malformed input — never panic, never abort. This target feeds raw
//! libfuzzer bytes through two paths:
//!
//! 1. Through the **container demuxer** (which expects `fLaC` magic
//!    or an ID3v2 prefix). Any panic from `next_packet` or downstream
//!    decoding fails the harness.
//! 2. As a **raw codec packet** with synthetic STREAMINFO extradata
//!    (so the decoder is configured but the packet bytes are
//!    arbitrary). Same panic-freedom contract.
//!
//! Errors from `send_packet` / `receive_frame` / `next_packet` are
//! fine — the trait is defined to surface them. We only fail on a
//! Rust-level panic.

use libfuzzer_sys::fuzz_target;
use oxideav_core::{
    CodecId, CodecParameters, ContainerRegistry, NullCodecResolver, Packet, TimeBase,
};

fuzz_target!(|data: &[u8]| {
    // -----------------------------------------------------------------
    // Path 1: container demuxer + decoder.
    //
    // Build a one-shot ContainerRegistry, register the FLAC demuxer
    // factory, then ask it to open a Cursor over `data`. Drain up to
    // 32 packets; any panic escapes the harness. Most random byte
    // streams won't pass the `fLaC` magic check — that's fine.
    // -----------------------------------------------------------------
    {
        let mut reg = ContainerRegistry::new();
        oxideav_flac::register_containers(&mut reg);
        let resolver = NullCodecResolver;
        let cursor: Box<dyn oxideav_core::ReadSeek> = Box::new(std::io::Cursor::new(data.to_vec()));
        if let Ok(mut demux) = reg.open_demuxer("flac", cursor, &resolver) {
            // Pull the first stream's params so we can build a matching
            // decoder.
            let streams = demux.streams().to_vec();
            let mut dec = streams
                .first()
                .and_then(|s| oxideav_flac::decoder::make_decoder(&s.params).ok());
            for _ in 0..32 {
                let pkt = match demux.next_packet() {
                    Ok(p) => p,
                    Err(_) => break,
                };
                if let Some(d) = dec.as_mut() {
                    let _ = d.send_packet(&pkt);
                    // Drain whatever frames are ready.
                    for _ in 0..4 {
                        if d.receive_frame().is_err() {
                            break;
                        }
                    }
                }
            }
        }
    }

    // -----------------------------------------------------------------
    // Path 2: raw decoder fed arbitrary packet bytes with synthetic
    // STREAMINFO extradata. Carves a minimal valid STREAMINFO block
    // out of the input prefix so the decoder can be constructed; the
    // packet bytes after that are whatever the fuzzer threw at us.
    // -----------------------------------------------------------------
    if data.len() >= 8 {
        let extradata = synth_streaminfo(data);
        let mut params = CodecParameters::audio(CodecId::new("flac"));
        params.channels = Some(2);
        params.sample_rate = Some(44_100);
        params.extradata = extradata;
        if let Ok(mut dec) = oxideav_flac::decoder::make_decoder(&params) {
            let pkt = Packet::new(0, TimeBase::new(1, 44_100), data.to_vec());
            let _ = dec.send_packet(&pkt);
            for _ in 0..4 {
                if dec.receive_frame().is_err() {
                    break;
                }
            }
        }
    }
});

/// Build a minimal METADATA_BLOCK header + STREAMINFO payload (4 + 34
/// bytes) marked LAST. Field values are forced to a sane prefix so
/// `make_decoder` can construct the decoder; the actual packet bytes
/// are independent. We deliberately don't depend on the encoder for
/// this since we want to exercise code paths the encoder never
/// produces (e.g., 8 bps, 32 channels post-clamp, exotic sample rates).
fn synth_streaminfo(seed: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + 34);
    // Header: last=1, type=0 (STREAMINFO), length=34.
    out.push(0x80);
    out.extend_from_slice(&[0, 0, 34]);
    // Payload (34 bytes):
    //   2 B  min_block_size
    //   2 B  max_block_size
    //   3 B  min_frame_size
    //   3 B  max_frame_size
    //   20 b sample_rate
    //   3 b  channels - 1
    //   5 b  bps - 1
    //   36 b total_samples
    //   16 B md5 signature
    out.extend_from_slice(&4096u16.to_be_bytes()); // min_block
    out.extend_from_slice(&4096u16.to_be_bytes()); // max_block
    out.extend_from_slice(&[0, 0, 0]); // min_frame
    out.extend_from_slice(&[0, 0, 0]); // max_frame
                                       // Pack: sr=44100 (20 b), ch=2-1=1 (3 b), bps=16-1=15 (5 b), total=0 (36 b).
                                       // 44100 == 0xAC44; 20 bits.
    let sr: u32 = 44_100;
    let ch_minus_1: u32 = 1;
    let bps_minus_1: u32 = 15;
    let total: u64 = 0;
    let mut packed: u64 = 0;
    packed |= (sr as u64 & 0xFFFFF) << 44;
    packed |= (ch_minus_1 as u64 & 0x7) << 41;
    packed |= (bps_minus_1 as u64 & 0x1F) << 36;
    packed |= total & 0xF_FFFF_FFFF;
    out.extend_from_slice(&packed.to_be_bytes());
    // MD5 signature — derived from the seed for variety, but unused
    // for decode correctness.
    let mut md5 = [0u8; 16];
    for (i, b) in seed.iter().take(16).enumerate() {
        md5[i] = *b;
    }
    out.extend_from_slice(&md5);
    out
}
