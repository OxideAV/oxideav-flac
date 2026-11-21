//! FLAC demuxer `seek_to` smoke test.
//!
//! Builds a synthetic FLAC prefix (`fLaC` + STREAMINFO [+ SEEKTABLE])
//! followed by three hand-rolled fixed-blocking frames whose headers parse
//! and CRC-8-verify. We exercise both seek paths — SEEKTABLE-driven
//! byte-offset seeking and the seektable-less frame-header scanning
//! fallback (RFC 9639 §8.5) — and assert the next emitted packet carries
//! the expected sample number.

use std::io::Cursor;

use oxideav_core::ContainerRegistry;
use oxideav_flac::crc::crc8;

/// Pack a FLAC metadata-block header: last-flag (1 bit) + type (7 bits) +
/// 24-bit big-endian length.
fn block_header(is_last: bool, block_type: u8, length: usize) -> [u8; 4] {
    let b0 = if is_last { 0x80 } else { 0x00 } | (block_type & 0x7F);
    [
        b0,
        ((length >> 16) & 0xFF) as u8,
        ((length >> 8) & 0xFF) as u8,
        (length & 0xFF) as u8,
    ]
}

/// Build a STREAMINFO block. Fixed block size 192, 48kHz mono 16-bit,
/// total_samples unknown (0).
fn build_streaminfo() -> Vec<u8> {
    let mut block = vec![0u8; 34];
    block[0..2].copy_from_slice(&192u16.to_be_bytes()); // min block size
    block[2..4].copy_from_slice(&192u16.to_be_bytes()); // max block size
    // packed: sr(20)=48000, ch-1(3)=0 (mono), bps-1(5)=15 (16-bit), total(36)=0
    let packed: u64 = (48_000u64 << 44) | (15u64 << 36);
    block[10..18].copy_from_slice(&packed.to_be_bytes());
    block
}

/// Build a single FLAC fixed-blocking frame whose header carries
/// `frame_number`. Returns (frame_bytes, first_sample_of_frame).
///
/// Layout mirrors the parser in `frame.rs`:
/// * bytes 0..2: sync `0xFF 0xF8` (fixed blocking, reserved bits 0)
/// * byte 2:     block_size_code=1 (192 samples) <<4 | sample_rate_code=10 (48000)
/// * byte 3:     channel_code=0 (mono) <<4 | sample_size_code=4 (16bps) <<1 | reserved 0
/// * byte 4:     UTF-8 coded frame number (single byte for small values)
/// * byte 5:     CRC-8 over the previous 5 bytes
/// * 32 bytes of zero body, chosen to contain no 0xFF 0xF8/0xF9 pair.
fn build_frame(frame_number: u8) -> (Vec<u8>, u64) {
    assert!(frame_number < 0x80, "extend UTF-8 encoding for >=128");
    let mut bytes = Vec::with_capacity(6 + 32);
    bytes.push(0xFF);
    bytes.push(0xF8);
    bytes.push((1u8 << 4) | 10u8); // block_size_code=1, sample_rate_code=10
    bytes.push(4u8 << 1); // channel_code=0, sample_size_code=4, reserved=0
    bytes.push(frame_number); // UTF-8 coded number (1 byte for <=0x7F)
    let c = crc8(&bytes);
    bytes.push(c);
    // Frame body: 32 zero bytes. No 0xFF so no false sync.
    bytes.extend_from_slice(&[0u8; 32]);
    (bytes, frame_number as u64 * 192)
}

/// Pack a single SEEKPOINT: sample_number (u64 BE) + offset (u64 BE)
/// + frame_samples (u16 BE).
fn seek_point(sample: u64, offset: u64, frame_samples: u16) -> Vec<u8> {
    let mut out = Vec::with_capacity(18);
    out.extend_from_slice(&sample.to_be_bytes());
    out.extend_from_slice(&offset.to_be_bytes());
    out.extend_from_slice(&frame_samples.to_be_bytes());
    out
}

