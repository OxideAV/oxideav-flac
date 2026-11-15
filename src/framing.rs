//! Cutting a FLAC frame stream into frames.
//!
//! A frame carries no length. Its start is recognisable — a sync code and
//! a header protected by a CRC-8 (RFC 9639 §9.1) — and so is its end,
//! because the last two bytes are a CRC-16 over everything before them
//! (§9.3). The sync code alone is weak: the same 15 bits occur inside
//! coded audio, and one in 256 of those is followed by bytes whose CRC-8
//! happens to match. Cutting at every such position would split a good
//! frame in two, and the decoder would then reject both halves.
//!
//! [`FrameScanner`] therefore treats a header-shaped position only as a
//! *candidate* for the start of the next frame, and accepts it when the
//! two bytes before it are the CRC-16 of the bytes since the current
//! frame started. A frame whose CRC-16 never matches (it is damaged, or
//! the scan began in the middle of one after a seek) is cut at the first
//! candidate once the scan has gone further than a frame of that shape
//! can be long, and is handed out marked unverified so that the decoder
//! reports the damage; scanning resumes at that candidate, which is how
//! the stream resynchronises.
//!
//! The work is bounded for hostile input: the search for candidates never
//! revisits a byte, and the CRC-16 passes — which restart whenever a frame
//! is given up on — are paid for from a budget proportional to the input
//! consumed. When the budget is spent the scanner stops verifying and
//! cuts at every candidate until it has earned more.

use std::collections::VecDeque;

use oxideav_core::Error;

use crate::crc::Crc16;
use crate::frame::{FrameHeader, MAX_FRAME_HEADER_LEN, parse_frame_header};

/// CRC-16 work allowed per byte of input.
const CRC_BUDGET_PER_BYTE: u64 = 16;

/// CRC-16 work allowed before any input was seen.
const CRC_BUDGET_FLOOR: u64 = 1 << 20;

/// Consumed bytes are dropped from the front of the buffer once there are
/// this many of them.
const COMPACT_AT: usize = 64 * 1024;

/// One frame found by [`FrameScanner::next_frame`].
#[derive(Clone, Debug)]
pub(crate) struct FrameSpan {
    /// Range of the frame in the scanner's buffer, valid until the next
    /// call that mutates the scanner.
    pub start: usize,
    pub end: usize,
    /// The frame's parsed header.
    pub header: FrameHeader,
    /// True when the frame ends in a CRC-16 that matches its bytes. False
    /// for a frame that was cut without that confirmation.
    #[cfg_attr(not(test), allow(dead_code))]
    pub verified: bool,
}

enum Probe {
    /// A header-shaped position.
    Found(usize, FrameHeader),
    /// The buffered bytes are exhausted and more may follow.
    NeedData,
    /// The buffered bytes are exhausted and the stream has ended.
    End,
}

pub(crate) struct FrameScanner {
    buffer: Vec<u8>,
    /// Stream offset of `buffer[0]`, counted from where scanning began.
    base: u64,
    /// Start of the current frame, or — while no frame is open — the
    /// position before which no frame can start.
    head: usize,
    head_frame: Option<FrameHeader>,
    /// Next position to examine for a header. Never moves backwards.
    probe: usize,
    /// Header-shaped positions after `head`, in order.
    candidates: VecDeque<(usize, FrameHeader)>,
    /// How many of `candidates` were already ruled out as the end of the
    /// current frame.
    tested: usize,
    /// CRC-16 of `buffer[head..crc_pos]`.
    crc: Crc16,
    crc_pos: usize,
    /// A frame with the current header cannot be longer than this.
    limit: usize,
    /// Bit depth to assume when a header defers to STREAMINFO.
    stream_bits_per_sample: u32,
    crc_work: u64,
    crc_budget: u64,
}

