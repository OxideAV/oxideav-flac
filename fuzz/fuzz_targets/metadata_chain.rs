#![no_main]

//! Fuzz: whole-chain `MetadataChain` parse / write round-trip with a
//! structure-preserving mutation oracle (round 283).
//!
//! `metadata_walker` (r200) predates the typed `MetadataChain` API
//! (r279) and exercises each block-level parser/writer pair in
//! isolation; nothing fuzzes the chain level itself — the surface
//! that owns RFC 9639 §8's *stream-level* MUSTs (STREAMINFO first,
//! singleton STREAMINFO / SEEKTABLE / VORBIS_COMMENT, at most one
//! each of picture types 1 and 2, forbidden block-type 127, `last`
//! flag placement) plus the parse-Ok ⇒ write-Ok rewrite contract
//! that a chain editor (read, retag, re-emit) depends on.
//!
//! Three phases per iteration:
//!
//! 1. **Raw bytes.** Feed the input (with and without a leading
//!    `fLaC` marker) to `MetadataChain::parse`. Every `Ok` chain
//!    must validate, re-write, re-parse to an equal chain consuming
//!    exactly the written length, and re-write byte-stably (the
//!    typed representation is the normal form: PADDING content and
//!    seek-table placeholder positions normalise, everything else
//!    round-trips exactly).
//!
//! 2. **Synthesizer + independent rule oracle.** Build a typed chain
//!    from fuzzer-chosen blocks whose *field values* are always
//!    writer-acceptable, but whose *chain shape* (first-block kind,
//!    singleton duplication, icon-type repetition) is fuzzer
//!    controlled. The harness re-states the §8 chain rules
//!    independently and asserts `validate()` agrees exactly. Valid
//!    chains must write + re-parse to equality; invalid chains must
//!    be refused by `write` AND by `parse` when the same blocks are
//!    hand-assembled into wire bytes (parser-side enforcement, not
//!    just writer-side).
//!
//! 3. **Structure-preserving mutations** on the valid wire image:
//!    * setting `last` on block k truncates the chain — parse must
//!      return exactly the first k+1 blocks and stop at block k's
//!      end (every prefix of a valid chain is valid: the rule
//!      counts are monotone and the first block is unchanged);
//!    * rewriting any header's type code to 127 must be rejected
//!      (§8.1 Table 2 forbidden value);
//!    * corrupting a PADDING payload byte must parse to the *same*
//!      chain (§8.3 defines the content, so it is not preserved)
//!      and re-write with the byte scrubbed back to zero;
//!    * an arbitrary single-byte XOR anywhere must never panic, and
//!      when the mutant still parses, the parse-Ok ⇒ write-Ok ⇒
//!      equal-re-parse contract must still hold.
//!
//! Findings fixed alongside this target (regression-pinned in
//! `src/metadata.rs` tests):
//! * `parse_cuesheet` accepted a media catalog number with
//!   non-printable leading bytes that `write_cuesheet` refuses
//!   (RFC 9639 §8.7 printable-ASCII rule) — parse now mirrors the
//!   writer's check;
//! * `MetadataChain::parse` accepted SEEKTABLE blocks whose real
//!   points violate the §8.5.1 sorted/unique MUSTs that
//!   `write_seektable` enforces — the chain parser now rejects
//!   them, keeping parse-Ok ⇒ write-Ok.

use libfuzzer_sys::fuzz_target;
use oxideav_flac::metadata::{
    Application, BlockHeader, CueSheet, CueSheetIndex, CueSheetTrack, MetadataBlock, MetadataChain,
    Picture, SeekPoint, StreamInfo, VorbisComment, FLAC_MAGIC,
};

/// Inputs larger than this are pure throughput waste for a
/// structural target; the interesting state space is shape, not
/// payload bulk.
const MAX_INPUT: usize = 64 * 1024;
/// Cap synthesized chains so one iteration stays cheap.
const MAX_SYNTH_BLOCKS: usize = 10;
/// Cap variable-length synthesized payload slices.
const MAX_SLICE: usize = 512;

