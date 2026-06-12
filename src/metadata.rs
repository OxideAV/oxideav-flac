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
#[derive(Clone, Debug, PartialEq, Eq)]
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

impl CueSheetTrack {
    /// Track number reserved by Red Book for the CD-DA lead-out
    /// (RFC 9639 §8.7.1). The lead-out is always the trailing track and
    /// carries the total audio sample count in `offset_samples` with an
    /// empty `indices` list.
    pub const CDDA_LEAD_OUT: u8 = 170;

    /// Track number reserved for the non-CD-DA lead-out (RFC 9639
    /// §8.7.1). Used in place of [`CDDA_LEAD_OUT`](Self::CDDA_LEAD_OUT)
    /// when the enclosing [`CueSheet::is_cdda`] flag is false.
    pub const NON_CDDA_LEAD_OUT: u8 = 255;

    /// True when this track's number is one of the two spec-defined
    /// lead-out sentinels (170 for CD-DA, 255 for non-CD-DA per RFC 9639
    /// §8.7.1). The lead-out is a structural terminator, not a playable
    /// track: it carries the stream's total sample count in
    /// `offset_samples` and no index points.
    ///
    /// This is a *value* test on the track number alone — it does not
    /// consult position in the track list or the CD-DA flag, so it
    /// answers "could this number denote a lead-out" rather than "is this
    /// the trailing entry". A well-formed [`CueSheet`] only ever places a
    /// lead-out-numbered track last, so on parser output the two notions
    /// coincide; callers building a `CueSheet` by hand should keep them
    /// aligned.
    pub fn is_lead_out(&self) -> bool {
        self.number == Self::CDDA_LEAD_OUT || self.number == Self::NON_CDDA_LEAD_OUT
    }

    /// The 12-byte International Standard Recording Code as a printable
    /// string, or `None` when the field is absent.
    ///
    /// RFC 9639 §8.7.1 permits an all-zero ISRC to denote "absent"; this
    /// accessor maps that case (and only that case) to `None`. A
    /// non-absent ISRC is returned only when every byte of the 12-byte
    /// field is printable ASCII (`0x20..=0x7E`) — the character class the
    /// ISRC standard (a 2-char country code + 3-char registrant + 2-digit
    /// year + 5-digit designation) lives within. A field with embedded
    /// NULs or other non-printable bytes is neither all-zero nor a clean
    /// string, so it returns `None`; callers needing the raw bytes in
    /// that case read [`isrc`](Self::isrc) directly.
    ///
    /// Unlike the media catalog number there is no NUL-termination
    /// convention here: the ISRC field is exactly 12 bytes wide and a
    /// real ISRC fills all 12, so the accessor treats the whole field as
    /// one unit rather than slicing at a terminator.
    pub fn isrc_str(&self) -> Option<&str> {
        if self.isrc.iter().all(|&b| b == 0) {
            return None;
        }
        if self.isrc.iter().all(|&b| (0x20..=0x7E).contains(&b)) {
            // All bytes printable ASCII ⇒ valid UTF-8 by construction.
            std::str::from_utf8(&self.isrc).ok()
        } else {
            None
        }
    }
}

impl CueSheet {
    /// Spec maximum overall track count (99 + lead-out for CD-DA, but
    /// the on-wire `num_tracks` field is u8 so 256 is the absolute
    /// upper bound). We use this to size pre-allocations safely; it is
    /// not enforced as a parse-rejection threshold because non-CD-DA
    /// CUESHEETs can legitimately use the full u8 range.
    pub const MAX_TRACKS: usize = 256;

    /// The media catalog number as a printable string, or `None` when the
    /// field is absent.
    ///
    /// RFC 9639 §8.7 stores the catalog as 128 bytes of printable ASCII
    /// (`0x20..=0x7E`) right-padded with `0x00`; an all-zero field means
    /// "the originator declined to record one". This accessor returns the
    /// leading run of bytes up to the first `0x00` terminator, decoded as
    /// a string. It returns `None` when:
    ///
    /// * the field is empty (first byte is `0x00`, i.e. no catalog), or
    /// * any byte in the leading run is outside the printable-ASCII range
    ///   (a malformed catalog — callers wanting the raw bytes in that case
    ///   read [`media_catalog_number`](Self::media_catalog_number)
    ///   directly).
    ///
    /// Because the returned run is all printable ASCII it is always valid
    /// UTF-8, so the `str` conversion cannot fail. This mirrors the
    /// printable-leading-run rule [`write_cuesheet`] enforces when
    /// serialising, so a catalog this accessor returns `Some` for will
    /// round-trip through a write/parse cycle unchanged.
    pub fn media_catalog(&self) -> Option<&str> {
        let leading = self
            .media_catalog_number
            .iter()
            .position(|&b| b == 0)
            .unwrap_or(self.media_catalog_number.len());
        if leading == 0 {
            return None;
        }
        let run = &self.media_catalog_number[..leading];
        if run.iter().all(|&b| (0x20..=0x7E).contains(&b)) {
            std::str::from_utf8(run).ok()
        } else {
            None
        }
    }

    /// The trailing lead-out track, or `None` when the track list is
    /// empty.
    ///
    /// Per RFC 9639 §8.7 the lead-out is always the last entry in the
    /// track list and carries the stream's total audio sample count in
    /// its `offset_samples` with no index points. Parser output always
    /// satisfies this (the writer requires a non-empty list and the
    /// parser requires `num_tracks ≥ 1`), so on a parsed `CueSheet` this
    /// returns `Some`. It does not verify the trailing track's *number*
    /// is a lead-out sentinel — use
    /// [`CueSheetTrack::is_lead_out`](CueSheetTrack::is_lead_out) on the
    /// result if that distinction matters.
    pub fn lead_out(&self) -> Option<&CueSheetTrack> {
        self.tracks.last()
    }