#[test]
fn flac_seek_lands_on_expected_frame() {
    // 1. Build three frames with known first samples.
    let (f0, s0) = build_frame(0); // samples [0, 192)
    let (f1, s1) = build_frame(1); // samples [192, 384)
    let (f2, s2) = build_frame(2); // samples [384, 576)
    assert_eq!(s0, 0);
    assert_eq!(s1, 192);
    assert_eq!(s2, 384);

    let o1 = f0.len() as u64;
    let o2 = (f0.len() + f1.len()) as u64;

    // 2. Build a SEEKTABLE pointing at each frame.
    let mut seektable = Vec::new();
    seektable.extend_from_slice(&seek_point(s0, 0, 192));
    seektable.extend_from_slice(&seek_point(s1, o1, 192));
    seektable.extend_from_slice(&seek_point(s2, o2, 192));
    // Also include a placeholder — the parser should drop it.
    seektable.extend_from_slice(&seek_point(0xFFFF_FFFF_FFFF_FFFF, 0, 0));

    // 3. Assemble the file.
    let streaminfo = build_streaminfo();
    let mut file = Vec::new();
    file.extend_from_slice(b"fLaC");
    file.extend_from_slice(&block_header(false, 0, streaminfo.len()));
    file.extend_from_slice(&streaminfo);
    file.extend_from_slice(&block_header(true, 3, seektable.len()));
    file.extend_from_slice(&seektable);
    file.extend_from_slice(&f0);
    file.extend_from_slice(&f1);
    file.extend_from_slice(&f2);

    // 4. Open through the container registry.
    let mut reg = ContainerRegistry::new();
    oxideav_flac::register_containers(&mut reg);
    let cursor: Box<dyn oxideav_core::ReadSeek> = Box::new(Cursor::new(file));
    let mut demuxer = reg
        .open_demuxer("flac", cursor, &oxideav_core::NullCodecResolver)
        .expect("open flac");

    // 5. Seek to target_pts = 300 (inside frame 1 which covers [192, 384)).
    let landed = demuxer.seek_to(0, 300).expect("seek");
    assert_eq!(landed, 192, "expected to land on frame 1's first sample");
    assert!(landed <= 300, "landing pts must be <= target");

    // 6. Next packet should have pts == 192 (frame 1's first sample).
    let pkt = demuxer.next_packet().expect("packet after seek");
    assert_eq!(pkt.pts, Some(192));
    assert_eq!(pkt.duration, Some(192));

    // 7. Following packet should be frame 2 at sample 384 — strictly > target - epsilon.
    let pkt = demuxer.next_packet().expect("second packet after seek");
    assert_eq!(pkt.pts, Some(384));
}

#[test]
fn flac_seek_before_first_seekpoint_lands_on_frame_zero() {
    // Same synthetic file, but we seek to a negative-looking pts. This
    // should clamp to 0 and land on the very first frame.
    let (f0, _) = build_frame(0);
    let (f1, _) = build_frame(1);
    let (f2, _) = build_frame(2);

    // SEEKTABLE that intentionally skips sample 0 — first entry is s1.
    // That way a seek to anything below s1 exercises the "before first
    // seekpoint" fallback path.
    let o1 = f0.len() as u64;
    let mut seektable = Vec::new();
    seektable.extend_from_slice(&seek_point(192, o1, 192));

    let streaminfo = build_streaminfo();
    let mut file = Vec::new();
    file.extend_from_slice(b"fLaC");
    file.extend_from_slice(&block_header(false, 0, streaminfo.len()));
    file.extend_from_slice(&streaminfo);
    file.extend_from_slice(&block_header(true, 3, seektable.len()));
    file.extend_from_slice(&seektable);
    file.extend_from_slice(&f0);
    file.extend_from_slice(&f1);
    file.extend_from_slice(&f2);

    let mut reg = ContainerRegistry::new();
    oxideav_flac::register_containers(&mut reg);
    let cursor: Box<dyn oxideav_core::ReadSeek> = Box::new(Cursor::new(file));
    let mut demuxer = reg
        .open_demuxer("flac", cursor, &oxideav_core::NullCodecResolver)
        .expect("open flac");

    let landed = demuxer.seek_to(0, 0).expect("seek");
    assert_eq!(landed, 0);
    let pkt = demuxer.next_packet().expect("first packet");
    assert_eq!(pkt.pts, Some(0));
}

/// Assemble a synthetic seektable-less FLAC file: `fLaC` + STREAMINFO
/// (marked last) + the three known frames. No SEEKTABLE block, so a
/// `seek_to` must use the frame-scanning fallback (RFC 9639 §8.5).
fn build_seektableless_file() -> Vec<u8> {
    let (f0, _) = build_frame(0);
    let (f1, _) = build_frame(1);
    let (f2, _) = build_frame(2);
    let streaminfo = build_streaminfo();
    let mut file = Vec::new();
    file.extend_from_slice(b"fLaC");
    file.extend_from_slice(&block_header(true, 0, streaminfo.len()));
    file.extend_from_slice(&streaminfo);
    file.extend_from_slice(&f0);
    file.extend_from_slice(&f1);
    file.extend_from_slice(&f2);
    file
}

