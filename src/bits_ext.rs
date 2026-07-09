//! FLAC-specific bit-reader / bit-writer extensions.
//!
//! The generic bit I/O lives in [`oxideav_core::bits`]. FLAC adds one
//! codec-specific primitive that doesn't belong in the shared module:
//! the UTF-8-shaped variable-length integer (§8.1 Frame Header —
//! `sample_number`/`frame_number`). The encoding follows the byte
//! layout of UTF-8 but allows up to 36-bit values via a `0xFE` lead
//! byte, which isn't standard UTF-8.
//!
//! Callers pick up the methods via `use
//! oxideav_flac::bits_ext::{BitReaderExt, BitWriterExt};`.

use oxideav_core::{
    bits::{BitReader, BitWriter},
    Error, Result,
};

pub trait BitReaderExt {
    /// Read a FLAC-style UTF-8 variable-length integer. Reader must be
    /// byte-aligned on entry. Supports values up to 36 bits via the
    /// FLAC-extended `0xFE` lead byte.
    fn read_utf8_u64(&mut self) -> Result<u64>;

    /// Read `n` bits (0..=64) as a signed integer, sign-extended from the
    /// high bit. The generic [`BitReader::read_i32`] tops out at 32 bits;
    /// FLAC needs 33-bit reads for the side channel of a 32-bit
    /// stereo-decorrelated frame (RFC 9639 §4.2 / Appendix A.2 — "the side
    /// channel needs one extra bit of bit depth"), so the widest subframe
    /// samples do not fit in `i32`.
    fn read_i64(&mut self, n: u32) -> Result<i64>;
}

impl BitReaderExt for BitReader<'_> {
    fn read_i64(&mut self, n: u32) -> Result<i64> {
        if n == 0 {
            return Ok(0);
        }
        debug_assert!(n <= 64, "read_i64 supports up to 64 bits");
        let raw = self.read_u64(n)? as i64;
        let shift = 64 - n;
        // Arithmetic shift right sign-extends from bit `n-1`.
        Ok((raw << shift) >> shift)
    }

    fn read_utf8_u64(&mut self) -> Result<u64> {
        if !self.is_byte_aligned() {
            return Err(Error::invalid(
                "flac: read_utf8_u64 requires byte alignment",
            ));
        }
        let b0 = self.read_u32(8)? as u8;
        // Lead byte starts with N ones followed by a 0. Payload bits in lead = 7 - N.
        let (n_extra, lead_payload_bits) = match b0 {
            0x00..=0x7F => (0u32, 7u32), // 0xxxxxxx
            0xC0..=0xDF => (1, 5),       // 110xxxxx
            0xE0..=0xEF => (2, 4),       // 1110xxxx
            0xF0..=0xF7 => (3, 3),       // 11110xxx
            0xF8..=0xFB => (4, 2),       // 111110xx
            0xFC..=0xFD => (5, 1),       // 1111110x
            0xFE => (6, 0),              // 11111110
            _ => return Err(Error::invalid("flac: invalid UTF-8 leading byte")),
        };
        let lead_mask: u8 = if lead_payload_bits == 0 {
            0
        } else {
            ((1u16 << lead_payload_bits) - 1) as u8
        };
        let mut value = (b0 & lead_mask) as u64;
        for _ in 0..n_extra {
            let cont = self.read_u32(8)? as u8;
            if cont & 0xC0 != 0x80 {
                return Err(Error::invalid("flac: invalid UTF-8 continuation byte"));
            }
            value = (value << 6) | ((cont & 0x3F) as u64);
        }
        Ok(value)
    }
}

pub trait BitWriterExt {
    /// Append a FLAC-style UTF-8 variable-length integer. Writer must
    /// be byte-aligned on entry. Supports values up to 36 bits.
    fn write_utf8_u64(&mut self, value: u64);
}

impl BitWriterExt for BitWriter {
    fn write_utf8_u64(&mut self, value: u64) {
        debug_assert!(self.is_byte_aligned());
        let bits_needed = if value == 0 {
            1
        } else {
            64 - value.leading_zeros()
        };
        let (n_extra, lead_prefix, lead_payload_bits): (u32, u8, u32) = match bits_needed {
            0..=7 => (0, 0x00, 7),
            8..=11 => (1, 0xC0, 5),
            12..=16 => (2, 0xE0, 4),
            17..=21 => (3, 0xF0, 3),
            22..=26 => (4, 0xF8, 2),
            27..=31 => (5, 0xFC, 1),
            32..=36 => (6, 0xFE, 0),
            _ => panic!("UTF-8 varint value exceeds 36 bits"),
        };
        if n_extra == 0 {
            self.write_u32((value as u32) & 0x7F, 8);
        } else {
            let lead_payload =
                ((value >> (n_extra * 6)) & ((1u64 << lead_payload_bits) - 1)) as u32;
            self.write_u32(lead_prefix as u32 | lead_payload, 8);
            for i in (0..n_extra).rev() {
                let chunk = ((value >> (i * 6)) & 0x3F) as u32;
                self.write_u32(0x80 | chunk, 8);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_i64_sign_extends_wide_widths() {
        // Round-trip a set of values through 33-bit signed writes/reads,
        // including the extremes of the 33-bit range and values whose
        // bit-32 pattern a 32-bit read would misinterpret.
        for &v in &[
            0i64,
            1,
            -1,
            4_294_967_295,  // max positive 33-bit
            -4_294_967_296, // min negative 33-bit
            3_000_000_000,  // bit 31 set: positive at 33-bit, negative at 32-bit
            -3_000_000_000,
        ] {
            let mut w = BitWriter::new();
            w.write_u64(v as u64, 33);
            w.align_to_byte();
            let bytes = w.finish();
            let mut r = BitReader::new(&bytes);
            assert_eq!(r.read_i64(33).unwrap(), v, "read_i64(33) for {v}");
        }
        // Zero-width read yields 0 and consumes nothing.
        let mut r = BitReader::new(&[0xFF]);
        assert_eq!(r.read_i64(0).unwrap(), 0);
    }

    #[test]
    fn utf8_u64_roundtrip_small() {
        for &v in &[
            0u64,
            0x7F,
            0x80,
            0x7FF,
            0x800,
            0xFFFF,
            0x10000,
            (1 << 36) - 1,
        ] {
            let mut w = BitWriter::new();
            w.write_utf8_u64(v);
            let bytes = w.finish();
            let mut r = BitReader::new(&bytes);
            let got = r.read_utf8_u64().unwrap();
            assert_eq!(got, v, "utf8 roundtrip for {v:#x}");
        }
    }
}