/// Deterministic byte lexer over the fuzz input; yields zeros once
/// exhausted so every input maps to a well-defined chain.
struct Lex<'a> {
    data: &'a [u8],
    pos: usize,
}

impl<'a> Lex<'a> {
    fn new(data: &'a [u8]) -> Self {
        Self { data, pos: 0 }
    }
    fn u8(&mut self) -> u8 {
        let b = self.data.get(self.pos).copied().unwrap_or(0);
        self.pos += 1;
        b
    }
    fn u16(&mut self) -> u16 {
        u16::from_be_bytes([self.u8(), self.u8()])
    }
    fn u32(&mut self) -> u32 {
        u32::from_be_bytes([self.u8(), self.u8(), self.u8(), self.u8()])
    }
    fn u64(&mut self) -> u64 {
        ((self.u32() as u64) << 32) | self.u32() as u64
    }
    fn slice(&mut self, max: usize) -> &'a [u8] {
        let want = (self.u16() as usize) % (max + 1);
        let start = self.pos.min(self.data.len());
        let end = (start + want).min(self.data.len());
        self.pos = end;
        &self.data[start..end]
    }
    /// A short string of printable ASCII (0x20..=0x7E) excluding
    /// `=` — valid both as a Vorbis field name (§8.6) and inside a
    /// cuesheet media catalog number (§8.7).
    fn printable(&mut self, max: usize) -> String {
        self.slice(max)
            .iter()
            .map(|&b| {
                let c = 0x20 + (b % 0x5F); // 0x20..=0x7E
                if c == b'=' {
                    b'-'
                } else {
                    c
                }
            })
            .map(char::from)
            .collect()
    }
}

/// Independent re-statement of the RFC 9639 §8 chain-level rules
/// (the oracle `MetadataChain::validate` is checked against). Kept
/// deliberately separate from the crate's implementation: first
/// block is STREAMINFO (§8), at most one STREAMINFO (§8.2), at most
/// one SEEKTABLE (§8.5), at most one VORBIS_COMMENT (§8.6), at most
/// one each of picture types 1 and 2 (§8.8 Table 13).
fn chain_rules_hold(blocks: &[MetadataBlock]) -> bool {
    if !matches!(blocks.first(), Some(MetadataBlock::StreamInfo(_))) {
        return false;
    }
    let count = |f: &dyn Fn(&MetadataBlock) -> bool| blocks.iter().filter(|b| f(b)).count();
    count(&|b| matches!(b, MetadataBlock::StreamInfo(_))) <= 1
        && count(&|b| matches!(b, MetadataBlock::SeekTable { .. })) <= 1
        && count(&|b| matches!(b, MetadataBlock::VorbisComment(_))) <= 1
        && count(&|b| matches!(b, MetadataBlock::Picture(p) if p.picture_type == 1)) <= 1
        && count(&|b| matches!(b, MetadataBlock::Picture(p) if p.picture_type == 2)) <= 1
}

/// Hand-assemble wire bytes for a block list (header + payload each,
/// `last` on the final block) WITHOUT `MetadataChain::write`, so the
/// parser's chain-rule enforcement is tested independently of the
/// writer's. Returns `None` for an empty list.
fn assemble_raw(blocks: &[MetadataBlock]) -> Option<Vec<u8>> {
    if blocks.is_empty() {
        return None;
    }
    let mut out = Vec::new();
    for (i, b) in blocks.iter().enumerate() {
        let payload = b
            .write_payload()
            .expect("synthesized block payloads are writer-acceptable by construction");
        BlockHeader {
            last: i + 1 == blocks.len(),
            block_type: b.block_type(),
            length: payload.len() as u32,
        }
        .write_into(&mut out)
        .expect("synthesized block types are always serialisable");
        out.extend_from_slice(&payload);
    }
    Some(out)
}

