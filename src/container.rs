//! FLAC native container: `fLaC` magic + metadata blocks + frame stream.
//!
//! The demuxer walks the metadata blocks to populate
//! [`CodecParameters`] from STREAMINFO, then emits frames as packets by
//! scanning for the FLAC frame sync pattern (0xFF, 0xF8/0xF9),
//! CRC-8-verifying each candidate header and confirming the end of each
//! frame by its CRC-16 (the `framing` module). The
//! full set of metadata blocks (including their headers) is preserved
//! verbatim in `extradata` so the muxer can round-trip byte-identical
//! output, and per-packet timestamps are computed directly from the
//! frame header.

use std::io::{Read, Seek, SeekFrom};

use oxideav_core::{
    AttachedPicture, Chapter, CodecId, CodecParameters, CodecResolver, Error, MediaType, Packet,
    PictureType, Result, StreamInfo, TimeBase, Timestamp,
};
use oxideav_core::{Demuxer, Muxer, ReadSeek, WriteSeek};

use crate::frame::{BlockingStrategy, parse_frame_header};
use crate::framing::FrameScanner;
use crate::metadata::{
    BlockHeader, BlockType, CueSheet, FLAC_MAGIC, MetadataBlock, MetadataChain, SeekPoint,
    StreamInfo as Si, parse_cuesheet, parse_picture, parse_seektable,
};

pub fn register(reg: &mut oxideav_core::ContainerRegistry) {
    reg.register_demuxer("flac", open_demuxer);
    reg.register_muxer("flac", open_muxer);
    reg.register_extension("flac", "flac");
    reg.register_extension("fla", "flac");
    reg.register_probe("flac", probe);
}

/// `fLaC` magic at offset 0, or after an ID3v2 tag at offset 0.
fn probe(p: &oxideav_core::ProbeData) -> u8 {
    if p.buf.len() >= 4 && &p.buf[0..4] == b"fLaC" {
        return 100;
    }
    if p.buf.len() >= 14 && &p.buf[0..3] == b"ID3" {
        let size = ((p.buf[6] as usize) << 21)
            | ((p.buf[7] as usize) << 14)
            | ((p.buf[8] as usize) << 7)
            | (p.buf[9] as usize);
        let off = 10 + size;
        if off + 4 <= p.buf.len() && &p.buf[off..off + 4] == b"fLaC" {
            return 100;
        }
    }
    0
}

// --- Demuxer ---------------------------------------------------------------

fn open_demuxer(
    mut input: Box<dyn ReadSeek>,
    _codecs: &dyn CodecResolver,
) -> Result<Box<dyn Demuxer>> {
    let mut metadata: Vec<(String, String)> = Vec::new();
    let mut pictures: Vec<AttachedPicture> = Vec::new();
    read_id3v2_if_present(&mut input, &mut metadata, &mut pictures)?;

    let mut magic = [0u8; 4];
    input.read_exact(&mut magic)?;
    if magic != FLAC_MAGIC {
        return Err(Error::invalid("not a FLAC stream (missing fLaC magic)"));
    }

    let mut extradata = Vec::new();
    let mut streaminfo: Option<Si> = None;
    let mut seek_points: Vec<SeekPoint> = Vec::new();
    let mut cuesheet: Option<CueSheet> = None;
    loop {
        let mut hdr = [0u8; 4];
        input.read_exact(&mut hdr)?;
        let parsed = BlockHeader::parse(&hdr)?;
        let payload = read_exact_vec(&mut input, parsed.length as usize)?;
        if streaminfo.is_none() && parsed.block_type == BlockType::StreamInfo {
            streaminfo = Some(Si::parse(&payload)?);
        }
        if parsed.block_type == BlockType::SeekTable {
            // Only the first SEEKTABLE is meaningful per spec.
            if seek_points.is_empty() {
                seek_points = parse_seektable(&payload);
            }
        }
        if parsed.block_type == BlockType::VorbisComment {
            parse_vorbis_comment(&payload, &mut metadata);
        }
        if parsed.block_type == BlockType::Picture
            && let Some(pic) = parse_flac_picture_block(&payload)
        {
            pictures.push(pic);
        }
        // RFC 9639 §8.7: only the first CUESHEET block is meaningful;
        // additional ones (some taggers append per-disc CUESHEETs in
        // multi-disc rips) are ignored at this layer. A malformed
        // CUESHEET is *not* treated as fatal — it just doesn't get
        // surfaced as chapters. Logging that decision in the demuxer
        // would be nicer, but we don't have a log surface here.
        if cuesheet.is_none()
            && parsed.block_type == BlockType::CueSheet
            && let Ok(cs) = parse_cuesheet(&payload)
        {
            cuesheet = Some(cs);
        }
        extradata.extend_from_slice(&hdr);
        extradata.extend_from_slice(&payload);
        if parsed.last {
            break;
        }
    }
    let info = streaminfo.ok_or_else(|| Error::invalid("FLAC stream missing STREAMINFO block"))?;

    // Byte position immediately after the final metadata block — this
    // is where the frame stream begins. The demuxer's seek_to adds the
    // SEEKPOINT's offset to this value to land on a frame boundary.
    let first_frame_offset = input.stream_position()?;

    // Each depth of RFC 9639 §8.2 (4..=32 bits) is delivered in the
    // narrowest whole-byte format that holds it, left-justified; the
    // decoder applies the same mapping.
    let sample_format =
        crate::decoder::output_format_for(info.bits_per_sample as u32).ok_or_else(|| {
            Error::unsupported(format!(
                "unsupported FLAC bit depth {}",
                info.bits_per_sample
            ))
        })?;

    let mut params = CodecParameters::audio(CodecId::new(crate::CODEC_ID_STR));
    params.media_type = MediaType::Audio;
    params.channels = Some(info.channels as u16);
    params.sample_rate = Some(info.sample_rate);
    params.sample_format = Some(sample_format);
    params.extradata = extradata;

    let time_base = TimeBase::new(1, info.sample_rate as i64);
    let total = if info.total_samples == 0 {
        None
    } else {
        Some(info.total_samples as i64)
    };
    let stream = StreamInfo {
        index: 0,
        time_base,
        duration: total,
        start_time: Some(0),
        params,
    };

    let duration_micros: i64 = if info.sample_rate > 0 && info.total_samples > 0 {
        (info.total_samples as i128 * 1_000_000 / info.sample_rate as i128) as i64
    } else {
        0
    };

    // RFC 9639 §8.7 — convert CUESHEET tracks to standard
    // `Chapter` records. The lead-out track is dropped from the
    // chapter list because it carries no audio (it just marks the end
    // of the stream); its sample offset becomes the implicit end of
    // the final real chapter. For tracks that have at least one INDEX
    // point we use the track's stream-level offset as the chapter
    // `start`; the chapter `end` is taken from the next track's
    // `start` so consumers can compute chapter durations directly.
    let chapters = build_chapters_from_cuesheet(cuesheet.as_ref(), time_base);

    Ok(Box::new(FlacDemuxer {
        input,
        streams: vec![stream],
        scan: FrameScanner::new(info.bits_per_sample as u32),
        block_size: info.min_block_size as u32,
        scan_bits_per_sample: info.bits_per_sample as u32,
        eof: false,
        metadata,
        pictures,
        duration_micros,
        seek_points,
        first_frame_offset,
        total_samples: info.total_samples,
        chapters,
        cuesheet,
    }))
}