    /// Iterator over the playable (non-lead-out) tracks, in order.
    ///
    /// Skips the trailing lead-out terminator (RFC 9639 §8.7), yielding
    /// only the audio/data tracks a player would expose as selectable
    /// entries. Equivalent to `tracks` with the final element dropped
    /// when that element's number is a lead-out sentinel; if the trailing
    /// track is *not* lead-out-numbered (a hand-built `CueSheet` that
    /// omitted the terminator) every track is yielded.
    pub fn playable_tracks(&self) -> impl Iterator<Item = &CueSheetTrack> {
        let drop_last = self.tracks.last().is_some_and(CueSheetTrack::is_lead_out);
        let keep = if drop_last {
            self.tracks.len() - 1
        } else {
            self.tracks.len()
        };
        self.tracks[..keep].iter()
    }
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
///   * the media catalog number's leading run (up to the first NUL)
///     contains a byte outside printable ASCII `0x20..=0x7E`
///     (RFC 9639 §8.7; mirrors the [`write_cuesheet`] check so every
///     accepted block re-writes cleanly through its own writer),
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
    // RFC 9639 §8.7: "Media catalog number in ASCII printable
    // characters 0x20-0x7E", right-padded with 0x00 when shorter
    // than 128 bytes. Enforce the printable rule on the leading run
    // (up to the first NUL) — exactly the check `write_cuesheet`
    // applies — so every CUESHEET this parser accepts can be
    // re-emitted by its own writer. Without this the chain-level
    // round-trip breaks: `MetadataChain::parse` would accept a
    // block that `MetadataChain::write` then refuses.
    let catalog_leading = media_catalog_number
        .iter()
        .position(|&b| b == 0)
        .unwrap_or(media_catalog_number.len());
    for &b in &media_catalog_number[..catalog_leading] {
        if !(0x20..=0x7E).contains(&b) {
            return Err(Error::invalid(format!(
                "FLAC CUESHEET: media catalog byte 0x{b:02X} outside printable \
                 ASCII range 0x20..=0x7E (RFC 9639 §8.7)"
            )));
        }
    }
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

/// The defined PICTURE-block picture types (RFC 9639 §8.8, Table 13).
///
/// The on-wire `picture_type` field is a 32-bit unsigned integer, but
/// only values 0 through 20 carry a defined meaning; Table 13 says
/// "values other than those listed in the table are reserved". This
/// enum names the 21 defined codes and keeps every other (reserved or
/// out-of-range) value in the catch-all [`Reserved`](Self::Reserved)
/// variant, which preserves the raw `u32` so an application can re-emit
/// a value it parsed without loss.
///
/// The taxonomy is shared with the ID3v2 `APIC` frame, which is why the
/// numbering has the historical curiosity at value 17 ("A bright
/// colored fish"); RFC 9639 §8.8 notes the origin of that value is
/// unclear and that it was copied to maintain compatibility, and
/// discourages applications from offering it to users.
///
/// Two of the codes carry a file-level multiplicity constraint that
/// this value-level enum does not police: Table 13 states "there MAY
/// only be one each of picture types 1 and 2 in a file". That is a
/// chain-level invariant the demuxer/muxer would enforce across a
/// metadata block list, not a property of a single decoded block.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PictureType {
    /// 0 — Other.
    Other,
    /// 1 — PNG file icon of 32×32 pixels (RFC 9639 §8.8 cites
    /// [RFC2083]). Subject to the "one per file" constraint.
    FileIcon32,
    /// 2 — General file icon. Subject to the "one per file" constraint.
    GeneralFileIcon,
    /// 3 — Front cover. Players commonly display this during playback.
    FrontCover,
    /// 4 — Back cover.
    BackCover,
    /// 5 — Liner notes page.
    LinerNotes,
    /// 6 — Media label (e.g. CD, Vinyl or Cassette label).
    MediaLabel,
    /// 7 — Lead artist, lead performer, or soloist.
    LeadArtist,
    /// 8 — Artist or performer.
    Artist,
    /// 9 — Conductor.
    Conductor,
    /// 10 — Band or orchestra.
    Band,
    /// 11 — Composer.
    Composer,
    /// 12 — Lyricist or text writer.
    Lyricist,
    /// 13 — Recording location.
    RecordingLocation,
    /// 14 — During recording.
    DuringRecording,
    /// 15 — During performance.
    DuringPerformance,
    /// 16 — Movie or video screen capture.
    ScreenCapture,
    /// 17 — "A bright colored fish". Origin unclear; retained for
    /// ID3v2 compatibility, discouraged for new use (RFC 9639 §8.8).
    BrightColoredFish,
    /// 18 — Illustration.
    Illustration,
    /// 19 — Band or artist logotype.
    BandLogo,
    /// 20 — Publisher or studio logotype.
    PublisherLogo,
    /// Any value outside the defined `0..=20` range. RFC 9639 §8.8
    /// reserves these; the raw code is preserved so a parsed value can
    /// be re-emitted unchanged.
    Reserved(u32),
}

impl PictureType {
    /// Map a raw on-wire `picture_type` code to its typed variant.
    /// Codes outside the defined `0..=20` range map to
    /// [`Reserved`](Self::Reserved) carrying the raw value.
    pub fn from_code(code: u32) -> Self {
        match code {
            0 => Self::Other,
            1 => Self::FileIcon32,
            2 => Self::GeneralFileIcon,
            3 => Self::FrontCover,
            4 => Self::BackCover,
            5 => Self::LinerNotes,
            6 => Self::MediaLabel,
            7 => Self::LeadArtist,
            8 => Self::Artist,
            9 => Self::Conductor,
            10 => Self::Band,
            11 => Self::Composer,
            12 => Self::Lyricist,
            13 => Self::RecordingLocation,
            14 => Self::DuringRecording,
            15 => Self::DuringPerformance,
            16 => Self::ScreenCapture,
            17 => Self::BrightColoredFish,
            18 => Self::Illustration,
            19 => Self::BandLogo,
            20 => Self::PublisherLogo,
            other => Self::Reserved(other),
        }
    }

    /// The raw on-wire `picture_type` code for this variant. For
    /// [`Reserved`](Self::Reserved) this is the preserved raw value, so
    /// `PictureType::from_code(x).code() == x` for every `u32`.
    pub fn code(self) -> u32 {
        match self {
            Self::Other => 0,
            Self::FileIcon32 => 1,
            Self::GeneralFileIcon => 2,
            Self::FrontCover => 3,
            Self::BackCover => 4,
            Self::LinerNotes => 5,
            Self::MediaLabel => 6,
            Self::LeadArtist => 7,
            Self::Artist => 8,
            Self::Conductor => 9,
            Self::Band => 10,
            Self::Composer => 11,
            Self::Lyricist => 12,
            Self::RecordingLocation => 13,
            Self::DuringRecording => 14,
            Self::DuringPerformance => 15,
            Self::ScreenCapture => 16,
            Self::BrightColoredFish => 17,
            Self::Illustration => 18,
            Self::BandLogo => 19,
            Self::PublisherLogo => 20,
            Self::Reserved(v) => v,
        }
    }

    /// True for the two codes (1 = 32×32 PNG file icon, 2 = general
    /// file icon) that RFC 9639 §8.8 / Table 13 limits to at most one
    /// occurrence per file. A muxer assembling a metadata block list
    /// can use this to enforce that constraint.
    pub fn is_unique_per_file(self) -> bool {
        matches!(self, Self::FileIcon32 | Self::GeneralFileIcon)
    }
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

impl Picture {
    /// The MIME-type sentinel `"-->"` that, when stored in
    /// [`mime_type`](Self::mime_type), signals that [`data`](Self::data)
    /// holds a URI string pointing at the image rather than the encoded
    /// image bytes themselves (RFC 9639 §8.8).
    pub const URI_SENTINEL: &'static str = "-->";

    /// The pixel width and height of the type-1 ("PNG file icon")
    /// picture, which RFC 9639 §8.8 / Table 13 fixes at 32×32. A muxer
    /// emitting a [`PictureType::FileIcon32`] block can use this to fill
    /// the `width`/`height` fields; a reader can use it to validate one.
    pub const FILE_ICON_DIMENSION: u32 = 32;

    /// The typed [`PictureType`] for this block's raw `picture_type`
    /// code (RFC 9639 §8.8, Table 13). Codes outside the defined
    /// `0..=20` range come back as [`PictureType::Reserved`] carrying
    /// the raw value, so this never discards information.
    pub fn picture_kind(&self) -> PictureType {
        PictureType::from_code(self.picture_type)
    }

    /// True when this block carries a URI reference rather than embedded
    /// image bytes — i.e. when [`mime_type`](Self::mime_type) is the
    /// `"-->"` sentinel (RFC 9639 §8.8). In that case
    /// [`data`](Self::data) is the URI's bytes; callers wanting it as a
    /// string should use [`uri`](Self::uri).
    pub fn is_uri(&self) -> bool {
        self.mime_type == Self::URI_SENTINEL
    }

