//! Verify-decoder MD5 path (RFC 9639 §8.2 + Appendix D.2.9).
//!
//! The STREAMINFO metadata block carries an MD5 checksum of the *unencoded*
//! audio: every interchannel sample of every channel, interleaved, each
//! sample stored signed two's-complement little-endian, sign-extended to a
//! whole number of bytes. A decoder can recompute that digest from the PCM
//! it reconstructs and compare it against the stored value to confirm the
//! decode was lossless end to end — this is FLAC's canonical integrity
//! check, complementary to the per-frame CRC-16 (which only proves a single
//! frame's bytes were not corrupted, not that the whole-stream decode
//! matches the original audio).
//!
//! This module provides:
//!
//! * [`Md5Verifier`] — an incremental accumulator. Feed it the decoded
//!   per-channel `i32` planes of each frame in order; it folds them into a
//!   running MD5 in the canonical byte order and, when finished, compares
//!   against a 16-byte STREAMINFO signature.
//! * [`verify_stream`] — a one-shot whole-stream check: given the raw bytes
//!   of a `.flac` file (or a bare frame stream + an explicit STREAMINFO),
//!   decode every frame and report whether the recomputed MD5 matches.
//!
//! Per RFC 9639 §8.2, an all-zero signature means "MD5 not computed"; the
//! verifier treats that as *unverifiable* (neither pass nor fail) rather
//! than a mismatch, matching the reference behaviour where an absent
//! signature simply skips the check.

use oxideav_core::{Error, Result};

use crate::decoder::{decode_frame_channels, DecodedFrame};
use crate::md5::Md5;
use crate::metadata::{BlockHeader, BlockType, StreamInfo};

/// Bytes used to store one sample at `bits_per_sample` in the MD5 message,
/// per RFC 9639 §8.2: the sample is sign-extended to the next whole
/// number of bytes (1 byte for ≤ 8 bps, 2 for ≤ 16, 3 for ≤ 24, 4 for
/// ≤ 32).
fn md5_bytes_per_sample(bits_per_sample: u32) -> usize {
    match bits_per_sample {
        0..=8 => 1,
        9..=16 => 2,
        17..=24 => 3,
        _ => 4,
    }
}

/// Append one interchannel sample's worth of channels to `buf` in the
/// canonical signed-little-endian, sign-extended layout.
fn push_sample_le(buf: &mut Vec<u8>, value: i32, bytes_per_sample: usize) {
    match bytes_per_sample {
        1 => buf.push((value & 0xFF) as u8),
        2 => buf.extend_from_slice(&(value as i16).to_le_bytes()),
        3 => {
            buf.push((value & 0xFF) as u8);
            buf.push(((value >> 8) & 0xFF) as u8);
            buf.push(((value >> 16) & 0xFF) as u8);
        }
        _ => buf.extend_from_slice(&value.to_le_bytes()),
    }
}

/// Incremental MD5 accumulator over decoded FLAC PCM (RFC 9639 §8.2).
///
/// Construct with the stream's bit depth, feed each frame's decoded
/// per-channel planes in stream order via [`Md5Verifier::update`], then call
/// [`Md5Verifier::finalize`] / [`Md5Verifier::verify`].
pub struct Md5Verifier {
    md5: Md5,
    bytes_per_sample: usize,
    samples_seen: u64,
}

impl Md5Verifier {
    /// Start a verifier for a stream of the given bit depth.
    pub fn new(bits_per_sample: u32) -> Self {
        Self {
            md5: Md5::new(),
            bytes_per_sample: md5_bytes_per_sample(bits_per_sample),
            samples_seen: 0,
        }
    }

    /// Fold one frame's decoded planes into the digest.
    ///
    /// `channels` is one `Vec<i32>` per channel, all of equal length (the
    /// frame's interchannel sample count) — exactly the shape
    /// [`decode_frame_channels`] returns. Samples are emitted interleaved:
    /// channel 0 sample 0, channel 1 sample 0, …, channel 0 sample 1, …
    pub fn update(&mut self, channels: &[Vec<i32>]) {
        let n_ch = channels.len();
        if n_ch == 0 {
            return;
        }
        let n = channels[0].len();
        let mut buf = Vec::with_capacity(n * n_ch * self.bytes_per_sample);
        for i in 0..n {
            for c in 0..n_ch {
                push_sample_le(&mut buf, channels[c][i], self.bytes_per_sample);
            }
        }
        self.md5.update(&buf);
        self.samples_seen += n as u64;
    }