/// Parse a FLAC `METADATA_BLOCK_PICTURE` payload (block type 6) and
/// project it onto the framework's `AttachedPicture` surface (which
/// carries the four fields every container shares — mime, picture
/// type, description, raw data — but not the four FLAC-specific
/// informational fields).
///
/// The wire-format parsing is delegated to
/// [`crate::metadata::parse_picture`], which captures the full RFC
/// 9639 §8.8 layout including width / height / depth / colour_count.
/// The pre-round-163 inline parser had two surface bugs the typed
/// parser fixes: it silently dropped 16 bytes of pixel-metadata, and
/// it used `from_utf8(...).unwrap_or("")` on the mime / description
/// fields so an attacker-controlled payload with non-UTF-8 bytes
/// would round-trip through an empty string. The typed parser rejects
/// both cases instead (mime: printable-ASCII enforcement per spec;
/// description: strict UTF-8 per spec).
///
/// Returns `None` on a malformed PICTURE block — matching the
/// pre-round-163 demuxer's non-fatal behaviour (a bad PICTURE block
/// is dropped rather than failing the whole demux). A future
/// container revision could surface the underlying `Error` instead.
fn parse_flac_picture_block(buf: &[u8]) -> Option<AttachedPicture> {
    let pic = parse_picture(buf).ok()?;
    Some(AttachedPicture {
        mime_type: pic.mime_type,
        picture_type: PictureType::from_u8((pic.picture_type & 0xFF) as u8),
        description: pic.description,
        data: pic.data,
    })
}

/// Parse a Vorbis-comment block payload (FLAC block type 4 shares the
/// Vorbis comment wire format). Appends normalised (lowercase key, value)
/// pairs to `out`.
fn parse_vorbis_comment(buf: &[u8], out: &mut Vec<(String, String)>) {
    let mut i = 0usize;
    // Vendor string
    if buf.len() < 4 {
        return;
    }
    let vlen = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    i += 4;
    if i + vlen > buf.len() {
        return;
    }
    let vendor = String::from_utf8_lossy(&buf[i..i + vlen]).to_string();
    i += vlen;
    if !vendor.is_empty() {
        out.push(("vendor".into(), vendor));
    }
    // Comment count + KEY=VALUE entries
    if i + 4 > buf.len() {
        return;
    }
    let n = u32::from_le_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]) as usize;
    i += 4;
    for _ in 0..n {
        if i + 4 > buf.len() {
            break;
        }
        let clen = u32::from_le_bytes([buf[i], buf[i + 1], buf[i + 2], buf[i + 3]]) as usize;
        i += 4;
        if i + clen > buf.len() {
            break;
        }
        let entry = &buf[i..i + clen];
        i += clen;
        if let Some(eq) = entry.iter().position(|&b| b == b'=') {
            let key = String::from_utf8_lossy(&entry[..eq])
                .to_ascii_lowercase()
                .trim()
                .to_string();
            let value = String::from_utf8_lossy(&entry[eq + 1..]).trim().to_string();
            if !key.is_empty() && !value.is_empty() {
                out.push((key, value));
            }
        }
    }
}

