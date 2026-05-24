//! FLAC metadata block parsing (FLAC format spec, §METADATA_BLOCK).

use oxideav_core::{Error, Result};

pub const FLAC_MAGIC: [u8; 4] = *b"fLaC";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u8)]
pub enum BlockType {
    StreamInfo = 0,
    Padding = 1,
    Application = 2,
    SeekTable = 3,
    VorbisComment = 4,
    CueSheet = 5,
    Picture = 6,
    Reserved(u8),
    Invalid,
}

impl BlockType {
    pub fn from_byte(b: u8) -> Self {
        match b & 0x7F {
            0 => Self::StreamInfo,
            1 => Self::Padding,
            2 => Self::Application,
            3 => Self::SeekTable,
            4 => Self::VorbisComment,
            5 => Self::CueSheet,
            6 => Self::Picture,
            127 => Self::Invalid,
            other => Self::Reserved(other),
        }
    }
}

/// Header of a single metadata block.
#[derive(Clone, Debug)]
pub struct BlockHeader {
    pub last: bool,
    pub block_type: BlockType,
    pub length: u32,
}

impl BlockHeader {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 4 {
            return Err(Error::NeedMore);
        }
        let b0 = bytes[0];
        let last = b0 & 0x80 != 0;
        let block_type = BlockType::from_byte(b0);
        let length = ((bytes[1] as u32) << 16) | ((bytes[2] as u32) << 8) | (bytes[3] as u32);
        Ok(Self {
            last,
            block_type,
            length,
        })
    }
}

/// Parsed STREAMINFO block contents.
#[derive(Clone, Debug)]
pub struct StreamInfo {
    pub min_block_size: u16,
    pub max_block_size: u16,
    pub min_frame_size: u32,
    pub max_frame_size: u32,
    pub sample_rate: u32,
    pub channels: u8,
    pub bits_per_sample: u8,
    pub total_samples: u64,
    pub md5: [u8; 16],
}

/// A single index point inside a CUESHEET track (RFC 9639 §8.7.1.1).
///
/// For CD-DA, `offset_samples` MUST be a multiple of 588 (44100/75) and
/// is measured from the track's start; the wall payload also carries
/// 3 reserved bytes after the index point number which the parser
/// validates are zero per spec.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CueSheetIndex {
    /// Offset, in samples, from the *start of the track* to this index
    /// point. Always 0 for INDEX 01 in CD-DA (the track's first audio
    /// sample). Non-zero for INDEX 00 (pre-gap) when present.
    pub offset_samples: u64,
    /// Index point number. INDEX 00 = pre-gap, INDEX 01 = main entry,
    /// 02..=99 = subindex points.
    pub number: u8,
}

/// A single CUESHEET track (RFC 9639 §8.7.1).
///
/// Track number 0 is reserved by Red Book for the lead-in and MUST NOT
/// appear here. The last track in a CUESHEET is the lead-out (number
/// 170 for CD-DA, 255 for non-CD-DA) and carries zero index points.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CueSheetTrack {
    /// Offset, in samples, of the first index point of this track from
    /// the *start of the FLAC audio stream*. For CD-DA, MUST be a
    /// multiple of 588 samples. The lead-out track carries the total
    /// audio sample count here.
    pub offset_samples: u64,
    /// Track number. 1..=99 for CD-DA audio tracks, 170 for the CD-DA
    /// lead-out, 255 for the non-CD-DA lead-out, 1..=254 for non-CD-DA
    /// payload tracks.
    pub number: u8,
    /// 12-byte International Standard Recording Code. An all-zero ISRC
    /// is spec-permitted to denote "absent" and the parser preserves
    /// it verbatim (callers can detect via `isrc.iter().all(|&b| b ==
    /// 0)`).
    pub isrc: [u8; 12],
    /// Track type bit from the CD-DA Q-channel control. 0 = audio, 1 =
    /// non-audio (data).
    pub is_audio: bool,
    /// Pre-emphasis flag from the CD-DA Q-channel control. 0 = no
    /// pre-emphasis, 1 = pre-emphasis applied during mastering.
    pub pre_emphasis: bool,
    /// Index points within this track. Empty for the lead-out track,
    /// otherwise at least one (typically INDEX 01).
    pub indices: Vec<CueSheetIndex>,
}

/// A full FLAC CUESHEET metadata block (RFC 9639 §8.7).
///
/// Used to describe the track structure of a CD-DA mastered to FLAC or
/// to mark locations of interest in arbitrary streams. The track list
/// always ends with a lead-out track that carries zero index points.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CueSheet {
    /// Media catalog number in ASCII printable characters 0x20-0x7E,
    /// right-padded with 0x00 if shorter than 128 bytes. Empty (all
    /// zeros) when the originator declined to record one.
    pub media_catalog_number: [u8; 128],
    /// Number of lead-in samples. Meaningful only when `is_cdda` is
    /// true; otherwise spec says it SHOULD be 0.
    pub lead_in_samples: u64,
    /// True if the cuesheet describes a CD-DA, false otherwise.
    pub is_cdda: bool,
    /// Track list including the trailing lead-out track. Per spec the
    /// list always carries at least one entry (the lead-out).
    pub tracks: Vec<CueSheetTrack>,
}

impl CueSheet {
    /// Spec maximum overall track count (99 + lead-out for CD-DA, but
    /// the on-wire `num_tracks` field is u8 so 256 is the absolute
    /// upper bound). We use this to size pre-allocations safely; it is
    /// not enforced as a parse-rejection threshold because non-CD-DA
    /// CUESHEETs can legitimately use the full u8 range.
    pub const MAX_TRACKS: usize = 256;
}