    /// The image URI when this block is a URI reference, or `None` when
    /// it carries embedded image data.
    ///
    /// Returns `Some` only when [`is_uri`](Self::is_uri) holds and the
    /// stored [`data`](Self::data) bytes are valid UTF-8 (a URI is a
    /// text string per RFC 9639 §8.8 / [RFC3986]); a URI block whose
    /// data is not valid UTF-8 is malformed and returns `None`, leaving
    /// the raw bytes available through [`data`](Self::data). For a
    /// block holding embedded image bytes this always returns `None`.
    ///
    /// RFC 9639 §8.8 notes a URI may be absolute or relative (resolved
    /// against the FLAC content's own URI) and that applications MUST
    /// obtain explicit user approval before retrieving remote or
    /// out-of-directory images; this accessor only surfaces the string
    /// and performs no retrieval.
    pub fn uri(&self) -> Option<&str> {
        if self.is_uri() {
            std::str::from_utf8(&self.data).ok()
        } else {
            None
        }
    }
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

/// One typed metadata block in a FLAC metadata chain (RFC 9639 §8).
///
/// Each variant wraps the typed contents this module already
/// parses / writes for the matching block kind, so a caller editing
/// a metadata chain works entirely at the typed level and never
/// touches the 4-byte block-header framing by hand:
///
/// * [`StreamInfo`](MetadataBlock::StreamInfo) — §8.2, via
///   [`StreamInfo::parse`] / [`StreamInfo::write`];
/// * [`Padding`](MetadataBlock::Padding) — §8.3, the payload byte
///   count (the payload content is all-zero by definition, so the
///   length is the only degree of freedom);
/// * [`Application`](MetadataBlock::Application) — §8.4, via
///   [`parse_application`] / [`write_application`];
/// * [`SeekTable`](MetadataBlock::SeekTable) — §8.5, via
///   [`parse_seektable`] / [`write_seektable`]. The placeholder
///   count is carried alongside the real points so a table with a
///   pre-reserved placeholder tail keeps its on-wire size across a
///   parse / write cycle (the parser drops placeholder entries from
///   the point list, as it must per §8.5.1);
/// * [`VorbisComment`](MetadataBlock::VorbisComment) — §8.6, via
///   [`parse_vorbis_comment`] / [`write_vorbis_comment`];
/// * [`CueSheet`](MetadataBlock::CueSheet) — §8.7, via
///   [`parse_cuesheet`] / [`write_cuesheet`];
/// * [`Picture`](MetadataBlock::Picture) — §8.8, via
///   [`parse_picture`] / [`write_picture`];
/// * [`Reserved`](MetadataBlock::Reserved) — block-type codes
///   7..=126, which §8.1 Table 2 reserves for future versions of the
///   format. RFC 9639 §5 says future versions MAY use the reserved
///   space without breaking older streams, so a chain editor MUST
///   NOT destroy blocks it does not recognise: the payload is kept
///   as opaque bytes and re-emitted unchanged.
///
/// Block-type code 127 has no variant: §8.1 Table 2 marks it
/// forbidden (it would collide with the frame sync code), so
/// [`MetadataChain::parse`] rejects it and the writer cannot be
/// asked to produce it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum MetadataBlock {
    /// STREAMINFO (§8.2) — the mandatory first block.
    StreamInfo(StreamInfo),
    /// PADDING (§8.3) — `len` zero bytes of reserved space.
    Padding(u32),
    /// APPLICATION (§8.4) — registered ID + opaque payload.
    Application(Application),
    /// SEEKTABLE (§8.5) — real seek points plus the number of
    /// trailing placeholder entries (§8.5.1) the on-wire table
    /// reserves for future insertions.
    SeekTable {
        /// Real (non-placeholder) seek points.
        points: Vec<SeekPoint>,
        /// Number of placeholder entries at the table's tail.
        placeholder_count: usize,
    },
    /// VORBIS_COMMENT (§8.6) — vendor string + tag fields.
    VorbisComment(VorbisComment),
    /// CUESHEET (§8.7) — track / index-point structure.
    CueSheet(CueSheet),
    /// PICTURE (§8.8) — attached picture or picture URI.
    Picture(Picture),
    /// Reserved block-type codes 7..=126 (§8.1 Table 2), preserved
    /// opaque so future-format blocks survive a chain rewrite.
    Reserved {
        /// The 7-bit block-type code (7..=126).
        code: u8,
        /// The unmodified block payload.
        payload: Vec<u8>,
    },
}

impl MetadataBlock {
    /// The [`BlockType`] this block serialises under (§8.1 Table 2).
    pub fn block_type(&self) -> BlockType {
        match self {
            Self::StreamInfo(_) => BlockType::StreamInfo,
            Self::Padding(_) => BlockType::Padding,
            Self::Application(_) => BlockType::Application,
            Self::SeekTable { .. } => BlockType::SeekTable,
            Self::VorbisComment(_) => BlockType::VorbisComment,
            Self::CueSheet(_) => BlockType::CueSheet,
            Self::Picture(_) => BlockType::Picture,
            Self::Reserved { code, .. } => BlockType::Reserved(*code),
        }
    }

    /// Serialise this block's **payload** (no 4-byte header) through
    /// the matching typed writer. Each writer enforces its own
    /// wire-format invariants; see [`write_seektable`],
    /// [`write_cuesheet`], [`write_picture`],
    /// [`write_vorbis_comment`], [`write_application`],
    /// [`write_padding`], and [`StreamInfo::write`]. A `Reserved`
    /// payload is returned unchanged after the 24-bit length-ceiling
    /// check every other writer applies.
    pub fn write_payload(&self) -> Result<Vec<u8>> {
        match self {
            Self::StreamInfo(si) => si.write(),
            Self::Padding(len) => write_padding(*len as usize),
            Self::Application(app) => write_application(app),
            Self::SeekTable {
                points,
                placeholder_count,
            } => write_seektable(points, *placeholder_count),
            Self::VorbisComment(vc) => write_vorbis_comment(vc),
            Self::CueSheet(cs) => write_cuesheet(cs),
            Self::Picture(pic) => write_picture(pic),
            Self::Reserved { payload, .. } => {
                if payload.len() > BlockHeader::MAX_LENGTH as usize {
                    return Err(Error::invalid(format!(
                        "FLAC reserved metadata block: payload size {} exceeds 24-bit \
                         metadata length max ({})",
                        payload.len(),
                        BlockHeader::MAX_LENGTH
                    )));
                }
                Ok(payload.clone())
            }
        }
    }
}

/// A complete FLAC metadata block chain (RFC 9639 §8): every block
/// between the `fLaC` stream marker and the first audio frame.
///
/// [`MetadataChain::parse`] / [`MetadataChain::write`] round-trip the
/// chain through the typed [`MetadataBlock`] representation and both
/// enforce the stream-level structural rules the per-block parsers
/// cannot see, each a MUST in RFC 9639:
///
/// * §8: at least one metadata block is present and the first one is
///   a streaminfo block;
/// * §8.2: no more than one streaminfo block;
/// * §8.5: no more than one seek table block;
/// * §8.6: no more than one Vorbis comment block;
/// * §8.8 Table 13: no more than one each of picture types 1 and 2
///   (the 32×32 file icon and the general file icon);
/// * §5 / §8.1 Table 2: block-type code 127 is forbidden anywhere in
///   the chain;
/// * §8.1: the `last` flag is set on exactly the final block — the
///   parser stops after the flagged block and the writer places the
///   flag itself, so a hand-assembled chain cannot mis-place it.
///
/// The chain bytes start at the first block **header** — i.e.
/// immediately after the 4-byte [`FLAC_MAGIC`] in a native FLAC file,
/// or byte 0 of the `extradata` this crate's encoder produces and its
/// muxer consumes (extradata is exactly a metadata chain without the
/// marker).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetadataChain {
    /// The chain's blocks in stream order. Directly editable;
    /// [`MetadataChain::write`] re-validates before emitting bytes.
    pub blocks: Vec<MetadataBlock>,
}