impl FrameScanner {
    /// `stream_bits_per_sample` is the STREAMINFO bit depth, used to
    /// bound the length of frames whose header does not state one.
    pub fn new(stream_bits_per_sample: u32) -> Self {
        Self {
            buffer: Vec::with_capacity(COMPACT_AT),
            base: 0,
            head: 0,
            head_frame: None,
            probe: 0,
            candidates: VecDeque::new(),
            tested: 0,
            crc: Crc16::new(),
            crc_pos: 0,
            limit: 0,
            stream_bits_per_sample,
            crc_work: 0,
            crc_budget: CRC_BUDGET_FLOOR,
        }
    }

    /// Forget everything buffered and start again at stream offset 0 of
    /// whatever is pushed next.
    pub fn reset(&mut self) {
        self.buffer.clear();
        self.base = 0;
        self.head = 0;
        self.head_frame = None;
        self.probe = 0;
        self.candidates.clear();
        self.tested = 0;
        self.crc_pos = 0;
    }

    /// Append stream bytes.
    pub fn push(&mut self, bytes: &[u8]) {
        self.buffer.extend_from_slice(bytes);
        self.crc_budget = self
            .crc_budget
            .saturating_add(bytes.len() as u64 * CRC_BUDGET_PER_BYTE);
    }

    /// The bytes of a span returned by the latest [`Self::next_frame`].
    pub fn bytes(&self, span: &FrameSpan) -> &[u8] {
        &self.buffer[span.start..span.end]
    }

    /// Stream offset (from where scanning began) of a buffer position.
    pub fn offset_of(&self, position: usize) -> u64 {
        self.base + position as u64
    }

    /// Drop consumed bytes from the front of the buffer. Invalidates
    /// spans handed out earlier.
    pub fn compact(&mut self) {
        let drop = self.head;
        if drop < COMPACT_AT {
            return;
        }
        self.buffer.drain(..drop);
        self.base += drop as u64;
        self.head = 0;
        self.probe -= drop;
        self.crc_pos -= drop;
        for (position, _) in &mut self.candidates {
            *position -= drop;
        }
    }

    /// Upper bound on the length of a frame with this header: twice the
    /// size of the same block stored verbatim. An encoder that finds no
    /// better model stores a block verbatim (§9.2.7.3), so a longer frame
    /// would be one that was expanded for no reason.
    fn frame_limit(&self, header: &FrameHeader) -> usize {
        let bits = match header.bits_per_sample {
            0 if (1..=32).contains(&self.stream_bits_per_sample) => self.stream_bits_per_sample,
            0 => 32,
            n => n as u32,
        } as usize;
        let channels = header.channels.channel_count() as usize;
        // Per subframe: header and wasted-bits run, then the samples at
        // one bit more than the frame depth (a side channel).
        let subframe = 6 + (header.block_size as usize * (bits + 1)).div_ceil(8);
        2 * (MAX_FRAME_HEADER_LEN + channels * subframe + 2) + 64
    }

    fn begin(&mut self, position: usize, header: FrameHeader) {
        self.limit = self.frame_limit(&header);
        self.head = position;
        self.head_frame = Some(header);
        self.tested = 0;
        self.crc = Crc16::new();
        self.crc_pos = position;
    }

    /// Extend the running CRC-16 of the current frame up to `end`.
    fn crc_to(&mut self, end: usize) {
        self.crc.update(&self.buffer[self.crc_pos..end]);
        self.crc_work += (end - self.crc_pos) as u64;
        self.crc_pos = end;
    }

    /// True when `buffer[head..end]` ends in its own CRC-16.
    fn frame_ends_at(&mut self, end: usize, header_len: usize) -> bool {
        // Header, at least one subframe byte, CRC-16.
        if end < self.head + header_len + 3 {
            return false;
        }
        self.crc_to(end - 2);
        self.crc.value() == u16::from_be_bytes([self.buffer[end - 2], self.buffer[end - 1]])
    }