/// Parse a CUESHEET metadata block payload (RFC 9639 §8.7).
///
/// Layout in bytes (big-endian, byte-aligned throughout — the block is
/// designed for byte slicing rather than bit reading):
///
/// ```text
///   128 B  media catalog number (ASCII, zero-padded)
///     8 B  lead-in samples (u64 BE)
///     1 B  bit 7 = is_cdda; bits 0..=6 reserved (must be zero)
///   258 B  reserved (all bytes must be zero)
///     1 B  num_tracks
///   N × cuesheet_track (variable, terminated by num_tracks counter)
/// ```
///
/// Each cuesheet_track is `8 + 1 + 12 + 1 + 13 + 1 = 36` fixed bytes
/// followed by `12 B × num_indices` index points.
///
/// Returns `Error::invalid` when:
///   * payload is shorter than the 396-byte fixed prefix,
///   * any reserved bit/byte is non-zero,
///   * the declared track count would consume more bytes than the
///     payload contains,
///   * a track other than the trailing entry declares zero index points
///     (only the lead-out may have zero per spec).
pub fn parse_cuesheet(bytes: &[u8]) -> Result<CueSheet> {
    // Fixed-prefix size: 128 (catalog) + 8 (lead-in) + 1 (flags) +
    // 258 (reserved) + 1 (num_tracks) = 396 bytes.
    const FIXED_PREFIX: usize = 128 + 8 + 1 + 258 + 1;
    if bytes.len() < FIXED_PREFIX {
        return Err(Error::invalid(format!(
            "FLAC CUESHEET: payload too short ({} < {FIXED_PREFIX})",
            bytes.len()
        )));
    }
    let mut media_catalog_number = [0u8; 128];
    media_catalog_number.copy_from_slice(&bytes[0..128]);
    let lead_in_samples = u64::from_be_bytes(bytes[128..136].try_into().expect("8 bytes"));
    let flags = bytes[136];
    if flags & 0x7F != 0 {
        return Err(Error::invalid(
            "FLAC CUESHEET: reserved bits in flag byte must be zero",
        ));
    }
    let is_cdda = (flags & 0x80) != 0;
    // RFC 9639 §8.7 requires the 258-byte reserved span be all zeros.
    // Some FLAC files in the wild leave non-zero bytes here; we follow
    // the spec strictly so a fuzzer can't sneak garbage through.
    if bytes[137..395].iter().any(|&b| b != 0) {
        return Err(Error::invalid(
            "FLAC CUESHEET: reserved 258-byte region must be all zero",
        ));
    }
    let num_tracks = bytes[395];
    if num_tracks == 0 {
        return Err(Error::invalid(
            "FLAC CUESHEET: track count must be at least 1 (lead-out)",
        ));
    }
    let mut cursor = FIXED_PREFIX;
    let mut tracks = Vec::with_capacity(num_tracks as usize);
    for track_idx in 0..num_tracks {
        // Fixed track prefix: 8 (offset) + 1 (number) + 12 (isrc) +
        // 1 (flags) + 13 (reserved) + 1 (num_indices) = 36 bytes.
        const TRACK_PREFIX: usize = 8 + 1 + 12 + 1 + 13 + 1;
        if cursor + TRACK_PREFIX > bytes.len() {
            return Err(Error::invalid(format!(
                "FLAC CUESHEET: track {track_idx} prefix overruns payload"
            )));
        }
        let offset_samples =
            u64::from_be_bytes(bytes[cursor..cursor + 8].try_into().expect("8 bytes"));
        let number = bytes[cursor + 8];
        if number == 0 {
            // Per spec a track number of 0 is reserved for the lead-in
            // and MUST NOT appear in the CUESHEET track list itself.
            return Err(Error::invalid(
                "FLAC CUESHEET: track number 0 is reserved (lead-in only)",
            ));
        }
        let mut isrc = [0u8; 12];
        isrc.copy_from_slice(&bytes[cursor + 9..cursor + 21]);
        let track_flags = bytes[cursor + 21];
        if track_flags & 0x3F != 0 {
            return Err(Error::invalid(
                "FLAC CUESHEET: reserved bits in track flag byte must be zero",
            ));
        }
        let is_audio = (track_flags & 0x80) == 0;
        let pre_emphasis = (track_flags & 0x40) != 0;
        if bytes[cursor + 22..cursor + 35].iter().any(|&b| b != 0) {
            return Err(Error::invalid(
                "FLAC CUESHEET: track reserved 13-byte region must be all zero",
            ));
        }
        let num_indices = bytes[cursor + 35];
        let is_last = track_idx + 1 == num_tracks;
        if num_indices == 0 && !is_last {
            return Err(Error::invalid(
                "FLAC CUESHEET: only the lead-out track may have zero indices",
            ));
        }
        cursor += TRACK_PREFIX;
        // Each index point is 8 (offset) + 1 (number) + 3 (reserved) = 12 bytes.
        const INDEX_SIZE: usize = 8 + 1 + 3;
        let indices_len = (num_indices as usize) * INDEX_SIZE;
        if cursor + indices_len > bytes.len() {
            return Err(Error::invalid(format!(
                "FLAC CUESHEET: track {track_idx} index list overruns payload"
            )));
        }
        let mut indices = Vec::with_capacity(num_indices as usize);
        for _ in 0..num_indices {
            let i_off = u64::from_be_bytes(bytes[cursor..cursor + 8].try_into().expect("8 bytes"));
            let i_num = bytes[cursor + 8];
            if bytes[cursor + 9..cursor + 12].iter().any(|&b| b != 0) {
                return Err(Error::invalid(
                    "FLAC CUESHEET: index point reserved bytes must be zero",
                ));
            }
            indices.push(CueSheetIndex {
                offset_samples: i_off,
                number: i_num,
            });
            cursor += INDEX_SIZE;
        }
        tracks.push(CueSheetTrack {
            offset_samples,
            number,
            isrc,
            is_audio,
            pre_emphasis,
            indices,
        });
    }
    Ok(CueSheet {
        media_catalog_number,
        lead_in_samples,
        is_cdda,
        tracks,
    })
}