/// Per-block (header_offset, end_offset) positions inside a wire
/// image produced by `MetadataChain::write`.
fn block_spans(bytes: &[u8]) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut pos = 0usize;
    loop {
        let hdr = BlockHeader::parse(&bytes[pos..]).expect("own writer output");
        let end = pos + 4 + hdr.length as usize;
        spans.push((pos, end));
        pos = end;
        if hdr.last {
            return spans;
        }
    }
}

fn synth_streaminfo(lex: &mut Lex) -> StreamInfo {
    let mut md5 = [0u8; 16];
    for b in md5.iter_mut() {
        *b = lex.u8();
    }
    StreamInfo {
        min_block_size: lex.u16(),
        max_block_size: lex.u16(),
        min_frame_size: lex.u32() & StreamInfo::MAX_FRAME_SIZE,
        max_frame_size: lex.u32() & StreamInfo::MAX_FRAME_SIZE,
        sample_rate: lex.u32() & StreamInfo::MAX_SAMPLE_RATE,
        channels: 1 + lex.u8() % 8,
        bits_per_sample: 1 + lex.u8() % 32,
        total_samples: lex.u64() & StreamInfo::MAX_TOTAL_SAMPLES,
        md5,
    }
}

/// One writer-acceptable block of a fuzzer-chosen kind. Field values
/// are clamped into the ranges the typed writers accept; chain-shape
/// validity is NOT enforced here — that's the oracle's job.
fn synth_block(lex: &mut Lex) -> MetadataBlock {
    match lex.u8() % 8 {
        0 => MetadataBlock::StreamInfo(synth_streaminfo(lex)),
        1 => MetadataBlock::Padding(lex.u16() as u32 % 4096),
        2 => MetadataBlock::Application(Application {
            id: lex.u32(),
            data: lex.slice(MAX_SLICE).to_vec(),
        }),
        3 => {
            // Strictly ascending real points (§8.5.1), bounded well
            // below the placeholder sentinel.
            let n = lex.u8() % 8;
            let mut sample = lex.u32() as u64;
            let mut points = Vec::with_capacity(n as usize);
            for _ in 0..n {
                points.push(SeekPoint {
                    sample_number: sample,
                    offset: lex.u64(),
                    frame_samples: lex.u16(),
                });
                sample += 1 + lex.u16() as u64;
            }
            MetadataBlock::SeekTable {
                points,
                placeholder_count: (lex.u8() % 4) as usize,
            }
        }
        4 => {
            let n = lex.u8() % 4;
            let mut comments = Vec::with_capacity(n as usize);
            for _ in 0..n {
                comments.push((
                    lex.printable(16),
                    String::from_utf8_lossy(lex.slice(64)).into_owned(),
                ));
            }
            MetadataBlock::VorbisComment(VorbisComment {
                vendor: String::from_utf8_lossy(lex.slice(32)).into_owned(),
                comments,
            })
        }
        5 => {
            let mut media_catalog_number = [0u8; 128];
            let catalog = lex.printable(32);
            media_catalog_number[..catalog.len()].copy_from_slice(catalog.as_bytes());
            let is_cdda = lex.u8() & 1 != 0;
            let n_payload = lex.u8() % 3;
            let mut tracks = Vec::with_capacity(n_payload as usize + 1);
            for _ in 0..n_payload {
                let mut isrc = [0u8; 12];
                for b in isrc.iter_mut() {
                    *b = lex.u8();
                }
                let n_idx = 1 + lex.u8() % 3;
                let indices = (0..n_idx)
                    .map(|_| CueSheetIndex {
                        offset_samples: lex.u64(),
                        number: lex.u8(),
                    })
                    .collect();
                tracks.push(CueSheetTrack {
                    offset_samples: lex.u64(),
                    number: 1 + lex.u8() % 254, // 0 is reserved (§8.7.1)
                    isrc,
                    is_audio: lex.u8() & 1 == 0,
                    pre_emphasis: lex.u8() & 1 != 0,
                    indices,
                });
            }
            // Trailing lead-out: the only track allowed zero indices.
            tracks.push(CueSheetTrack {
                offset_samples: lex.u64(),
                number: if is_cdda {
                    CueSheetTrack::CDDA_LEAD_OUT
                } else {
                    CueSheetTrack::NON_CDDA_LEAD_OUT
                },
                isrc: [0u8; 12],
                is_audio: true,
                pre_emphasis: false,
                indices: Vec::new(),
            });
            MetadataBlock::CueSheet(CueSheet {
                media_catalog_number,
                lead_in_samples: lex.u64(),
                is_cdda,
                tracks,
            })
        }
        6 => {
            // Bias picture_type towards the singleton codes 1 and 2
            // so the Table 13 uniqueness rule is hit constantly.
            let sel = lex.u8();
            let picture_type = match sel % 4 {
                0 => 1,
                1 => 2,
                2 => (sel >> 2) as u32,
                _ => lex.u32(),
            };
            let uri = lex.u8().is_multiple_of(4);
            MetadataBlock::Picture(Picture {
                picture_type,
                mime_type: if uri {
                    "-->".into()
                } else {
                    "image/png".into()
                },
                description: String::from_utf8_lossy(lex.slice(32)).into_owned(),
                width: lex.u32(),
                height: lex.u32(),
                depth: lex.u32(),
                colour_count: lex.u32(),
                data: lex.slice(MAX_SLICE).to_vec(),
            })
        }
        _ => MetadataBlock::Reserved {
            code: 7 + lex.u8() % 120, // 7..=126 (§8.1 Table 2)
            payload: lex.slice(MAX_SLICE).to_vec(),
        },
    }
}