impl MetadataChain {
    /// Parse a metadata block chain starting at a block header (the
    /// byte after the `fLaC` marker, or `extradata` byte 0).
    ///
    /// Walks block headers until the block with the `last` flag set
    /// (§8.1), parsing each payload through its typed parser, then
    /// validates the chain-level rules (see [`MetadataChain::validate`]).
    /// Returns the chain plus the number of bytes consumed, so a
    /// caller positioned at the start of a file body knows where the
    /// first audio frame begins; trailing bytes after the last block
    /// are not touched.
    ///
    /// Returns `Error::NeedMore` when the bytes run out before a
    /// `last`-flagged block completes, and `Error::invalid` for the
    /// forbidden block-type 127 (§8.1 Table 2), a payload its typed
    /// parser rejects, or a chain-rule violation.
    ///
    /// Two §8.5.1 placeholder-tail notes: placeholder seek points are
    /// represented as a count (not as points), and a spec-violating
    /// table that interleaved placeholders mid-table parses to the
    /// same count and re-writes with the placeholders moved to the
    /// tail where they MUST occur. Real seek points that are not
    /// strictly ascending by sample number are rejected outright
    /// (§8.5.1 sorted + unique MUSTs — the same rules
    /// [`write_seektable`] enforces, so every accepted chain can
    /// re-write). Similarly, a PADDING payload is
    /// represented by its length alone: §8.3 defines the content as
    /// all-zero bits, so a (non-conformant) producer's non-zero
    /// padding bytes are not preserved and re-write as zeros.
    pub fn parse(bytes: &[u8]) -> Result<(Self, usize)> {
        let mut blocks = Vec::new();
        let mut pos = 0usize;
        loop {
            if bytes.len() < pos + 4 {
                return Err(Error::NeedMore);
            }
            let hdr = BlockHeader::parse(&bytes[pos..])?;
            if hdr.block_type == BlockType::Invalid {
                return Err(Error::invalid(
                    "FLAC metadata chain: block-type code 127 is forbidden (RFC 9639 §8.1)",
                ));
            }
            let start = pos + 4;
            let end = start
                .checked_add(hdr.length as usize)
                .ok_or(Error::NeedMore)?;
            if bytes.len() < end {
                return Err(Error::NeedMore);
            }
            let payload = &bytes[start..end];
            let block = match hdr.block_type {
                BlockType::StreamInfo => MetadataBlock::StreamInfo(StreamInfo::parse(payload)?),
                BlockType::Padding => MetadataBlock::Padding(hdr.length),
                BlockType::Application => MetadataBlock::Application(parse_application(payload)?),
                BlockType::SeekTable => {
                    let points = parse_seektable(payload);
                    // §8.5.1: "Seek points within a table MUST be
                    // sorted in ascending order by sample number"
                    // and "MUST be unique by sample number, with the
                    // exception of placeholder points". The
                    // block-level `parse_seektable` has no error
                    // channel (it returns a bare `Vec` and tolerates
                    // wire-order quirks), but the typed chain must
                    // reject a violating table here: `write_seektable`
                    // enforces both MUSTs, so accepting the block
                    // would produce a chain that parses Ok yet fails
                    // its own `write` — breaking the parse/write
                    // round-trip contract this type documents.
                    if points
                        .windows(2)
                        .any(|p| p[0].sample_number >= p[1].sample_number)
                    {
                        return Err(Error::invalid(
                            "FLAC metadata chain: SEEKTABLE seek points must be strictly \
                             ascending by sample number (RFC 9639 §8.5.1)",
                        ));
                    }
                    // Entries the parser dropped were placeholders
                    // (sentinel sample number, §8.5.1); a partial
                    // trailing entry (length not a multiple of 18)
                    // is ignored by the parser and not counted.
                    let placeholder_count = payload.len() / 18 - points.len();
                    MetadataBlock::SeekTable {
                        points,
                        placeholder_count,
                    }
                }
                BlockType::VorbisComment => {
                    MetadataBlock::VorbisComment(parse_vorbis_comment(payload)?)
                }
                BlockType::CueSheet => MetadataBlock::CueSheet(parse_cuesheet(payload)?),
                BlockType::Picture => MetadataBlock::Picture(parse_picture(payload)?),
                BlockType::Reserved(code) => MetadataBlock::Reserved {
                    code,
                    payload: payload.to_vec(),
                },
                BlockType::Invalid => unreachable!("rejected above"),
            };
            blocks.push(block);
            pos = end;
            if hdr.last {
                break;
            }
        }
        let chain = Self { blocks };
        chain.validate()?;
        Ok((chain, pos))
    }

    /// Validate the chain-level structural rules (each a MUST in
    /// RFC 9639; see the type-level docs for the rule list). Called
    /// by both [`MetadataChain::parse`] and [`MetadataChain::write`];
    /// public so a caller assembling a chain by hand can check it
    /// without serialising.
    pub fn validate(&self) -> Result<()> {
        // §8: "one or more metadata blocks MUST be present ... The
        // first metadata block MUST be a streaminfo metadata block."
        match self.blocks.first() {
            None => {
                return Err(Error::invalid(
                    "FLAC metadata chain: at least one metadata block must be present \
                     (RFC 9639 §8)",
                ));
            }
            Some(MetadataBlock::StreamInfo(_)) => {}
            Some(other) => {
                return Err(Error::invalid(format!(
                    "FLAC metadata chain: first block must be STREAMINFO (RFC 9639 §8), \
                     got {:?}",
                    other.block_type()
                )));
            }
        }
        let mut streaminfo = 0usize;
        let mut seektables = 0usize;
        let mut vorbis_comments = 0usize;
        let mut icon_32 = 0usize; // picture type 1
        let mut icon_other = 0usize; // picture type 2
        for b in &self.blocks {
            match b {
                MetadataBlock::StreamInfo(_) => streaminfo += 1,
                MetadataBlock::SeekTable { .. } => seektables += 1,
                MetadataBlock::VorbisComment(_) => vorbis_comments += 1,
                MetadataBlock::Picture(p) if p.picture_kind().is_unique_per_file() => {
                    // Table 13 limits exactly two codes to one
                    // occurrence per file: 1 (32×32 file icon) and
                    // 2 (general file icon).
                    if p.picture_type == 1 {
                        icon_32 += 1;
                    } else {
                        icon_other += 1;
                    }
                }
                _ => {}
            }
        }
        if streaminfo > 1 {
            return Err(Error::invalid(format!(
                "FLAC metadata chain: no more than one STREAMINFO block (RFC 9639 §8.2), \
                 got {streaminfo}"
            )));
        }
        if seektables > 1 {
            return Err(Error::invalid(format!(
                "FLAC metadata chain: no more than one SEEKTABLE block (RFC 9639 §8.5), \
                 got {seektables}"
            )));
        }
        if vorbis_comments > 1 {
            return Err(Error::invalid(format!(
                "FLAC metadata chain: no more than one VORBIS_COMMENT block (RFC 9639 §8.6), \
                 got {vorbis_comments}"
            )));
        }
        if icon_32 > 1 || icon_other > 1 {
            return Err(Error::invalid(format!(
                "FLAC metadata chain: no more than one each of picture types 1 and 2 \
                 (RFC 9639 §8.8 Table 13); got {icon_32} of type 1 and {icon_other} of type 2"
            )));
        }
        Ok(())
    }