/// Serialise a [`CueSheet`] back into a CUESHEET metadata-block payload
/// (RFC 9639 §8.7). This is the inverse of [`parse_cuesheet`]: feeding
/// the output back through `parse_cuesheet` returns an equal `CueSheet`.
///
/// The function is **structural**: it writes whatever the caller put in
/// the struct. It does *not* enforce CD-DA semantics (Red Book limits
/// like "track numbers 1..=99 plus 170 lead-out", "offsets multiple of
/// 588 samples", "track 1 INDEX 01 at offset 0") because non-CD-DA
/// cuesheets legitimately violate them. Callers that need those
/// invariants should validate before encoding.
///
/// The on-wire reserved spans (258 bytes after the flags byte, 13 bytes
/// per track, 3 bytes per index point) are emitted as zeros so the
/// strict reserved-bit checks in `parse_cuesheet` accept the result. The
/// flag bytes likewise leave their reserved low bits as zero.
///
/// Returns `Error::invalid` when:
/// * the track list is empty (the spec requires at least the lead-out),
/// * the track list contains more than `u8::MAX` (255) entries (the on-
///   wire `num_tracks` field is a single byte),
/// * any non-trailing track declares zero index points (only the lead-
///   out is permitted to do so),
/// * any track declares more than `u8::MAX` index points (the on-wire
///   `num_indices` field is a single byte),
/// * any track carries track number 0 (reserved by Red Book for the
///   lead-in slot, which is not represented inside the cuesheet),
/// * the media catalog number contains a non-ASCII / non-printable byte
///   in the leading run (every byte before the first NUL terminator
///   must be in `0x20..=0x7E` per spec).
pub fn write_cuesheet(cs: &CueSheet) -> Result<Vec<u8>> {
    if cs.tracks.is_empty() {
        return Err(Error::invalid(
            "FLAC CUESHEET: track list must contain at least the lead-out",
        ));
    }
    if cs.tracks.len() > u8::MAX as usize {
        return Err(Error::invalid(format!(
            "FLAC CUESHEET: track count {} exceeds u8 (255)",
            cs.tracks.len()
        )));
    }
    // Per RFC 9639 §8.7 the media catalog number is ASCII printable
    // (0x20-0x7E) up to the first 0x00 terminator. Anything after the
    // first NUL is ignored on the wire, but we still refuse to emit a
    // catalog with non-printable bytes in the leading run because that
    // would round-trip through a reader that rejects the same bytes —
    // and we don't want the writer producing payloads our own reader
    // would reject.
    let leading = cs
        .media_catalog_number
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(cs.media_catalog_number.len());
    for &b in &cs.media_catalog_number[..leading] {
        if !(0x20..=0x7E).contains(&b) {
            return Err(Error::invalid(format!(
                "FLAC CUESHEET: media catalog byte 0x{b:02X} is non-printable ASCII"
            )));
        }
    }

    // Pre-size the output: 396-byte fixed prefix + per-track payload.
    const FIXED_PREFIX: usize = 128 + 8 + 1 + 258 + 1;
    const TRACK_PREFIX: usize = 8 + 1 + 12 + 1 + 13 + 1;
    const INDEX_SIZE: usize = 8 + 1 + 3;
    let mut total = FIXED_PREFIX;
    for t in &cs.tracks {
        total += TRACK_PREFIX + t.indices.len() * INDEX_SIZE;
    }
    let mut out = Vec::with_capacity(total);

    out.extend_from_slice(&cs.media_catalog_number);
    out.extend_from_slice(&cs.lead_in_samples.to_be_bytes());
    out.push(if cs.is_cdda { 0x80 } else { 0x00 });
    out.extend_from_slice(&[0u8; 258]);
    out.push(cs.tracks.len() as u8);

    for (idx, t) in cs.tracks.iter().enumerate() {
        if t.number == 0 {
            return Err(Error::invalid(
                "FLAC CUESHEET: track number 0 is reserved (lead-in only)",
            ));
        }
        let is_last = idx + 1 == cs.tracks.len();
        if !is_last && t.indices.is_empty() {
            return Err(Error::invalid(
                "FLAC CUESHEET: only the lead-out track may have zero indices",
            ));
        }
        if t.indices.len() > u8::MAX as usize {
            return Err(Error::invalid(format!(
                "FLAC CUESHEET: index count {} exceeds u8 (255)",
                t.indices.len()
            )));
        }
        out.extend_from_slice(&t.offset_samples.to_be_bytes());
        out.push(t.number);
        out.extend_from_slice(&t.isrc);
        // Track flag byte: bit 7 = non-audio, bit 6 = pre-emphasis,
        // bits 0..=5 reserved (zero). `is_audio == true` means bit 7
        // stays clear.
        let mut flags = 0u8;
        if !t.is_audio {
            flags |= 0x80;
        }
        if t.pre_emphasis {
            flags |= 0x40;
        }
        out.push(flags);
        out.extend_from_slice(&[0u8; 13]);
        out.push(t.indices.len() as u8);
        for ix in &t.indices {
            out.extend_from_slice(&ix.offset_samples.to_be_bytes());
            out.push(ix.number);
            out.extend_from_slice(&[0u8; 3]);
        }
    }

    debug_assert_eq!(out.len(), total);
    Ok(out)
}

/// A single entry in a FLAC SEEKTABLE (§SEEKPOINT). Placeholder
/// entries (`sample_number == 0xFFFF_FFFF_FFFF_FFFF`) are filtered
/// out at parse time so callers never see them.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SeekPoint {
    /// Sample number of the first sample in the target frame.
    pub sample_number: u64,
    /// Byte offset, from the first byte of the first frame in the
    /// stream, of the target frame.
    pub offset: u64,
    /// Number of samples in the target frame.
    pub frame_samples: u16,
}