fn open_seektableless() -> Box<dyn oxideav_core::Demuxer> {
    let mut reg = ContainerRegistry::new();
    oxideav_flac::register_containers(&mut reg);
    let cursor: Box<dyn oxideav_core::ReadSeek> = Box::new(Cursor::new(build_seektableless_file()));
    reg.open_demuxer("flac", cursor, &oxideav_core::NullCodecResolver)
        .expect("open flac")
}

#[test]
fn flac_seek_without_seektable_scans_to_target_frame() {
    // No SEEKTABLE present: the demuxer scans frame headers to find the
    // last frame whose first sample is <= the target. Target 300 sits
    // inside frame 1 ([192, 384)), so we land on sample 192.
    let mut demuxer = open_seektableless();
    let landed = demuxer.seek_to(0, 300).expect("scanning seek");
    assert_eq!(landed, 192, "expected to land on frame 1's first sample");
    assert!(landed <= 300, "landing pts must be <= target");

    let pkt = demuxer.next_packet().expect("packet after seek");
    assert_eq!(pkt.pts, Some(192));
    assert_eq!(pkt.duration, Some(192));

    let pkt = demuxer.next_packet().expect("second packet after seek");
    assert_eq!(pkt.pts, Some(384));
}

#[test]
fn flac_seek_without_seektable_clamps_negative_to_frame_zero() {
    // A negative-looking target clamps to 0 and lands on the first frame.
    let mut demuxer = open_seektableless();
    let landed = demuxer.seek_to(0, -10).expect("scanning seek");
    assert_eq!(landed, 0);
    let pkt = demuxer.next_packet().expect("packet after seek");
    assert_eq!(pkt.pts, Some(0));
}

#[test]
fn flac_seek_without_seektable_past_end_lands_on_last_frame() {
    // A target beyond every frame's first sample exhausts the scan and
    // lands on the last decodable frame (sample 384) rather than failing.
    let mut demuxer = open_seektableless();
    let landed = demuxer.seek_to(0, 10_000).expect("scanning seek");
    assert_eq!(landed, 384, "expected to land on the final frame");
    let pkt = demuxer.next_packet().expect("packet after seek");
    assert_eq!(pkt.pts, Some(384));
}

/// A SEEKTABLE whose 64-bit offsets (RFC 9639 §8.5.1) carry the absolute
/// position past `u64::MAX` cannot address any byte of the stream. The
/// demuxer must neither overflow nor fail: it discards the unusable point
/// and locates the frame by scanning headers, exactly as it does for a
/// file with no SEEKTABLE.
#[test]
fn flac_seek_with_overflowing_seekpoint_offset_falls_back_to_scan() {
    let (f0, _) = build_frame(0);
    let (f1, _) = build_frame(1);
    let (f2, _) = build_frame(2);

    let mut seektable = Vec::new();
    seektable.extend_from_slice(&seek_point(0, u64::MAX, 192));
    seektable.extend_from_slice(&seek_point(192, u64::MAX - 1, 192));
    seektable.extend_from_slice(&seek_point(384, u64::MAX - 41, 192));

    let streaminfo = build_streaminfo();
    let mut file = Vec::new();
    file.extend_from_slice(b"fLaC");
    file.extend_from_slice(&block_header(false, 0, streaminfo.len()));
    file.extend_from_slice(&streaminfo);
    file.extend_from_slice(&block_header(true, 3, seektable.len()));
    file.extend_from_slice(&seektable);
    file.extend_from_slice(&f0);
    file.extend_from_slice(&f1);
    file.extend_from_slice(&f2);

    let mut reg = ContainerRegistry::new();
    oxideav_flac::register_containers(&mut reg);
    let cursor: Box<dyn oxideav_core::ReadSeek> = Box::new(Cursor::new(file));
    let mut demuxer = reg
        .open_demuxer("flac", cursor, &oxideav_core::NullCodecResolver)
        .expect("open flac");

    for (target, want) in [(0i64, 0i64), (300, 192), (384, 384), (i64::MAX, 384)] {
        let landed = demuxer.seek_to(0, target).expect("seek");
        assert_eq!(landed, want, "target {target}");
        let pkt = demuxer.next_packet().expect("packet after seek");
        assert_eq!(pkt.pts, Some(want), "target {target}");
    }
}

