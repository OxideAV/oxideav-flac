//! CRC-8 and CRC-16 tables used by FLAC frames.
//!
//! - **CRC-8** (poly 0x07, init 0): covers the frame header, ending right
//!   before the 8-bit CRC field.
//! - **CRC-16** (poly 0x8005, init 0, non-reflected): covers the entire
//!   frame including the header, ending before the 16-bit CRC at the end.
//!
//! Both checksums are also exposed through small streaming validator
//! types ([`Crc8`] / [`Crc16`]) so callers that consume FLAC bytes in
//! pieces (an Ogg-FLAC demuxer fetching one Ogg segment at a time, a
//! network-fed decoder fronted by a ring buffer, an in-place verifier
//! that wants to early-fail before allocating subframe sample vectors)
//! don't have to concatenate the whole frame before calling the
//! buffer-shaped helpers.

const CRC8_TABLE: [u8; 256] = {
    let mut t = [0u8; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = i as u8;
        let mut j = 0;
        while j < 8 {
            c = if c & 0x80 != 0 {
                (c << 1) ^ 0x07
            } else {
                c << 1
            };
            j += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

/// Streaming CRC-8 (polynomial `0x07`, init 0, no input reflection).
///
/// Fed one byte at a time or one slice at a time; the running value
/// is recovered through [`Crc8::value`]. The buffer-shaped [`crc8`]
/// helper is a thin wrapper that creates a fresh `Crc8`, feeds the
/// slice, and reads the value back.
#[derive(Debug, Clone, Copy, Default)]
pub struct Crc8 {
    state: u8,
}

impl Crc8 {
    /// New CRC-8 with the FLAC initial value of 0.
    #[inline]
    pub const fn new() -> Self {
        Self { state: 0 }
    }

    /// Feed a single byte into the running CRC.
    #[inline]
    pub fn update_byte(&mut self, b: u8) {
        self.state = CRC8_TABLE[(self.state ^ b) as usize];
    }

    /// Feed a contiguous slice of bytes into the running CRC.
    #[inline]
    pub fn update(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.state = CRC8_TABLE[(self.state ^ b) as usize];
        }
    }

    /// Current running value. Reading does not consume the validator.
    #[inline]
    pub const fn value(&self) -> u8 {
        self.state
    }
}

pub fn crc8(bytes: &[u8]) -> u8 {
    let mut c = Crc8::new();
    c.update(bytes);
    c.value()
}

const CRC16_TABLE: [u16; 256] = {
    let mut t = [0u16; 256];
    let mut i = 0;
    while i < 256 {
        let mut c = (i as u16) << 8;
        let mut j = 0;
        while j < 8 {
            c = if c & 0x8000 != 0 {
                (c << 1) ^ 0x8005
            } else {
                c << 1
            };
            j += 1;
        }
        t[i] = c;
        i += 1;
    }
    t
};

/// Streaming CRC-16 (polynomial `0x8005`, init 0, no input reflection).
///
/// The FLAC frame footer carries this checksum over the whole frame —
/// header, subframe payloads, padding-to-byte-alignment. Callers that
/// receive those bytes incrementally fold them into a `Crc16` and read
/// [`Crc16::value`] at the byte-aligned end of the frame body to
/// compare against the on-wire footer. Equivalent to calling [`crc16`]
/// on a single concatenated buffer of the same bytes in the same
/// order, with no padding semantics of its own (callers handle the
/// align-to-byte step themselves).
#[derive(Debug, Clone, Copy, Default)]
pub struct Crc16 {
    state: u16,
}

impl Crc16 {
    /// New CRC-16 with the FLAC initial value of 0.
    #[inline]
    pub const fn new() -> Self {
        Self { state: 0 }
    }

    /// Feed a single byte into the running CRC.
    #[inline]
    pub fn update_byte(&mut self, b: u8) {
        let idx = ((self.state >> 8) as u8 ^ b) as usize;
        self.state = (self.state << 8) ^ CRC16_TABLE[idx];
    }

    /// Feed a contiguous slice of bytes into the running CRC.
    #[inline]
    pub fn update(&mut self, bytes: &[u8]) {
        for &b in bytes {
            let idx = ((self.state >> 8) as u8 ^ b) as usize;
            self.state = (self.state << 8) ^ CRC16_TABLE[idx];
        }
    }

    /// Current running value. Reading does not consume the validator.
    #[inline]
    pub const fn value(&self) -> u16 {
        self.state
    }
}

pub fn crc16(bytes: &[u8]) -> u16 {
    let mut c = Crc16::new();
    c.update(bytes);
    c.value()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc8_zero_for_empty() {
        assert_eq!(crc8(&[]), 0);
        assert_eq!(crc16(&[]), 0);
    }

    #[test]
    fn deterministic() {
        assert_eq!(crc8(b"hello"), crc8(b"hello"));
        assert_ne!(crc8(b"hello"), crc8(b"hellp"));
        assert_eq!(crc16(b"world"), crc16(b"world"));
        assert_ne!(crc16(b"world"), crc16(b"worle"));
    }

    /// Streaming CRC-8 fed byte-at-a-time agrees with the one-shot
    /// `crc8(slice)` helper. The two paths share the same table, but
    /// chunked feeders need the byte-loop to stay glued to the table
    /// lookup; this test would catch a regression that broke
    /// `update_byte` while leaving `crc8(&[u8])` correct.
    #[test]
    fn crc8_streaming_matches_oneshot_byte_at_a_time() {
        let payload: &[u8] = b"the quick brown fox jumps over the lazy dog";
        let one_shot = crc8(payload);
        let mut c = Crc8::new();
        for &b in payload {
            c.update_byte(b);
        }
        assert_eq!(c.value(), one_shot);
    }

    /// Streaming CRC-8 fed in arbitrary-length slices agrees with the
    /// one-shot value. Picks split points that don't align with the
    /// table boundary (every 7 bytes) so a buggy implementation that
    /// silently reset state between chunks would diverge.
    #[test]
    fn crc8_streaming_matches_oneshot_chunked() {
        let payload: &[u8] =
            b"FLAC-frame-header-bytes-then-some-subframe-payload-bytes-then-padding";
        let one_shot = crc8(payload);
        let mut c = Crc8::new();
        for chunk in payload.chunks(7) {
            c.update(chunk);
        }
        assert_eq!(c.value(), one_shot);
    }

    /// Same byte-at-a-time streaming check for CRC-16. The table
    /// indexing is more involved than CRC-8 (`(state >> 8) ^ b` vs
    /// `state ^ b`), so a typo in the streaming update — taking the
    /// low byte of state by mistake — would still produce a sensible
    /// looking number from a single call but diverge from the one-shot.
    #[test]
    fn crc16_streaming_matches_oneshot_byte_at_a_time() {
        let payload: &[u8] = b"the quick brown fox jumps over the lazy dog";
        let one_shot = crc16(payload);
        let mut c = Crc16::new();
        for &b in payload {
            c.update_byte(b);
        }
        assert_eq!(c.value(), one_shot);
    }

    /// Streaming CRC-16 fed in three uneven chunks and compared against
    /// the one-shot. Three chunks of 1/30/(n-31) bytes exercises a
    /// short initial chunk (state still 0, low byte mostly preserved),
    /// a medium chunk (state cycling through table entries) and a tail
    /// chunk (boundary across a previous update call), so any state-
    /// reset bug breaks at least one of the three.
    #[test]
    fn crc16_streaming_matches_oneshot_chunked() {
        let payload: &[u8] =
            b"FLAC-frame-header-bytes-then-some-subframe-payload-bytes-then-padding";
        let one_shot = crc16(payload);
        let mut c = Crc16::new();
        c.update(&payload[..1]);
        c.update(&payload[1..31]);
        c.update(&payload[31..]);
        assert_eq!(c.value(), one_shot);
    }

    /// Empty-slice updates on the streaming validator are a no-op:
    /// the running state must not change. Streaming callers may
    /// receive a zero-length segment at end-of-stream (Ogg-FLAC last
    /// packet of an EOS page, network reader returning a zero-byte
    /// read between two non-empty reads) and must be able to keep
    /// folding without resetting.
    #[test]
    fn streaming_empty_update_is_noop() {
        let payload: &[u8] = b"some-bytes";
        let mut a = Crc8::new();
        let mut b = Crc16::new();
        a.update(&payload[..3]);
        b.update(&payload[..3]);
        let a_mid = a.value();
        let b_mid = b.value();
        a.update(&[]);
        b.update(&[]);
        assert_eq!(a.value(), a_mid);
        assert_eq!(b.value(), b_mid);
        a.update(&payload[3..]);
        b.update(&payload[3..]);
        assert_eq!(a.value(), crc8(payload));
        assert_eq!(b.value(), crc16(payload));
    }

    /// Two `Crc16` validators fed the same bytes in different chunk
    /// boundaries land on the same value. The property test for the
    /// associative-over-concat invariant streaming callers rely on
    /// (concatenating four chunks into one buffer must produce the
    /// same checksum as feeding each chunk separately, in order).
    #[test]
    fn crc16_chunk_boundary_independent() {
        let payload: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
        let one_shot = crc16(payload);
        // Try every split point 1..=n-1 and confirm each pair-split
        // streams to the same value.
        for split in 1..payload.len() {
            let mut c = Crc16::new();
            c.update(&payload[..split]);
            c.update(&payload[split..]);
            assert_eq!(
                c.value(),
                one_shot,
                "split at {split} drifted from one-shot"
            );
        }
    }
}