    /// Total interchannel samples folded in so far.
    pub fn samples_seen(&self) -> u64 {
        self.samples_seen
    }

    /// Consume the verifier, returning the 16-byte MD5 of the PCM fed so far.
    pub fn finalize(self) -> [u8; 16] {
        self.md5.finalize()
    }

    /// Consume the verifier and compare against a STREAMINFO signature.
    ///
    /// Returns:
    /// * `Ok(true)` — the recomputed digest equals `expected`.
    /// * `Ok(false)` — `expected` is all-zero ("MD5 not computed",
    ///   RFC 9639 §8.2): unverifiable, treated as not-a-mismatch but
    ///   reported `false` so callers can distinguish a real match.
    /// * `Err(Error::InvalidData)` — the digests differ: the decode was not
    ///   lossless (or the stream is corrupt). The message includes both
    ///   the recomputed and the expected digest in hex.
    pub fn verify(self, expected: &[u8; 16]) -> Result<bool> {
        if expected.iter().all(|&b| b == 0) {
            // Spec: an all-zero signature means the encoder did not compute
            // the MD5. There is nothing to verify against.
            let _ = self.finalize();
            return Ok(false);
        }
        let got = self.finalize();
        if &got == expected {
            Ok(true)
        } else {
            Err(Error::invalid(format!(
                "FLAC MD5 verification failed: decoded {}, STREAMINFO {}",
                hex16(&got),
                hex16(expected),
            )))
        }
    }
}

/// Lowercase-hex render of a 16-byte digest for diagnostic messages.
fn hex16(d: &[u8; 16]) -> String {
    let mut s = String::with_capacity(32);
    for b in d {
        s.push_str(&format!("{:02x}", b));
    }
    s
}

/// Outcome of a whole-stream MD5 verification.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerifyOutcome {
    /// Recomputed MD5 matched the STREAMINFO signature: decode was lossless.
    Match,
    /// STREAMINFO carried an all-zero signature: nothing to verify against.
    NoSignature,
}

/// Decode every frame in a bare FLAC *frame stream* (no `fLaC` magic, no
/// metadata blocks) against an explicit STREAMINFO and verify the MD5.
///
/// `frames` must begin at the first frame sync code. Frames are decoded
/// back to back; each frame's byte length is taken from the decode itself
/// (header + body + CRC-16). Decoding stops cleanly at the end of the
/// buffer. Any per-frame decode error (bad CRC-16, malformed subframe)
/// propagates.
pub fn verify_frame_stream(frames: &[u8], streaminfo: &StreamInfo) -> Result<VerifyOutcome> {
    let mut verifier = Md5Verifier::new(streaminfo.bits_per_sample as u32);
    let mut pos = 0usize;
    while pos < frames.len() {
        let DecodedFrame {
            channels,
            frame_byte_len,
            ..
        } = decode_frame_channels(&frames[pos..], streaminfo)?;
        verifier.update(&channels);
        if frame_byte_len == 0 {
            return Err(Error::invalid("FLAC verify: zero-length frame"));
        }
        pos += frame_byte_len;
    }
    match verifier.verify(&streaminfo.md5)? {
        true => Ok(VerifyOutcome::Match),
        false => Ok(VerifyOutcome::NoSignature),
    }
}