    /// Serialise the chain back into on-wire bytes: each block's
    /// 4-byte header (§8.1) followed by its payload, with the `last`
    /// flag set on exactly the final block. Validates the chain-level
    /// rules first (see [`MetadataChain::validate`]) and then defers
    /// each payload to its typed writer, so every per-block invariant
    /// those writers enforce applies here too.
    ///
    /// The output starts at the first block header — prepend
    /// [`FLAC_MAGIC`] for a full native-FLAC stream prefix, or use it
    /// directly as muxer `extradata`.
    pub fn write(&self) -> Result<Vec<u8>> {
        self.validate()?;
        let mut out = Vec::new();
        let final_index = self.blocks.len() - 1;
        for (i, block) in self.blocks.iter().enumerate() {
            let payload = block.write_payload()?;
            let length = u32::try_from(payload.len()).map_err(|_| {
                Error::invalid(format!(
                    "FLAC metadata chain: block payload size {} exceeds 24-bit \
                     metadata length max ({})",
                    payload.len(),
                    BlockHeader::MAX_LENGTH
                ))
            })?;
            BlockHeader {
                last: i == final_index,
                block_type: block.block_type(),
                length,
            }
            .write_into(&mut out)?;
            out.extend_from_slice(&payload);
        }
        Ok(out)
    }

    /// The chain's STREAMINFO contents, when the first block is one
    /// (always true for a chain [`MetadataChain::validate`] accepts;
    /// `None` only for a hand-built chain that would fail validation).
    pub fn stream_info(&self) -> Option<&StreamInfo> {
        match self.blocks.first() {
            Some(MetadataBlock::StreamInfo(si)) => Some(si),
            _ => None,
        }
    }
}

/// Parse a FLAC metadata block chain (RFC 9639 §8).
///
/// Free-function alias for [`MetadataChain::parse`] — kept for
/// symmetry with the other top-level parsers in this module.
pub fn parse_metadata_chain(bytes: &[u8]) -> Result<(MetadataChain, usize)> {
    MetadataChain::parse(bytes)
}

