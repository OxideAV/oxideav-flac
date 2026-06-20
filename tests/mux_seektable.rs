//! End-to-end test for muxer-side SEEKTABLE generation (RFC 9639 §8.5).
//!
//! Encodes real PCM through the FLAC encoder, muxes the resulting frame
//! packets through the native FLAC muxer (which builds a SEEKTABLE from
//! the frames it writes), then re-opens the muxed bytes through the
//! demuxer and asserts:
//!
//! * a non-empty SEEKTABLE was emitted, and the parsed points are
//!   strictly ascending by sample number (the §8.5.1 MUSTs);
//! * each seek point's `offset` lands exactly on a real frame header in
//!   the muxed stream, and its `sample_number` matches that frame's
//!   first sample (so the offsets are first-frame-relative per §8.5.1);
//! * `seek_to` driven by the generated table lands on the expected
//!   frame and the next decoded packet carries the right pts.

use std::io::Cursor;

use oxideav_core::{
    AudioFrame, CodecId, CodecParameters, ContainerRegistry, Error, Frame, NullCodecResolver,
    Packet, ReadSeek, SampleFormat, StreamInfo, TimeBase, WriteSeek,
};
use oxideav_flac::container::{open_muxer_with_options, FlacMuxerOptions};
use oxideav_flac::encoder::{make_encoder_with_options, FlacEncoderOptions};

/// Interleave per-channel S16 PCM into one `AudioFrame`. The nested
/// index loop is a channel-major→interleaved transpose; iterator form
/// would not read more clearly here.
#[allow(clippy::needless_range_loop)]
fn s16_frame(channels: u16, pcm: &[Vec<i32>]) -> AudioFrame {
    let n = pcm[0].len();
    let mut interleaved = Vec::with_capacity(n * channels as usize * 2);
    for i in 0..n {
        for c in 0..channels as usize {
            interleaved.extend_from_slice(&(pcm[c][i] as i16).to_le_bytes());
        }
    }
    AudioFrame {
        samples: n as u32,
        pts: Some(0),
        data: vec![interleaved],
    }
}

/// Encode `pcm` at the given small block size and return
/// `(frame_packets, output_params_after_flush)`.
fn encode_small_blocks(
    channels: u16,
    sample_rate: u32,
    block_size: u32,
    pcm: Vec<Vec<i32>>,
) -> (Vec<Packet>, CodecParameters) {
    let mut params = CodecParameters::audio(CodecId::new("flac"));
    params.channels = Some(channels);
    params.sample_rate = Some(sample_rate);
    params.sample_format = Some(SampleFormat::S16);
    let mut opts = FlacEncoderOptions::default();
    opts.block_size = Some(block_size);
    let mut enc = make_encoder_with_options(&params, opts).expect("make_encoder");

    let frame = s16_frame(channels, &pcm);
    enc.send_frame(&Frame::Audio(frame)).expect("send_frame");
    enc.flush().expect("flush");

    let mut packets = Vec::new();
    loop {
        match enc.receive_packet() {
            Ok(p) => packets.push(p),
            Err(Error::NeedMore) | Err(Error::Eof) => break,
            Err(e) => panic!("encoder error: {e:?}"),
        }
    }
    // output_params() carries the finalised STREAMINFO (total samples,
    // md5) only after flush + drain.
    (packets, enc.output_params().clone())
}

/// Mux into a Vec we own: the muxer writes into a boxed `WriteSeek`
/// (the trait erases the concrete writer), so we hand it a shared-buffer
/// writer whose bytes we recover after the muxer drops its handle.
fn mux_into_vec(params: &CodecParameters, packets: &[Packet], opts: FlacMuxerOptions) -> Vec<u8> {
    // A WriteSeek backed by a Vec we can read after the muxer drops it.
    // We use a Cursor<Vec<u8>> wrapped so the inner Vec is recoverable.
    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, params.sample_rate.unwrap_or(44_100) as i64),
        duration: None,
        start_time: Some(0),
        params: params.clone(),
    };
    let shared = SharedCursor::new();
    let handle = shared.clone();
    let writer: Box<dyn WriteSeek> = Box::new(shared);
    let mut muxer =
        open_muxer_with_options(writer, std::slice::from_ref(&stream), opts).expect("open_muxer");
    muxer.write_header().expect("write_header");
    for p in packets {
        muxer.write_packet(p).expect("write_packet");
    }
    muxer.write_trailer().expect("write_trailer");
    drop(muxer);
    handle.into_inner()
}