/// Placeholder seek point sentinel value for the `sample_number` field
/// (RFC 9639 §8.5.1). Placeholders carry undefined `offset` /
/// `frame_samples` values and MUST appear only at the end of the seek
/// table.
pub const SEEK_POINT_PLACEHOLDER: u64 = 0xFFFF_FFFF_FFFF_FFFF;

/// Parse a FLAC SEEKTABLE block payload into a vector of valid
/// (non-placeholder) seek points. Each wire-format SEEKPOINT is 18
/// bytes: `sample_number: u64 BE`, `offset: u64 BE`, `frame_samples:
/// u16 BE`. A payload whose length isn't a multiple of 18 has its
/// trailing bytes ignored (matches the tolerance shown by typical FLAC files in the wild).
pub fn parse_seektable(bytes: &[u8]) -> Vec<SeekPoint> {
    let n = bytes.len() / 18;
    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let off = i * 18;
        let sample_number = u64::from_be_bytes(bytes[off..off + 8].try_into().expect("8 bytes"));
        if sample_number == SEEK_POINT_PLACEHOLDER {
            continue;
        }
        let offset = u64::from_be_bytes(bytes[off + 8..off + 16].try_into().expect("8 bytes"));
        let frame_samples = u16::from_be_bytes([bytes[off + 16], bytes[off + 17]]);
        out.push(SeekPoint {
            sample_number,
            offset,
            frame_samples,
        });
    }
    out
}

/// Serialise a slice of [`SeekPoint`]s back into a RFC 9639 §8.5.1
/// SEEKTABLE block payload, optionally padding the tail with
/// `placeholder_count` placeholder entries (`sample_number =
/// 0xFFFF_FFFF_FFFF_FFFF`) so callers can pre-reserve space for
/// future seek-point insertions without rewriting the surrounding
/// metadata chain.
///
/// Each seek point serialises to 18 bytes: `sample_number: u64 BE`,
/// `offset: u64 BE`, `frame_samples: u16 BE`. Placeholder entries
/// use the same 18-byte shape with all other fields zeroed (the spec
/// leaves the two trailing fields undefined for placeholders, and
/// the in-tree parser drops the entire entry on a placeholder match,
/// so the trailing-byte values are not observable).
///
/// The writer enforces every wire-format invariant the spec
/// (RFC 9639 §8.5.1) imposes on a written table:
///
/// * Real seek points must be strictly ascending by `sample_number`
///   ("Seek points within a table MUST be sorted in ascending order
///   by sample number" plus "MUST be unique by sample number ...
///   with the exception of placeholder points"). The strict ordering
///   collapses the two MUSTs into a single check.
/// * No real point may carry the placeholder sentinel value as its
///   `sample_number` (that field is reserved for placeholder slots
///   and the parser uses it as the placeholder discriminator).
/// * Placeholders are only emitted at the tail, after every real
///   point ("any number of placeholder points, but they MUST all
///   occur at the end of the table"). The caller supplies the count
///   separately so the real-points slice cannot accidentally smuggle
///   in a placeholder entry mid-table.
///
/// Round-trips through [`parse_seektable`] losslessly: the parser
/// drops the placeholder tail (as it must per spec), so a writer
/// invocation followed by a parser invocation returns the original
/// real-points slice unchanged.
pub fn write_seektable(real_points: &[SeekPoint], placeholder_count: usize) -> Result<Vec<u8>> {
    // Per-point sanity: no real point may shadow the placeholder
    // sentinel, otherwise the parser would silently drop it.
    for sp in real_points {
        if sp.sample_number == SEEK_POINT_PLACEHOLDER {
            return Err(Error::invalid(
                "FLAC SEEKTABLE: real seek point uses the placeholder \
                 sample number sentinel (0xFFFF_FFFF_FFFF_FFFF)",
            ));
        }
    }
    // RFC 9639 §8.5.1: real points MUST be sorted ascending by
    // sample_number AND MUST be unique by sample_number. Strict
    // ordering covers both, since `<` rejects equality.
    for pair in real_points.windows(2) {
        if pair[0].sample_number >= pair[1].sample_number {
            return Err(Error::invalid(format!(
                "FLAC SEEKTABLE: seek points must be strictly ascending \
                 by sample_number; saw {} then {}",
                pair[0].sample_number, pair[1].sample_number
            )));
        }
    }
    // The metadata block header carries the payload length as a
    // u24 BE field (max 16 MiB - 1). Each seek point is 18 bytes, so
    // the absolute hard cap is (2^24 - 1) / 18 ≈ 932 067 entries.
    // We use a slightly tighter `u32` ceiling for the multiplication
    // itself; the block header validation downstream catches anything
    // closer to the spec limit.
    let total_points = real_points
        .len()
        .checked_add(placeholder_count)
        .ok_or_else(|| Error::invalid("FLAC SEEKTABLE: total seek-point count overflows usize"))?;
    let total_bytes = total_points
        .checked_mul(18)
        .ok_or_else(|| Error::invalid("FLAC SEEKTABLE: total payload size overflows usize"))?;

    let mut out = Vec::with_capacity(total_bytes);
    for sp in real_points {
        out.extend_from_slice(&sp.sample_number.to_be_bytes());
        out.extend_from_slice(&sp.offset.to_be_bytes());
        out.extend_from_slice(&sp.frame_samples.to_be_bytes());
    }
    // Each placeholder serialises as: u64 sentinel + 10 zero bytes
    // (u64 offset + u16 frame_samples). Per spec the trailing fields
    // are undefined for placeholders, so we deterministically zero
    // them out — both for byte-stable round-trips and so the in-tree
    // parser (which short-circuits on the sentinel) ignores them
    // regardless of value.
    for _ in 0..placeholder_count {
        out.extend_from_slice(&SEEK_POINT_PLACEHOLDER.to_be_bytes());
        out.extend_from_slice(&[0u8; 10]);
    }

    debug_assert_eq!(out.len(), total_bytes);
    Ok(out)
}

