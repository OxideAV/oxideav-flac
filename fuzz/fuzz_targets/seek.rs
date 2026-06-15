#![no_main]

//! Fuzz target 13 (round 318): panic-freedom of the FLAC demuxer's
//! seek path — `<FlacDemuxer as Demuxer>::seek_to` followed by packet
//! drain.
//!
//! ## Why a dedicated target
//!
//! Every existing host-pipeline target (`panic_free_decode`, `decode`)
//! drains the demuxer linearly with `next_packet` only; none of them
//! ever calls `seek_to`. The `benches/seek.rs` harness *does* drive
//! `seek_to`, but only over a **valid** encoder-produced stream with a
//! strictly-ascending, in-range SEEKTABLE (built by `write_seektable`,
//! which enforces the §8.5.1 sorted/unique MUSTs). That leaves the
//! demuxer's seek arithmetic completely unfuzzed against adversarial
//! input:
//!
//!   * `seek_points.partition_point(|sp| sp.sample_number <= target)`
//!     over a SEEKTABLE the *parser* accepted but whose points are
//!     unsorted, duplicated, placeholder (`sample_number ==
//!     0xFFFF_FFFF_FFFF_FFFF`, RFC 9639 §8.5.2), or zero — `parse_seektable`
//!     does not re-sort, so the binary-search predicate sees whatever
//!     the wire bytes carried.
//!   * `self.input.seek(SeekFrom::Start(self.first_frame_offset +
//!     byte_offset))` — an unchecked `u64 + u64` against a fuzzer-chosen
//!     `byte_offset` that can be `u64::MAX` (overflow candidate in a
//!     debug build) or simply far past EOF (the post-seek `next_packet`
//!     scan must recover, not panic).
//!   * `self.scan.reset_to(anchor_sample)` re-anchoring the frame
//!     scanner to an arbitrary sample number, then resuming the
//!     CRC-verified frame walk from a byte offset that may land in the
//!     middle of a frame, in metadata, or past the end.
//!
//! ## Contract under test
//!
//! Pure panic-freedom. `seek_to` returns `Ok(landed_sample)` /
//! `Err(unsupported | invalid | io)`; the following `next_packet` /
//! `send_packet` / `receive_frame` calls return `Result`. No path may
//! panic, abort, integer-overflow (debug), or index out of bounds for
//! any `(stream bytes, seek target sequence)` the fuzzer supplies.
//!
//! ## Two construction paths per iteration
//!
//! **Path A — raw arbitrary bytes.** Open the demuxer directly on the
//! fuzz input and fire a sweep of seek targets at it. Most inputs fall
//! out at the `fLaC` magic check or hit the "no SEEKTABLE" /
//! "stream index out of range" early returns — that exercises the
//! error arms cheaply. The rare input that *does* carry a parseable
//! STREAMINFO + SEEKTABLE threads the real binary-search path.
//!
//! **Path B — valid stream + fuzzer-planted SEEKTABLE.** Synthesise a
//! genuinely decodable mono stream from a fixed PCM tail, then splice
//! in a SEEKTABLE whose seekpoints are **hand-assembled from fuzzer
//! bytes** (18 bytes each: 8 B sample_number + 8 B offset + 2 B
//! frame_samples) so they bypass `write_seektable`'s ordering guard and
//! reach `seek_to` unsorted / out-of-range / placeholder. This is the
//! only way to reach the `first_frame_offset + byte_offset` arithmetic
//! and the post-seek scanner-recovery walk with adversarial values,
//! because the parser accepts any 18-byte-aligned point run verbatim.

use libfuzzer_sys::fuzz_target;
use oxideav_core::{
    AudioFrame, CodecId, CodecParameters, ContainerRegistry, Frame, NullCodecResolver, Packet,
    ReadSeek, SampleFormat,
};
use oxideav_flac::metadata::{BlockHeader, BlockType, FLAC_MAGIC};

/// Seek targets derived from the fuzz input. Always includes the fixed
/// boundary values (negative, zero, `i64::MAX`) so the
/// `pts.max(0) as u64` clamp and the `partition_point` extremes are hit
/// on every iteration regardless of how short the input is, plus a few
/// fuzzer-controlled mid-range values pulled from 8-byte windows.
fn seek_targets(data: &[u8]) -> Vec<i64> {
    let mut targets = vec![i64::MIN, -1, 0, 1, i64::MAX];
    // Up to four fuzzer-chosen targets from little-endian 8-byte windows.
    for chunk in data.chunks(8).take(4) {
        let mut buf = [0u8; 8];
        buf[..chunk.len()].copy_from_slice(chunk);
        targets.push(i64::from_le_bytes(buf));
    }
    targets
}

/// Drive a constructed demuxer through the seek sweep. After each
/// `seek_to` (whatever it returns) drain a bounded number of packets so
/// the post-seek scanner-recovery path runs; feed each packet to a
/// decoder so the seek landing point also flows through subframe decode.
fn exercise_seeks(
    demux: &mut Box<dyn oxideav_core::Demuxer>,
    dec: &mut Option<Box<dyn oxideav_core::Decoder>>,
    data: &[u8],
) {
    // `seek_to` only accepts stream index 0; also fire one out-of-range
    // index per iteration to keep the index-guard error arm warm.
    let _ = demux.seek_to(1, 0);

    for target in seek_targets(data) {
        // Result intentionally ignored — Ok(landed) and every Err arm
        // are in-contract; only a panic fails the harness.
        let _ = demux.seek_to(0, target);
        for _ in 0..4 {
            let pkt = match demux.next_packet() {
                Ok(p) => p,
                Err(_) => break,
            };
            if let Some(d) = dec.as_mut() {
                let _ = d.send_packet(&pkt);
                for _ in 0..2 {
                    if d.receive_frame().is_err() {
                        break;
                    }
                }
            }
        }
    }
}