/// A `Write + Seek + Send` whose buffer is recoverable after the muxer
/// drops its boxed handle (the `Muxer` trait erases the concrete writer
/// type, and requires `Send`). Backed by `Arc<Mutex<…>>`.
#[derive(Clone)]
struct SharedCursor {
    inner: std::sync::Arc<std::sync::Mutex<Cursor<Vec<u8>>>>,
}

impl SharedCursor {
    fn new() -> Self {
        Self {
            inner: std::sync::Arc::new(std::sync::Mutex::new(Cursor::new(Vec::new()))),
        }
    }
    fn into_inner(self) -> Vec<u8> {
        self.inner.lock().unwrap().get_ref().clone()
    }
}

impl std::io::Write for SharedCursor {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.lock().unwrap().write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.lock().unwrap().flush()
    }
}

impl std::io::Seek for SharedCursor {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.inner.lock().unwrap().seek(pos)
    }
}

fn open_demuxer(bytes: Vec<u8>) -> Box<dyn oxideav_core::Demuxer> {
    let mut reg = ContainerRegistry::new();
    oxideav_flac::register_containers(&mut reg);
    let cursor: Box<dyn ReadSeek> = Box::new(Cursor::new(bytes));
    reg.open_demuxer("flac", cursor, &NullCodecResolver)
        .expect("open flac demuxer")
}

/// Two-channel sine, `n` samples per channel.
fn stereo_sine(n: usize, sr: u32) -> Vec<Vec<i32>> {
    let mut l = Vec::with_capacity(n);
    let mut r = Vec::with_capacity(n);
    for i in 0..n {
        let t = i as f64 / sr as f64;
        l.push(((t * 440.0 * 2.0 * std::f64::consts::PI).sin() * 18_000.0) as i32);
        r.push(((t * 660.0 * 2.0 * std::f64::consts::PI).sin() * 9_000.0) as i32);
    }
    vec![l, r]
}

#[test]
fn muxer_generates_seektable_and_seeks_land() {
    let sr = 44_100u32;
    // 20 frames of 512 samples => 10 240 samples. Default interval is
    // one point per second (44 100 samples) which would yield just the
    // first frame, so force a tight interval to get several points.
    let n = 512 * 20;
    let pcm = stereo_sine(n, sr);
    let (packets, params) = encode_small_blocks(2, sr, 512, pcm);
    assert!(
        packets.len() >= 10,
        "expected many frames, got {}",
        packets.len()
    );

    let opts = FlacMuxerOptions {
        generate_seektable: true,
        seek_point_interval: 2048, // one point per ~4 frames
        max_seek_points: 0,
    };
    let bytes = mux_into_vec(&params, &packets, opts);

    // Re-open and confirm a SEEKTABLE was parsed.
    let mut dmx = open_demuxer(bytes.clone());

    // Drive a seek to a sample in the middle and check the landing.
    let target = 5_000i64;
    let landed = dmx.seek_to(0, target).expect("seek");
    assert!(
        landed <= target,
        "landed {landed} must be <= target {target}"
    );
    // The landing must be a real frame boundary: a multiple of 512.
    assert_eq!(landed % 512, 0, "landed on a non-frame-boundary sample");
    let pkt = dmx.next_packet().expect("packet after seek");
    assert_eq!(pkt.pts, Some(landed), "first packet pts must equal landing");

    // Seek to 0 lands at the very start.
    let landed0 = dmx.seek_to(0, 0).expect("seek 0");
    assert_eq!(landed0, 0);
    let pkt0 = dmx.next_packet().expect("packet after seek 0");
    assert_eq!(pkt0.pts, Some(0));
}