impl StreamInfo {
    /// Parse a STREAMINFO block. The block payload is exactly 34 bytes.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < 34 {
            return Err(Error::invalid("FLAC STREAMINFO block must be 34 bytes"));
        }
        let min_block_size = u16::from_be_bytes([bytes[0], bytes[1]]);
        let max_block_size = u16::from_be_bytes([bytes[2], bytes[3]]);
        let min_frame_size =
            ((bytes[4] as u32) << 16) | ((bytes[5] as u32) << 8) | (bytes[6] as u32);
        let max_frame_size =
            ((bytes[7] as u32) << 16) | ((bytes[8] as u32) << 8) | (bytes[9] as u32);
        // Bytes 10..18: 64 packed bits — sample_rate(20), channels-1(3), bps-1(5), total_samples(36).
        let packed = u64::from_be_bytes(bytes[10..18].try_into().expect("8 bytes"));
        let sample_rate = ((packed >> 44) & 0x000F_FFFF) as u32;
        let channels = (((packed >> 41) & 0x07) as u8) + 1;
        let bits_per_sample = (((packed >> 36) & 0x1F) as u8) + 1;
        let total_samples = packed & 0x0000_000F_FFFF_FFFF;
        let mut md5 = [0u8; 16];
        md5.copy_from_slice(&bytes[18..34]);
        Ok(Self {
            min_block_size,
            max_block_size,
            min_frame_size,
            max_frame_size,
            sample_rate,
            channels,
            bits_per_sample,
            total_samples,
            md5,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_streaminfo_round_trip() {
        // Build a STREAMINFO block by hand: sr=48000, ch=2, bps=16, total=96000.
        let mut block = vec![0u8; 34];
        block[0..2].copy_from_slice(&4096u16.to_be_bytes()); // min block size
        block[2..4].copy_from_slice(&4096u16.to_be_bytes()); // max block size
                                                             // min/max frame size: 0 (unknown) — leave zeros.
                                                             // packed: sample_rate(20)=48000, channels-1(3)=1, bps-1(5)=15, total(36)=96000
        let packed: u64 = (48_000u64 << 44) | (1u64 << 41) | (15u64 << 36) | 96_000u64;
        block[10..18].copy_from_slice(&packed.to_be_bytes());
        // md5 left as zeros.
        let info = StreamInfo::parse(&block).unwrap();
        assert_eq!(info.sample_rate, 48_000);
        assert_eq!(info.channels, 2);
        assert_eq!(info.bits_per_sample, 16);
        assert_eq!(info.total_samples, 96_000);
    }

    #[test]
    fn seektable_parses_and_drops_placeholders() {
        let mut buf = Vec::new();
        // real point: sample 0, offset 0, frame_samples 4096
        buf.extend_from_slice(&0u64.to_be_bytes());
        buf.extend_from_slice(&0u64.to_be_bytes());
        buf.extend_from_slice(&4096u16.to_be_bytes());
        // placeholder
        buf.extend_from_slice(&0xFFFF_FFFF_FFFF_FFFFu64.to_be_bytes());
        buf.extend_from_slice(&0u64.to_be_bytes());
        buf.extend_from_slice(&0u16.to_be_bytes());
        // real point: sample 8192, offset 1234, frame_samples 4096
        buf.extend_from_slice(&8192u64.to_be_bytes());
        buf.extend_from_slice(&1234u64.to_be_bytes());
        buf.extend_from_slice(&4096u16.to_be_bytes());

        let sps = parse_seektable(&buf);
        assert_eq!(sps.len(), 2);
        assert_eq!(sps[0].sample_number, 0);
        assert_eq!(sps[0].offset, 0);
        assert_eq!(sps[0].frame_samples, 4096);
        assert_eq!(sps[1].sample_number, 8192);
        assert_eq!(sps[1].offset, 1234);
    }

    #[test]
    fn block_header_decodes_last_flag() {
        let h = BlockHeader::parse(&[0x80, 0x00, 0x00, 0x22]).unwrap();
        assert!(h.last);
        assert_eq!(h.block_type, BlockType::StreamInfo);
        assert_eq!(h.length, 34);
    }

    /// Build a synthetic CUESHEET payload with `tracks` entries. The
    /// last entry is always treated as the lead-out (zero indices).
    /// Each non-lead-out track gets one INDEX 01 at offset 0. This
    /// helper exists only for the unit tests — the demuxer always
    /// reads its byte stream from a real metadata block.
    fn synth_cuesheet(mcn: &[u8], is_cdda: bool, tracks: &[(u64, u8, u8)]) -> Vec<u8> {
        // tracks: (offset_samples, number, num_indices)
        let mut buf = Vec::new();
        let mut catalog = [0u8; 128];
        catalog[..mcn.len().min(128)].copy_from_slice(&mcn[..mcn.len().min(128)]);
        buf.extend_from_slice(&catalog);
        buf.extend_from_slice(&0u64.to_be_bytes()); // lead-in samples
        buf.push(if is_cdda { 0x80 } else { 0x00 }); // flags
        buf.extend_from_slice(&[0u8; 258]); // reserved
        buf.push(tracks.len() as u8); // num_tracks
        for &(offset, number, num_indices) in tracks {
            buf.extend_from_slice(&offset.to_be_bytes());
            buf.push(number);
            buf.extend_from_slice(&[0u8; 12]); // ISRC
            buf.push(0); // track flags: audio, no pre-emphasis
            buf.extend_from_slice(&[0u8; 13]); // reserved
            buf.push(num_indices);
            for ix in 0..num_indices {
                buf.extend_from_slice(&0u64.to_be_bytes()); // index offset
                buf.push(ix + 1); // index number (1-based)
                buf.extend_from_slice(&[0u8; 3]); // reserved
            }
        }
        buf
    }

    #[test]
    fn cuesheet_parses_two_tracks_plus_leadout() {
        // Real-shape: track 1 with one INDEX, track 2 with one INDEX,
        // then a lead-out (zero indices). For CD-DA the lead-out track
        // number is 170.
        let payload = synth_cuesheet(
            b"0123456789012",
            true,
            &[
                (0, 1, 1),                 // track 1
                (44_100 * 60 * 3, 2, 1),   // track 2 at 3:00
                (44_100 * 60 * 5, 170, 0), // lead-out at 5:00 (CD-DA)
            ],
        );
        let cs = parse_cuesheet(&payload).unwrap();
        assert!(cs.is_cdda);
        assert_eq!(cs.tracks.len(), 3);
        assert_eq!(cs.tracks[0].number, 1);
        assert_eq!(cs.tracks[0].indices.len(), 1);
        assert_eq!(cs.tracks[1].number, 2);
        assert_eq!(cs.tracks[1].offset_samples, 44_100 * 60 * 3);
        assert_eq!(cs.tracks[2].number, 170);
        assert_eq!(cs.tracks[2].indices.len(), 0);
        // Media catalog preserved verbatim, zero-padded.
        assert_eq!(&cs.media_catalog_number[..13], b"0123456789012");
        assert!(cs.media_catalog_number[13..].iter().all(|&b| b == 0));
    }

    #[test]
    fn cuesheet_rejects_track_number_zero() {
        let payload = synth_cuesheet(b"", false, &[(0, 0, 1), (1000, 255, 0)]);
        assert!(parse_cuesheet(&payload).is_err());
    }

    #[test]
    fn cuesheet_rejects_zero_indices_on_non_leadout() {
        // First track with zero indices is illegal: only the lead-out
        // (last entry) may carry zero indices.
        let payload = synth_cuesheet(b"", false, &[(0, 1, 0), (1000, 255, 0)]);
        assert!(parse_cuesheet(&payload).is_err());
    }

    #[test]
    fn cuesheet_rejects_short_payload() {
        // Anything below the 396-byte fixed prefix must fail.
        assert!(parse_cuesheet(&[]).is_err());
        assert!(parse_cuesheet(&[0u8; 395]).is_err());
    }

    #[test]
    fn cuesheet_rejects_zero_tracks() {
        let mut buf = vec![0u8; 396];
        // num_tracks at offset 395 is already 0 — must reject.
        assert!(parse_cuesheet(&buf).is_err());
        // Setting num_tracks = 1 but truncating before the track itself
        // must also fail (overrun check).
        buf[395] = 1;
        assert!(parse_cuesheet(&buf).is_err());
    }

    #[test]
    fn cuesheet_rejects_nonzero_reserved_bits() {
        let mut payload = synth_cuesheet(b"", false, &[(0, 1, 1), (1000, 255, 0)]);
        // Corrupt the 258-byte reserved region (right after flags).
        payload[137 + 5] = 0x01;
        assert!(parse_cuesheet(&payload).is_err());
    }

    #[test]
    fn cuesheet_round_trip_cdda_three_tracks() {
        // Build a realistic CD-DA cuesheet (two audio tracks + lead-out),
        // serialise, re-parse, and assert structural equality. Track 1
        // has a pre-gap INDEX 00 + main INDEX 01; track 2 carries an
        // ISRC. This exercises every non-empty field on the writer
        // side.
        let isrc_track2 = *b"GBAYE0601477";
        let cs_in = CueSheet {
            media_catalog_number: {
                let mut m = [0u8; 128];
                m[..13].copy_from_slice(b"0123456789012");
                m
            },
            lead_in_samples: 88_200, // 2 sec @ 44.1 kHz pre-gap
            is_cdda: true,
            tracks: vec![
                CueSheetTrack {
                    offset_samples: 0,
                    number: 1,
                    isrc: [0u8; 12],
                    is_audio: true,
                    pre_emphasis: false,
                    indices: vec![
                        CueSheetIndex {
                            offset_samples: 0,
                            number: 0,
                        },
                        CueSheetIndex {
                            offset_samples: 588 * 150, // 2 sec into the track
                            number: 1,
                        },
                    ],
                },
                CueSheetTrack {
                    offset_samples: 44_100 * 60 * 3,
                    number: 2,
                    isrc: isrc_track2,
                    is_audio: true,
                    pre_emphasis: true,
                    indices: vec![CueSheetIndex {
                        offset_samples: 0,
                        number: 1,
                    }],
                },
                CueSheetTrack {
                    offset_samples: 44_100 * 60 * 5,
                    number: 170,
                    isrc: [0u8; 12],
                    is_audio: true,
                    pre_emphasis: false,
                    indices: vec![],
                },
            ],
        };
        let bytes = write_cuesheet(&cs_in).unwrap();
        let cs_out = parse_cuesheet(&bytes).unwrap();
        assert_eq!(cs_in, cs_out);
        // Spot-check on-wire structure: fixed prefix + 2×(36+12) +
        // 2×(36+24) - wait: track 1 has 2 indices, track 2 has 1, lead-
        // out has 0 → 396 + (36+24) + (36+12) + 36 = 540.
        assert_eq!(bytes.len(), 396 + (36 + 24) + (36 + 12) + 36);
    }

    #[test]
    fn cuesheet_round_trip_non_cdda_single_leadout() {
        // Minimum legal cuesheet: a single lead-out track, no pre-gap
        // catalog. Confirms the writer accepts lead-out-only payloads
        // (so does the parser; this exercises the cs.tracks.len()==1
        // edge case end-to-end).
        let cs_in = CueSheet {
            media_catalog_number: [0u8; 128],
            lead_in_samples: 0,
            is_cdda: false,
            tracks: vec![CueSheetTrack {
                offset_samples: 1_000_000,
                number: 255, // non-CD-DA lead-out marker
                isrc: [0u8; 12],
                is_audio: true,
                pre_emphasis: false,
                indices: vec![],
            }],
        };
        let bytes = write_cuesheet(&cs_in).unwrap();
        assert_eq!(bytes.len(), 396 + 36);
        // Flag byte should have bit 7 clear (non-CD-DA) and reserved
        // bits zero.
        assert_eq!(bytes[136], 0x00);
        // The 258-byte reserved span must be all zero.
        assert!(bytes[137..395].iter().all(|&b| b == 0));
        let cs_out = parse_cuesheet(&bytes).unwrap();
        assert_eq!(cs_in, cs_out);
    }

    #[test]
    fn cuesheet_writer_rejects_empty_track_list() {
        let cs = CueSheet {
            media_catalog_number: [0u8; 128],
            lead_in_samples: 0,
            is_cdda: false,
            tracks: vec![],
        };
        assert!(write_cuesheet(&cs).is_err());
    }

    #[test]
    fn cuesheet_writer_rejects_track_zero() {
        let cs = CueSheet {
            media_catalog_number: [0u8; 128],
            lead_in_samples: 0,
            is_cdda: false,
            tracks: vec![CueSheetTrack {
                offset_samples: 0,
                number: 0, // reserved for lead-in, illegal here
                isrc: [0u8; 12],
                is_audio: true,
                pre_emphasis: false,
                indices: vec![],
            }],
        };
        assert!(write_cuesheet(&cs).is_err());
    }

    #[test]
    fn cuesheet_writer_rejects_zero_indices_on_non_leadout() {
        let cs = CueSheet {
            media_catalog_number: [0u8; 128],
            lead_in_samples: 0,
            is_cdda: false,
            tracks: vec![
                CueSheetTrack {
                    offset_samples: 0,
                    number: 1,
                    isrc: [0u8; 12],
                    is_audio: true,
                    pre_emphasis: false,
                    indices: vec![], // illegal: not the lead-out
                },
                CueSheetTrack {
                    offset_samples: 1000,
                    number: 255,
                    isrc: [0u8; 12],
                    is_audio: true,
                    pre_emphasis: false,
                    indices: vec![],
                },
            ],
        };
        assert!(write_cuesheet(&cs).is_err());
    }

    #[test]
    fn cuesheet_writer_rejects_non_printable_catalog_byte() {
        let mut m = [0u8; 128];
        m[..3].copy_from_slice(b"\x01AB"); // 0x01 is non-printable
        let cs = CueSheet {
            media_catalog_number: m,
            lead_in_samples: 0,
            is_cdda: false,
            tracks: vec![CueSheetTrack {
                offset_samples: 0,
                number: 255,
                isrc: [0u8; 12],
                is_audio: true,
                pre_emphasis: false,
                indices: vec![],
            }],
        };
        assert!(write_cuesheet(&cs).is_err());
    }

    #[test]
    fn cuesheet_writer_emits_track_flag_bits_correctly() {
        // Audio with pre-emphasis: bit 6 set, bit 7 clear → 0x40.
        // Non-audio (data) with no pre-emphasis: bit 7 set → 0x80.
        let cs = CueSheet {
            media_catalog_number: [0u8; 128],
            lead_in_samples: 0,
            is_cdda: true,
            tracks: vec![
                CueSheetTrack {
                    offset_samples: 0,
                    number: 1,
                    isrc: [0u8; 12],
                    is_audio: true,
                    pre_emphasis: true,
                    indices: vec![CueSheetIndex {
                        offset_samples: 0,
                        number: 1,
                    }],
                },
                CueSheetTrack {
                    offset_samples: 1000,
                    number: 2,
                    isrc: [0u8; 12],
                    is_audio: false,
                    pre_emphasis: false,
                    indices: vec![CueSheetIndex {
                        offset_samples: 0,
                        number: 1,
                    }],
                },
                CueSheetTrack {
                    offset_samples: 2000,
                    number: 170,
                    isrc: [0u8; 12],
                    is_audio: true,
                    pre_emphasis: false,
                    indices: vec![],
                },
            ],
        };
        let bytes = write_cuesheet(&cs).unwrap();
        // Track 1 flag byte sits at offset 396 + 8 + 1 + 12 = 417.
        assert_eq!(bytes[396 + 21], 0x40);
        // Track 2 flag byte sits one full track-record later: 417 +
        // 36-byte prefix + 12-byte single index = 465.
        assert_eq!(bytes[396 + 36 + 12 + 21], 0x80);
        // Round-trip preserves the flags.
        let cs2 = parse_cuesheet(&bytes).unwrap();
        assert!(cs2.tracks[0].pre_emphasis);
        assert!(cs2.tracks[0].is_audio);
        assert!(!cs2.tracks[1].pre_emphasis);
        assert!(!cs2.tracks[1].is_audio);
    }

    #[test]
    fn cuesheet_round_trip_synthetic_parsed_bytes() {
        // Take the synth_cuesheet helper's output, parse it, write it
        // back, and confirm the writer reproduces the exact same bytes.
        // This pins the writer to the parser's understanding of the
        // wire format byte-for-byte.
        let original = synth_cuesheet(
            b"ABCDEFGHIJKLM",
            true,
            &[(0, 1, 2), (44_100 * 60, 2, 1), (44_100 * 120, 170, 0)],
        );
        let cs = parse_cuesheet(&original).unwrap();
        let reemitted = write_cuesheet(&cs).unwrap();
        assert_eq!(original, reemitted);
    }

    #[test]
    fn cuesheet_does_not_panic_on_random_input() {
        // Sweep payloads of various lengths near the fixed-prefix
        // boundary to confirm we never panic on malformed input —
        // every result must be an explicit Err.
        for len in [0, 100, 395, 396, 397, 500, 1024] {
            let buf = vec![0xABu8; len];
            // 0xAB has high bits set in the reserved span / track
            // number bytes etc., so every length must produce Err. We
            // care here only that no panic escapes.
            let _ = parse_cuesheet(&buf);
        }
    }

    #[test]
    fn seektable_writer_round_trips_three_real_points() {
        // Three ordered seek points + no placeholders. Confirm the
        // writer reproduces the byte layout the parser already
        // recognised in `seektable_parses_and_drops_placeholders`.
        let points = vec![
            SeekPoint {
                sample_number: 0,
                offset: 0,
                frame_samples: 4096,
            },
            SeekPoint {
                sample_number: 4096,
                offset: 1500,
                frame_samples: 4096,
            },
            SeekPoint {
                sample_number: 8192,
                offset: 1234,
                frame_samples: 4096,
            },
        ];
        let bytes = write_seektable(&points, 0).expect("writer");
        assert_eq!(bytes.len(), 3 * 18);
        let parsed = parse_seektable(&bytes);
        assert_eq!(parsed, points);
    }

    #[test]
    fn seektable_writer_emits_placeholder_tail_byte_for_byte() {
        // One real point + two placeholders. Each placeholder must be
        // the 8-byte sentinel followed by ten zero bytes, and the
        // parser must drop both of them.
        let points = vec![SeekPoint {
            sample_number: 0,
            offset: 0,
            frame_samples: 4096,
        }];
        let bytes = write_seektable(&points, 2).expect("writer");
        assert_eq!(bytes.len(), 3 * 18);
        // First 18 bytes: the real point. Bytes 8..16 = offset (0), 16..18 = frame_samples (4096).
        assert_eq!(&bytes[0..8], &0u64.to_be_bytes());
        assert_eq!(&bytes[8..16], &0u64.to_be_bytes());
        assert_eq!(&bytes[16..18], &4096u16.to_be_bytes());
        // Placeholder 1
        assert_eq!(&bytes[18..26], &SEEK_POINT_PLACEHOLDER.to_be_bytes());
        assert!(bytes[26..36].iter().all(|&b| b == 0));
        // Placeholder 2
        assert_eq!(&bytes[36..44], &SEEK_POINT_PLACEHOLDER.to_be_bytes());
        assert!(bytes[44..54].iter().all(|&b| b == 0));
        // Parser drops both placeholders.
        let parsed = parse_seektable(&bytes);
        assert_eq!(parsed, points);
    }

    #[test]
    fn seektable_writer_accepts_empty_input() {
        // An empty (all-placeholder, or fully-empty) table is a valid
        // on-wire encoding — RFC 9639 §8.5 explicitly allows "zero or
        // more seek points". Confirm both fully-empty and
        // placeholders-only.
        let none = write_seektable(&[], 0).expect("zero entries");
        assert!(none.is_empty());
        assert_eq!(parse_seektable(&none), vec![]);
        let placeholders = write_seektable(&[], 4).expect("placeholders only");
        assert_eq!(placeholders.len(), 4 * 18);
        // Every placeholder slot carries the sentinel in the first
        // 8 bytes followed by ten zero bytes.
        for i in 0..4 {
            let off = i * 18;
            assert_eq!(
                &placeholders[off..off + 8],
                &SEEK_POINT_PLACEHOLDER.to_be_bytes()
            );
            assert!(placeholders[off + 8..off + 18].iter().all(|&b| b == 0));
        }
        // Parser drops every placeholder.
        assert_eq!(parse_seektable(&placeholders), vec![]);
    }

    #[test]
    fn seektable_writer_rejects_non_ascending_points() {
        // Equal sample numbers (RFC 9639 §8.5.1 "MUST be unique").
        let equal = vec![
            SeekPoint {
                sample_number: 4096,
                offset: 0,
                frame_samples: 4096,
            },
            SeekPoint {
                sample_number: 4096,
                offset: 1500,
                frame_samples: 4096,
            },
        ];
        assert!(write_seektable(&equal, 0).is_err());
        // Descending sample numbers (RFC 9639 §8.5.1 "MUST be sorted
        // in ascending order").
        let descending = vec![
            SeekPoint {
                sample_number: 8192,
                offset: 0,
                frame_samples: 4096,
            },
            SeekPoint {
                sample_number: 4096,
                offset: 1500,
                frame_samples: 4096,
            },
        ];
        assert!(write_seektable(&descending, 0).is_err());
    }

    #[test]
    fn seektable_writer_rejects_placeholder_sentinel_as_real_point() {
        // A real seek point can't carry the 0xFFFF... sample number;
        // the parser would silently drop it as a placeholder.
        let bad = vec![SeekPoint {
            sample_number: SEEK_POINT_PLACEHOLDER,
            offset: 0,
            frame_samples: 4096,
        }];
        assert!(write_seektable(&bad, 0).is_err());
    }

    #[test]
    fn seektable_round_trip_through_parser_preserves_real_points() {
        // Build a SEEKTABLE by hand (mixing real points with a
        // placeholder), parse it, re-serialise with the same placeholder
        // count, and confirm a second parse pass round-trips bit-for-bit.
        // This pins the writer to the parser's exact wire-format
        // understanding, the same way the cuesheet
        // `cuesheet_round_trip_synthetic_parsed_bytes` test does.
        let mut original = Vec::new();
        // Real point at sample 0
        original.extend_from_slice(&0u64.to_be_bytes());
        original.extend_from_slice(&0u64.to_be_bytes());
        original.extend_from_slice(&4096u16.to_be_bytes());
        // Real point at sample 4096
        original.extend_from_slice(&4096u64.to_be_bytes());
        original.extend_from_slice(&1500u64.to_be_bytes());
        original.extend_from_slice(&4096u16.to_be_bytes());
        // Placeholder at end (sentinel + 10 zero bytes — the same shape
        // the writer emits).
        original.extend_from_slice(&SEEK_POINT_PLACEHOLDER.to_be_bytes());
        original.extend_from_slice(&[0u8; 10]);

        let parsed = parse_seektable(&original);
        assert_eq!(parsed.len(), 2);
        let reemitted = write_seektable(&parsed, 1).expect("writer");
        assert_eq!(reemitted, original);
        let reparsed = parse_seektable(&reemitted);
        assert_eq!(reparsed, parsed);
    }
}
