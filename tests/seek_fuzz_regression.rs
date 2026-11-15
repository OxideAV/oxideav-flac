//! Regression corpus for the scheduled `seek` fuzz target.
//!
//! Each case carries a byte input that once made the demuxer's seek path
//! panic. The driver below repeats the two construction paths of
//! `fuzz/fuzz_targets/seek.rs` over the same public surface: the raw bytes
//! opened as a FLAC file, and a valid encoded stream carrying a SEEKTABLE
//! whose seek points are the input bytes verbatim. The contract is simply
//! that every call returns.

use oxideav_core::{
    AudioFrame, CodecId, CodecParameters, ContainerRegistry, Decoder, Demuxer, Frame,
    NullCodecResolver, ReadSeek, SampleFormat,
};

fn seek_targets(data: &[u8]) -> Vec<i64> {
    let mut targets = vec![i64::MIN, -1, 0, 1, i64::MAX];
    for chunk in data.chunks(8).take(4) {
        let mut buf = [0u8; 8];
        buf[..chunk.len()].copy_from_slice(chunk);
        targets.push(i64::from_le_bytes(buf));
    }
    targets
}

fn exercise_seeks(demux: &mut Box<dyn Demuxer>, dec: &mut Option<Box<dyn Decoder>>, data: &[u8]) {
    let _ = demux.seek_to(1, 0);
    for target in seek_targets(data) {
        let _ = demux.seek_to(0, target);
        for _ in 0..4 {
            let Ok(pkt) = demux.next_packet() else { break };
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

fn open_and_exercise(file: Vec<u8>, data: &[u8]) -> bool {
    let mut reg = ContainerRegistry::new();
    oxideav_flac::register_containers(&mut reg);
    let cursor: Box<dyn ReadSeek> = Box::new(std::io::Cursor::new(file));
    let Ok(mut demux) = reg.open_demuxer("flac", cursor, &NullCodecResolver) else {
        return false;
    };
    let mut dec = demux
        .streams()
        .first()
        .and_then(|s| oxideav_flac::decoder::make_decoder(&s.params).ok());
    exercise_seeks(&mut demux, &mut dec, data);
    true
}

/// A decodable 512-sample mono stream whose SEEKTABLE payload is `data`
/// cut to whole 18-byte seek points (at most 64).
fn stream_with_planted_seektable(data: &[u8]) -> Vec<u8> {
    let mut params = CodecParameters::audio(CodecId::new("flac"));
    params.channels = Some(1);
    params.sample_rate = Some(44_100);
    params.sample_format = Some(SampleFormat::S16);
    let mut enc = oxideav_flac::encoder::make_encoder(&params).expect("encoder");

    let n = 512usize;
    let mut pcm = Vec::with_capacity(n * 2);
    let mut s: u32 = 0x1234_5678;
    for i in 0..n {
        s ^= s << 13;
        s ^= s >> 17;
        s ^= s << 5;
        let v = ((i as i32 % 97) - 48) * 64 + ((s as i32) >> 26);
        pcm.extend_from_slice(&(v as i16).to_le_bytes());
    }
    enc.send_frame(&Frame::Audio(AudioFrame {
        samples: n as u32,
        pts: Some(0),
        data: vec![pcm],
    }))
    .expect("send");
    enc.flush().expect("flush");
    let mut frames = Vec::new();
    while let Ok(p) = enc.receive_packet() {
        frames.extend_from_slice(&p.data);
    }
    // The encoder's extradata is a metadata chain: a 4-byte block header
    // followed by the 34-byte STREAMINFO payload (RFC 9639 §8.2).
    let chain = enc.output_params().extradata.clone();
    assert!(chain.len() >= 38 && chain[0] & 0x7F == 0);
    let streaminfo = &chain[4..38];

    let points = (data.len() / 18).min(64);
    let table = &data[..points * 18];
    let mut file = Vec::new();
    file.extend_from_slice(b"fLaC");
    file.extend_from_slice(&[0x00, 0x00, 0x00, 34]);
    file.extend_from_slice(streaminfo);
    file.push(0x80 | 3);
    file.extend_from_slice(&(table.len() as u32).to_be_bytes()[1..]);
    file.extend_from_slice(table);
    file.extend_from_slice(&frames);
    file
}

fn drive(data: &[u8]) {
    open_and_exercise(data.to_vec(), data);
    if data.len() >= 18 {
        assert!(
            open_and_exercise(stream_with_planted_seektable(data), data),
            "the synthesised stream must open"
        );
    }
}

/// Scheduled run of 2026-11-14: a seek point whose 64-bit offset
/// (RFC 9639 §8.5.1), added to the position of the first frame, exceeds
/// `u64::MAX`. The sum overflowed in a build with overflow checks
/// (`src/container.rs`, "attempt to add with overflow"). Such a point is
/// now discarded and the target located by the frame-header scan.
#[test]
fn seek_point_offset_overflowing_the_file_position_returns() {
    drive(include_bytes!(
        "fixtures/seek_crash_offset_overflow_r489.bin"
    ));
}

/// The same class, minimal: every seek point is all-ones except the first
/// byte, so no point is a placeholder and every offset overflows.
#[test]
fn seek_points_with_maximal_offsets_return() {
    let mut data = vec![0xFFu8; 18 * 4];
    for (i, point) in data.chunks_mut(18).enumerate() {
        point[..8].copy_from_slice(&(i as u64 * 100).to_be_bytes());
    }
    drive(&data);
}