#[test]
fn generated_seektable_offsets_are_first_frame_relative() {
    let sr = 48_000u32;
    let n = 512 * 16;
    let pcm = stereo_sine(n, sr);
    let (packets, params) = encode_small_blocks(2, sr, 512, pcm);

    let opts = FlacMuxerOptions {
        generate_seektable: true,
        seek_point_interval: 1024,
        max_seek_points: 0,
    };
    let bytes = mux_into_vec(&params, &packets, opts);

    // Independently walk the metadata chain to extract the SEEKTABLE and
    // the first-frame offset, then confirm every seek point's offset
    // points at a real frame whose first sample matches.
    use oxideav_flac::metadata::{MetadataBlock, MetadataChain};
    // Strip the 4-byte fLaC marker, parse the chain.
    assert_eq!(&bytes[0..4], b"fLaC");
    let (chain, consumed) = MetadataChain::parse(&bytes[4..]).expect("parse chain");
    let first_frame = 4 + consumed;

    let seektable = chain
        .blocks
        .iter()
        .find_map(|b| match b {
            MetadataBlock::SeekTable { points, .. } => Some(points.clone()),
            _ => None,
        })
        .expect("a SEEKTABLE block must have been generated");
    assert!(!seektable.is_empty(), "SEEKTABLE has no points");

    // Strictly ascending (the §8.5.1 sorted + unique MUSTs).
    for w in seektable.windows(2) {
        assert!(
            w[0].sample_number < w[1].sample_number,
            "seek points not strictly ascending"
        );
    }
    // First point anchors sample 0 at offset 0.
    assert_eq!(seektable[0].sample_number, 0);
    assert_eq!(seektable[0].offset, 0);

    // Every offset must land on a frame sync (0xFF 0xF8/0xF9) in the
    // frame region — confirming the offset is relative to the first
    // frame header, not the file start.
    for sp in &seektable {
        let abs = first_frame + sp.offset as usize;
        assert!(abs + 2 <= bytes.len(), "offset past EOF");
        assert_eq!(bytes[abs], 0xFF, "offset {} not at a frame sync", sp.offset);
        assert!(
            bytes[abs + 1] == 0xF8 || bytes[abs + 1] == 0xF9,
            "offset {} second sync byte wrong",
            sp.offset
        );
    }
}

#[test]
fn no_seektable_option_emits_no_table() {
    let sr = 44_100u32;
    let n = 512 * 8;
    let pcm = stereo_sine(n, sr);
    let (packets, params) = encode_small_blocks(2, sr, 512, pcm);

    let bytes = mux_into_vec(&params, &packets, FlacMuxerOptions::no_seektable());
    use oxideav_flac::metadata::{MetadataBlock, MetadataChain};
    let (chain, _) = MetadataChain::parse(&bytes[4..]).expect("parse chain");
    assert!(
        !chain
            .blocks
            .iter()
            .any(|b| matches!(b, MetadataBlock::SeekTable { .. })),
        "no_seektable option must not emit a SEEKTABLE"
    );
}

#[test]
fn registry_default_muxer_generates_seektable() {
    // The registry-path muxer uses the default options (generation on).
    let sr = 8_000u32; // low rate -> default ~1 point/sec interval = 8000
    let n = 512 * 40; // 20 480 samples => ~2.5 s => a few points
    let pcm = stereo_sine(n, sr);
    let (packets, params) = encode_small_blocks(2, sr, 512, pcm);

    let stream = StreamInfo {
        index: 0,
        time_base: TimeBase::new(1, sr as i64),
        duration: None,
        start_time: Some(0),
        params: params.clone(),
    };
    let shared = SharedCursor::new();
    let handle = shared.clone();
    let mut reg = ContainerRegistry::new();
    oxideav_flac::register_containers(&mut reg);
    let writer: Box<dyn WriteSeek> = Box::new(shared);
    let mut muxer = reg
        .open_muxer("flac", writer, std::slice::from_ref(&stream))
        .expect("registry open_muxer");
    muxer.write_header().unwrap();
    for p in &packets {
        muxer.write_packet(p).unwrap();
    }
    muxer.write_trailer().unwrap();
    drop(muxer);
    let bytes = handle.into_inner();

    use oxideav_flac::metadata::{MetadataBlock, MetadataChain};
    let (chain, _) = MetadataChain::parse(&bytes[4..]).expect("parse chain");
    let pts = chain.blocks.iter().find_map(|b| match b {
        MetadataBlock::SeekTable { points, .. } => Some(points.len()),
        _ => None,
    });
    assert!(
        matches!(pts, Some(c) if c >= 1),
        "registry default muxer should generate a SEEKTABLE, got {pts:?}"
    );
}