/// Serialise a [`MetadataChain`] back into on-wire bytes.
///
/// Free-function alias for [`MetadataChain::write`] — kept for
/// symmetry with the other top-level writers in this module.
pub fn write_metadata_chain(chain: &MetadataChain) -> Result<Vec<u8>> {
    chain.write()
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

    fn track(number: u8, isrc: [u8; 12]) -> CueSheetTrack {
        CueSheetTrack {
            offset_samples: 0,
            number,
            isrc,
            is_audio: true,
            pre_emphasis: false,
            indices: vec![CueSheetIndex {
                offset_samples: 0,
                number: 1,
            }],
        }
    }

    fn lead_out(number: u8) -> CueSheetTrack {
        CueSheetTrack {
            offset_samples: 1_000_000,
            number,
            isrc: [0u8; 12],
            is_audio: true,
            pre_emphasis: false,
            indices: vec![],
        }
    }

    #[test]
    fn cuesheet_track_is_lead_out_recognises_both_sentinels() {
        assert!(lead_out(CueSheetTrack::CDDA_LEAD_OUT).is_lead_out());
        assert!(lead_out(CueSheetTrack::NON_CDDA_LEAD_OUT).is_lead_out());
        assert_eq!(CueSheetTrack::CDDA_LEAD_OUT, 170);
        assert_eq!(CueSheetTrack::NON_CDDA_LEAD_OUT, 255);
        // A normal audio track number is not a lead-out.
        assert!(!track(1, [0u8; 12]).is_lead_out());
        assert!(!track(99, [0u8; 12]).is_lead_out());
        assert!(!track(254, [0u8; 12]).is_lead_out());
    }

    #[test]
    fn cuesheet_track_isrc_str_round_trips_printable() {
        let t = track(2, *b"GBAYE0601477");
        assert_eq!(t.isrc_str(), Some("GBAYE0601477"));
    }

    #[test]
    fn cuesheet_track_isrc_str_absent_is_none() {
        // All-zero ISRC is the spec's "absent" encoding.
        assert_eq!(track(1, [0u8; 12]).isrc_str(), None);
    }

    #[test]
    fn cuesheet_track_isrc_str_rejects_embedded_nul_and_nonprintable() {
        // A field that is neither all-zero nor all-printable (embedded
        // NUL mid-run, or a control byte) yields None — callers read the
        // raw `isrc` bytes in that case.
        let mut isrc = *b"GBAYE0601477";
        isrc[5] = 0x00; // embedded NUL -> not all-zero, not all-printable
        assert_eq!(track(1, isrc).isrc_str(), None);
        let mut ctrl = *b"GBAYE0601477";
        ctrl[0] = 0x07; // BEL control char
        assert_eq!(track(1, ctrl).isrc_str(), None);
        let mut hi = *b"GBAYE0601477";
        hi[0] = 0x80; // high bit set
        assert_eq!(track(1, hi).isrc_str(), None);
    }

    #[test]
    fn cuesheet_media_catalog_returns_leading_printable_run() {
        let cs = CueSheet {
            media_catalog_number: {
                let mut m = [0u8; 128];
                m[..13].copy_from_slice(b"0123456789012");
                m
            },
            lead_in_samples: 0,
            is_cdda: true,
            tracks: vec![lead_out(CueSheetTrack::CDDA_LEAD_OUT)],
        };
        assert_eq!(cs.media_catalog(), Some("0123456789012"));
    }

    #[test]
    fn cuesheet_media_catalog_empty_is_none() {
        let cs = CueSheet {
            media_catalog_number: [0u8; 128],
            lead_in_samples: 0,
            is_cdda: false,
            tracks: vec![lead_out(CueSheetTrack::NON_CDDA_LEAD_OUT)],
        };
        assert_eq!(cs.media_catalog(), None);
    }

    #[test]
    fn cuesheet_media_catalog_rejects_nonprintable_leading_run() {
        let cs = CueSheet {
            media_catalog_number: {
                let mut m = [0u8; 128];
                m[..4].copy_from_slice(&[b'A', 0x07, b'C', b'D']); // BEL in run
                m
            },
            lead_in_samples: 0,
            is_cdda: false,
            tracks: vec![lead_out(CueSheetTrack::NON_CDDA_LEAD_OUT)],
        };
        assert_eq!(cs.media_catalog(), None);
    }

    #[test]
    fn cuesheet_media_catalog_round_trips_through_writer() {
        // A catalog media_catalog() accepts must survive write + parse.
        let cs_in = CueSheet {
            media_catalog_number: {
                let mut m = [0u8; 128];
                m[..13].copy_from_slice(b"0123456789012");
                m
            },
            lead_in_samples: 0,
            is_cdda: true,
            tracks: vec![lead_out(CueSheetTrack::CDDA_LEAD_OUT)],
        };
        let bytes = write_cuesheet(&cs_in).unwrap();
        let cs_out = parse_cuesheet(&bytes).unwrap();
        assert_eq!(cs_in.media_catalog(), cs_out.media_catalog());
        assert_eq!(cs_out.media_catalog(), Some("0123456789012"));
    }

    #[test]
    fn cuesheet_lead_out_and_playable_tracks() {
        let cs = CueSheet {
            media_catalog_number: [0u8; 128],
            lead_in_samples: 0,
            is_cdda: true,
            tracks: vec![
                track(1, [0u8; 12]),
                track(2, *b"GBAYE0601477"),
                lead_out(CueSheetTrack::CDDA_LEAD_OUT),
            ],
        };
        // lead_out() is the trailing entry.
        assert_eq!(cs.lead_out().map(|t| t.number), Some(170));
        assert!(cs.lead_out().unwrap().is_lead_out());
        // playable_tracks() drops the lead-out terminator.
        let playable: Vec<u8> = cs.playable_tracks().map(|t| t.number).collect();
        assert_eq!(playable, vec![1, 2]);
    }

    #[test]
    fn cuesheet_playable_tracks_keeps_all_when_no_leadout_sentinel() {
        // A hand-built cuesheet whose trailing track is NOT lead-out
        // numbered: every track is playable.
        let cs = CueSheet {
            media_catalog_number: [0u8; 128],
            lead_in_samples: 0,
            is_cdda: false,
            tracks: vec![track(1, [0u8; 12]), track(2, [0u8; 12])],
        };
        let playable: Vec<u8> = cs.playable_tracks().map(|t| t.number).collect();
        assert_eq!(playable, vec![1, 2]);
        assert_eq!(cs.lead_out().map(|t| t.number), Some(2));
        assert!(!cs.lead_out().unwrap().is_lead_out());
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
    fn picture_type_code_round_trip_all_defined() {
        // RFC 9639 §8.8 Table 13 defines codes 0..=20. Each must map to
        // a named variant and back to its own code with no loss.
        for code in 0u32..=20 {
            let kind = PictureType::from_code(code);
            assert!(
                !matches!(kind, PictureType::Reserved(_)),
                "code {code} should be a defined variant, got {kind:?}"
            );
            assert_eq!(kind.code(), code);
        }
    }

    #[test]
    fn picture_type_named_codes_match_table_13() {
        // Spot-check the named variants against Table 13's exact rows so
        // a typo in the dispatch table is caught.
        assert_eq!(PictureType::from_code(0), PictureType::Other);
        assert_eq!(PictureType::from_code(1), PictureType::FileIcon32);
        assert_eq!(PictureType::from_code(2), PictureType::GeneralFileIcon);
        assert_eq!(PictureType::from_code(3), PictureType::FrontCover);
        assert_eq!(PictureType::from_code(4), PictureType::BackCover);
        assert_eq!(PictureType::from_code(17), PictureType::BrightColoredFish);
        assert_eq!(PictureType::from_code(20), PictureType::PublisherLogo);
    }

    #[test]
    fn picture_type_reserved_preserves_raw_value() {
        // Codes outside 0..=20 are reserved; the raw value must survive
        // a from_code/code cycle so a parsed block re-emits unchanged.
        for code in [21u32, 100, 0xFFFF, u32::MAX] {
            let kind = PictureType::from_code(code);
            assert_eq!(kind, PictureType::Reserved(code));
            assert_eq!(kind.code(), code);
        }
    }

    #[test]
    fn picture_type_unique_per_file_only_icons() {
        // Table 13: "there MAY only be one each of picture types 1 and
        // 2 in a file" — and no others carry that constraint.
        for code in 0u32..=20 {
            let kind = PictureType::from_code(code);
            let expect = code == 1 || code == 2;
            assert_eq!(
                kind.is_unique_per_file(),
                expect,
                "code {code} unique-per-file mismatch"
            );
        }
        assert!(!PictureType::Reserved(99).is_unique_per_file());
    }

    #[test]
    fn picture_kind_accessor_reflects_field() {
        let pic = Picture {
            picture_type: 3,
            mime_type: "image/png".into(),
            description: String::new(),
            width: 600,
            height: 600,
            depth: 24,
            colour_count: 0,
            data: b"PNG".to_vec(),
        };
        assert_eq!(pic.picture_kind(), PictureType::FrontCover);
        assert!(!pic.is_uri());
        assert_eq!(pic.uri(), None);
    }

    #[test]
    fn picture_uri_accessor_decodes_sentinel_block() {
        let pic = Picture {
            picture_type: 0,
            mime_type: Picture::URI_SENTINEL.into(),
            description: String::new(),
            width: 0,
            height: 0,
            depth: 0,
            colour_count: 0,
            data: b"https://example.invalid/cover.jpg".to_vec(),
        };
        assert!(pic.is_uri());
        assert_eq!(pic.uri(), Some("https://example.invalid/cover.jpg"));
    }

    #[test]
    fn picture_uri_accessor_rejects_non_utf8_uri_bytes() {
        // A "-->" block whose data is not valid UTF-8 is malformed; the
        // accessor returns None rather than lossily decoding, leaving
        // the raw bytes reachable through `data`.
        let pic = Picture {
            picture_type: 0,
            mime_type: Picture::URI_SENTINEL.into(),
            description: String::new(),
            width: 0,
            height: 0,
            depth: 0,
            colour_count: 0,
            data: vec![0xFF, 0xFE, 0x80],
        };
        assert!(pic.is_uri());
        assert_eq!(pic.uri(), None);
        assert_eq!(pic.data, vec![0xFF, 0xFE, 0x80]);
    }

    #[test]
    fn picture_file_icon_dimension_constant() {
        // RFC 9639 §8.8 / Table 13 fixes the type-1 file icon at 32×32.
        assert_eq!(Picture::FILE_ICON_DIMENSION, 32);
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

    // --- MetadataChain (RFC 9639 §8 chain-level rules) -----------------

    fn chain_streaminfo() -> StreamInfo {
        StreamInfo {
            min_block_size: 4096,
            max_block_size: 4096,
            min_frame_size: 0,
            max_frame_size: 0,
            sample_rate: 44_100,
            channels: 2,
            bits_per_sample: 16,
            total_samples: 44_100,
            md5: [0u8; 16],
        }
    }

    fn chain_picture(picture_type: u32) -> Picture {
        Picture {
            picture_type,
            mime_type: "image/png".into(),
            description: "cover".into(),
            width: 32,
            height: 32,
            depth: 24,
            colour_count: 0,
            data: vec![0x89, 0x50, 0x4E, 0x47],
        }
    }

    #[test]
    fn metadata_chain_full_round_trip() {
        // One of every defined block kind plus a reserved block, in a
        // legal arrangement; write → parse must reproduce the chain
        // and report consuming exactly the written byte count.
        let chain = MetadataChain {
            blocks: vec![
                MetadataBlock::StreamInfo(chain_streaminfo()),
                MetadataBlock::Application(Application {
                    id: 0x4154_4348,
                    data: vec![1, 2, 3],
                }),
                MetadataBlock::SeekTable {
                    points: vec![
                        SeekPoint {
                            sample_number: 0,
                            offset: 0,
                            frame_samples: 4096,
                        },
                        SeekPoint {
                            sample_number: 8192,
                            offset: 4242,
                            frame_samples: 4096,
                        },
                    ],
                    placeholder_count: 3,
                },
                MetadataBlock::VorbisComment(VorbisComment {
                    vendor: "oxideav-flac test".into(),
                    comments: vec![("TITLE".into(), "chain".into())],
                }),
                MetadataBlock::Picture(chain_picture(3)),
                MetadataBlock::Reserved {
                    code: 42,
                    payload: vec![0xAA, 0xBB],
                },
                MetadataBlock::Padding(64),
            ],
        };
        let bytes = chain.write().expect("write");
        let (back, consumed) = MetadataChain::parse(&bytes).expect("parse");
        assert_eq!(consumed, bytes.len());
        assert_eq!(back, chain);
        // Second generation is byte-stable.
        assert_eq!(back.write().expect("re-write"), bytes);
        // The free-function aliases dispatch to the same paths.
        let (back2, _) = parse_metadata_chain(&bytes).expect("alias parse");
        assert_eq!(write_metadata_chain(&back2).expect("alias write"), bytes);
        assert_eq!(back.stream_info(), Some(&chain_streaminfo()));
    }

    #[test]
    fn metadata_chain_last_flag_on_final_block_only() {
        let chain = MetadataChain {
            blocks: vec![
                MetadataBlock::StreamInfo(chain_streaminfo()),
                MetadataBlock::Padding(8),
                MetadataBlock::Padding(4),
            ],
        };
        let bytes = chain.write().expect("write");
        // Walk the three headers by hand: STREAMINFO(34), PADDING(8),
        // PADDING(4). §8.1: high bit of byte 0 is the `last` flag.
        assert_eq!(bytes[0] & 0x80, 0, "first block must not be last");
        let second = 4 + 34;
        assert_eq!(bytes[second] & 0x80, 0, "middle block must not be last");
        let third = second + 4 + 8;
        assert_ne!(bytes[third] & 0x80, 0, "final block must carry last flag");
        assert_eq!(bytes.len(), third + 4 + 4);
    }

    #[test]
    fn metadata_chain_trailing_bytes_left_untouched() {
        let chain = MetadataChain {
            blocks: vec![MetadataBlock::StreamInfo(chain_streaminfo())],
        };
        let mut bytes = chain.write().expect("write");
        let chain_len = bytes.len();
        bytes.extend_from_slice(&[0xFF, 0xF8, 0x00]); // pretend frame data
        let (back, consumed) = MetadataChain::parse(&bytes).expect("parse");
        assert_eq!(consumed, chain_len);
        assert_eq!(back.blocks.len(), 1);
    }

    #[test]
    fn metadata_chain_rejects_empty_and_wrong_first_block() {
        // §8: at least one block, and the first MUST be STREAMINFO.
        let empty = MetadataChain { blocks: vec![] };
        assert!(empty.validate().is_err());
        assert!(empty.write().is_err());
        let padding_first = MetadataChain {
            blocks: vec![
                MetadataBlock::Padding(8),
                MetadataBlock::StreamInfo(chain_streaminfo()),
            ],
        };
        assert!(padding_first.validate().is_err());
        assert!(padding_first.write().is_err());
        // Same rule on the parse side: a chain whose first block is
        // PADDING parses block-wise but fails chain validation.
        let mut bytes = Vec::new();
        BlockHeader {
            last: true,
            block_type: BlockType::Padding,
            length: 2,
        }
        .write_into(&mut bytes)
        .unwrap();
        bytes.extend_from_slice(&[0, 0]);
        assert!(MetadataChain::parse(&bytes).is_err());
    }

    #[test]
    fn metadata_chain_rejects_duplicate_singleton_blocks() {
        // §8.2 / §8.5 / §8.6: at most one STREAMINFO / SEEKTABLE /
        // VORBIS_COMMENT per stream.
        let dup_streaminfo = MetadataChain {
            blocks: vec![
                MetadataBlock::StreamInfo(chain_streaminfo()),
                MetadataBlock::StreamInfo(chain_streaminfo()),
            ],
        };
        assert!(dup_streaminfo.validate().is_err());
        let seektable = MetadataBlock::SeekTable {
            points: vec![],
            placeholder_count: 1,
        };
        let dup_seektable = MetadataChain {
            blocks: vec![
                MetadataBlock::StreamInfo(chain_streaminfo()),
                seektable.clone(),
                seektable,
            ],
        };
        assert!(dup_seektable.validate().is_err());
        let vc = MetadataBlock::VorbisComment(VorbisComment::default());
        let dup_vc = MetadataChain {
            blocks: vec![
                MetadataBlock::StreamInfo(chain_streaminfo()),
                vc.clone(),
                vc,
            ],
        };
        assert!(dup_vc.validate().is_err());
    }

    #[test]
    fn metadata_chain_picture_type_1_and_2_once_per_file() {
        // §8.8 Table 13: at most one each of picture types 1 and 2;
        // every other type may repeat freely.
        for unique_type in [1u32, 2] {
            let dup = MetadataChain {
                blocks: vec![
                    MetadataBlock::StreamInfo(chain_streaminfo()),
                    MetadataBlock::Picture(chain_picture(unique_type)),
                    MetadataBlock::Picture(chain_picture(unique_type)),
                ],
            };
            assert!(dup.validate().is_err(), "type {unique_type} must be unique");
        }
        // One of each unique type together is fine, as are repeated
        // non-unique types.
        let ok = MetadataChain {
            blocks: vec![
                MetadataBlock::StreamInfo(chain_streaminfo()),
                MetadataBlock::Picture(chain_picture(1)),
                MetadataBlock::Picture(chain_picture(2)),
                MetadataBlock::Picture(chain_picture(3)),
                MetadataBlock::Picture(chain_picture(3)),
            ],
        };
        assert!(ok.validate().is_ok());
        let bytes = ok.write().expect("write");
        assert_eq!(MetadataChain::parse(&bytes).expect("parse").0, ok);
    }

    #[test]
    fn metadata_chain_parse_rejects_forbidden_type_127() {
        // §8.1 Table 2: code 127 is forbidden (frame-sync collision).
        // Header byte 0x7F = last:0 + type:127.
        let bytes = [0x7Fu8, 0, 0, 0];
        assert!(MetadataChain::parse(&bytes).is_err());
    }

    #[test]
    fn metadata_chain_parse_needs_complete_last_flagged_block() {
        let chain = MetadataChain {
            blocks: vec![
                MetadataBlock::StreamInfo(chain_streaminfo()),
                MetadataBlock::Padding(16),
            ],
        };
        let bytes = chain.write().expect("write");
        // Truncations at every interesting boundary: mid-header,
        // mid-payload, and exactly before the final block — all must
        // report NeedMore rather than succeed or panic.
        for cut in [0, 2, 4, 20, 4 + 34, 4 + 34 + 2, bytes.len() - 1] {
            assert!(
                matches!(MetadataChain::parse(&bytes[..cut]), Err(Error::NeedMore)),
                "cut at {cut} must yield NeedMore"
            );
        }
    }

    #[test]
    fn metadata_chain_seektable_placeholders_keep_block_size() {
        // §8.5.1: placeholders reserve table space. The typed chain
        // carries them as a count so the re-written block occupies the
        // same number of bytes as the original.
        let chain = MetadataChain {
            blocks: vec![
                MetadataBlock::StreamInfo(chain_streaminfo()),
                MetadataBlock::SeekTable {
                    points: vec![SeekPoint {
                        sample_number: 0,
                        offset: 0,
                        frame_samples: 4096,
                    }],
                    placeholder_count: 7,
                },
            ],
        };
        let bytes = chain.write().expect("write");
        let (back, _) = MetadataChain::parse(&bytes).expect("parse");
        assert_eq!(
            back.blocks[1],
            MetadataBlock::SeekTable {
                points: vec![SeekPoint {
                    sample_number: 0,
                    offset: 0,
                    frame_samples: 4096,
                }],
                placeholder_count: 7,
            }
        );
        assert_eq!(back.write().expect("re-write").len(), bytes.len());
    }

    /// Serialise a typed chain by hand (header + payload per block,
    /// `last` on the final one) WITHOUT going through
    /// `MetadataChain::write`, so parser-side rejection tests aren't
    /// circular with the writer's own validation.
    fn assemble_chain_bytes(blocks: &[MetadataBlock]) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, b) in blocks.iter().enumerate() {
            let payload = b.write_payload().expect("payload");
            BlockHeader {
                last: i + 1 == blocks.len(),
                block_type: b.block_type(),
                length: payload.len() as u32,
            }
            .write_into(&mut out)
            .expect("header");
            out.extend_from_slice(&payload);
        }
        out
    }

    #[test]
    fn cuesheet_parser_rejects_nonprintable_catalog_byte() {
        // Fuzz regression (r283 `metadata_chain` target): the parser
        // accepted a media catalog number whose leading run carried a
        // non-printable byte, but `write_cuesheet` rejects exactly
        // that — so a parsed CUESHEET could fail its own re-write.
        // RFC 9639 §8.7 requires printable ASCII 0x20..=0x7E; the
        // parser now mirrors the writer's check.
        let good = synth_cuesheet(b"1234567890123", false, &[(0, 255, 0)]);
        assert!(parse_cuesheet(&good).is_ok());

        let mut bad = good.clone();
        bad[0] = 0x01; // non-printable byte in the leading run
        assert!(
            parse_cuesheet(&bad).is_err(),
            "non-printable catalog byte must be rejected at parse time"
        );

        // Bytes after the first NUL terminator are right-padding
        // (§8.7) and stay outside the check — exactly the leniency
        // boundary `write_cuesheet` applies, so parse/write stay
        // symmetric.
        let mut padded = good.clone();
        padded[13] = 0x00;
        padded[14] = 0x01; // garbage after the terminator
        let cs = parse_cuesheet(&padded).expect("post-NUL bytes are padding, not catalog");
        let rewritten = write_cuesheet(&cs).expect("parser-accepted block must re-write");
        assert_eq!(parse_cuesheet(&rewritten).expect("re-parse"), cs);
    }

    #[test]
    fn metadata_chain_rejects_unsorted_seektable_points() {
        // Fuzz regression (r283 `metadata_chain` target): §8.5.1 says
        // seek points MUST be sorted ascending and unique by sample
        // number. `parse_seektable` has no error channel, so the chain
        // parser accepted an out-of-order table that `write_seektable`
        // (and therefore `MetadataChain::write`) then refused —
        // breaking the parse-Ok ⇒ write-Ok contract.
        let si = MetadataBlock::StreamInfo(chain_streaminfo());
        let point = |n: u64| SeekPoint {
            sample_number: n,
            offset: n * 2,
            frame_samples: 4096,
        };
        let table_payload = |numbers: &[u64]| -> Vec<u8> {
            let mut p = Vec::new();
            for &n in numbers {
                p.extend_from_slice(&point(n).sample_number.to_be_bytes());
                p.extend_from_slice(&point(n).offset.to_be_bytes());
                p.extend_from_slice(&point(n).frame_samples.to_be_bytes());
            }
            p
        };
        let chain_with_table = |numbers: &[u64]| -> Vec<u8> {
            let mut out = assemble_chain_bytes(std::slice::from_ref(&si));
            out[0] &= 0x7F; // clear `last` on STREAMINFO
            let payload = table_payload(numbers);
            BlockHeader {
                last: true,
                block_type: BlockType::SeekTable,
                length: payload.len() as u32,
            }
            .write_into(&mut out)
            .expect("header");
            out.extend_from_slice(&payload);
            out
        };

        // Descending order: must be rejected.
        assert!(MetadataChain::parse(&chain_with_table(&[8192, 0])).is_err());
        // Duplicate sample number: must be rejected (uniqueness MUST).
        assert!(MetadataChain::parse(&chain_with_table(&[0, 0])).is_err());
        // Strictly ascending: accepted, and the accepted chain must
        // survive its own write + re-parse.
        let (chain, consumed) =
            MetadataChain::parse(&chain_with_table(&[0, 4096, 8192])).expect("sorted table");
        assert_eq!(consumed, chain_with_table(&[0, 4096, 8192]).len());
        let rewritten = chain.write().expect("parse-Ok chain must re-write");
        let (back, _) = MetadataChain::parse(&rewritten).expect("re-parse");
        assert_eq!(back, chain);
    }

    #[test]
    fn metadata_chain_rejects_cuesheet_with_nonprintable_catalog() {
        // Chain-level pin for the same r283 finding: the whole-chain
        // parser must surface the §8.7 catalog rule so a parsed chain
        // can always be re-written.
        let si = MetadataBlock::StreamInfo(chain_streaminfo());
        let mut out = assemble_chain_bytes(std::slice::from_ref(&si));
        out[0] &= 0x7F; // clear `last` on STREAMINFO
        let mut payload = synth_cuesheet(b"CATALOG", false, &[(0, 255, 0)]);
        payload[0] = 0x07; // non-printable leading catalog byte
        BlockHeader {
            last: true,
            block_type: BlockType::CueSheet,
            length: payload.len() as u32,
        }
        .write_into(&mut out)
        .expect("header");
        out.extend_from_slice(&payload);
        assert!(MetadataChain::parse(&out).is_err());
    }

    #[test]
    fn metadata_chain_parses_encoder_extradata() {
        // The encoder's extradata is a metadata chain without the
        // fLaC marker; the chain parser must accept it directly.
        use oxideav_core::{CodecId, CodecParameters, SampleFormat};
        let mut params = CodecParameters::audio(CodecId::new("flac"));
        params.channels = Some(1);
        params.sample_rate = Some(8_000);
        params.sample_format = Some(SampleFormat::S16);
        let enc = crate::encoder::make_encoder(&params).expect("encoder");
        let extradata = enc.output_params().extradata.clone();
        let (chain, consumed) = MetadataChain::parse(&extradata).expect("parse extradata");
        assert_eq!(consumed, extradata.len());
        let si = chain.stream_info().expect("streaminfo first");
        assert_eq!(si.channels, 1);
        assert_eq!(si.sample_rate, 8_000);
        assert_eq!(si.bits_per_sample, 16);
    }

    #[test]
    fn metadata_chain_reserved_block_payload_survives_round_trip() {
        // RFC 9639 §5: future versions may use reserved space; a chain
        // editor must carry unknown blocks through unchanged.
        let chain = MetadataChain {
            blocks: vec![
                MetadataBlock::StreamInfo(chain_streaminfo()),
                MetadataBlock::Reserved {
                    code: 126,
                    payload: (0u8..=255).collect(),
                },
            ],
        };
        let bytes = chain.write().expect("write");
        let (back, _) = MetadataChain::parse(&bytes).expect("parse");
        assert_eq!(back, chain);
        // The forbidden / out-of-range codes are unwritable.
        for bad_code in [127u8, 128, 200] {
            let bad = MetadataChain {
                blocks: vec![
                    MetadataBlock::StreamInfo(chain_streaminfo()),
                    MetadataBlock::Reserved {
                        code: bad_code,
                        payload: vec![],
                    },
                ],
            };
            assert!(bad.write().is_err(), "code {bad_code} must not serialise");
        }
    }
}
