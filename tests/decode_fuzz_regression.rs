//! Regression corpus for the scheduled `decode` fuzz target.
//!
//! Each case carries a byte input that once made the full demux+decode
//! pipeline panic, integer-overflow, or otherwise fail the "must only
//! return" contract of `fuzz/fuzz_targets/decode.rs`. Driving them through
//! the identical public surface keeps the fix pinned.

use oxideav_core::{ContainerRegistry, NullCodecResolver};

/// Run bytes through the exact pipeline the `decode` fuzz target exercises:
/// container registry → `open_demuxer("flac", …)` → bounded packet drain →
/// per-packet decode. The contract is simply that it returns.
fn drive_decode_pipeline(data: &[u8]) {
    let mut reg = ContainerRegistry::new();
    oxideav_flac::register_containers(&mut reg);
    let resolver = NullCodecResolver;
    let cursor: Box<dyn oxideav_core::ReadSeek> = Box::new(std::io::Cursor::new(data.to_vec()));
    if let Ok(mut demux) = reg.open_demuxer("flac", cursor, &resolver) {
        let streams = demux.streams().to_vec();
        let mut dec = streams
            .first()
            .and_then(|s| oxideav_flac::decoder::make_decoder(&s.params).ok());
        for _ in 0..8 {
            let pkt = match demux.next_packet() {
                Ok(p) => p,
                Err(_) => break,
            };
            if let Some(d) = dec.as_mut() {
                let _ = d.send_packet(&pkt);
                for _ in 0..4 {
                    if d.receive_frame().is_err() {
                        break;
                    }
                }
            }
        }
    }
}

/// r418 `decode` crash: a malformed 32-bit stereo-decorrelated frame whose
/// 33-bit *side* subframe is modeled by a wide predictor. The wrapping
/// predictor let a reconstructed side sample grow to an arbitrary `i64`,
/// and the inverse mid/side decorrelation then evaluated `(m − side) >> 1`
/// with plain `i64` arithmetic — underflowing `i64` and panicking in a
/// debug build (`src/decoder.rs` `attempt to subtract with overflow`). A
/// valid stream can never reach that point: RFC 9639 §4.2 bounds the side
/// channel to 33 bits and the mid to 32, so `m ± side` stays well inside
/// `i64`. The decoder now rejects the out-of-range reconstruction with a
/// typed decode error instead of overflowing.
#[test]
fn decode_wide_side_channel_underflow_returns() {
    let data = include_bytes!("fixtures/decode_crash_wide_underflow_r418.bin");
    drive_decode_pipeline(data);
}