/// Phase-1/2 shared assertions on a chain `parse` accepted: it must
/// validate, re-write, re-parse to equality, and re-write
/// byte-stably (the typed chain is the normal form).
fn assert_parse_ok_contract(chain: &MetadataChain) -> Vec<u8> {
    chain
        .validate()
        .expect("MetadataChain::parse output must pass validate()");
    assert!(
        chain.stream_info().is_some(),
        "validated chain must expose its STREAMINFO"
    );
    let written = chain
        .write()
        .expect("parse-accepted chain must re-write (parse-Ok implies write-Ok)");
    let (back, consumed) =
        MetadataChain::parse(&written).expect("writer output must re-parse cleanly");
    assert_eq!(consumed, written.len(), "writer output has trailing slack");
    assert_eq!(&back, chain, "typed chain drift across write + re-parse");
    assert_eq!(
        back.write().expect("second write"),
        written,
        "MetadataChain::write is non-deterministic"
    );
    written
}

/// Phase 3: structure-preserving mutations of a valid wire image.
fn mutate_wire(wire: &[u8], chain: &MetadataChain, lex: &mut Lex) {
    let spans = block_spans(wire);
    let n = spans.len();

    // M1: promote block k's header to `last` — a truncation. Every
    // non-empty prefix of a valid chain is itself valid (the §8
    // counts are monotone and the first block is unchanged), so
    // parse must succeed with exactly k+1 blocks and stop at block
    // k's end, ignoring the trailing bytes (documented behaviour).
    let k = (lex.u8() as usize) % n;
    let mut truncated = wire.to_vec();
    truncated[spans[k].0] |= 0x80;
    let (prefix, consumed) =
        MetadataChain::parse(&truncated).expect("prefix of a valid chain must parse");
    assert_eq!(
        consumed, spans[k].1,
        "truncated parse overran the last flag"
    );
    assert_eq!(
        prefix.blocks,
        chain.blocks[..=k],
        "truncated chain is not the typed prefix"
    );
    assert_parse_ok_contract(&prefix);

    // M2: rewrite block k's type code to the forbidden 127 (§8.1
    // Table 2) — the parser must reject the whole chain.
    let mut forbidden = wire.to_vec();
    forbidden[spans[k].0] = (forbidden[spans[k].0] & 0x80) | 127;
    assert!(
        MetadataChain::parse(&forbidden).is_err(),
        "block-type 127 must be rejected anywhere in the chain"
    );

    // M3: corrupt one byte inside a PADDING payload. §8.3 defines
    // the content as all-zero, so the typed chain must be unchanged
    // and the re-write must scrub the byte back to zero.
    for (i, block) in chain.blocks.iter().enumerate() {
        if let MetadataBlock::Padding(len) = block {
            if *len > 0 {
                let mut dirty = wire.to_vec();
                let off = spans[i].0 + 4 + (lex.u32() as usize) % (*len as usize);
                dirty[off] = 0xAA;
                let (scrubbed, _) =
                    MetadataChain::parse(&dirty).expect("non-zero padding content still parses");
                assert_eq!(
                    &scrubbed, chain,
                    "padding content leaked into the typed chain"
                );
                assert_eq!(
                    scrubbed.write().expect("re-write"),
                    wire,
                    "padding corruption survived a chain re-write"
                );
                break;
            }
        }
    }

    // M4: arbitrary single-byte XOR — panic-freedom plus the full
    // parse-Ok contract on whatever still parses.
    let pos = (lex.u32() as usize) % wire.len();
    let mut mutant = wire.to_vec();
    mutant[pos] ^= lex.u8() | 1;
    if let Ok((c, consumed)) = MetadataChain::parse(&mutant) {
        assert!(consumed <= mutant.len());
        assert_parse_ok_contract(&c);
    }
}