/// If the file begins with an ID3v2 tag, parse it and advance past
/// it. Many FLAC files in the wild have one even though the FLAC spec
/// does not require support. The parsed metadata/pictures are merged
/// into the caller's accumulators; FLAC's own Vorbis-comment and
/// PICTURE blocks later take precedence for duplicate keys.
fn read_id3v2_if_present(
    input: &mut Box<dyn ReadSeek>,
    metadata: &mut Vec<(String, String)>,
    pictures: &mut Vec<AttachedPicture>,
) -> Result<()> {
    let mut head = [0u8; 10];
    let n = read_up_to(input, &mut head)?;
    if n < 10 || &head[0..3] != b"ID3" {
        // Rewind whatever we read and return — no tag.
        input.seek(std::io::SeekFrom::Current(-(n as i64)))?;
        return Ok(());
    }
    let flags = head[5];
    let size = ((head[6] as u32) << 21)
        | ((head[7] as u32) << 14)
        | ((head[8] as u32) << 7)
        | (head[9] as u32);
    let footer = if flags & 0x10 != 0 { 10 } else { 0 };
    let Ok(body) = read_exact_vec(input, size as usize) else {
        if footer > 0 {
            input.seek(std::io::SeekFrom::Current(footer as i64))?;
        }
        return Ok(());
    };
    let mut full = Vec::with_capacity(10 + body.len());
    full.extend_from_slice(&head);
    full.extend_from_slice(&body);
    if let Ok((tag, _)) = oxideav_id3::parse_tag(&full) {
        for (k, v) in oxideav_id3::to_key_value_pairs(&tag) {
            metadata.push((k, v));
        }
        pictures.extend(oxideav_id3::attached_pictures(&tag));
    }
    if footer > 0 {
        input.seek(std::io::SeekFrom::Current(footer as i64))?;
    }
    Ok(())
}

/// Read exactly `len` bytes into a new vector. The vector grows with the
/// bytes that actually arrive, so a length field that promises more than
/// the input holds (a metadata block may claim 16 MiB, an ID3v2 tag
/// 256 MiB) costs no more memory than the input is long.
fn read_exact_vec(input: &mut Box<dyn ReadSeek>, len: usize) -> Result<Vec<u8>> {
    let mut out = Vec::with_capacity(len.min(64 * 1024));
    let got = input.by_ref().take(len as u64).read_to_end(&mut out)?;
    if got < len {
        return Err(Error::Io(std::io::Error::new(
            std::io::ErrorKind::UnexpectedEof,
            "FLAC: input ends inside a length-prefixed block",
        )));
    }
    Ok(out)
}

