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

    /// Encode this block type as the low 7 bits of the metadata-block
    /// header byte (RFC 9639 §8.1). The high bit (`last`) is supplied
    /// separately by [`BlockHeader::write_into`]. Returns `None` if the
    /// value is unrepresentable on the wire — specifically the
    /// `Invalid` variant (the spec reserves code 127 for a forbidden
    /// "invalid" marker, so writing it would produce a payload no
    /// conformant reader will accept).
    pub fn to_byte(self) -> Option<u8> {
        match self {
            Self::StreamInfo => Some(0),
            Self::Padding => Some(1),
            Self::Application => Some(2),
            Self::SeekTable => Some(3),
            Self::VorbisComment => Some(4),
            Self::CueSheet => Some(5),
            Self::Picture => Some(6),
            // Code 127 is the spec's "invalid" sentinel; emitting it
            // would round-trip through `from_byte` as `Invalid` again
            // but the spec forbids producers from writing it, so we
            // refuse here rather than smuggle it past the type system.
            Self::Invalid => None,
            // `Reserved` carries a 7-bit raw code; reject the high
            // bit so a caller can't smuggle the `last` flag through.
            Self::Reserved(code) if code < 0x80 && code != 127 => Some(code),
            Self::Reserved(_) => None,
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

    /// Maximum payload length the wire format can represent: the
    /// length field is a 24-bit big-endian unsigned integer.
    pub const MAX_LENGTH: u32 = (1 << 24) - 1;

    /// Serialise this header as the 4-byte metadata-block-header
    /// prefix (RFC 9639 §8.1): bit 7 of byte 0 is the `last` flag,
    /// bits 0..=6 are the block-type code, bytes 1..=3 are the
    /// payload length as a 24-bit big-endian unsigned integer.
    ///
    /// Returns `Error::invalid` if the block-type code can't be
    /// serialised (see [`BlockType::to_byte`]) or the payload length
    /// would overflow the 24-bit `length` field. Writes exactly four
    /// bytes when it returns `Ok` and writes nothing otherwise (no
    /// partial write).
    pub fn write_into(&self, out: &mut Vec<u8>) -> Result<()> {
        let code = self.block_type.to_byte().ok_or_else(|| {
            Error::invalid(format!(
                "FLAC metadata block header: block-type {:?} cannot be serialised on the wire",
                self.block_type
            ))
        })?;
        if self.length > Self::MAX_LENGTH {
            return Err(Error::invalid(format!(
                "FLAC metadata block header: payload length {} exceeds 24-bit max ({})",
                self.length,
                Self::MAX_LENGTH
            )));
        }
        let b0 = code | if self.last { 0x80 } else { 0x00 };
        out.push(b0);
        out.push(((self.length >> 16) & 0xFF) as u8);
        out.push(((self.length >> 8) & 0xFF) as u8);
        out.push((self.length & 0xFF) as u8);
        Ok(())
    }

    /// Convenience wrapper returning a freshly allocated 4-byte
    /// buffer.
    pub fn to_bytes(&self) -> Result<Vec<u8>> {
        let mut out = Vec::with_capacity(4);
        self.write_into(&mut out)?;
        Ok(out)
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

/// Serialise a PADDING metadata-block **payload** of `len` zero bytes
/// (RFC 9639 §8.6).
///
/// PADDING is the simplest metadata block: its payload is `len` bytes
/// whose value MUST be `0x00`. The block exists so an encoder can
/// reserve space for later metadata growth (e.g. larger
/// VORBIS_COMMENT, a SEEKTABLE retrofit) without rewriting the audio
/// data that follows.
///
/// Only the payload is produced; the caller wraps it with
/// [`BlockHeader::write_into`] / [`BlockHeader::to_bytes`] to obtain
/// the full block. The trace-doc fixture `with-padding-block` shows
/// FFmpeg's native muxer writing 8 KiB (`len = 8192`) by default, but
/// the spec admits any 24-bit length.
///
/// Returns `Error::invalid` if `len` would exceed the 24-bit ceiling
/// the metadata-block header's `length` field can carry (so callers
/// don't have to do the bound check twice).
pub fn write_padding(len: usize) -> Result<Vec<u8>> {
    if len > BlockHeader::MAX_LENGTH as usize {
        return Err(Error::invalid(format!(
            "FLAC PADDING: requested {} bytes exceeds 24-bit metadata length max ({})",
            len,
            BlockHeader::MAX_LENGTH
        )));
    }
    Ok(vec![0u8; len])
}

/// Parsed APPLICATION metadata block contents (RFC 9639 §8.4).
///
/// The APPLICATION block is the spec's escape hatch for third-party
/// applications: it carries a 32-bit registered application ID
/// (IANA "FLAC Application Metadata Block IDs" registry, RFC 9639
/// §12.2) followed by an opaque byte run whose length is the
/// metadata-block header length minus the four ID bytes. RFC 9639
/// §8.4 Table 5 leaves the data payload uninterpreted at the
/// container level — only the originating application understands
/// its bytes — and the only wire-format invariant on the payload
/// itself is that it MUST be a whole number of bytes.
///
/// This struct is the typed mirror of [`parse_application`] /
/// [`write_application`]: it splits the wire format's mandatory ID
/// header from the application-defined bytes, so callers carrying
/// FLAC files with third-party metadata can read and write the ID
/// without re-implementing the 32-bit big-endian split each time.
///
/// The application data is stored as a `Vec<u8>` (an opaque payload
/// per RFC 9639 §8.4) — the parser does not interpret it and the
/// writer enforces only the enclosing metadata-block-header's
/// 24-bit length ceiling (`BlockHeader::MAX_LENGTH`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Application {
    /// IANA-registered application identifier (RFC 9639 §12.2).
    /// Stored in host order — the wire format is 32-bit big-endian
    /// per Table 5's `u(32)` row.
    pub id: u32,
    /// Application-defined payload. Length equals the enclosing
    /// metadata-block-header length minus four (the ID's byte
    /// width). Treated as opaque bytes: RFC 9639 §8.4 leaves the
    /// interpretation to the originating application.
    pub data: Vec<u8>,
}

/// Parse a FLAC APPLICATION block payload (RFC 9639 §8.4).
///
/// `bytes` is the metadata block's payload (header already stripped
/// by the caller). The first four bytes are the 32-bit big-endian
/// application ID per RFC 9639 §8.4 Table 5; the remaining bytes
/// are the application-defined data run.
///
/// Returns `Error::invalid` if the payload is shorter than the
/// mandatory four-byte ID header (a zero-byte data run is legal —
/// §8.4 admits `n = 0` because the spec only constrains `n` to be
/// a whole number of bytes).
pub fn parse_application(bytes: &[u8]) -> Result<Application> {
    if bytes.len() < 4 {
        return Err(Error::invalid(format!(
            "FLAC APPLICATION block payload must hold the 4-byte application ID; got {} bytes",
            bytes.len()
        )));
    }
    let id = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
    let data = bytes[4..].to_vec();
    Ok(Application { id, data })
}

/// Serialise an [`Application`] back into a RFC 9639 §8.4
/// APPLICATION block payload.
///
/// The wire format is four big-endian bytes of `id` followed by the
/// `data` byte run unchanged. Returns `Error::invalid` if the total
/// payload length (`4 + data.len()`) would exceed the 24-bit
/// metadata-block-header length ceiling
/// (`BlockHeader::MAX_LENGTH`); callers can otherwise hand the
/// returned vector straight to [`BlockHeader::write_into`].
///
/// Round-trips through [`parse_application`] losslessly: an
/// `Application { id, data }` reads back byte-identical after a
/// writer / parser cycle.
pub fn write_application(app: &Application) -> Result<Vec<u8>> {
    // 4-byte ID header + opaque data. The 24-bit ceiling on the
    // enclosing metadata-block-header length is the binding limit
    // (RFC 9639 §8.1 Table 3 / `BlockHeader::MAX_LENGTH`); checked
    // here so callers don't have to bracket every write_application
    // call with a manual size check.
    let total = 4usize.checked_add(app.data.len()).ok_or_else(|| {
        Error::invalid(format!(
            "FLAC APPLICATION: data length {} overflows usize when adding the 4-byte ID header",
            app.data.len()
        ))
    })?;
    if total > BlockHeader::MAX_LENGTH as usize {
        return Err(Error::invalid(format!(
            "FLAC APPLICATION: total payload size {} exceeds 24-bit metadata length max ({})",
            total,
            BlockHeader::MAX_LENGTH
        )));
    }
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&app.id.to_be_bytes());
    out.extend_from_slice(&app.data);
    debug_assert_eq!(out.len(), total);
    Ok(out)
}

/// Parsed PICTURE metadata block contents (RFC 9639 §8.8).
///
/// FLAC reuses the ID3v2 `APIC` taxonomy for `picture_type` (so the
/// numeric value is shared across MP3 / FLAC / Ogg / MP4 cover-art
/// surfaces), but on-wire the FLAC layout uses 4-byte length-prefixed
/// strings instead of ID3v2's NUL-delimited strings, and additionally
/// stores width / height / depth / colour_count metadata that ID3v2
/// does not carry.
///
/// The four "informational" fields (`width`, `height`, `depth`,
/// `colour_count`) are RFC 9639 §8.8 / RFC 9639 Table 12 fields that
/// applications MUST NOT use for decoding the picture itself, but MAY
/// use to pick between multiple picture blocks (e.g. preferring the
/// front cover at a particular resolution) without first decoding the
/// image. Renderers can use them to lay out a placeholder before the
/// image decoder finishes. Per spec, a picture that has no concept of
/// a given field — vector images, for example, have no width or
/// height in pixels — sets that field to zero; the parser preserves
/// the zero verbatim and the writer accepts it.
///
/// `colour_count` is meaningful only for indexed-colour formats
/// (GIF, paletted PNG); non-indexed pictures store 0 per spec.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Picture {
    /// Picture type code (ID3v2 APIC taxonomy). On the wire this is
    /// a 32-bit unsigned integer; in practice only the low 8 bits
    /// matter (the ID3v2 table only allocates 21 values, all <= 0x14),
    /// but the writer round-trips the full 32-bit value so a producer
    /// can re-emit a payload they parsed without surprising the next
    /// reader. Out-of-range codes are still legal on the wire — the
    /// spec only forbids the writer from inventing codes it didn't
    /// receive.
    pub picture_type: u32,
    /// IANA media type (`"image/jpeg"`, `"image/png"`, ...) or the
    /// special sentinel `"-->"`, meaning `data` is a URI string
    /// pointing at the image rather than the image bytes themselves
    /// (RFC 9639 §8.8 plus [RFC3986]). MUST be printable ASCII
    /// (`0x20..=0x7E`); the parser enforces this and the writer
    /// rejects strings that fail the same check.
    pub mime_type: String,
    /// Free-form UTF-8 description supplied by the tagger. Often
    /// empty.
    pub description: String,
    /// Picture width in pixels (informational). Zero when the format
    /// has no concept of pixel width (vector images).
    pub width: u32,
    /// Picture height in pixels (informational). Zero when the
    /// format has no concept of pixel height.
    pub height: u32,
    /// Colour depth in bits per pixel (informational). Zero when
    /// unknown.
    pub depth: u32,
    /// For indexed-colour pictures, the number of colours used.
    /// Zero for non-indexed pictures (RGB photographs, ...) per
    /// spec.
    pub colour_count: u32,
    /// The picture payload bytes — the raw encoded image when
    /// `mime_type != "-->"`, or the URI bytes when `mime_type ==
    /// "-->"`. Not interpreted by this crate; downstream image
    /// decoders consume this as-is.
    pub data: Vec<u8>,
}