fuzz_target!(|data: &[u8]| {
    if data.len() > MAX_INPUT {
        return;
    }

    // ---------------------------------------------------------------
    // Phase 1: raw bytes straight into the chain parser, both as-is
    // and past a leading stream marker when one is present.
    // ---------------------------------------------------------------
    let body = if data.len() >= 4 && data[..4] == FLAC_MAGIC {
        &data[4..]
    } else {
        data
    };
    for input in [data, body] {
        if let Ok((chain, consumed)) = MetadataChain::parse(input) {
            assert!(consumed <= input.len());
            assert_parse_ok_contract(&chain);
        }
    }

    // ---------------------------------------------------------------
    // Phase 2: synthesizer + independent §8 rule oracle.
    // ---------------------------------------------------------------
    let mut lex = Lex::new(data);
    let n_blocks = (lex.u8() as usize) % (MAX_SYNTH_BLOCKS + 1);
    let mut blocks = Vec::with_capacity(n_blocks);
    if n_blocks > 0 {
        // Bias the first block towards STREAMINFO (valid shape) but
        // keep a fuzzer-reachable path to any other kind so the
        // first-block rule is exercised from the rejecting side too.
        if !lex.u8().is_multiple_of(8) {
            blocks.push(MetadataBlock::StreamInfo(synth_streaminfo(&mut lex)));
        } else {
            blocks.push(synth_block(&mut lex));
        }
        for _ in 1..n_blocks {
            blocks.push(synth_block(&mut lex));
        }
    }

    let expect_valid = chain_rules_hold(&blocks);
    let chain = MetadataChain { blocks };
    assert_eq!(
        chain.validate().is_ok(),
        expect_valid,
        "validate() disagrees with the independent §8 rule oracle on {:?}",
        chain
            .blocks
            .iter()
            .map(|b| b.block_type())
            .collect::<Vec<_>>()
    );

    if expect_valid {
        let wire = assert_parse_ok_contract(&chain);
        // -----------------------------------------------------------
        // Phase 3: structure-preserving mutations.
        // -----------------------------------------------------------
        mutate_wire(&wire, &chain, &mut lex);
    } else {
        assert!(
            chain.write().is_err(),
            "writer accepted a chain violating the §8 rules"
        );
        // Parser-side enforcement: the same rule-violating blocks
        // hand-assembled into wire bytes must be rejected too.
        if let Some(raw) = assemble_raw(&chain.blocks) {
            assert!(
                MetadataChain::parse(&raw).is_err(),
                "parser accepted wire bytes for a chain violating the §8 rules"
            );
        }
    }
});
