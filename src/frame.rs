//! FLAC frame header parsing (RFC 9639 §9.1).

use oxideav_core::{Error, Result};

use crate::crc;

/// Per-channel layout of the frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelAssignment {
    /// Channels stored independently (1..=8 channels).
    Independent(u8),
    /// Stereo, channel 0 = left, channel 1 = side (right = left - side).
    LeftSide,
    /// Stereo, channel 0 = side, channel 1 = right (left = right + side).
    RightSide,
    /// Stereo, channel 0 = mid, channel 1 = side (decorrelated).
    MidSide,
}

impl ChannelAssignment {
    pub fn channel_count(&self) -> u8 {
        match self {
            Self::Independent(n) => *n,
            _ => 2,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BlockingStrategy {
    Fixed,
    Variable,
}

#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct FrameHeader {
    pub blocking_strategy: BlockingStrategy,
    pub block_size: u32,
    /// Sample rate in Hz, or 0 if unknown (must be taken from STREAMINFO).
    pub sample_rate: u32,
    pub channels: ChannelAssignment,
    pub bits_per_sample: u8,
    /// Frame number (fixed-blocking) or sample number (variable-blocking).
    pub coded_number: u64,
    /// Number of bytes consumed by the header (including the trailing CRC-8).
    pub header_byte_len: usize,
}

impl FrameHeader {
    /// First sample number (within the stream) covered by this frame.
    /// Requires a streaminfo block size for fixed-blocking files.
    pub fn first_sample(&self, streaminfo_block_size: u32) -> u64 {
        match self.blocking_strategy {
            BlockingStrategy::Fixed => self.coded_number * streaminfo_block_size as u64,
            BlockingStrategy::Variable => self.coded_number,
        }
    }
}

/// Longest possible frame header: 4 fixed bytes, a 7-byte coded number,
/// a 2-byte uncommon block size, a 2-byte uncommon sample rate and the
/// CRC-8 (RFC 9639 §9.1).
pub const MAX_FRAME_HEADER_LEN: usize = 16;

/// Largest block size a frame header may carry. The 16-bit uncommon
/// block size field could express 65536, but RFC 9639 §9.1.6 forbids it
/// because STREAMINFO cannot represent it.
pub const MAX_BLOCK_SIZE: u32 = 65_535;

/// Decode the coded number of RFC 9639 §9.1.5 (Table 18) from the start
/// of `bytes`: a UTF-8-shaped variable-length code extended to 7 bytes /
/// 36 bits. Returns the value and the number of bytes it occupies, or
/// `Error::NeedMore` when `bytes` ends inside the code.
fn parse_coded_number(bytes: &[u8]) -> Result<(u64, usize)> {
    let Some(&lead) = bytes.first() else {
        return Err(Error::NeedMore);
    };
    // The number of leading one bits is the length of the sequence; a
    // single byte has none, and one leading one bit (a continuation
    // byte) or eight of them cannot start a sequence.
    let len = match lead.leading_ones() {
        0 => 1,
        n @ 2..=7 => n as usize,
        _ => {
            return Err(Error::invalid(
                "FLAC frame: invalid first byte of coded number",
            ));
        }
    };
    // A sequence of `len` bytes keeps `7 - len` payload bits in its first
    // byte (seven of them when it is a single byte).
    let mut value = if len == 1 {
        lead as u64
    } else {
        (lead as u64) & (0x7F >> len)
    };
    for i in 1..len {
        let Some(&byte) = bytes.get(i) else {
            return Err(Error::NeedMore);
        };
        if byte & 0xC0 != 0x80 {
            return Err(Error::invalid(
                "FLAC frame: invalid continuation byte in coded number",
            ));
        }
        value = (value << 6) | (byte & 0x3F) as u64;
    }
    Ok((value, len))
}

/// Parse a frame header from `bytes`. Verifies the trailing CRC-8.
///
/// Returns `Error::NeedMore` when `bytes` ends before the header does
/// (a header is at most [`MAX_FRAME_HEADER_LEN`] bytes), and
/// `Error::InvalidData` when the bytes are not a frame header: wrong sync
/// code, a reserved or forbidden field value, a set reserved bit, or a
/// CRC-8 mismatch.
pub fn parse_frame_header(bytes: &[u8]) -> Result<FrameHeader> {
    if bytes.len() < 2 {
        return Err(Error::NeedMore);
    }
    // §9.1: the 15-bit sync code 0b111111111111100, then the blocking
    // strategy bit.
    if bytes[0] != 0xFF || (bytes[1] & 0xFE) != 0xF8 {
        return Err(Error::invalid("FLAC frame: missing sync"));
    }
    if bytes.len() < 4 {
        return Err(Error::NeedMore);
    }
    let blocking_strategy = if bytes[1] & 1 == 0 {
        BlockingStrategy::Fixed
    } else {
        BlockingStrategy::Variable
    };
    let block_size_code = bytes[2] >> 4;
    let sample_rate_code = bytes[2] & 0x0F;
    let channel_code = bytes[3] >> 4;
    let sample_size_code = (bytes[3] >> 1) & 0x07;

    // Everything the first four bytes can rule out is checked before the
    // variable part is read, so a false sync code is rejected without
    // waiting for more input.
    if block_size_code == 0 {
        return Err(Error::invalid("FLAC frame: reserved block size code 0"));
    }
    if sample_rate_code == 15 {
        return Err(Error::invalid("FLAC frame: invalid sample rate code 15"));
    }
    let channels = match channel_code {
        0..=7 => ChannelAssignment::Independent(channel_code + 1),
        8 => ChannelAssignment::LeftSide,
        9 => ChannelAssignment::RightSide,
        10 => ChannelAssignment::MidSide,
        _ => {
            return Err(Error::invalid(format!(
                "FLAC frame: reserved channel assignment {channel_code}"
            )));
        }
    };
    // §9.1.4, Table 17. Code 0 means "only stored in STREAMINFO".
    let bits_per_sample = match sample_size_code {
        0 => 0,
        1 => 8,
        2 => 12,
        3 => return Err(Error::invalid("FLAC frame: reserved sample size code 3")),
        4 => 16,
        5 => 20,
        6 => 24,
        _ => 32,
    };
    if bytes[3] & 1 != 0 {
        return Err(Error::invalid("FLAC frame: reserved header bit is set"));
    }

    let (coded_number, coded_len) = parse_coded_number(&bytes[4..])?;
    // §9.1.5: a frame number is at most 31 bits (6 bytes); only a sample
    // number may use the 7-byte form.
    if blocking_strategy == BlockingStrategy::Fixed && coded_number > 0x7FFF_FFFF {
        return Err(Error::invalid(
            "FLAC frame: frame number does not fit in 31 bits",
        ));
    }
    let mut pos = 4 + coded_len;
    // Big-endian field of `len` bytes at `pos` (§9.1.6, §9.1.7).
    let mut field = |len: usize| -> Result<u32> {
        let Some(raw) = bytes.get(pos..pos + len) else {
            return Err(Error::NeedMore);
        };
        pos += len;
        Ok(raw.iter().fold(0u32, |acc, &b| (acc << 8) | b as u32))
    };

    // §9.1.1, Table 14; an uncommon block size follows the coded number.
    let block_size = match block_size_code {
        1 => 192,
        2..=5 => 576 << (block_size_code - 2),
        6 => field(1)? + 1,
        7 => field(2)? + 1,
        _ => 256u32 << (block_size_code - 8),
    };
    if block_size > MAX_BLOCK_SIZE {
        return Err(Error::invalid("FLAC frame: forbidden block size 65536"));
    }
    // §9.1.2, Table 15; an uncommon sample rate follows the uncommon
    // block size. Code 0 means "only stored in STREAMINFO".
    let sample_rate = match sample_rate_code {
        0 => 0,
        1 => 88_200,
        2 => 176_400,
        3 => 192_000,
        4 => 8_000,
        5 => 16_000,
        6 => 22_050,
        7 => 24_000,
        8 => 32_000,
        9 => 44_100,
        10 => 48_000,
        11 => 96_000,
        12 => field(1)? * 1000,
        13 => field(2)?,
        _ => field(2)? * 10,
    };

    // §9.1.8: CRC-8 over everything before it, sync code included.
    let Some(&claimed_crc) = bytes.get(pos) else {
        return Err(Error::NeedMore);
    };
    let computed = crc::crc8(&bytes[..pos]);
    if computed != claimed_crc {
        return Err(Error::invalid(format!(
            "FLAC frame header CRC-8 mismatch (got {:#04x}, want {:#04x})",
            computed, claimed_crc
        )));
    }

    Ok(FrameHeader {
        blocking_strategy,
        block_size,
        sample_rate,
        channels,
        bits_per_sample,
        coded_number,
        header_byte_len: pos + 1,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_minimal_header() {
        // Build a minimal valid frame header by hand:
        // sync 0xFFF8 (fixed blocking, reserved bits 0).
        // block_size_code = 0b0001 (192 samples)
        // sample_rate_code = 0b1010 (48000 Hz)
        // channel_code = 0b0000 (mono)
        // sample_size_code = 0b100 (16 bits)
        // reserved 0, frame number = 0 (1 byte UTF-8 = 0x00)
        // then CRC-8 over those 5 bytes.
        let mut hdr = vec![0xFF, 0xF8];
        hdr.push((0b0001 << 4) | 0b1010); // block_size + sample_rate
        hdr.push(0b100 << 1); // channel + sample_size + reserved
        hdr.push(0x00); // frame number = 0
        let c = crc::crc8(&hdr);
        hdr.push(c);
        let parsed = parse_frame_header(&hdr).unwrap();
        assert_eq!(parsed.block_size, 192);
        assert_eq!(parsed.sample_rate, 48_000);
        assert_eq!(parsed.bits_per_sample, 16);
        assert_eq!(parsed.channels, ChannelAssignment::Independent(1));
        assert_eq!(parsed.coded_number, 0);
        assert_eq!(parsed.blocking_strategy, BlockingStrategy::Fixed);
        assert_eq!(parsed.header_byte_len, hdr.len());
    }

    #[test]
    fn rejects_bad_crc() {
        let mut hdr = vec![0xFF, 0xF8, 0x14, 0x08, 0x00, 0xAA];
        // last byte is bogus CRC
        assert!(parse_frame_header(&hdr).is_err());
        // Fix it.
        let last = crc::crc8(&hdr[..5]);
        *hdr.last_mut().unwrap() = last;
        assert!(parse_frame_header(&hdr).is_ok());
    }

    /// Build a minimal 192-sample / 48 kHz / mono frame header carrying
    /// the given 3-bit `sample_size_code`, returning the parse result.
    fn parse_with_sample_size_code(code: u8) -> Result<FrameHeader> {
        let mut hdr = vec![0xFF, 0xF8];
        hdr.push((0b0001 << 4) | 0b1010); // block_size 192 + sample_rate 48k
        hdr.push((code & 0b111) << 1); // channel(mono=0) + sample_size + reserved 0
        hdr.push(0x00); // frame number = 0
        let c = crc::crc8(&hdr);
        hdr.push(c);
        parse_frame_header(&hdr)
    }

    #[test]
    fn sample_size_code_table_rfc9639_table17() {
        // RFC 9639 §9.1.4 Table 17: every bit-depth code maps to the
        // documented depth (0 → "from STREAMINFO", reported as 0).
        assert_eq!(parse_with_sample_size_code(0).unwrap().bits_per_sample, 0);
        assert_eq!(parse_with_sample_size_code(1).unwrap().bits_per_sample, 8);
        assert_eq!(parse_with_sample_size_code(2).unwrap().bits_per_sample, 12);
        assert!(parse_with_sample_size_code(3).is_err()); // 0b011 reserved
        assert_eq!(parse_with_sample_size_code(4).unwrap().bits_per_sample, 16);
        assert_eq!(parse_with_sample_size_code(5).unwrap().bits_per_sample, 20);
        assert_eq!(parse_with_sample_size_code(6).unwrap().bits_per_sample, 24);
        // 0b111 → 32 bits per sample (previously rejected as reserved).
        assert_eq!(parse_with_sample_size_code(7).unwrap().bits_per_sample, 32);
    }
}