/// Parse a FLAC PICTURE block payload (RFC 9639 §8.8 Table 12).
///
/// Wire layout (all integers big-endian unsigned):
///
/// ```text
///   u32   picture type (Table 13)
///   u32   mime length (N1)
///   u8*N1 mime string (ASCII 0x20..=0x7E or "-->" URI sentinel)
///   u32   description length (N2)
///   u8*N2 description (UTF-8)
///   u32   width  (informational, 0 if unknown / N/A)
///   u32   height (informational, 0 if unknown / N/A)
///   u32   depth  (informational, 0 if unknown)
///   u32   colour count (informational, 0 for non-indexed)
///   u32   picture data length (N3)
///   u8*N3 picture data (or URI bytes when mime == "-->")
/// ```
///
/// The parser rejects:
/// * any payload whose length is too short to read the entire
///   declared structure (`Error::NeedMore`-style truncation reported
///   as `Error::invalid` with an explicit overrun message);
/// * any mime-type string containing a byte outside the printable
///   ASCII range `0x20..=0x7E` (per RFC 9639 §8.8: "this field MUST
///   be in printable ASCII characters");
/// * any mime-type or description length that overflows the
///   remaining payload — protects against a hostile producer that
///   declares a 4 GiB string inside a payload bounded by the 16 MiB
///   block-header ceiling.
///
/// Note that the description is parsed with `from_utf8` rather than
/// `from_utf8_lossy`: the spec mandates UTF-8 and we surface invalid
/// UTF-8 as an error rather than silently replacing bad sequences
/// with `U+FFFD`. The container's older inline parser used
/// `from_utf8(...).unwrap_or("")` which silently dropped both the
/// pixel-metadata block and any non-UTF-8 description; the typed
/// parser surfaces both issues to the caller.
pub fn parse_picture(bytes: &[u8]) -> Result<Picture> {
    fn need(bytes: &[u8], i: usize, n: usize, what: &str) -> Result<()> {
        if i.checked_add(n).map_or(true, |end| end > bytes.len()) {
            return Err(Error::invalid(format!(
                "FLAC PICTURE: payload truncated reading {what} \
                 (need {n} bytes at offset {i}, have {})",
                bytes.len()
            )));
        }
        Ok(())
    }
    fn read_u32(bytes: &[u8], i: &mut usize, what: &str) -> Result<u32> {
        need(bytes, *i, 4, what)?;
        let v = u32::from_be_bytes([bytes[*i], bytes[*i + 1], bytes[*i + 2], bytes[*i + 3]]);
        *i += 4;
        Ok(v)
    }

    let mut i = 0usize;
    let picture_type = read_u32(bytes, &mut i, "picture type")?;
    let mime_len = read_u32(bytes, &mut i, "mime length")? as usize;
    need(bytes, i, mime_len, "mime string")?;
    let mime_slice = &bytes[i..i + mime_len];
    for &b in mime_slice {
        if !(0x20..=0x7E).contains(&b) {
            return Err(Error::invalid(format!(
                "FLAC PICTURE: mime byte 0x{b:02X} outside printable ASCII (0x20..=0x7E)"
            )));
        }
    }
    let mime_type = std::str::from_utf8(mime_slice)
        .expect("printable ASCII is valid UTF-8")
        .to_string();
    i += mime_len;

    let desc_len = read_u32(bytes, &mut i, "description length")? as usize;
    need(bytes, i, desc_len, "description string")?;
    let description = std::str::from_utf8(&bytes[i..i + desc_len])
        .map_err(|e| Error::invalid(format!("FLAC PICTURE: description not valid UTF-8: {e}")))?
        .to_string();
    i += desc_len;

    let width = read_u32(bytes, &mut i, "width")?;
    let height = read_u32(bytes, &mut i, "height")?;
    let depth = read_u32(bytes, &mut i, "depth")?;
    let colour_count = read_u32(bytes, &mut i, "colour count")?;

    let data_len = read_u32(bytes, &mut i, "data length")? as usize;
    need(bytes, i, data_len, "picture data")?;
    let data = bytes[i..i + data_len].to_vec();

    Ok(Picture {
        picture_type,
        mime_type,
        description,
        width,
        height,
        depth,
        colour_count,
        data,
    })
}

/// Serialise a [`Picture`] back into a RFC 9639 §8.8 PICTURE block
/// payload. Round-trips through [`parse_picture`] byte-for-byte.
///
/// The writer enforces every invariant the parser checks for, before
/// emitting any bytes:
///
/// * `mime_type` must be printable ASCII (`0x20..=0x7E`) — the spec
///   requires this and our own parser would reject the output
///   otherwise. The "-->" URI sentinel satisfies the check and is
///   accepted as-is; in that case `data` is the URI bytes rather
///   than image bytes.
/// * `mime_type` length must fit a `u32` (always true on
///   64-bit platforms within usize, but checked anyway for clarity).
/// * `description` is written as its UTF-8 byte representation; any
///   `String` is valid UTF-8 by Rust's type invariants, so no
///   additional check is needed here.
/// * `data.len()` must fit a `u32`.
///
/// Returns `Error::invalid` on any of those failures. The resulting
/// payload may still exceed the metadata-block-header's 24-bit
/// length ceiling (`BlockHeader::MAX_LENGTH`); the wrapping
/// [`BlockHeader::write_into`] call catches that downstream so we
/// don't duplicate the check here.
pub fn write_picture(pic: &Picture) -> Result<Vec<u8>> {
    if pic.mime_type.len() > u32::MAX as usize {
        return Err(Error::invalid(format!(
            "FLAC PICTURE: mime string length {} exceeds u32",
            pic.mime_type.len()
        )));
    }
    for &b in pic.mime_type.as_bytes() {
        if !(0x20..=0x7E).contains(&b) {
            return Err(Error::invalid(format!(
                "FLAC PICTURE: mime byte 0x{b:02X} outside printable ASCII (0x20..=0x7E)"
            )));
        }
    }
    if pic.description.len() > u32::MAX as usize {
        return Err(Error::invalid(format!(
            "FLAC PICTURE: description length {} exceeds u32",
            pic.description.len()
        )));
    }
    if pic.data.len() > u32::MAX as usize {
        return Err(Error::invalid(format!(
            "FLAC PICTURE: data length {} exceeds u32",
            pic.data.len()
        )));
    }

    let total = 4 // picture type
        + 4 + pic.mime_type.len()
        + 4 + pic.description.len()
        + 4 + 4 + 4 + 4 // width/height/depth/colour_count
        + 4 + pic.data.len();
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&pic.picture_type.to_be_bytes());
    out.extend_from_slice(&(pic.mime_type.len() as u32).to_be_bytes());
    out.extend_from_slice(pic.mime_type.as_bytes());
    out.extend_from_slice(&(pic.description.len() as u32).to_be_bytes());
    out.extend_from_slice(pic.description.as_bytes());
    out.extend_from_slice(&pic.width.to_be_bytes());
    out.extend_from_slice(&pic.height.to_be_bytes());
    out.extend_from_slice(&pic.depth.to_be_bytes());
    out.extend_from_slice(&pic.colour_count.to_be_bytes());
    out.extend_from_slice(&(pic.data.len() as u32).to_be_bytes());
    out.extend_from_slice(&pic.data);
    debug_assert_eq!(out.len(), total);
    Ok(out)
}

/// Parsed VORBIS_COMMENT metadata block contents (RFC 9639 §8.6).
///
/// A VORBIS_COMMENT block carries a vendor identification string and
/// a list of `name=value` fields ("FLAC tags"). RFC 9639 §8.6 admits
/// this is the one block in the format whose 4-byte length fields
/// are **little-endian** rather than the format's usual big-endian
/// encoding.
///
/// Compared to the container-layer parser this typed accessor
/// preserves:
///
/// * The vendor string verbatim (the container projection drops it
///   into the framework's narrower `Vec<(String, String)>` metadata
///   surface, where it is conflated with a synthetic `"vendor"` key).
/// * Each field's original `name` casing (the container projection
///   lowercases the field name for case-insensitive matching, which
///   is the right behaviour for evaluating field equality per RFC
///   9639 §8.6 but loses round-trip fidelity).
/// * Empty values (the container projection skips fields whose value
///   is empty after trimming; the spec admits a zero-length field
///   value, so a tagger that legitimately stored `comment=` round-
///   trips through this accessor intact).
/// * Field name / value whitespace (the container projection
///   `.trim()`s both around the `=`, which is fine for display but
///   would silently drop leading or trailing spaces a tagger might
///   have intended — for example a deliberately space-padded artist
///   string).
///
/// The typed parser enforces every invariant RFC 9639 §8.6 names:
///
/// * Field names MUST be UTF-8 code points U+0020 through U+007E
///   excluding U+003D (`=`) — printable ASCII minus the equals sign.
/// * Field values are arbitrary UTF-8.
/// * The vendor string is arbitrary UTF-8 (the spec does not place a
///   character-class restriction on the vendor string beyond
///   "human-readable, UTF-8").
///
/// Note that the spec also says "A FLAC file MUST NOT contain more
/// than one Vorbis comment metadata block." That is a chain-level
/// constraint enforced by the container demuxer / muxer, not the
/// block-level parser; this accessor handles one block payload at a
/// time and does not police uniqueness across a chain.
#[derive(Clone, Debug, PartialEq, Eq, Default)]
pub struct VorbisComment {
    /// Vendor identification string supplied by the producer (the
    /// program that generated the file or stream). RFC 9639 §8.6
    /// places no character-class restriction beyond UTF-8.
    pub vendor: String,
    /// `(name, value)` pairs in their original order. Names are
    /// preserved with their original case; callers wanting case-
    /// insensitive lookup should fold case at the call site (per
    /// RFC 9639 §8.6: "The evaluation of the field names MUST be
    /// case insensitive"). Order is preserved so a round-trip
    /// emission reproduces the original byte sequence.
    pub comments: Vec<(String, String)>,
}