fn read_up_to(input: &mut Box<dyn ReadSeek>, buf: &mut [u8]) -> Result<usize> {
    let mut got = 0;
    while got < buf.len() {
        match input.read(&mut buf[got..]) {
            Ok(0) => break,
            Ok(n) => got += n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(got)
}

struct FlacDemuxer {
    input: Box<dyn ReadSeek>,
    streams: Vec<StreamInfo>,
    scan: FrameScanner,
    /// STREAMINFO minimum block size: the block size of a fixed block
    /// size stream, which turns a frame number into a sample number.
    block_size: u32,
    /// STREAMINFO bit depth, for scanners created while seeking.
    scan_bits_per_sample: u32,
    eof: bool,
    metadata: Vec<(String, String)>,
    pictures: Vec<AttachedPicture>,
    duration_micros: i64,
    /// Parsed SEEKTABLE entries (placeholders filtered out). Empty if
    /// the file carried no SEEKTABLE block.
    seek_points: Vec<SeekPoint>,
    /// Byte offset of the first frame in the stream (immediately
    /// after the metadata-block sequence).
    first_frame_offset: u64,
    /// Total interchannel samples from STREAMINFO (0 if the field was
    /// left unset, i.e. "unknown"). Used as an upper bound for the
    /// seektable-less scanning seek's RFC 9639 §11 plausibility check.
    total_samples: u64,
    /// Chapter list derived from the CUESHEET block (when present).
    /// Each entry corresponds to one CUESHEET track except the trailing
    /// lead-out (whose offset is folded into the previous chapter's
    /// `end`).
    chapters: Vec<Chapter>,
    /// Raw parsed CUESHEET (when present). Kept on the demuxer so
    /// future callers can read CD-DA-specific bits (lead-in samples,
    /// per-track ISRC, INDEX 00 pre-gap points) that the generic
    /// `Chapter` representation flattens away.
    #[allow(dead_code)] // exposed via accessor below for callers that want CD-DA fidelity
    cuesheet: Option<CueSheet>,
}

/// Convert a parsed CUESHEET into a [`Chapter`] list keyed on
/// `time_base` (which is `1/sample_rate` for FLAC streams). The
/// transform is:
///
/// * Each non-lead-out track becomes one chapter.
/// * `start` = where the track begins. RFC 9639 §8.7.1 notes that the
///   FLAC track offset is that of the track's *first* index point,
///   unlike a CD table of contents, which lists index point 1 even when
///   there is an index point 0 (the pre-gap, §8.7.1.1). A track that
///   opens with index point 0 therefore starts at its index point 1.
/// * `end`   = the start of the next chapter, so the chapters are
///   contiguous and a pre-gap plays out at the end of the chapter before
///   it, as it does on a disc; the lead-out offset ends the last one.
/// * `id`    = the CUESHEET track number (1..=99 for CD-DA audio).
/// * `title` = `None`; the CUESHEET block doesn't carry track titles.
///   A Vorbis-comment `TRACK01=…` style tag is the usual companion
///   source, and we leave it to the caller to cross-reference if
///   desired.
fn build_chapters_from_cuesheet(cs: Option<&CueSheet>, time_base: TimeBase) -> Vec<Chapter> {
    let Some(cs) = cs else {
        return Vec::new();
    };
    let Some((lead_out, tracks)) = cs.tracks.split_last() else {
        return Vec::new();
    };
    let limit = lead_out.offset_samples;
    let start_of = |track: &crate::metadata::CueSheetTrack| {
        let past_pre_gap = match track.indices.as_slice() {
            [first, second, ..] if first.number == 0 && second.number == 1 => second.offset_samples,
            _ => 0,
        };
        track.offset_samples.saturating_add(past_pre_gap).min(limit)
    };
    let mut out = Vec::with_capacity(tracks.len());
    for (i, track) in tracks.iter().enumerate() {
        let start = start_of(track);
        let end = tracks.get(i + 1).map_or(limit, start_of).max(start);
        out.push(Chapter {
            id: track.number as u64,
            start: Timestamp::new(start as i64, time_base),
            end: Timestamp::new(end as i64, time_base),
            title: None,
            language: None,
        });
    }
    out
}

/// A frame found in the input while seeking.
#[derive(Clone, Copy, Debug)]
struct Located {
    /// Number of the frame's first sample.
    first: u64,
    /// Samples in the frame.
    samples: u64,
    /// Absolute byte position of the frame's sync code.
    offset: u64,
    /// The frame ends in a CRC-16 that matches its bytes.
    verified: bool,
}

/// Below this many bytes between the bounds, a seek stops halving the
/// range and reads forward frame by frame.
const SEEK_SCAN_WINDOW: u64 = 64 * 1024;

impl FlacDemuxer {
    /// Cut frames from absolute position `start` onwards and hand each
    /// to `visit` until it returns a value, the input ends, or more than
    /// `limit` bytes have been read.
    fn walk<T>(
        &mut self,
        start: u64,
        limit: u64,
        mut visit: impl FnMut(Located) -> Option<T>,
    ) -> Result<Option<T>> {
        let block_size = self.block_size;
        self.input.seek(SeekFrom::Start(start))?;
        // The same framing the packet reader uses, so a header-shaped run
        // inside a frame's audio is not mistaken for a frame.
        let mut scan = FrameScanner::new(self.scan_bits_per_sample);
        let mut eof = false;
        let mut read = 0u64;
        loop {
            while let Some(span) = scan.next_frame(eof) {
                let located = Located {
                    first: span.header.first_sample(block_size),
                    samples: span.header.block_size as u64,
                    offset: start + scan.offset_of(span.start),
                    verified: span.verified,
                };
                scan.compact();
                if let Some(found) = visit(located) {
                    return Ok(Some(found));
                }
            }
            if eof || read > limit {
                return Ok(None);
            }
            let mut chunk = [0u8; 8192];
            let n = read_up_to(&mut self.input, &mut chunk)?;
            if n == 0 {
                eof = true;
            } else {
                read += n as u64;
                scan.push(&chunk[..n]);
            }
        }
    }

    /// Read frames forward from absolute position `start` and return the
    /// one that holds sample `target`: the last frame whose first sample
    /// is `<= target`. `None` when the first frame met already starts
    /// after the target, or there is no frame at all.
    ///
    /// RFC 9639 §11 plausibility checks: frames are cut by
    /// [`FrameScanner`] (CRC-8-verified headers, CRC-16-confirmed ends);
    /// a frame's first sample must be strictly greater than the
    /// previously accepted frame's; and no first sample may exceed the
    /// STREAMINFO total-sample count when that count is known. A frame
    /// failing a check is skipped. The scan cannot loop: the scanner
    /// never revisits a byte and stops at the end of the input.
    fn scan_forward(&mut self, start: u64, target: u64) -> Result<Option<Located>> {
        let bound = self.total_samples; // 0 == unknown, no upper clamp
        let mut best: Option<Located> = None;
        let overshot = self.walk(start, u64::MAX, |frame| {
            let monotonic = best.is_none_or(|prev| frame.first > prev.first);
            if !(monotonic && (bound == 0 || frame.first < bound)) {
                // Implausible coded number: not a frame of this stream.
                return None;
            }
            if frame.first > target {
                // First frame past the target: the best so far is it.
                return Some(());
            }
            best = Some(frame);
            // The frame holds the target: no need to look at the next.
            (target - frame.first < frame.samples).then_some(())
        })?;
        let _ = overshot;
        Ok(best)
    }

    /// Find the frame that holds sample `target` and return the number
    /// of its first sample and its absolute byte position.
    ///
    /// The search narrows a byte range `[low, high)` known to contain
    /// the frame's start. It begins as the whole frame stream; the
    /// SEEKTABLE (RFC 9639 §8.5), when there is one, tightens it to the
    /// two points around the target; then the range is halved by reading
    /// one frame from its middle, until it is small enough to read
    /// through. So a seek reads a few frames per halving instead of the
    /// stream up to the target, with or without a SEEKTABLE.
    ///
    /// Nothing in the file is trusted on the way. A seek point is used
    /// only if a frame with its sample number really starts at its
    /// offset; a frame read from the middle counts only with a matching
    /// CRC-16 and a sample number inside the stream. Should the narrowed
    /// range still not hold the target (possible only in a stream whose
    /// sample numbers do not increase), the frames are read from the
    /// start, which gives the same answer as a file without any of this.
    fn locate(&mut self, target: u64) -> Result<(u64, u64)> {
        let stream_start = self.first_frame_offset;
        let end = self.input.seek(SeekFrom::End(0))?;
        let mut low = stream_start;
        let mut high = end;

        // Seek points on either side of the target.
        let after = self
            .seek_points
            .partition_point(|sp| sp.sample_number <= target);
        if after > 0 {
            let point = self.seek_points[after - 1];
            if let Some(at) = stream_start
                .checked_add(point.offset)
                .filter(|&at| at < end)
            {
                let genuine = self.walk(at, 0, |frame| {
                    Some(frame.offset == at && frame.first == point.sample_number)
                })?;
                if genuine == Some(true) {
                    low = at;
                    let next = self.seek_points.get(after).and_then(|next| {
                        stream_start
                            .checked_add(next.offset)
                            .filter(|&at| at > low && at < end)
                    });
                    if let Some(next) = next {
                        high = next;
                    }
                }
            }
        }

        let bound = self.total_samples;
        while high - low > SEEK_SCAN_WINDOW {
            let middle = low + (high - low) / 2;
            let frame = self.walk(middle, high - middle, |frame| {
                (frame.verified && (bound == 0 || frame.first < bound)).then_some(frame)
            })?;
            match frame {
                Some(frame) if frame.offset < high && frame.first <= target => low = frame.offset,
                _ => high = middle,
            }
        }

        let mut found = self.scan_forward(low, target)?;
        if found.is_none() && low != stream_start {
            found = self.scan_forward(stream_start, target)?;
        }
        Ok(found.map_or((0, stream_start), |frame| (frame.first, frame.offset)))
    }
}

impl Demuxer for FlacDemuxer {
    fn format_name(&self) -> &str {
        "flac"
    }

    fn streams(&self) -> &[StreamInfo] {
        &self.streams
    }

    fn next_packet(&mut self) -> Result<Packet> {
        loop {
            if let Some(span) = self.scan.next_frame(self.eof) {
                let pts = span.header.first_sample(self.block_size) as i64;
                let duration = span.header.block_size as i64;
                let data = self.scan.bytes(&span).to_vec();
                self.scan.compact();
                let stream = &self.streams[0];
                let mut pkt = Packet::new(0, stream.time_base, data);
                pkt.pts = Some(pts);
                pkt.dts = Some(pts);
                pkt.duration = Some(duration);
                pkt.flags.keyframe = true;
                return Ok(pkt);
            }
            if self.eof {
                return Err(Error::Eof);
            }

            let mut chunk = [0u8; 8192];
            let n = read_up_to(&mut self.input, &mut chunk)?;
            if n == 0 {
                self.eof = true;
            } else {
                self.scan.push(&chunk[..n]);
            }
        }
    }

    fn seek_to(&mut self, stream_index: u32, pts: i64) -> Result<i64> {
        if stream_index != 0 {
            return Err(Error::invalid(format!(
                "FLAC: stream index {stream_index} out of range (only stream 0 exists)"
            )));
        }
        let target = pts.max(0) as u64;

        // The frame that holds the target; a target past the end lands
        // on the last frame.
        let (anchor_sample, position) = self.locate(target)?;

        self.input.seek(SeekFrom::Start(position))?;
        self.scan.reset();
        self.eof = false;
        Ok(anchor_sample as i64)
    }

    fn metadata(&self) -> &[(String, String)] {
        &self.metadata
    }

    fn attached_pictures(&self) -> &[AttachedPicture] {
        &self.pictures
    }

    fn duration_micros(&self) -> Option<i64> {
        if self.duration_micros > 0 {
            Some(self.duration_micros)
        } else {
            None
        }
    }

    fn chapters(&self) -> &[Chapter] {
        &self.chapters
    }
}

// --- Muxer -----------------------------------------------------------------

/// Configuration for the FLAC muxer's SEEKTABLE generation (RFC 9639
/// §8.5).
///
/// The registry-path muxer (`open_muxer`) uses [`FlacMuxerOptions::default`];
/// callers wanting different behaviour construct the muxer through
/// [`open_muxer_with_options`].
///
/// # SEEKTABLE generation
///
/// A FLAC frame stream is self-describing enough to seek without a
/// table — the demuxer can scan CRC-8-verified frame headers forward —
/// but "the delay can be unpredictable since the bitrate may vary
/// widely within a stream" (RFC 9639 §8.5). A SEEKTABLE turns that scan
/// into a binary search. The muxer is the only layer that knows each
/// frame's byte offset as it lands, so it is the natural place to build
/// one.
///
/// To build a table the muxer buffers the written frames in memory
/// until [`Muxer::write_trailer`], records `(first_sample,
/// offset_from_first_frame, frame_samples)` for every frame from the
/// frame header itself (not from the packet timestamp, which a
/// hand-rolled packet may omit), then inserts a single SEEKTABLE block
/// into the metadata chain and writes the whole file. The seek-point
/// `offset` is "from the first byte of the first frame header to the
/// first byte of the target frame's header" (RFC 9639 §8.5.1) — exactly
/// the offset the demuxer adds to its first-frame position — so
/// inserting the SEEKTABLE block ahead of the frames does **not**
/// invalidate the offsets, which are relative to the first frame, not
/// the file start.
#[derive(Clone, Debug)]
#[non_exhaustive]
pub struct FlacMuxerOptions {
    /// Generate a SEEKTABLE block at [`Muxer::write_trailer`] time.
    ///
    /// When `false` the muxer streams frames straight through with no
    /// buffering and emits the metadata chain verbatim — the historical
    /// behaviour, byte-for-byte. When `true` (the default) and the
    /// metadata chain does not already carry a SEEKTABLE, the muxer
    /// builds one at the configured density. An existing SEEKTABLE in
    /// the extradata is always preserved untouched (RFC 9639 §8.5: "There
    /// MUST NOT be more than one seek table metadata block in a stream").
    pub generate_seektable: bool,

    /// Target spacing between generated seek points, in interchannel
    /// samples. The muxer places a seek point on the first frame whose
    /// first sample is at or past the next multiple of this value, so
    /// the realised spacing is rounded up to a frame boundary. `0` is
    /// treated as "one seek point per frame".
    ///
    /// [`FlacMuxerOptions::default`] derives this from the stream sample
    /// rate so the table lands roughly one point per second of audio
    /// (see [`FlacMuxerOptions::for_sample_rate`]); for a 44.1 kHz file
    /// that is a point every 44 100 samples. The §8.5 worked example
    /// ("a seek table with 1% resolution within a stream adds less than
    /// 2 kilobytes of data") shows the density/size trade-off — finer
    /// spacing means more 18-byte points.
    pub seek_point_interval: u64,

    /// Upper bound on the number of real seek points generated, to keep
    /// the table size bounded for very long streams. `0` means
    /// unbounded. When the bound would be exceeded the effective
    /// interval is widened so the points spread evenly across the whole
    /// stream rather than clustering at the start.
    pub max_seek_points: usize,
}

impl FlacMuxerOptions {
    /// Builder-style setter for [`generate_seektable`](Self::generate_seektable).
    pub fn with_generate_seektable(mut self, generate_seektable: bool) -> Self {
        self.generate_seektable = generate_seektable;
        self
    }
    /// Builder-style setter for [`seek_point_interval`](Self::seek_point_interval).
    pub fn with_seek_point_interval(mut self, seek_point_interval: u64) -> Self {
        self.seek_point_interval = seek_point_interval;
        self
    }
    /// Builder-style setter for [`max_seek_points`](Self::max_seek_points).
    pub fn with_max_seek_points(mut self, max_seek_points: usize) -> Self {
        self.max_seek_points = max_seek_points;
        self
    }
}

impl Default for FlacMuxerOptions {
    fn default() -> Self {
        // Sample-rate-agnostic fallback used until the muxer reads the
        // STREAMINFO sample rate from the extradata; `for_sample_rate`
        // refines it. 44 100 ≈ one point per second of CD-DA audio.
        Self::for_sample_rate(44_100)
    }
}

impl FlacMuxerOptions {
    /// Default options tuned for a given sample rate: a seek point
    /// roughly every second of audio, capped at 2048 points so even a
    /// multi-hour stream keeps the table under ~36 KiB.
    pub fn for_sample_rate(sample_rate: u32) -> Self {
        let interval = if sample_rate == 0 {
            44_100
        } else {
            sample_rate as u64
        };
        Self {
            generate_seektable: true,
            seek_point_interval: interval,
            max_seek_points: 2048,
        }
    }

    /// Options that disable SEEKTABLE generation entirely: the muxer
    /// streams frames through with no buffering and writes the metadata
    /// chain verbatim (the pre-SEEKTABLE-generation behaviour).
    pub fn no_seektable() -> Self {
        Self {
            generate_seektable: false,
            seek_point_interval: 0,
            max_seek_points: 0,
        }
    }
}

fn validate_mux_stream(streams: &[StreamInfo]) -> Result<&StreamInfo> {
    if streams.len() != 1 {
        return Err(Error::unsupported(
            "FLAC native container holds exactly one stream",
        ));
    }
    let s = &streams[0];
    if s.params.codec_id.as_str() != crate::CODEC_ID_STR {
        return Err(Error::invalid(format!(
            "FLAC muxer requires codec_id=flac (got {})",
            s.params.codec_id
        )));
    }
    if s.params.extradata.is_empty() {
        return Err(Error::invalid(
            "FLAC muxer needs extradata containing metadata blocks",
        ));
    }
    Ok(s)
}

/// Open the registry-path FLAC muxer with [`FlacMuxerOptions::default`]
/// (SEEKTABLE generation enabled, ~1 point per second of audio).
fn open_muxer(output: Box<dyn WriteSeek>, streams: &[StreamInfo]) -> Result<Box<dyn Muxer>> {
    let s = validate_mux_stream(streams)?;
    // Refine the default density from the STREAMINFO sample rate when we
    // can read it; fall back to the rate-agnostic default otherwise.
    let opts = match MetadataChain::parse(&s.params.extradata) {
        Ok((chain, _)) => chain
            .stream_info()
            .map(|si| FlacMuxerOptions::for_sample_rate(si.sample_rate))
            .unwrap_or_default(),
        Err(_) => FlacMuxerOptions::default(),
    };
    Ok(Box::new(FlacMuxer::new(
        output,
        s.params.extradata.clone(),
        opts,
    )))
}

/// Open a FLAC muxer with caller-chosen [`FlacMuxerOptions`] — the
/// SEEKTABLE-generation control point (density, point cap, on/off).
///
/// `streams` must hold exactly one `flac` stream carrying a metadata
/// chain in `params.extradata` (the encoder produces exactly this).
pub fn open_muxer_with_options(
    output: Box<dyn WriteSeek>,
    streams: &[StreamInfo],
    options: FlacMuxerOptions,
) -> Result<Box<dyn Muxer>> {
    let s = validate_mux_stream(streams)?;
    Ok(Box::new(FlacMuxer::new(
        output,
        s.params.extradata.clone(),
        options,
    )))
}

struct FlacMuxer {
    output: Box<dyn WriteSeek>,
    extradata: Vec<u8>,
    options: FlacMuxerOptions,
    header_written: bool,
    trailer_written: bool,
    /// STREAMINFO block size, used to map a fixed-blocking frame's coded
    /// frame number to its first sample. Filled from the extradata at
    /// `write_header`.
    streaminfo_block_size: u32,
    /// In SEEKTABLE-generation mode, the frame packets buffered until
    /// `write_trailer`. Empty (and unused) when generation is disabled,
    /// in which case frames stream straight to `output`.
    buffered_frames: Vec<Vec<u8>>,
    /// Recorded per-frame seek anchors `(first_sample, offset, samples)`,
    /// where `offset` is bytes from the first frame header. Built
    /// alongside `buffered_frames`.
    frame_anchors: Vec<SeekPoint>,
    /// Running byte offset of the next frame from the first frame header.
    next_offset: u64,
}

impl FlacMuxer {
    fn new(output: Box<dyn WriteSeek>, extradata: Vec<u8>, options: FlacMuxerOptions) -> Self {
        Self {
            output,
            extradata,
            options,
            header_written: false,
            trailer_written: false,
            streaminfo_block_size: 0,
            buffered_frames: Vec::new(),
            frame_anchors: Vec::new(),
            next_offset: 0,
        }
    }

    /// Whether this muxer is buffering frames to emit a SEEKTABLE.
    fn buffering(&self) -> bool {
        self.options.generate_seektable
    }

    /// Record a frame's seek anchor from its header, advancing the
    /// running byte offset. A frame whose header does not parse (e.g. a
    /// caller passing non-frame bytes) is still counted for the byte
    /// offset but contributes no anchor, so a malformed packet can never
    /// poison the table.
    fn record_anchor(&mut self, frame: &[u8]) {
        let offset = self.next_offset;
        self.next_offset = self.next_offset.saturating_add(frame.len() as u64);
        if let Ok(h) = parse_frame_header(frame) {
            let first_sample = match h.blocking_strategy {
                BlockingStrategy::Fixed => h
                    .coded_number
                    .saturating_mul(self.streaminfo_block_size as u64),
                BlockingStrategy::Variable => h.coded_number,
            };
            // SEEKPOINT frame_samples is a u16; clamp a pathological
            // block size into range rather than overflow.
            let frame_samples = u16::try_from(h.block_size).unwrap_or(u16::MAX);
            self.frame_anchors.push(SeekPoint {
                sample_number: first_sample,
                offset,
                frame_samples,
            });
        }
    }

    /// Select the seek points to emit from the recorded per-frame
    /// anchors, honouring the configured interval and point cap. The
    /// first frame always gets a point (so a seek to sample 0 lands at
    /// offset 0), then a new point is taken on the first frame at or
    /// past each successive interval boundary. Anchors are already
    /// strictly ascending by sample number (frames are written in
    /// order), so the result satisfies the §8.5.1 sorted+unique MUSTs.
    fn select_seek_points(&self) -> Vec<SeekPoint> {
        if self.frame_anchors.is_empty() {
            return Vec::new();
        }
        // Effective interval: widen it if the naive interval would blow
        // past the point cap, so points spread across the whole stream.
        let last_sample = self
            .frame_anchors
            .last()
            .map(|p| p.sample_number)
            .unwrap_or(0);
        let mut interval = self.options.seek_point_interval;
        if interval == 0 {
            interval = 1; // one point per frame
        }
        if self.options.max_seek_points > 1 {
            let span = last_sample.max(1);
            let min_interval = span / (self.options.max_seek_points as u64 - 1).max(1);
            if min_interval > interval {
                interval = min_interval.max(1);
            }
        }

        let mut out: Vec<SeekPoint> = Vec::new();
        let mut next_boundary = 0u64;
        for anchor in &self.frame_anchors {
            if anchor.sample_number >= next_boundary {
                out.push(*anchor);
                // Advance the boundary to the next multiple strictly past
                // this frame's first sample so each interval yields one
                // point at most.
                let steps = anchor.sample_number / interval + 1;
                next_boundary = steps.saturating_mul(interval);
            }
            if self.options.max_seek_points != 0 && out.len() >= self.options.max_seek_points {
                break;
            }
        }
        out
    }

    /// Build the final metadata chain: parse the extradata, and if
    /// SEEKTABLE generation is on and the chain has none, insert a
    /// freshly-built SEEKTABLE right after STREAMINFO. Returns the
    /// serialised `fLaC` + chain prefix.
    fn build_header_with_seektable(&self) -> Result<Vec<u8>> {
        let (mut chain, _) = MetadataChain::parse(&self.extradata)?;
        let has_seektable = chain
            .blocks
            .iter()
            .any(|b| matches!(b, MetadataBlock::SeekTable { .. }));
        if self.options.generate_seektable && !has_seektable {
            let points = self.select_seek_points();
            if !points.is_empty() {
                // Insert directly after STREAMINFO (block 0). §8.5 places
                // no ordering requirement on the SEEKTABLE beyond the
                // STREAMINFO-first rule, but right after STREAMINFO keeps
                // it ahead of the bulky VORBIS_COMMENT / PICTURE blocks so
                // a seeking reader meets it early.
                let insert_at = 1.min(chain.blocks.len());
                chain.blocks.insert(
                    insert_at,
                    MetadataBlock::SeekTable {
                        points,
                        placeholder_count: 0,
                    },
                );
            }
        }
        let mut out = Vec::with_capacity(4 + self.extradata.len() + 64);
        out.extend_from_slice(&FLAC_MAGIC);
        out.extend_from_slice(&chain.write()?);
        Ok(out)
    }
}

impl Muxer for FlacMuxer {
    fn format_name(&self) -> &str {
        "flac"
    }

    fn write_header(&mut self) -> Result<()> {
        if self.header_written {
            return Err(Error::other("FLAC muxer: write_header called twice"));
        }
        // Read the STREAMINFO block size up front so a fixed-blocking
        // frame's coded number maps to its first sample. A parse failure
        // is non-fatal here (block size stays 0); the streaming path
        // doesn't need it and the buffering path tolerates a missing
        // mapping by simply not emitting unmappable anchors.
        // Match the demuxer's `first_sample` mapping, which keys on
        // STREAMINFO min_block_size. For a fixed-blocking stream the
        // encoder writes min == max == the nominal block size, so a
        // fixed frame's `coded_number * block_size` is its first sample.
        if let Ok((chain, _)) = MetadataChain::parse(&self.extradata)
            && let Some(si) = chain.stream_info()
        {
            self.streaminfo_block_size = si.min_block_size as u32;
        }
        if self.buffering() {
            // Defer all output to write_trailer, where the SEEKTABLE is
            // built and inserted ahead of the frames.
            self.header_written = true;
            return Ok(());
        }
        use std::io::Write;
        self.output.write_all(&FLAC_MAGIC)?;
        self.output.write_all(&self.extradata)?;
        self.header_written = true;
        Ok(())
    }

    fn write_packet(&mut self, packet: &Packet) -> Result<()> {
        if !self.header_written {
            return Err(Error::other("FLAC muxer: write_header not called"));
        }
        if self.buffering() {
            self.record_anchor(&packet.data);
            self.buffered_frames.push(packet.data.clone());
            return Ok(());
        }
        use std::io::Write;
        self.output.write_all(&packet.data)?;
        Ok(())
    }

    fn write_trailer(&mut self) -> Result<()> {
        if self.trailer_written {
            return Ok(());
        }
        use std::io::Write;
        if self.buffering() {
            let header = self.build_header_with_seektable()?;
            self.output.write_all(&header)?;
            for frame in &self.buffered_frames {
                self.output.write_all(frame)?;
            }
        }
        self.output.flush()?;
        self.trailer_written = true;
        Ok(())
    }
}

#[cfg(test)]
mod muxer_tests {
    use super::*;

    fn anchor(sample: u64, offset: u64, samples: u16) -> SeekPoint {
        SeekPoint {
            sample_number: sample,
            offset,
            frame_samples: samples,
        }
    }

    /// Build a `FlacMuxer` (no real output) with a hand-set anchor list
    /// so the point-selection logic can be unit-tested in isolation.
    fn muxer_with_anchors(opts: FlacMuxerOptions, anchors: Vec<SeekPoint>) -> FlacMuxer {
        let sink: Box<dyn WriteSeek> = Box::new(std::io::Cursor::new(Vec::new()));
        let mut m = FlacMuxer::new(sink, Vec::new(), opts);
        m.frame_anchors = anchors;
        m
    }

    #[test]
    fn select_points_honours_interval_rounding_up_to_frame() {
        // Frames every 100 samples; interval 250. Each chosen frame
        // resets the boundary to the next multiple strictly past its
        // first sample: pick 0 → boundary 250; pick 300 → boundary 500;
        // pick 500 → boundary 750; pick 800 → boundary 1000.
        let anchors: Vec<SeekPoint> = (0..10).map(|i| anchor(i * 100, i * 40, 100)).collect();
        let opts = FlacMuxerOptions {
            generate_seektable: true,
            seek_point_interval: 250,
            max_seek_points: 0,
        };
        let m = muxer_with_anchors(opts, anchors);
        let pts = m.select_seek_points();
        let samples: Vec<u64> = pts.iter().map(|p| p.sample_number).collect();
        assert_eq!(samples, vec![0, 300, 500, 800]);
        // Offsets must track the chosen anchors, not be recomputed.
        assert_eq!(pts[1].offset, 300 / 100 * 40);
    }

    #[test]
    fn select_points_zero_interval_is_one_per_frame() {
        let anchors: Vec<SeekPoint> = (0..4).map(|i| anchor(i * 192, i * 50, 192)).collect();
        let opts = FlacMuxerOptions {
            generate_seektable: true,
            seek_point_interval: 0,
            max_seek_points: 0,
        };
        let m = muxer_with_anchors(opts, anchors);
        assert_eq!(m.select_seek_points().len(), 4);
    }

    #[test]
    fn select_points_caps_count_by_widening_interval() {
        // 1000 frames, naive interval 1 (one per frame) but capped at 8.
        let anchors: Vec<SeekPoint> = (0..1000).map(|i| anchor(i * 100, i * 10, 100)).collect();
        let opts = FlacMuxerOptions {
            generate_seektable: true,
            seek_point_interval: 1,
            max_seek_points: 8,
        };
        let m = muxer_with_anchors(opts, anchors);
        let pts = m.select_seek_points();
        assert!(pts.len() <= 8, "point cap exceeded: {}", pts.len());
        // Strictly ascending + unique (the §8.5.1 MUSTs).
        for w in pts.windows(2) {
            assert!(w[0].sample_number < w[1].sample_number);
        }
        // Points spread across the stream rather than clustering at the
        // start: the last chosen point is well past the midpoint.
        assert!(pts.last().unwrap().sample_number >= 100 * 1000 / 2);
    }

    #[test]
    fn select_points_empty_anchor_list_yields_empty() {
        let m = muxer_with_anchors(FlacMuxerOptions::default(), Vec::new());
        assert!(m.select_seek_points().is_empty());
    }

    #[test]
    fn select_points_first_frame_always_present() {
        // A huge interval still yields the first frame so a seek to
        // sample 0 lands at offset 0.
        let anchors = vec![anchor(0, 0, 4096), anchor(4096, 500, 4096)];
        let opts = FlacMuxerOptions {
            generate_seektable: true,
            seek_point_interval: 1_000_000,
            max_seek_points: 0,
        };
        let m = muxer_with_anchors(opts, anchors);
        let pts = m.select_seek_points();
        assert_eq!(pts.len(), 1);
        assert_eq!(pts[0].sample_number, 0);
        assert_eq!(pts[0].offset, 0);
    }

    #[test]
    fn for_sample_rate_targets_one_point_per_second() {
        let o = FlacMuxerOptions::for_sample_rate(48_000);
        assert_eq!(o.seek_point_interval, 48_000);
        assert!(o.generate_seektable);
        let z = FlacMuxerOptions::for_sample_rate(0);
        assert_eq!(z.seek_point_interval, 44_100, "zero rate falls back");
    }

    #[test]
    fn no_seektable_options_disable_generation() {
        let o = FlacMuxerOptions::no_seektable();
        assert!(!o.generate_seektable);
    }
}