/// Decode a complete `.flac` byte stream (`fLaC` magic + metadata blocks +
/// frames) and verify its STREAMINFO MD5 against the decoded PCM.
///
/// This is the whole-file form of FLAC's verify-decoder check: it walks the
/// metadata-block chain to find STREAMINFO, then decodes every frame and
/// recomputes the MD5. Returns [`VerifyOutcome::Match`] on a lossless
/// decode, [`VerifyOutcome::NoSignature`] when the file stores no MD5, and
/// `Err(Error::InvalidData)` when the recomputed digest disagrees with the
/// stored one.
pub fn verify_stream(file: &[u8]) -> Result<VerifyOutcome> {
    if file.len() < 4 || &file[0..4] != b"fLaC" {
        return Err(Error::invalid("FLAC verify: missing fLaC stream marker"));
    }
    let mut i = 4usize;
    let mut streaminfo: Option<StreamInfo> = None;
    loop {
        if i + 4 > file.len() {
            return Err(Error::invalid(
                "FLAC verify: truncated metadata block header",
            ));
        }
        let hdr = BlockHeader::parse(&file[i..i + 4])?;
        let payload_start = i + 4;
        let payload_end = payload_start + hdr.length as usize;
        if payload_end > file.len() {
            return Err(Error::invalid("FLAC verify: metadata block exceeds buffer"));
        }
        if hdr.block_type == BlockType::StreamInfo {
            streaminfo = Some(StreamInfo::parse(&file[payload_start..payload_end])?);
        }
        let last = hdr.last;
        i = payload_end;
        if last {
            break;
        }
    }
    let streaminfo =
        streaminfo.ok_or_else(|| Error::invalid("FLAC verify: no STREAMINFO metadata block"))?;
    verify_frame_stream(&file[i..], &streaminfo)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_per_sample_rounds_up_to_whole_bytes() {
        assert_eq!(md5_bytes_per_sample(8), 1);
        assert_eq!(md5_bytes_per_sample(12), 2);
        assert_eq!(md5_bytes_per_sample(16), 2);
        assert_eq!(md5_bytes_per_sample(20), 3);
        assert_eq!(md5_bytes_per_sample(24), 3);
        assert_eq!(md5_bytes_per_sample(32), 4);
    }

    #[test]
    fn appendix_d1_matches_documented_checksum() {
        // RFC 9639 Appendix D.1: a 16-bit stereo file with a single
        // interchannel sample whose channel values are 25588 (ch0) and
        // 10416 (ch1). The spec states the MD5 message is the interleaved
        // little-endian pair 0xf463 b028, and that the resulting STREAMINFO
        // checksum (starting at 0x1a) is
        //   0x3e84 b418 07dc 6903 0758 6a3d ad1a 2e0f.
        // Feeding the verifier the same single-interchannel-sample stereo
        // frame must reproduce that exact digest — a sample-exact tie of
        // our PCM-serialization byte order to the worked example.
        let mut v = Md5Verifier::new(16);
        v.update(&[vec![25588], vec![10416]]);
        let got = v.finalize();

        let expected: [u8; 16] = [
            0x3e, 0x84, 0xb4, 0x18, 0x07, 0xdc, 0x69, 0x03, 0x07, 0x58, 0x6a, 0x3d, 0xad, 0x1a,
            0x2e, 0x0f,
        ];
        assert_eq!(got, expected);

        // And the same message expressed as raw interleaved LE bytes.
        let from_bytes = crate::md5::compute(&[0xf4, 0x63, 0xb0, 0x28]);
        assert_eq!(got, from_bytes);
    }

    #[test]
    fn interleave_order_is_per_sample_not_per_channel() {
        // Two channels, two interchannel samples. Canonical order is
        // ch0[0], ch1[0], ch0[1], ch1[1] — NOT ch0[0],ch0[1],ch1[0],ch1[1].
        let mut v = Md5Verifier::new(16);
        v.update(&[vec![1, 3], vec![2, 4]]);
        let got = v.finalize();

        // Hand-built interleaved 16-bit LE message: 1,2,3,4.
        let mut msg = Vec::new();
        for s in [1i16, 2, 3, 4] {
            msg.extend_from_slice(&s.to_le_bytes());
        }
        assert_eq!(got, crate::md5::compute(&msg));
    }

    #[test]
    fn all_zero_signature_is_unverifiable() {
        let mut v = Md5Verifier::new(16);
        v.update(&[vec![1, 2, 3]]);
        let zero = [0u8; 16];
        assert!(!v.verify(&zero).unwrap());
    }

    #[test]
    fn mismatching_signature_is_invalid_data_error() {
        let mut v = Md5Verifier::new(16);
        v.update(&[vec![1, 2, 3]]);
        let wrong = [0xAAu8; 16];
        assert!(matches!(v.verify(&wrong), Err(Error::InvalidData(_))));
    }

    #[test]
    fn matching_signature_passes() {
        let mut v = Md5Verifier::new(16);
        v.update(&[vec![100, -200, 300]]);
        let sig = {
            let mut v2 = Md5Verifier::new(16);
            v2.update(&[vec![100, -200, 300]]);
            v2.finalize()
        };
        assert!(v.verify(&sig).unwrap());
    }

    #[test]
    fn samples_seen_counts_interchannel_samples() {
        let mut v = Md5Verifier::new(16);
        v.update(&[vec![1, 2, 3, 4], vec![5, 6, 7, 8]]);
        v.update(&[vec![9, 10], vec![11, 12]]);
        // 4 + 2 interchannel samples; channel count does not multiply.
        assert_eq!(v.samples_seen(), 6);
    }
}