/// Build a matching decoder for the demuxer's first stream, if any.
fn make_decoder_for(demux: &dyn oxideav_core::Demuxer) -> Option<Box<dyn oxideav_core::Decoder>> {
    demux
        .streams()
        .first()
        .and_then(|s| oxideav_flac::decoder::make_decoder(&s.params).ok())
}

fuzz_target!(|data: &[u8]| {
    // -----------------------------------------------------------------
    // Path A: raw arbitrary bytes straight into the demuxer + seek sweep.
    // -----------------------------------------------------------------
    {
        let mut reg = ContainerRegistry::new();
        oxideav_flac::register_containers(&mut reg);
        let resolver = NullCodecResolver;
        let cursor: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(data.to_vec()));
        if let Ok(mut demux) = reg.open_demuxer("flac", cursor, &resolver) {
            let mut dec = make_decoder_for(demux.as_ref());
            exercise_seeks(&mut demux, &mut dec, data);
        }
    }

    // -----------------------------------------------------------------
    // Path B: synthesised valid stream + fuzzer-planted SEEKTABLE so the
    // binary-search + `first_frame_offset + byte_offset` + scanner-reset
    // path runs with adversarial seekpoint values the §8.5.1-enforcing
    // `write_seektable` would never emit.
    // -----------------------------------------------------------------
    if let Some(stream) = synth_stream_with_planted_seektable(data) {
        let mut reg = ContainerRegistry::new();
        oxideav_flac::register_containers(&mut reg);
        let resolver = NullCodecResolver;
        let cursor: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(stream));
        if let Ok(mut demux) = reg.open_demuxer("flac", cursor, &resolver) {
            let mut dec = make_decoder_for(demux.as_ref());
            exercise_seeks(&mut demux, &mut dec, data);
        }
    }
});

/// Encode a small, deterministic mono S16 stream and re-assemble it with
/// a SEEKTABLE whose seekpoints are taken **verbatim from the fuzz
/// input** (18 bytes per point), bypassing `write_seektable`'s ordering
/// / range checks. Returns `None` if the input is too short to plant at
/// least one point (so Path A still carries the iteration).
///
/// The PCM tail is fixed (not fuzzer-derived) because the goal is a
/// *valid, decodable* frame stream behind an *invalid* seek index — the
/// fuzzer's leverage is the seekpoint bytes + the seek-target sequence,
/// not the audio content (that surface is already covered by
/// `roundtrip` / `decode`).
fn synth_stream_with_planted_seektable(data: &[u8]) -> Option<Vec<u8>> {
    // Need at least one 18-byte seekpoint worth of fuzzer bytes.
    if data.len() < 18 {
        return None;
    }

    // 1. Encode a fixed 512-sample mono S16 stream; grab its STREAMINFO
    //    payload (from extradata) and the per-frame packets.
    let mut params = CodecParameters::audio(CodecId::new("flac"));
    params.channels = Some(1);
    params.sample_rate = Some(44_100);
    params.sample_format = Some(SampleFormat::S16);
    let mut enc = oxideav_flac::encoder::make_encoder(&params).ok()?;

    let n = 512usize;
    let mut interleaved = Vec::with_capacity(n * 2);
    // Cheap deterministic ramp + xorshift wobble so the subframes aren't
    // a degenerate CONSTANT (keeps the post-seek decode path meaningful).
    let mut s: u32 = 0x1234_5678;
    for i in 0..n {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        let v = ((i as i32 % 97) - 48) * 64 + ((s as i32) >> 26);
        interleaved.extend_from_slice(&(v as i16).to_le_bytes());
    }
    let frame = AudioFrame {
        samples: n as u32,
        pts: Some(0),
        data: vec![interleaved],
    };
    enc.send_frame(&Frame::Audio(frame)).ok()?;
    enc.flush().ok()?;

    let mut packets: Vec<Packet> = Vec::new();
    loop {
        match enc.receive_packet() {
            Ok(p) => packets.push(p),
            Err(oxideav_core::Error::NeedMore) | Err(oxideav_core::Error::Eof) => break,
            Err(_) => return None,
        }
    }
    let final_params = enc.output_params().clone();
    let streaminfo_bytes = final_params.extradata;
    // `make_encoder` always stashes the 34-byte STREAMINFO payload in
    // extradata; bail defensively if a future encoder change doesn't.
    if streaminfo_bytes.len() < 34 {
        return None;
    }

    // 2. Hand-assemble the SEEKTABLE payload from fuzzer bytes: as many
    //    whole 18-byte seekpoints as the input affords, capped so a
    //    pathological run can't pin a worker (the seek sweep is what we
    //    want to stress, not a million-point parse).
    let max_points = 64usize;
    let n_points = (data.len() / 18).min(max_points);
    let mut seektable_payload = Vec::with_capacity(n_points * 18);
    seektable_payload.extend_from_slice(&data[..n_points * 18]);

    // 3. Splice the full stream: magic + STREAMINFO (last=false) +
    //    SEEKTABLE (last=true) + frame packets.
    let mut file = Vec::new();
    file.extend_from_slice(&FLAC_MAGIC);

    let si_header = BlockHeader {
        last: false,
        block_type: BlockType::StreamInfo,
        length: 34,
    };
    si_header.write_into(&mut file).ok()?;
    file.extend_from_slice(&streaminfo_bytes[..34]);

    let st_header = BlockHeader {
        last: true,
        block_type: BlockType::SeekTable,
        length: seektable_payload.len() as u32,
    };
    st_header.write_into(&mut file).ok()?;
    file.extend_from_slice(&seektable_payload);

    for p in &packets {
        file.extend_from_slice(&p.data);
    }

    Some(file)
}