    /// Find the next header-shaped position at or after `probe`.
    fn probe_next(&mut self, eof: bool) -> Probe {
        let len = self.buffer.len();
        let mut at = self.probe;
        while at < len {
            let Some(step) = self.buffer[at..].iter().position(|&b| b == 0xFF) else {
                break;
            };
            at += step;
            match parse_frame_header(&self.buffer[at..]) {
                Ok(header) => {
                    self.probe = at + 1;
                    return Probe::Found(at, header);
                }
                // The header runs past the buffered bytes: wait for them,
                // unless there are none to come.
                Err(Error::NeedMore) if !eof => {
                    self.probe = at;
                    return Probe::NeedData;
                }
                Err(_) => at += 1,
            }
        }
        self.probe = len;
        if eof { Probe::End } else { Probe::NeedData }
    }

    /// The next frame, if the buffered bytes settle where it ends.
    /// `eof` tells the scanner that no more bytes will be pushed.
    pub fn next_frame(&mut self, eof: bool) -> Option<FrameSpan> {
        loop {
            let Some(header) = self.head_frame.clone() else {
                match self.probe_next(eof) {
                    Probe::Found(position, header) => {
                        self.begin(position, header);
                        continue;
                    }
                    Probe::NeedData | Probe::End => {
                        // Nothing before `probe` starts a frame.
                        self.head = self.probe;
                        self.crc_pos = self.probe;
                        return None;
                    }
                }
            };
            let header_len = header.header_byte_len;
            let out_of_budget = self.crc_work > self.crc_budget;

            // Does the frame end right before one of the known candidates?
            let mut give_up = false;
            while self.tested < self.candidates.len() {
                let position = self.candidates[self.tested].0;
                if out_of_budget || position - self.head > self.limit {
                    give_up = true;
                    break;
                }
                if self.frame_ends_at(position, header_len) {
                    let (_, next) = self.candidates[self.tested].clone();
                    self.candidates.drain(..=self.tested);
                    let span = FrameSpan {
                        start: self.head,
                        end: position,
                        header,
                        verified: true,
                    };
                    self.begin(position, next);
                    return Some(span);
                }
                self.tested += 1;
            }

            if !give_up && self.probe - self.head <= self.limit {
                match self.probe_next(eof) {
                    Probe::Found(position, next) => {
                        self.candidates.push_back((position, next));
                        continue;
                    }
                    Probe::NeedData => return None,
                    Probe::End => {
                        // The stream ends here; so may the frame.
                        let end = self.buffer.len();
                        if !out_of_budget && self.frame_ends_at(end, header_len) {
                            let span = FrameSpan {
                                start: self.head,
                                end,
                                header,
                                verified: true,
                            };
                            self.candidates.clear();
                            self.head = end;
                            self.crc_pos = end;
                            self.head_frame = None;
                            return Some(span);
                        }
                    }
                }
            }

            // No position confirms the frame. Cut it at the first header
            // that follows, or — when none does — where the scan stopped.
            let start = self.head;
            let end = match self.candidates.pop_front() {
                Some((position, next)) => {
                    self.begin(position, next);
                    position
                }
                None => {
                    let end = self.probe;
                    self.head = end;
                    self.crc_pos = end;
                    self.head_frame = None;
                    end
                }
            };
            return Some(FrameSpan {
                start,
                end,
                header,
                verified: false,
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::crc::{crc8, crc16};

    /// A structurally complete frame: 192-sample mono 16-bit header with
    /// frame number `number`, `body` as the subframe bytes, valid CRCs.
    fn frame(number: u8, body: &[u8]) -> Vec<u8> {
        let mut f = vec![0xFF, 0xF8, 0x1A, 0x08, number];
        f.push(crc8(&f));
        f.extend_from_slice(body);
        let c = crc16(&f);
        f.extend_from_slice(&c.to_be_bytes());
        f
    }

    /// A complete frame header (valid CRC-8) with nothing after it, as it
    /// can occur by chance inside coded audio.
    fn false_sync(number: u8) -> Vec<u8> {
        let mut f = vec![0xFF, 0xF8, 0x1A, 0x08, number];
        f.push(crc8(&f));
        f
    }

    fn body(len: usize, seed: u8) -> Vec<u8> {
        // No 0xFF bytes, so the body holds no sync code of its own.
        (0..len)
            .map(|i| ((i as u8).wrapping_mul(31) ^ seed) & 0x7F)
            .collect()
    }

    /// Run a scanner over `stream` delivered in `chunk`-byte pieces.
    fn scan(stream: &[u8], chunk: usize) -> Vec<(u64, Vec<u8>, bool)> {
        let mut scanner = FrameScanner::new(16);
        let mut out = Vec::new();
        let mut fed = 0;
        loop {
            let eof = fed == stream.len();
            while let Some(span) = scanner.next_frame(eof) {
                out.push((
                    scanner.offset_of(span.start),
                    scanner.bytes(&span).to_vec(),
                    span.verified,
                ));
                scanner.compact();
            }
            if eof {
                return out;
            }
            let n = chunk.min(stream.len() - fed);
            scanner.push(&stream[fed..fed + n]);
            fed += n;
        }
    }

    #[test]
    fn frames_are_cut_at_their_crc16_for_every_chunking() {
        let frames: Vec<Vec<u8>> = (0..6u8)
            .map(|i| frame(i, &body(40 + 7 * i as usize, i)))
            .collect();
        let stream: Vec<u8> = frames.concat();
        for chunk in [1, 2, 3, 5, 7, 16, 64, 1000] {
            let got = scan(&stream, chunk);
            assert_eq!(got.len(), frames.len(), "chunk {chunk}");
            let mut offset = 0u64;
            for ((at, bytes, verified), want) in got.iter().zip(&frames) {
                assert_eq!(*at, offset, "chunk {chunk}");
                assert_eq!(bytes, want, "chunk {chunk}");
                assert!(verified, "chunk {chunk}");
                offset += want.len() as u64;
            }
        }
    }

    #[test]
    fn a_header_shaped_run_inside_a_frame_does_not_split_it() {
        // Frame 1 carries two complete, CRC-8-valid frame headers in its
        // audio bytes.
        let mut inner = body(20, 1);
        inner.extend_from_slice(&false_sync(9));
        inner.extend_from_slice(&body(33, 2));
        inner.extend_from_slice(&false_sync(2));
        inner.extend_from_slice(&body(11, 3));
        let frames = [
            frame(0, &body(50, 0)),
            frame(1, &inner),
            frame(2, &body(50, 4)),
        ];
        let stream: Vec<u8> = frames.concat();
        for chunk in [1, 4, 9, 4096] {
            let got = scan(&stream, chunk);
            assert_eq!(got.len(), 3, "chunk {chunk}");
            for ((_, bytes, verified), want) in got.iter().zip(&frames) {
                assert_eq!(bytes, want, "chunk {chunk}");
                assert!(verified);
            }
        }
    }

    #[test]
    fn a_damaged_frame_is_cut_unverified_and_the_stream_resynchronises() {
        let mut frames: Vec<Vec<u8>> = (0..5u8).map(|i| frame(i, &body(60, i))).collect();
        // Flip a bit in the audio of frame 2: its CRC-16 no longer matches.
        frames[2][20] ^= 0x10;
        let stream: Vec<u8> = frames.concat();
        for chunk in [1, 13, 4096] {
            let got = scan(&stream, chunk);
            assert_eq!(got.len(), 5, "chunk {chunk}");
            for (i, ((_, bytes, verified), want)) in got.iter().zip(&frames).enumerate() {
                assert_eq!(bytes, want, "chunk {chunk} frame {i}");
                assert_eq!(*verified, i != 2, "chunk {chunk} frame {i}");
            }
        }
    }

    #[test]
    fn a_damaged_header_merges_the_frame_into_its_predecessor_only() {
        let mut frames: Vec<Vec<u8>> = (0..5u8).map(|i| frame(i, &body(60, i))).collect();
        // Break the CRC-8 of frame 2's header: it is no longer a header.
        frames[2][5] ^= 0xFF;
        let stream: Vec<u8> = frames.concat();
        let got = scan(&stream, 17);
        // Frame 1 cannot be confirmed any more at the start of frame 2,
        // so it runs up to frame 3; frames 0, 3 and 4 are untouched.
        assert_eq!(got.len(), 4);
        assert_eq!(got[0].1, frames[0]);
        assert!(got[0].2);
        assert_eq!(got[1].1, [frames[1].clone(), frames[2].clone()].concat());
        assert!(!got[1].2);
        assert_eq!(got[2].1, frames[3]);
        assert!(got[2].2);
        assert_eq!(got[3].1, frames[4]);
        assert!(got[3].2);
    }

    #[test]
    fn leading_and_trailing_junk() {
        let frames = [frame(0, &body(30, 0)), frame(1, &body(30, 1))];
        let mut stream = vec![0x12, 0xFF, 0x00, 0xFF, 0xF8, 0x00];
        let lead = stream.len() as u64;
        stream.extend_from_slice(&frames.concat());
        stream.extend_from_slice(b"TAG trailing bytes that are not a frame");
        let got = scan(&stream, 5);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].0, lead);
        assert_eq!(got[0].1, frames[0]);
        assert!(got[0].2);
        // The last frame runs to the end of the stream, junk included; it
        // is not confirmed, and a decoder ignores bytes after the frame.
        assert!(got[1].1.starts_with(&frames[1]));
        assert!(!got[1].2);
    }

    #[test]
    fn no_frame_at_all_yields_nothing_and_keeps_the_buffer_small() {
        let mut scanner = FrameScanner::new(16);
        let junk = vec![0x55u8; 8192];
        for _ in 0..200 {
            scanner.push(&junk);
            assert!(scanner.next_frame(false).is_none());
            scanner.compact();
            assert!(scanner.buffer.len() <= COMPACT_AT + junk.len());
        }
        assert!(scanner.next_frame(true).is_none());
    }

    #[test]
    fn dense_false_headers_stay_within_the_crc_budget() {
        // Nothing but header-shaped runs: every position opens a "frame"
        // that never verifies. The CRC work must stay proportional to the
        // input rather than to input × frame limit.
        let unit = false_sync(0);
        let stream: Vec<u8> = unit.iter().copied().cycle().take(600_000).collect();
        let mut scanner = FrameScanner::new(16);
        let mut covered = 0usize;
        for chunk in stream.chunks(8192) {
            scanner.push(chunk);
            while let Some(span) = scanner.next_frame(false) {
                covered += span.end - span.start;
                scanner.compact();
            }
        }
        while let Some(span) = scanner.next_frame(true) {
            covered += span.end - span.start;
        }
        // Every byte was handed out exactly once.
        assert_eq!(covered, stream.len());
        assert!(
            scanner.crc_work
                <= CRC_BUDGET_FLOOR + (CRC_BUDGET_PER_BYTE + 1) * stream.len() as u64 + 4096,
            "CRC work {} for {} input bytes",
            scanner.crc_work,
            stream.len()
        );
    }

    #[test]
    fn reset_starts_a_new_stream() {
        let a = frame(0, &body(30, 0));
        let b = frame(7, &body(30, 1));
        let mut scanner = FrameScanner::new(16);
        scanner.push(&a[..20]);
        assert!(scanner.next_frame(false).is_none());
        scanner.reset();
        scanner.push(&b);
        let span = scanner.next_frame(true).expect("frame after reset");
        assert_eq!(scanner.offset_of(span.start), 0);
        assert_eq!(scanner.bytes(&span), &b[..]);
        assert_eq!(span.header.coded_number, 7);
        assert!(span.verified);
    }
}