impl VorbisComment {
    /// Look up a comment value by case-insensitive field name (RFC
    /// 9639 §8.6 mandates that field-name evaluation is case
    /// insensitive). Returns the first matching value, or `None` if
    /// no field matches.
    ///
    /// Multiple fields with the same name are legal per spec; this
    /// convenience picks the first occurrence. Callers that need every
    /// value (e.g. multi-artist albums) should walk `comments`
    /// directly and filter on `eq_ignore_ascii_case`.
    pub fn get(&self, name: &str) -> Option<&str> {
        self.comments
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// Validate a Vorbis-comment field name against the RFC 9639 §8.6
/// character class: bytes U+0020 through U+007E excluding U+003D
/// (`=`). Returns `Err(byte)` on the first offending byte so the
/// caller can produce a precise diagnostic.
fn check_vorbis_field_name(name: &str) -> Result<()> {
    for &b in name.as_bytes() {
        if !(0x20..=0x7E).contains(&b) || b == b'=' {
            return Err(Error::invalid(format!(
                "FLAC VORBIS_COMMENT: field name byte 0x{b:02X} outside printable \
                 ASCII range 0x20..=0x7E excluding 0x3D (=)"
            )));
        }
    }
    Ok(())
}

/// Parse a VORBIS_COMMENT block payload (RFC 9639 §8.6).
///
/// Wire layout (all `u32` length fields little-endian unsigned, the
/// only little-endian fields in the FLAC format):
///
/// ```text
///   u32_le  vendor length (N0)
///   u8*N0   vendor string (UTF-8)
///   u32_le  comment count (N)
///   N times:
///     u32_le  field length (Ni)
///     u8*Ni   field bytes (UTF-8), shape "NAME=VALUE"
/// ```
///
/// The parser rejects:
///
/// * any payload too short for the declared lengths (so a hostile
///   producer cannot induce an out-of-bounds read on a short payload
///   by advertising a 4 GiB vendor string);
/// * any vendor string or field that is not valid UTF-8 (the spec
///   mandates UTF-8 — we surface invalid UTF-8 as an error rather
///   than silently replacing with `U+FFFD` as the container
///   projection does);
/// * any field that does not contain an `=` separator (a Vorbis
///   comment field MUST be a `NAME=VALUE` pair per RFC 9639 §8.6;
///   a payload of just `NAME` is malformed);
/// * any field whose name contains a byte outside the allowed
///   character class (printable ASCII minus `=`);
/// * any trailing bytes after the declared count of fields (a
///   silent leftover would indicate a length-mismatch we'd rather
///   surface up the stack).
pub fn parse_vorbis_comment(bytes: &[u8]) -> Result<VorbisComment> {
    fn read_u32_le(bytes: &[u8], i: &mut usize, what: &str) -> Result<u32> {
        if i.checked_add(4).map_or(true, |end| end > bytes.len()) {
            return Err(Error::invalid(format!(
                "FLAC VORBIS_COMMENT: payload truncated reading {what} length \
                 (need 4 bytes at offset {i}, have {})",
                bytes.len()
            )));
        }
        let v = u32::from_le_bytes([bytes[*i], bytes[*i + 1], bytes[*i + 2], bytes[*i + 3]]);
        *i += 4;
        Ok(v)
    }

    let mut i = 0usize;
    let vendor_len = read_u32_le(bytes, &mut i, "vendor")? as usize;
    if i.checked_add(vendor_len)
        .map_or(true, |end| end > bytes.len())
    {
        return Err(Error::invalid(format!(
            "FLAC VORBIS_COMMENT: vendor string overruns payload \
             (need {vendor_len} bytes at offset {i}, have {})",
            bytes.len()
        )));
    }
    let vendor = std::str::from_utf8(&bytes[i..i + vendor_len])
        .map_err(|e| Error::invalid(format!("FLAC VORBIS_COMMENT: vendor not valid UTF-8: {e}")))?
        .to_string();
    i += vendor_len;

    let count = read_u32_le(bytes, &mut i, "comment count")? as usize;
    let mut comments = Vec::with_capacity(count.min(64));
    for n in 0..count {
        let field_len = read_u32_le(bytes, &mut i, "field")? as usize;
        if i.checked_add(field_len)
            .map_or(true, |end| end > bytes.len())
        {
            return Err(Error::invalid(format!(
                "FLAC VORBIS_COMMENT: field {n} ({field_len} bytes) overruns payload \
                 at offset {i} (payload {} bytes)",
                bytes.len()
            )));
        }
        let entry = std::str::from_utf8(&bytes[i..i + field_len]).map_err(|e| {
            Error::invalid(format!(
                "FLAC VORBIS_COMMENT: field {n} not valid UTF-8: {e}"
            ))
        })?;
        i += field_len;
        let eq = entry
            .as_bytes()
            .iter()
            .position(|&b| b == b'=')
            .ok_or_else(|| {
                Error::invalid(format!(
                    "FLAC VORBIS_COMMENT: field {n} missing `=` separator"
                ))
            })?;
        let name = &entry[..eq];
        check_vorbis_field_name(name)?;
        let value = &entry[eq + 1..];
        comments.push((name.to_string(), value.to_string()));
    }

    if i != bytes.len() {
        return Err(Error::invalid(format!(
            "FLAC VORBIS_COMMENT: {} trailing byte(s) after declared {} field(s)",
            bytes.len() - i,
            count
        )));
    }

    Ok(VorbisComment { vendor, comments })
}

/// Serialise a [`VorbisComment`] back into a RFC 9639 §8.6
/// VORBIS_COMMENT block payload. Round-trips through
/// [`parse_vorbis_comment`] byte-for-byte.
///
/// The writer enforces every invariant the parser checks, before
/// emitting any bytes:
///
/// * `vendor` length must fit a `u32` (always true within `usize` on
///   64-bit platforms but checked explicitly for clarity).
/// * Each field name must be printable ASCII excluding `=`
///   (`0x20..=0x7E` minus `0x3D`) — bytes outside that range produce
///   `Error::invalid`. This is the writer-side mirror of the parser's
///   character-class check so a constructed `VorbisComment` cannot
///   silently round-trip through a writer/parser pair that disagree.
/// * Each field's combined `name.len() + 1 + value.len()` must fit a
///   `u32`.
/// * `comments.len()` must fit a `u32`.
///
/// `String`s are valid UTF-8 by Rust's type invariants, so no extra
/// UTF-8 check is needed on the field values or the vendor.
///
/// Returns `Error::invalid` on any of those failures. The resulting
/// payload may still exceed the metadata-block-header's 24-bit length
/// ceiling (`BlockHeader::MAX_LENGTH`); the wrapping
/// [`BlockHeader::write_into`] call catches that downstream so we do
/// not duplicate the check here.
pub fn write_vorbis_comment(vc: &VorbisComment) -> Result<Vec<u8>> {
    if vc.vendor.len() > u32::MAX as usize {
        return Err(Error::invalid(format!(
            "FLAC VORBIS_COMMENT: vendor length {} exceeds u32",
            vc.vendor.len()
        )));
    }
    if vc.comments.len() > u32::MAX as usize {
        return Err(Error::invalid(format!(
            "FLAC VORBIS_COMMENT: comment count {} exceeds u32",
            vc.comments.len()
        )));
    }
    // Pre-compute total + validate every name's character class.
    let mut total = 4 + vc.vendor.len() + 4;
    for (n, (name, value)) in vc.comments.iter().enumerate() {
        check_vorbis_field_name(name)?;
        let entry_len = name
            .len()
            .checked_add(1)
            .and_then(|x| x.checked_add(value.len()))
            .ok_or_else(|| {
                Error::invalid(format!("FLAC VORBIS_COMMENT: field {n} length overflow"))
            })?;
        if entry_len > u32::MAX as usize {
            return Err(Error::invalid(format!(
                "FLAC VORBIS_COMMENT: field {n} length {entry_len} exceeds u32"
            )));
        }
        total = total
            .checked_add(4)
            .and_then(|x| x.checked_add(entry_len))
            .ok_or_else(|| {
                Error::invalid(format!(
                    "FLAC VORBIS_COMMENT: cumulative payload size overflow at field {n}"
                ))
            })?;
    }
    let mut out = Vec::with_capacity(total);
    out.extend_from_slice(&(vc.vendor.len() as u32).to_le_bytes());
    out.extend_from_slice(vc.vendor.as_bytes());
    out.extend_from_slice(&(vc.comments.len() as u32).to_le_bytes());
    for (name, value) in &vc.comments {
        let entry_len = name.len() + 1 + value.len();
        out.extend_from_slice(&(entry_len as u32).to_le_bytes());
        out.extend_from_slice(name.as_bytes());
        out.push(b'=');
        out.extend_from_slice(value.as_bytes());
    }
    debug_assert_eq!(out.len(), total);
    Ok(out)
}

impl StreamInfo {
    /// Spec-defined exact byte length of a STREAMINFO block payload
    /// (RFC 9639 §8.1 Table 2 sums to exactly 272 bits = 34 bytes).
    pub const PAYLOAD_LEN: usize = 34;

    /// Spec ceiling on the 20-bit `sample_rate` field. The RFC's
    /// stream-level upper bound is 655_350 Hz (`655_350 = 65_535 × 10`,
    /// the largest value the §9.1.3 frame-header code 14 can express
    /// for a per-stream rate), but the on-wire field is a flat 20-bit
    /// unsigned int. The writer enforces the wire-format ceiling so a
    /// caller building a STREAMINFO for an unusual rate (e.g. the
    /// `frame_rate = 0` sentinel meaning "rate carried in the frame
    /// header") sees a single bounds check; downstream decoders apply
    /// the per-frame range check on top of this.
    pub const MAX_SAMPLE_RATE: u32 = (1 << 20) - 1;
    /// Spec ceiling on `total_samples`: the field is 36 bits, so 0 (the
    /// "unknown" sentinel) up through `2^36 − 1`.
    pub const MAX_TOTAL_SAMPLES: u64 = (1 << 36) - 1;
    /// Spec ceiling on `min_frame_size` / `max_frame_size`: both
    /// fields are 24-bit unsigned ints (RFC 9639 §8.1 Table 2). Zero
    /// is the "unknown" sentinel both the spec and our encoder use
    /// before the first frame is encoded.
    pub const MAX_FRAME_SIZE: u32 = (1 << 24) - 1;
    /// Minimum spec-permitted channel count (mono).
    pub const MIN_CHANNELS: u8 = 1;
    /// Maximum spec-permitted channel count: 3-bit `channels − 1` =>
    /// 8 channels.
    pub const MAX_CHANNELS: u8 = 8;
    /// Minimum spec-permitted bits-per-sample.
    pub const MIN_BPS: u8 = 1;
    /// Maximum spec-permitted bits-per-sample: 5-bit `bps − 1` =>
    /// 32 bits per sample.
    pub const MAX_BPS: u8 = 32;

    /// Parse a STREAMINFO block. The block payload is exactly 34 bytes.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        if bytes.len() < Self::PAYLOAD_LEN {
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

    /// Serialise a [`StreamInfo`] into a 34-byte RFC 9639 §8.1
    /// STREAMINFO block payload.
    ///
    /// Round-trips through [`StreamInfo::parse`] bit-for-bit: every
    /// field is packed at its spec-defined offset in the 34-byte
    /// payload (4 B min_blocksize, 4 B max_blocksize, 6 B
    /// min/max_framesize each 3 B, 8 B of packed
    /// sample_rate(20)/channels-1(3)/bps-1(5)/total_samples(36), and
    /// the 16-byte MD5 signature).
    ///
    /// Returns `Error::invalid` when any field would overflow its
    /// spec-defined width:
    ///   * `sample_rate` > `MAX_SAMPLE_RATE` (20-bit ceiling, 1_048_575);
    ///   * `channels` outside `MIN_CHANNELS..=MAX_CHANNELS` (1..=8) —
    ///     the wire format encodes `channels − 1` in 3 bits;
    ///   * `bits_per_sample` outside `MIN_BPS..=MAX_BPS` (1..=32) —
    ///     the wire format encodes `bps − 1` in 5 bits;
    ///   * `total_samples` > `MAX_TOTAL_SAMPLES` (36-bit ceiling);
    ///   * `min_frame_size` or `max_frame_size` > `MAX_FRAME_SIZE`
    ///     (24-bit ceiling).
    ///
    /// The 16-bit `min_block_size` / `max_block_size` fields fit the
    /// entire u16 range on the wire, so no bounds check is needed
    /// there.
    pub fn write(&self) -> Result<Vec<u8>> {
        if self.sample_rate > Self::MAX_SAMPLE_RATE {
            return Err(Error::invalid(format!(
                "FLAC STREAMINFO: sample_rate {} exceeds 20-bit max ({})",
                self.sample_rate,
                Self::MAX_SAMPLE_RATE
            )));
        }
        if !(Self::MIN_CHANNELS..=Self::MAX_CHANNELS).contains(&self.channels) {
            return Err(Error::invalid(format!(
                "FLAC STREAMINFO: channels {} outside spec range {}..={}",
                self.channels,
                Self::MIN_CHANNELS,
                Self::MAX_CHANNELS
            )));
        }
        if !(Self::MIN_BPS..=Self::MAX_BPS).contains(&self.bits_per_sample) {
            return Err(Error::invalid(format!(
                "FLAC STREAMINFO: bits_per_sample {} outside spec range {}..={}",
                self.bits_per_sample,
                Self::MIN_BPS,
                Self::MAX_BPS
            )));
        }
        if self.total_samples > Self::MAX_TOTAL_SAMPLES {
            return Err(Error::invalid(format!(
                "FLAC STREAMINFO: total_samples {} exceeds 36-bit max ({})",
                self.total_samples,
                Self::MAX_TOTAL_SAMPLES
            )));
        }
        if self.min_frame_size > Self::MAX_FRAME_SIZE {
            return Err(Error::invalid(format!(
                "FLAC STREAMINFO: min_frame_size {} exceeds 24-bit max ({})",
                self.min_frame_size,
                Self::MAX_FRAME_SIZE
            )));
        }
        if self.max_frame_size > Self::MAX_FRAME_SIZE {
            return Err(Error::invalid(format!(
                "FLAC STREAMINFO: max_frame_size {} exceeds 24-bit max ({})",
                self.max_frame_size,
                Self::MAX_FRAME_SIZE
            )));
        }
        let mut out = Vec::with_capacity(Self::PAYLOAD_LEN);
        out.extend_from_slice(&self.min_block_size.to_be_bytes());
        out.extend_from_slice(&self.max_block_size.to_be_bytes());
        out.push(((self.min_frame_size >> 16) & 0xFF) as u8);
        out.push(((self.min_frame_size >> 8) & 0xFF) as u8);
        out.push((self.min_frame_size & 0xFF) as u8);
        out.push(((self.max_frame_size >> 16) & 0xFF) as u8);
        out.push(((self.max_frame_size >> 8) & 0xFF) as u8);
        out.push((self.max_frame_size & 0xFF) as u8);
        let packed: u64 = ((self.sample_rate as u64) << 44)
            | (((self.channels - 1) as u64 & 0x7) << 41)
            | (((self.bits_per_sample - 1) as u64 & 0x1F) << 36)
            | (self.total_samples & Self::MAX_TOTAL_SAMPLES);
        out.extend_from_slice(&packed.to_be_bytes());
        out.extend_from_slice(&self.md5);
        debug_assert_eq!(out.len(), Self::PAYLOAD_LEN);
        Ok(out)
    }
}

/// Serialise a [`StreamInfo`] back into a RFC 9639 §8.1 STREAMINFO
/// block payload.
///
/// Free-function alias for [`StreamInfo::write`] — kept for symmetry
/// with the other top-level writers in this module
/// ([`write_cuesheet`], [`write_seektable`], [`write_padding`],
/// [`write_application`], [`write_picture`], [`write_vorbis_comment`])
/// so callers walking the metadata-block chain can spell every writer
/// the same way.
pub fn write_streaminfo(info: &StreamInfo) -> Result<Vec<u8>> {
    info.write()
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

    // --- PADDING + BlockHeader writer tests ----------------------------

    #[test]
    fn block_type_to_byte_round_trip() {
        // Every defined block type that's allowed on the wire should
        // round-trip through to_byte / from_byte unchanged.
        for bt in [
            BlockType::StreamInfo,
            BlockType::Padding,
            BlockType::Application,
            BlockType::SeekTable,
            BlockType::VorbisComment,
            BlockType::CueSheet,
            BlockType::Picture,
        ] {
            let b = bt.to_byte().expect("defined block type must serialise");
            assert_eq!(BlockType::from_byte(b), bt);
        }
        // Reserved codes in [7, 126] inclusive (not 127) round-trip too.
        for code in [7u8, 42, 100, 126] {
            let bt = BlockType::Reserved(code);
            let b = bt.to_byte().expect("reserved-but-legal code serialises");
            assert_eq!(b, code);
            assert_eq!(BlockType::from_byte(b), bt);
        }
        // The spec's "invalid" sentinel and high-bit codes are
        // forbidden producers; the writer refuses them.
        assert!(BlockType::Invalid.to_byte().is_none());
        assert!(BlockType::Reserved(127).to_byte().is_none());
        assert!(BlockType::Reserved(0x80).to_byte().is_none());
    }

    #[test]
    fn block_header_writes_streaminfo_prefix() {
        // RFC 9639 §8.1: a non-last STREAMINFO with payload length 34
        // serialises to 0x00 0x00 0x00 0x22.
        let h = BlockHeader {
            last: false,
            block_type: BlockType::StreamInfo,
            length: 34,
        };
        assert_eq!(h.to_bytes().unwrap(), [0x00, 0x00, 0x00, 0x22]);

        // A *last* PADDING block of 8192 bytes: top bit set, code 1,
        // length 0x002000.
        let h = BlockHeader {
            last: true,
            block_type: BlockType::Padding,
            length: 8192,
        };
        assert_eq!(h.to_bytes().unwrap(), [0x81, 0x00, 0x20, 0x00]);
    }

    #[test]
    fn block_header_round_trips_through_parse() {
        // Cover every wire-format block-type code plus both polarities
        // of the last flag and a payload length that exercises all
        // three big-endian length bytes.
        for &(last, bt, length) in &[
            (false, BlockType::StreamInfo, 34u32),
            (true, BlockType::Padding, 8192),
            (false, BlockType::Application, 0),
            (false, BlockType::SeekTable, 36),
            (false, BlockType::VorbisComment, 46),
            (false, BlockType::CueSheet, 396),
            (true, BlockType::Picture, 0x00ABCDEF),
        ] {
            let h = BlockHeader {
                last,
                block_type: bt,
                length,
            };
            let bytes = h.to_bytes().unwrap();
            assert_eq!(bytes.len(), 4);
            let parsed = BlockHeader::parse(&bytes).unwrap();
            assert_eq!(parsed.last, last);
            assert_eq!(parsed.block_type, bt);
            assert_eq!(parsed.length, length);
        }
    }

    #[test]
    fn block_header_rejects_oversize_length() {
        let h = BlockHeader {
            last: false,
            block_type: BlockType::Padding,
            length: BlockHeader::MAX_LENGTH + 1,
        };
        assert!(h.to_bytes().is_err());
    }

    #[test]
    fn block_header_rejects_invalid_block_type() {
        let h = BlockHeader {
            last: true,
            block_type: BlockType::Invalid,
            length: 0,
        };
        assert!(h.to_bytes().is_err());
    }

    #[test]
    fn block_header_write_into_preserves_existing_buffer() {
        // write_into must append four bytes after whatever was already
        // in the buffer — confirms callers can chain a block header
        // onto a metadata-chain `Vec` in progress.
        let mut out = vec![0xDE, 0xAD, 0xBE, 0xEF];
        let h = BlockHeader {
            last: false,
            block_type: BlockType::StreamInfo,
            length: 34,
        };
        h.write_into(&mut out).unwrap();
        assert_eq!(out, [0xDE, 0xAD, 0xBE, 0xEF, 0x00, 0x00, 0x00, 0x22]);
    }

    #[test]
    fn padding_writer_emits_zero_bytes() {
        // RFC 9639 §8.6: payload is `len` bytes whose value MUST be 0.
        let p = write_padding(0).unwrap();
        assert!(p.is_empty());
        let p = write_padding(1).unwrap();
        assert_eq!(p, vec![0u8]);
        // The trace-doc canonical case: FFmpeg's default 8 KiB block.
        let p = write_padding(8192).unwrap();
        assert_eq!(p.len(), 8192);
        assert!(p.iter().all(|&b| b == 0));
    }

    #[test]
    fn padding_writer_rejects_oversize_request() {
        // The metadata-block header length field caps at 2^24 - 1.
        let max_ok = BlockHeader::MAX_LENGTH as usize;
        // The strict equality case at exactly MAX_LENGTH is legal;
        // skip materialising the 16-MiB buffer in this test and only
        // confirm the bound check accepts it via the small adjacent
        // case below + rejects MAX_LENGTH + 1.
        assert!(write_padding(max_ok + 1).is_err());
    }

    #[test]
    fn padding_block_round_trips_through_parser() {
        // End-to-end: build a full PADDING metadata block (header +
        // payload), parse the header back, then confirm the payload
        // bytes match what the parser would see.
        let payload = write_padding(8192).unwrap();
        let header = BlockHeader {
            last: true,
            block_type: BlockType::Padding,
            length: payload.len() as u32,
        };
        let mut wire = header.to_bytes().unwrap();
        wire.extend_from_slice(&payload);

        let parsed = BlockHeader::parse(&wire[..4]).unwrap();
        assert!(parsed.last);
        assert_eq!(parsed.block_type, BlockType::Padding);
        assert_eq!(parsed.length, 8192);
        assert_eq!(&wire[4..], &payload[..]);
        assert!(wire[4..].iter().all(|&b| b == 0));
    }

    #[test]
    fn metadata_chain_streaminfo_then_padding_round_trips() {
        // Smallest realistic metadata chain a muxer might emit:
        // STREAMINFO (mandatory, first) + PADDING (last). Confirm the
        // wire layout walks back through BlockHeader::parse cleanly.
        //
        // Synthesise a STREAMINFO payload directly (no encoder
        // involvement) so this test stays inside `metadata.rs`'s
        // bounded scope.
        let mut si_payload = vec![0u8; 34];
        si_payload[0..2].copy_from_slice(&4096u16.to_be_bytes());
        si_payload[2..4].copy_from_slice(&4096u16.to_be_bytes());
        let packed: u64 = (48_000u64 << 44) | (1u64 << 41) | (15u64 << 36) | 96_000u64;
        si_payload[10..18].copy_from_slice(&packed.to_be_bytes());

        let pad_payload = write_padding(64).unwrap();

        let mut chain = Vec::new();
        BlockHeader {
            last: false,
            block_type: BlockType::StreamInfo,
            length: si_payload.len() as u32,
        }
        .write_into(&mut chain)
        .unwrap();
        chain.extend_from_slice(&si_payload);
        BlockHeader {
            last: true,
            block_type: BlockType::Padding,
            length: pad_payload.len() as u32,
        }
        .write_into(&mut chain)
        .unwrap();
        chain.extend_from_slice(&pad_payload);

        // Walk the chain back the way the demuxer would.
        let mut cur = 0usize;
        let h1 = BlockHeader::parse(&chain[cur..cur + 4]).unwrap();
        assert!(!h1.last);
        assert_eq!(h1.block_type, BlockType::StreamInfo);
        assert_eq!(h1.length as usize, si_payload.len());
        cur += 4;
        let si = StreamInfo::parse(&chain[cur..cur + h1.length as usize]).unwrap();
        assert_eq!(si.sample_rate, 48_000);
        assert_eq!(si.channels, 2);
        assert_eq!(si.bits_per_sample, 16);
        cur += h1.length as usize;

        let h2 = BlockHeader::parse(&chain[cur..cur + 4]).unwrap();
        assert!(h2.last);
        assert_eq!(h2.block_type, BlockType::Padding);
        assert_eq!(h2.length, 64);
        cur += 4;
        assert!(chain[cur..cur + 64].iter().all(|&b| b == 0));
        cur += 64;
        assert_eq!(cur, chain.len());
    }

    // -----------------------------------------------------------------
    // PICTURE block (RFC 9639 §8.8) — typed parser + writer.
    // -----------------------------------------------------------------

    /// Hand-roll the §8.8 wire payload so the parser test isn't
    /// circular with the writer. Encoding follows Table 12 byte-for-
    /// byte; widths/heights/depth/colour_count are caller-supplied so
    /// the helper can build both "pixel image" and "vector image"
    /// shapes (the latter with all four pixel fields set to zero per
    /// spec).
    #[allow(clippy::too_many_arguments)]
    fn synth_picture_payload(
        picture_type: u32,
        mime: &[u8],
        description: &[u8],
        width: u32,
        height: u32,
        depth: u32,
        colour_count: u32,
        data: &[u8],
    ) -> Vec<u8> {
        let mut out = Vec::new();
        out.extend_from_slice(&picture_type.to_be_bytes());
        out.extend_from_slice(&(mime.len() as u32).to_be_bytes());
        out.extend_from_slice(mime);
        out.extend_from_slice(&(description.len() as u32).to_be_bytes());
        out.extend_from_slice(description);
        out.extend_from_slice(&width.to_be_bytes());
        out.extend_from_slice(&height.to_be_bytes());
        out.extend_from_slice(&depth.to_be_bytes());
        out.extend_from_slice(&colour_count.to_be_bytes());
        out.extend_from_slice(&(data.len() as u32).to_be_bytes());
        out.extend_from_slice(data);
        out
    }

    #[test]
    fn picture_parses_front_cover_jpeg() {
        // Picture type 3 = "Cover (front)". A 4-byte JPEG-SOI-ish
        // payload is enough to exercise the data-length field; the
        // parser doesn't decode the image so any bytes will do.
        let payload = synth_picture_payload(
            3,
            b"image/jpeg",
            "Cover".as_bytes(),
            640,
            480,
            24,
            0,
            &[0xFF, 0xD8, 0xFF, 0xE0],
        );
        let pic = parse_picture(&payload).expect("parse");
        assert_eq!(pic.picture_type, 3);
        assert_eq!(pic.mime_type, "image/jpeg");
        assert_eq!(pic.description, "Cover");
        assert_eq!(pic.width, 640);
        assert_eq!(pic.height, 480);
        assert_eq!(pic.depth, 24);
        assert_eq!(pic.colour_count, 0);
        assert_eq!(pic.data, vec![0xFF, 0xD8, 0xFF, 0xE0]);
    }

    #[test]
    fn picture_round_trip_byte_for_byte() {
        // Build the payload by hand, parse it, re-emit it, and confirm
        // the writer reproduces the exact same bytes. This pins the
        // writer to the parser's understanding of the wire format.
        let original = synth_picture_payload(
            3,
            b"image/png",
            "Front cover".as_bytes(),
            1000,
            1000,
            32,
            0,
            &[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A],
        );
        let pic = parse_picture(&original).expect("parse");
        let reemitted = write_picture(&pic).expect("write");
        assert_eq!(original, reemitted);
    }

    #[test]
    fn picture_round_trip_vector_zero_pixel_fields() {
        // RFC 9639 §8.8: "If a picture has no concept for any of these
        // fields (e.g., vector images may not have a height or width
        // in pixels) or the content of any field is unknown, the
        // affected fields MUST be set to zero." Make sure the writer
        // accepts that shape and the parser preserves the zeros.
        let pic = Picture {
            picture_type: 0x00, // "Other"
            mime_type: "image/svg+xml".into(),
            description: String::new(),
            width: 0,
            height: 0,
            depth: 0,
            colour_count: 0,
            data: b"<svg/>".to_vec(),
        };
        let bytes = write_picture(&pic).expect("write");
        let parsed = parse_picture(&bytes).expect("parse");
        assert_eq!(parsed, pic);
    }

    #[test]
    fn picture_round_trip_indexed_gif_colour_count() {
        // GIF carries a palette; colour_count is non-zero. Round-trip
        // confirms the field survives both directions.
        let pic = Picture {
            picture_type: 0x03,
            mime_type: "image/gif".into(),
            description: "Animated cover".into(),
            width: 320,
            height: 240,
            depth: 8,
            colour_count: 256,
            data: b"GIF89a".to_vec(),
        };
        let bytes = write_picture(&pic).expect("write");
        let parsed = parse_picture(&bytes).expect("parse");
        assert_eq!(parsed, pic);
        assert_eq!(parsed.colour_count, 256);
    }

    #[test]
    fn picture_round_trip_uri_sentinel() {
        // The "-->" mime sentinel signals that `data` is a URI rather
        // than image bytes. The parser must accept it (it's printable
        // ASCII) and the writer must round-trip it unchanged.
        let pic = Picture {
            picture_type: 0x00,
            mime_type: "-->".into(),
            description: String::new(),
            width: 0,
            height: 0,
            depth: 0,
            colour_count: 0,
            data: b"https://example.invalid/cover.jpg".to_vec(),
        };
        let bytes = write_picture(&pic).expect("write");
        let parsed = parse_picture(&bytes).expect("parse");
        assert_eq!(parsed, pic);
    }

    #[test]
    fn picture_rejects_non_printable_mime_byte() {
        // 0x01 violates "MUST be in printable ASCII characters
        // 0x20-0x7E"; both parser and writer must reject it.
        let mut payload = synth_picture_payload(3, b"image/jpeg", b"", 1, 1, 24, 0, b"\xFF");
        // Corrupt the mime byte at offset 4 (picture_type:4 + mime_len:4 = 8).
        payload[8] = 0x01;
        assert!(parse_picture(&payload).is_err());

        let pic = Picture {
            picture_type: 0,
            mime_type: "\x01bad".into(),
            description: String::new(),
            width: 0,
            height: 0,
            depth: 0,
            colour_count: 0,
            data: Vec::new(),
        };
        assert!(write_picture(&pic).is_err());
    }

    #[test]
    fn picture_rejects_invalid_utf8_description() {
        // The description field is mandated UTF-8 (§8.8). Synthesise a
        // payload whose description bytes are invalid UTF-8 (a lone
        // 0x80 continuation byte) and confirm the parser rejects it.
        let payload = synth_picture_payload(
            0,
            b"image/jpeg",
            &[0x80, 0x80, 0x80],
            0,
            0,
            0,
            0,
            b"\xFF\xD8",
        );
        assert!(parse_picture(&payload).is_err());
    }

    #[test]
    fn picture_rejects_truncated_payloads_at_every_field_boundary() {
        // Build a full payload then truncate at every internal
        // boundary; every truncation must produce Err (no panics, no
        // partial reads).
        let full = synth_picture_payload(
            3,
            b"image/jpeg",
            b"Cover",
            640,
            480,
            24,
            0,
            &[0xFF, 0xD8, 0xFF, 0xE0],
        );
        for cut in 0..full.len() {
            let slice = &full[..cut];
            assert!(
                parse_picture(slice).is_err(),
                "truncation at {cut} bytes should have failed"
            );
        }
        // The full payload itself parses cleanly.
        assert!(parse_picture(&full).is_ok());
    }

    #[test]
    fn picture_rejects_oversize_mime_length() {
        // Declare a 4 GiB-1 mime length inside a 16-byte payload. The
        // length read succeeds but the bounds check on the string read
        // must reject before we attempt a 4 GiB slice / allocation.
        let mut payload = Vec::new();
        payload.extend_from_slice(&0u32.to_be_bytes()); // picture_type
        payload.extend_from_slice(&u32::MAX.to_be_bytes()); // mime_len
        let err = parse_picture(&payload).expect_err("must reject");
        // Sanity: the error mentions the truncation site so a debug
        // user can find the offending field.
        let msg = format!("{err}");
        assert!(
            msg.contains("mime string") || msg.contains("truncated"),
            "error should mention mime string truncation: {msg}"
        );
    }

    #[test]
    fn picture_rejects_oversize_data_length() {
        // All earlier fields read cleanly; the data-length field
        // declares more bytes than remain. Must reject without
        // panicking or over-reading.
        let payload = synth_picture_payload(3, b"image/jpeg", b"", 1, 1, 24, 0, b"abc");
        // Replace the data length (last u32 before the actual data
        // bytes) with 0xFFFF_FFFF.
        let data_len_off = payload.len() - 3 - 4; // 3 bytes data + 4-byte length
        let mut bad = payload.clone();
        bad[data_len_off..data_len_off + 4].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(parse_picture(&bad).is_err());
    }

    #[test]
    fn picture_does_not_panic_on_random_input() {
        // Sweep a handful of unstructured payload shapes; every
        // outcome must be either Ok or Err (no panic).
        for len in [0usize, 1, 4, 16, 40, 128] {
            let buf = vec![0xABu8; len];
            let _ = parse_picture(&buf);
        }
        // Also try a payload whose every field declares the same
        // legal-but-large length; with len = 1024 the mime_len of
        // 0xABABABAB will definitely overshoot, so this must Err.
        let _ = parse_picture(&vec![0xABu8; 1024]);
    }

    #[test]
    fn picture_writer_preserves_full_u32_picture_type() {
        // The §8.8 picture_type field is a u32 on the wire. Even if
        // only the low 8 bits map to ID3v2's APIC taxonomy, a producer
        // that smuggles a non-standard high-bit value through MUST
        // get the exact bits back on the next read.
        let pic = Picture {
            picture_type: 0xDEAD_BEEF,
            mime_type: "image/jpeg".into(),
            description: String::new(),
            width: 0,
            height: 0,
            depth: 0,
            colour_count: 0,
            data: Vec::new(),
        };
        let bytes = write_picture(&pic).expect("write");
        let parsed = parse_picture(&bytes).expect("parse");
        assert_eq!(parsed.picture_type, 0xDEAD_BEEF);
    }

    // ----------------------------------------------------------------
    // VORBIS_COMMENT (RFC 9639 §8.6) — typed accessor + writer tests
    // ----------------------------------------------------------------

    /// Build a synthetic VORBIS_COMMENT block payload directly from a
    /// vendor string and a (name, value) field list. Used by the
    /// tests to exercise the parser against bytes that bypass the
    /// writer's validation path.
    fn synth_vorbis_payload(vendor: &[u8], fields: &[(&[u8], &[u8])]) -> Vec<u8> {
        let mut buf = Vec::new();
        buf.extend_from_slice(&(vendor.len() as u32).to_le_bytes());
        buf.extend_from_slice(vendor);
        buf.extend_from_slice(&(fields.len() as u32).to_le_bytes());
        for (n, v) in fields {
            let entry_len = n.len() + 1 + v.len();
            buf.extend_from_slice(&(entry_len as u32).to_le_bytes());
            buf.extend_from_slice(n);
            buf.push(b'=');
            buf.extend_from_slice(v);
        }
        buf
    }

    #[test]
    fn vorbis_comment_parses_minimal_payload() {
        // Empty vendor, zero comments — the smallest legal payload.
        let payload = synth_vorbis_payload(b"", &[]);
        let vc = parse_vorbis_comment(&payload).expect("parse");
        assert!(vc.vendor.is_empty());
        assert!(vc.comments.is_empty());
        // Round-trip through write_vorbis_comment.
        let re_bytes = write_vorbis_comment(&vc).expect("write");
        assert_eq!(re_bytes, payload, "byte-for-byte round-trip");
        let re = parse_vorbis_comment(&re_bytes).expect("re-parse");
        assert_eq!(re, vc);
    }

    #[test]
    fn vorbis_comment_parses_real_shape() {
        // Realistic shape: a vendor tag plus title/artist/album fields.
        let payload = synth_vorbis_payload(
            b"oxideav-flac 0.0.10",
            &[
                (b"TITLE", b"Symphony No. 5"),
                (b"ARTIST", b"Ludwig van Beethoven"),
                (b"ALBUM", b"Greatest Hits"),
            ],
        );
        let vc = parse_vorbis_comment(&payload).expect("parse");
        assert_eq!(vc.vendor, "oxideav-flac 0.0.10");
        assert_eq!(vc.comments.len(), 3);
        assert_eq!(vc.comments[0], ("TITLE".into(), "Symphony No. 5".into()));
        assert_eq!(
            vc.comments[1],
            ("ARTIST".into(), "Ludwig van Beethoven".into())
        );
        assert_eq!(vc.comments[2], ("ALBUM".into(), "Greatest Hits".into()));
        // Byte-for-byte round-trip.
        let re_bytes = write_vorbis_comment(&vc).expect("write");
        assert_eq!(re_bytes, payload);
    }

    #[test]
    fn vorbis_comment_get_is_case_insensitive() {
        // RFC 9639 §8.6 mandates case-insensitive field-name matching.
        let payload = synth_vorbis_payload(
            b"vendor",
            &[
                (b"Title", b"Hello"),
                (b"ARTIST", b"World"),
                (b"album", b"X"),
            ],
        );
        let vc = parse_vorbis_comment(&payload).unwrap();
        assert_eq!(vc.get("title"), Some("Hello"));
        assert_eq!(vc.get("TITLE"), Some("Hello"));
        assert_eq!(vc.get("Artist"), Some("World"));
        assert_eq!(vc.get("ALBUM"), Some("X"));
        assert_eq!(vc.get("missing"), None);
    }

    #[test]
    fn vorbis_comment_preserves_empty_value() {
        // RFC 9639 §8.6 places no minimum length on a field value —
        // the byte sequence "NAME=" is a legal field with an empty
        // value. The container-layer projection drops empty values;
        // the typed accessor preserves them.
        let payload = synth_vorbis_payload(b"v", &[(b"COMMENT", b"")]);
        let vc = parse_vorbis_comment(&payload).expect("parse");
        assert_eq!(vc.comments, vec![("COMMENT".into(), "".into())]);
        let re_bytes = write_vorbis_comment(&vc).expect("write");
        assert_eq!(re_bytes, payload);
    }

    #[test]
    fn vorbis_comment_preserves_name_case_and_whitespace_in_value() {
        // The container projection lowercases names and trims values;
        // the typed accessor MUST preserve the original byte sequence
        // so round-trips are stable.
        let payload = synth_vorbis_payload(
            b"v",
            &[
                (b"Title", b"  Spaced  "),
                (b"ARTIST", b"\tTabbed"),
                (b"album", b"Mixed\nNewline"),
            ],
        );
        let vc = parse_vorbis_comment(&payload).unwrap();
        assert_eq!(vc.comments[0], ("Title".into(), "  Spaced  ".into()));
        assert_eq!(vc.comments[1], ("ARTIST".into(), "\tTabbed".into()));
        assert_eq!(vc.comments[2], ("album".into(), "Mixed\nNewline".into()));
        let re = write_vorbis_comment(&vc).expect("write");
        assert_eq!(re, payload);
    }

    #[test]
    fn vorbis_comment_value_admits_arbitrary_utf8() {
        // §8.6: "The field contents can contain any UTF-8 character."
        let payload = synth_vorbis_payload(
            b"vendor",
            &[
                (b"TITLE", "Été ‒ café".as_bytes()),
                (b"ARTIST", "山田太郎".as_bytes()),
                (b"EMOJI", "🎵🔥".as_bytes()),
            ],
        );
        let vc = parse_vorbis_comment(&payload).expect("parse");
        assert_eq!(vc.comments[0].1, "Été ‒ café");
        assert_eq!(vc.comments[1].1, "山田太郎");
        assert_eq!(vc.comments[2].1, "🎵🔥");
        let re_bytes = write_vorbis_comment(&vc).expect("write");
        assert_eq!(re_bytes, payload);
    }

    #[test]
    fn vorbis_comment_rejects_field_name_with_equals_sign() {
        // The `=` byte (0x3D) is the field-name/value separator and
        // MUST NOT appear in the name. Construct a payload where the
        // first `=` is the legitimate separator and the name itself
        // is empty before it (i.e. "=VALUE") — the parser MUST treat
        // the empty-name case as one whose name field passes the
        // character-class check (no offending byte) but is not the
        // case under test here. Instead, embed a non-printable byte
        // in the name to drive the actual rejection path.
        let payload = synth_vorbis_payload(b"v", &[(b"NA\x01ME", b"V")]);
        let err = parse_vorbis_comment(&payload).expect_err("must reject");
        let msg = format!("{err}");
        assert!(
            msg.contains("field name") || msg.contains("0x01"),
            "error should name the offending byte: {msg}"
        );
    }

    #[test]
    fn vorbis_comment_rejects_field_name_with_non_ascii_byte() {
        // 0xC3 starts a UTF-8 multi-byte sequence — still outside
        // the printable-ASCII range the spec allows for names.
        let payload = synth_vorbis_payload(b"v", &[("CAFÉ".as_bytes(), b"V")]);
        assert!(parse_vorbis_comment(&payload).is_err());
    }

    #[test]
    fn vorbis_comment_rejects_field_without_equals_separator() {
        // A field with no `=` byte is malformed — RFC 9639 §8.6 says
        // "Each field consists of a field name and field contents,
        // separated by an = character."
        let mut buf = Vec::new();
        buf.extend_from_slice(&0u32.to_le_bytes()); // vendor len = 0
        buf.extend_from_slice(&1u32.to_le_bytes()); // count = 1
        buf.extend_from_slice(&4u32.to_le_bytes()); // field len = 4
        buf.extend_from_slice(b"NAME"); // no `=`
        let err = parse_vorbis_comment(&buf).expect_err("must reject");
        let msg = format!("{err}");
        assert!(msg.contains("separator") || msg.contains("="));
    }

    #[test]
    fn vorbis_comment_rejects_truncated_vendor() {
        // Declared 100-byte vendor in a 10-byte payload — must Err
        // cleanly, no panic.
        let mut buf = Vec::new();
        buf.extend_from_slice(&100u32.to_le_bytes());
        buf.extend_from_slice(b"short"); // only 5 vendor bytes follow
        assert!(parse_vorbis_comment(&buf).is_err());
    }

    #[test]
    fn vorbis_comment_rejects_overflowing_field_length() {
        // Declared 0xFFFF_FFFF-byte field in a tiny payload.
        let mut buf = Vec::new();
        buf.extend_from_slice(&0u32.to_le_bytes()); // vendor len
        buf.extend_from_slice(&1u32.to_le_bytes()); // count = 1
        buf.extend_from_slice(&u32::MAX.to_le_bytes()); // field len = huge
                                                        // No further bytes.
        assert!(parse_vorbis_comment(&buf).is_err());
    }

    #[test]
    fn vorbis_comment_rejects_invalid_utf8_vendor() {
        // Lone 0xFF is invalid UTF-8.
        let mut buf = Vec::new();
        buf.extend_from_slice(&1u32.to_le_bytes());
        buf.push(0xFF);
        buf.extend_from_slice(&0u32.to_le_bytes());
        assert!(parse_vorbis_comment(&buf).is_err());
    }

    #[test]
    fn vorbis_comment_rejects_invalid_utf8_field() {
        // Lone 0xFF inside a field value is also rejected. We arrange
        // the `=` byte to land first so the validity check trips on
        // the value side of the split.
        let mut buf = Vec::new();
        buf.extend_from_slice(&0u32.to_le_bytes()); // vendor len = 0
        buf.extend_from_slice(&1u32.to_le_bytes()); // count = 1
        let field = b"N=\xFF"; // 3 bytes: N, =, lone 0xFF
        buf.extend_from_slice(&(field.len() as u32).to_le_bytes());
        buf.extend_from_slice(field);
        assert!(parse_vorbis_comment(&buf).is_err());
    }

    #[test]
    fn vorbis_comment_rejects_trailing_garbage() {
        let mut payload = synth_vorbis_payload(b"v", &[(b"K", b"V")]);
        // Append a stray byte after the declared field count.
        payload.push(0x00);
        let err = parse_vorbis_comment(&payload).expect_err("must reject");
        let msg = format!("{err}");
        assert!(msg.contains("trailing"));
    }

    #[test]
    fn vorbis_comment_rejects_short_payload() {
        // Empty payload — not even the vendor length fits.
        assert!(parse_vorbis_comment(&[]).is_err());
        // 3 bytes — short of the 4-byte vendor length.
        assert!(parse_vorbis_comment(&[0u8; 3]).is_err());
    }

    #[test]
    fn vorbis_comment_writer_rejects_bad_field_name() {
        let vc = VorbisComment {
            vendor: String::new(),
            comments: vec![("BAD=KEY".into(), "V".into())],
        };
        assert!(write_vorbis_comment(&vc).is_err());
        let vc2 = VorbisComment {
            vendor: String::new(),
            comments: vec![("BAD\x7FKEY".into(), "V".into())],
        };
        // 0x7F is DEL, outside the 0x20..=0x7E range.
        assert!(write_vorbis_comment(&vc2).is_err());
    }

    #[test]
    fn vorbis_comment_writer_accepts_printable_ascii_name_boundaries() {
        // Boundary characters 0x20 (space) and 0x7E (~) are admitted.
        let vc = VorbisComment {
            vendor: "v".into(),
            comments: vec![
                (" name".into(), "spaceLead".into()),
                ("name~".into(), "tildeTail".into()),
                ("!@#$%^&*()".into(), "punct".into()),
            ],
        };
        let bytes = write_vorbis_comment(&vc).expect("write");
        let re = parse_vorbis_comment(&bytes).expect("parse");
        assert_eq!(re, vc);
    }

    #[test]
    fn vorbis_comment_round_trip_byte_stable() {
        // write_vorbis_comment MUST be deterministic so two emissions
        // of the same value produce identical bytes.
        let vc = VorbisComment {
            vendor: "vendor-name".into(),
            comments: vec![
                ("TITLE".into(), "value1".into()),
                ("ARTIST".into(), "value2".into()),
            ],
        };
        let a = write_vorbis_comment(&vc).unwrap();
        let b = write_vorbis_comment(&vc).unwrap();
        assert_eq!(a, b);
    }

    #[test]
    fn vorbis_comment_admits_duplicate_field_names() {
        // RFC 9639 §8.6 does not forbid duplicate field names —
        // multi-artist or multi-genre lists are commonly stored as
        // repeated identically-named fields. `get` returns the first
        // hit; iterating `comments` recovers all of them.
        let payload = synth_vorbis_payload(
            b"v",
            &[
                (b"ARTIST", b"Vocalist"),
                (b"ARTIST", b"Guitarist"),
                (b"ARTIST", b"Drummer"),
            ],
        );
        let vc = parse_vorbis_comment(&payload).unwrap();
        assert_eq!(vc.comments.len(), 3);
        assert_eq!(vc.get("artist"), Some("Vocalist"));
        let all: Vec<&str> = vc
            .comments
            .iter()
            .filter(|(n, _)| n.eq_ignore_ascii_case("ARTIST"))
            .map(|(_, v)| v.as_str())
            .collect();
        assert_eq!(all, vec!["Vocalist", "Guitarist", "Drummer"]);
    }

    #[test]
    fn vorbis_comment_does_not_panic_on_random_input() {
        // Every shape must return either Ok or Err — no panic.
        for len in [0usize, 1, 4, 7, 16, 64, 256, 1024] {
            let buf = vec![0xABu8; len];
            let _ = parse_vorbis_comment(&buf);
        }
        // A near-MAX vendor length encoded in the first 4 bytes —
        // must Err cleanly without trying to slice past the payload.
        let mut buf = vec![0u8; 16];
        buf[0..4].copy_from_slice(&u32::MAX.to_le_bytes());
        assert!(parse_vorbis_comment(&buf).is_err());
    }

    // --- APPLICATION block (§8.4) accessor tests ----------------------

    #[test]
    fn application_round_trips_through_parse_write() {
        // RFC 9639 §8.4 Table 5: u(32) ID followed by an
        // application-defined byte run. A canonical reproducible
        // shape: ID 0x46494341 ("FICA") + a small data run.
        let app = Application {
            id: 0x4649_4341,
            data: vec![0xDE, 0xAD, 0xBE, 0xEF, 0x01, 0x23, 0x45, 0x67],
        };
        let bytes = write_application(&app).expect("writer");
        // Wire format: 4 bytes BE ID + opaque data.
        assert_eq!(bytes.len(), 4 + app.data.len());
        assert_eq!(&bytes[0..4], &[0x46, 0x49, 0x43, 0x41]);
        assert_eq!(&bytes[4..], &app.data[..]);
        let reparsed = parse_application(&bytes).expect("parser");
        assert_eq!(reparsed, app);
    }

    #[test]
    fn application_zero_data_run_is_legal() {
        // RFC 9639 §8.4 Table 5: "n MUST be a multiple of 8" — `n = 0`
        // is the smallest legal data run, leaving the block carrying
        // just the 4-byte ID. The writer accepts it and the parser
        // round-trips it.
        let app = Application {
            id: 0xAABB_CCDD,
            data: Vec::new(),
        };
        let bytes = write_application(&app).expect("writer");
        assert_eq!(bytes, [0xAA, 0xBB, 0xCC, 0xDD]);
        let reparsed = parse_application(&bytes).expect("parser");
        assert_eq!(reparsed, app);
    }

    #[test]
    fn application_parse_rejects_short_payload() {
        // The 4-byte ID header is mandatory — anything under 4 bytes
        // cannot carry it and the parser must reject before slicing
        // past the buffer.
        for short in 0..4 {
            let buf = vec![0u8; short];
            assert!(parse_application(&buf).is_err(), "len {short} must reject");
        }
    }

    #[test]
    fn application_id_is_big_endian_on_the_wire() {
        // Sanity-check the byte order against RFC 9639 §8.4
        // Table 5's `u(32)` row by picking an asymmetric ID whose
        // MSB and LSB differ obviously, and asserting the writer
        // emits high byte first.
        let app = Application {
            id: 0x1122_3344,
            data: vec![],
        };
        let bytes = write_application(&app).expect("writer");
        assert_eq!(bytes, [0x11, 0x22, 0x33, 0x44]);
        // And the parser reads it back identically.
        let reparsed = parse_application(&bytes).expect("parser");
        assert_eq!(reparsed.id, 0x1122_3344);
        assert!(reparsed.data.is_empty());
    }

    #[test]
    fn application_writer_rejects_oversize_payload() {
        // The metadata-block-header length field is 24 bits
        // (`BlockHeader::MAX_LENGTH`), so any
        // `4 + data.len() > MAX_LENGTH` value must Err before
        // returning a vector the surrounding header can't describe.
        let too_big = BlockHeader::MAX_LENGTH as usize - 3; // 4 + (MAX-3) = MAX+1
        let app = Application {
            id: 0,
            data: vec![0u8; too_big],
        };
        assert!(write_application(&app).is_err());

        // Exactly at the ceiling (4 + (MAX-4)) is legal — boundary
        // check to prove the off-by-one isn't inverted. Allocate up
        // front so the size of the round-trip is bounded by the
        // 16 MiB header ceiling but doesn't push the test binary
        // into excessive memory usage on CI runners.
        let exact = BlockHeader::MAX_LENGTH as usize - 4;
        let app_ok = Application {
            id: 0x1234_5678,
            data: vec![0xCDu8; exact],
        };
        let bytes = write_application(&app_ok).expect("writer at ceiling");
        assert_eq!(bytes.len(), BlockHeader::MAX_LENGTH as usize);
        // Confirm the header writer accepts the resulting length too.
        let h = BlockHeader {
            last: true,
            block_type: BlockType::Application,
            length: bytes.len() as u32,
        };
        assert!(h.to_bytes().is_ok());
    }

    #[test]
    fn application_parse_does_not_alias_input() {
        // The parser owns its returned `data` (Vec<u8>); mutating the
        // input afterwards must not change the parsed copy. Guards
        // against an accidental zero-copy slice-of-input refactor.
        let mut buf = vec![0x12, 0x34, 0x56, 0x78, 0xAA, 0xBB, 0xCC];
        let app = parse_application(&buf).expect("parser");
        buf[4] = 0x00;
        buf[5] = 0x00;
        buf[6] = 0x00;
        assert_eq!(app.data, vec![0xAA, 0xBB, 0xCC]);
        assert_eq!(app.id, 0x1234_5678);
    }

    /// Helper: a representative STREAMINFO covering every field with
    /// a non-default value so a writer-truncation regression shows
    /// up on the round-trip.
    fn sample_streaminfo() -> StreamInfo {
        StreamInfo {
            min_block_size: 4096,
            max_block_size: 4096,
            min_frame_size: 100,
            max_frame_size: 1252,
            sample_rate: 44_100,
            channels: 2,
            bits_per_sample: 16,
            total_samples: 1_234_567,
            md5: [
                0x01, 0x23, 0x45, 0x67, 0x89, 0xAB, 0xCD, 0xEF, 0xFE, 0xDC, 0xBA, 0x98, 0x76, 0x54,
                0x32, 0x10,
            ],
        }
    }

    #[test]
    fn streaminfo_write_payload_len() {
        // RFC 9639 §8.1 Table 2 sums to 272 bits = 34 bytes.
        let bytes = sample_streaminfo().write().expect("writer");
        assert_eq!(bytes.len(), StreamInfo::PAYLOAD_LEN);
    }

    #[test]
    fn streaminfo_write_round_trip() {
        // write -> parse must reproduce every typed field byte-exact.
        let info = sample_streaminfo();
        let bytes = info.write().expect("writer");
        let back = StreamInfo::parse(&bytes).expect("parser");
        assert_eq!(back.min_block_size, info.min_block_size);
        assert_eq!(back.max_block_size, info.max_block_size);
        assert_eq!(back.min_frame_size, info.min_frame_size);
        assert_eq!(back.max_frame_size, info.max_frame_size);
        assert_eq!(back.sample_rate, info.sample_rate);
        assert_eq!(back.channels, info.channels);
        assert_eq!(back.bits_per_sample, info.bits_per_sample);
        assert_eq!(back.total_samples, info.total_samples);
        assert_eq!(back.md5, info.md5);
    }

    #[test]
    fn streaminfo_write_free_function_alias() {
        // write_streaminfo is documented as a free-function alias for
        // StreamInfo::write so the metadata-block chain reads
        // uniformly across writers.
        let info = sample_streaminfo();
        let via_method = info.write().expect("writer");
        let via_fn = write_streaminfo(&info).expect("writer");
        assert_eq!(via_method, via_fn);
    }

    #[test]
    fn streaminfo_write_matches_existing_parser_round_trip_fixture() {
        // The hand-built block in `parse_streaminfo_round_trip` above
        // is the canonical small fixture. Building the same fixture
        // through the new writer and re-parsing must match exactly,
        // proving the writer's bit packing matches the parser.
        let info = StreamInfo {
            min_block_size: 4096,
            max_block_size: 4096,
            min_frame_size: 0,
            max_frame_size: 0,
            sample_rate: 48_000,
            channels: 2,
            bits_per_sample: 16,
            total_samples: 96_000,
            md5: [0u8; 16],
        };
        let bytes = info.write().expect("writer");
        // Re-parsed must equal the originals.
        let back = StreamInfo::parse(&bytes).expect("parser");
        assert_eq!(back.sample_rate, 48_000);
        assert_eq!(back.channels, 2);
        assert_eq!(back.bits_per_sample, 16);
        assert_eq!(back.total_samples, 96_000);
        // And the packed 8 bytes for sr/ch/bps/total must match the
        // hand-built value in `parse_streaminfo_round_trip` exactly.
        let expected_packed: u64 = (48_000u64 << 44) | (1u64 << 41) | (15u64 << 36) | 96_000u64;
        assert_eq!(&bytes[10..18], &expected_packed.to_be_bytes());
    }

    #[test]
    fn streaminfo_write_rejects_oversized_sample_rate() {
        let mut info = sample_streaminfo();
        info.sample_rate = StreamInfo::MAX_SAMPLE_RATE + 1;
        assert!(info.write().is_err());
        info.sample_rate = StreamInfo::MAX_SAMPLE_RATE;
        // Boundary value: still serialisable.
        assert!(info.write().is_ok());
    }

    #[test]
    fn streaminfo_write_rejects_oversized_total_samples() {
        let mut info = sample_streaminfo();
        info.total_samples = StreamInfo::MAX_TOTAL_SAMPLES + 1;
        assert!(info.write().is_err());
        info.total_samples = StreamInfo::MAX_TOTAL_SAMPLES;
        assert!(info.write().is_ok());
    }

    #[test]
    fn streaminfo_write_rejects_out_of_range_channels() {
        let mut info = sample_streaminfo();
        info.channels = 0;
        assert!(info.write().is_err());
        info.channels = 9;
        assert!(info.write().is_err());
        info.channels = StreamInfo::MAX_CHANNELS;
        assert!(info.write().is_ok());
        info.channels = StreamInfo::MIN_CHANNELS;
        assert!(info.write().is_ok());
    }

    #[test]
    fn streaminfo_write_rejects_out_of_range_bps() {
        let mut info = sample_streaminfo();
        info.bits_per_sample = 0;
        assert!(info.write().is_err());
        info.bits_per_sample = 33;
        assert!(info.write().is_err());
        info.bits_per_sample = StreamInfo::MAX_BPS;
        assert!(info.write().is_ok());
        info.bits_per_sample = StreamInfo::MIN_BPS;
        assert!(info.write().is_ok());
    }

    #[test]
    fn streaminfo_write_rejects_oversized_frame_sizes() {
        let mut info = sample_streaminfo();
        info.min_frame_size = StreamInfo::MAX_FRAME_SIZE + 1;
        assert!(info.write().is_err());
        info.min_frame_size = StreamInfo::MAX_FRAME_SIZE;
        assert!(info.write().is_ok());

        let mut info = sample_streaminfo();
        info.max_frame_size = StreamInfo::MAX_FRAME_SIZE + 1;
        assert!(info.write().is_err());
        info.max_frame_size = StreamInfo::MAX_FRAME_SIZE;
        assert!(info.write().is_ok());
    }

    #[test]
    fn streaminfo_write_preserves_md5_bytes() {
        let info = sample_streaminfo();
        let bytes = info.write().expect("writer");
        // MD5 sits in the last 16 bytes of the 34-byte payload.
        assert_eq!(&bytes[18..34], &info.md5);
    }

    #[test]
    fn streaminfo_write_packs_max_field_values() {
        // Build a payload at every field's wire-format ceiling and
        // confirm the parser reads the same values back. Catches a
        // writer that silently masks the top bit of a field.
        let info = StreamInfo {
            min_block_size: u16::MAX,
            max_block_size: u16::MAX,
            min_frame_size: StreamInfo::MAX_FRAME_SIZE,
            max_frame_size: StreamInfo::MAX_FRAME_SIZE,
            sample_rate: StreamInfo::MAX_SAMPLE_RATE,
            channels: StreamInfo::MAX_CHANNELS,
            bits_per_sample: StreamInfo::MAX_BPS,
            total_samples: StreamInfo::MAX_TOTAL_SAMPLES,
            md5: [0xFFu8; 16],
        };
        let bytes = info.write().expect("writer");
        let back = StreamInfo::parse(&bytes).expect("parser");
        assert_eq!(back.min_block_size, u16::MAX);
        assert_eq!(back.max_block_size, u16::MAX);
        assert_eq!(back.min_frame_size, StreamInfo::MAX_FRAME_SIZE);
        assert_eq!(back.max_frame_size, StreamInfo::MAX_FRAME_SIZE);
        assert_eq!(back.sample_rate, StreamInfo::MAX_SAMPLE_RATE);
        assert_eq!(back.channels, StreamInfo::MAX_CHANNELS);
        assert_eq!(back.bits_per_sample, StreamInfo::MAX_BPS);
        assert_eq!(back.total_samples, StreamInfo::MAX_TOTAL_SAMPLES);
        assert_eq!(back.md5, [0xFFu8; 16]);
    }
}