/// A seek point whose offset is representable but lies past the end of
/// the file does not address a frame: it is left aside and the frame is
/// found by reading, as in a file without a SEEKTABLE.
#[test]
fn flac_seek_point_past_the_end_of_the_file_is_not_followed() {
    let (f0, _) = build_frame(0);
    let (f1, _) = build_frame(1);
    let o1 = f0.len() as u64;

    let mut seektable = Vec::new();
    seektable.extend_from_slice(&seek_point(0, 0, 192));
    seektable.extend_from_slice(&seek_point(192, 1 << 40, 192));

    let streaminfo = build_streaminfo();
    let mut file = Vec::new();
    file.extend_from_slice(b"fLaC");
    file.extend_from_slice(&block_header(false, 0, streaminfo.len()));
    file.extend_from_slice(&streaminfo);
    file.extend_from_slice(&block_header(true, 3, seektable.len()));
    file.extend_from_slice(&seektable);
    file.extend_from_slice(&f0);
    file.extend_from_slice(&f1);
    assert!(o1 > 0);

    let mut reg = ContainerRegistry::new();
    oxideav_flac::register_containers(&mut reg);
    let cursor: Box<dyn oxideav_core::ReadSeek> = Box::new(Cursor::new(file));
    let mut demuxer = reg
        .open_demuxer("flac", cursor, &oxideav_core::NullCodecResolver)
        .expect("open flac");

    assert_eq!(demuxer.seek_to(0, 200).expect("seek"), 192);
    assert_eq!(demuxer.next_packet().expect("packet").pts, Some(192));
    assert_eq!(demuxer.seek_to(0, 10).expect("seek"), 0);
    assert_eq!(demuxer.next_packet().expect("packet").pts, Some(0));
}

// --- Seeking in encoded streams ---------------------------------------------

mod common;

/// A reader that counts the bytes read through it.
struct Counting {
    inner: Cursor<Vec<u8>>,
    read: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

impl std::io::Read for Counting {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.read
            .fetch_add(n as u64, std::sync::atomic::Ordering::Relaxed);
        Ok(n)
    }
}

impl std::io::Seek for Counting {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.inner.seek(pos)
    }
}

/// A seek lands on the frame that holds the target sample — not on the
/// seek point before it — and gets there by reading a small part of the
/// file, whether the file has a dense SEEKTABLE, a sparse one, or none.
#[test]
fn seek_is_frame_accurate_and_reads_little_with_or_without_a_seektable() {
    use common::*;
    use oxideav_flac::container::FlacMuxerOptions;
    use oxideav_flac::encoder::FlacEncoderOptions;

    const BLOCK: usize = 1024;
    let planes = signal(Signal::Noise, 16, 1, 700_000, 11);
    let pcm = pack(&planes, 16);
    let options = FlacEncoderOptions::level(0).with_block_size(BLOCK as u32);
    let (extradata, packets) = encode_packets(&planes, 16, 44_100, options, 65_536).unwrap();

    for (label, mux) in [
        ("no seek table", FlacMuxerOptions::no_seektable()),
        ("a point per second", FlacMuxerOptions::default()),
        (
            "two points",
            FlacMuxerOptions::default().with_max_seek_points(2),
        ),
    ] {
        let file = mux_file(&extradata, 1, 16, 44_100, &packets, mux).unwrap();
        let size = file.len() as u64;
        assert!(size > 1_000_000, "the file is large enough to matter");
        let read = std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0));
        let input: Box<dyn oxideav_core::ReadSeek> = Box::new(Counting {
            inner: Cursor::new(file),
            read: read.clone(),
        });
        let mut reg = ContainerRegistry::new();
        oxideav_flac::register_containers(&mut reg);
        let mut demux = reg
            .open_demuxer("flac", input, &oxideav_core::NullCodecResolver)
            .unwrap();
        let mut dec = oxideav_flac::decoder::make_decoder(&demux.streams()[0].params).unwrap();

        for target in [699_999usize, 0, 350_000, 1023, 1024, 612_345, 87, 512_000] {
            let before = read.load(std::sync::atomic::Ordering::Relaxed);
            let landed = demux.seek_to(0, target as i64).unwrap() as usize;
            let cost = read.load(std::sync::atomic::Ordering::Relaxed) - before;
            assert_eq!(landed, target / BLOCK * BLOCK, "{label}: target {target}");
            assert!(
                cost < size / 4,
                "{label}: seeking to {target} read {cost} of {size} bytes"
            );
            let pkt = demux.next_packet().unwrap();
            assert_eq!(pkt.pts, Some(landed as i64), "{label}: target {target}");
            dec.send_packet(&pkt).unwrap();
            let oxideav_core::Frame::Audio(frame) = dec.receive_frame().unwrap() else {
                panic!("audio expected");
            };
            assert_eq!(
                frame.data[0],
                pcm[landed * 2..(landed + frame.samples as usize) * 2],
                "{label}: target {target}"
            );
        }
    }
}
