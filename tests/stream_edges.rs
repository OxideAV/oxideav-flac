//! Whole-stream edge cases: the demuxer's framing and resynchronisation,
//! variable block size streams, short last blocks, and the encoder's
//! output across block sizes, depths and levels — all through the public
//! container + codec surface.

use std::io::Cursor;

use oxideav_core::{
    AudioFrame, CodecId, CodecParameters, ContainerRegistry, Demuxer, Error, Frame,
    NullCodecResolver, Packet, ReadSeek, SampleFormat,
};
use oxideav_flac::crc::{crc8, crc16};
use oxideav_flac::encoder::{FlacEncoderOptions, make_encoder_with_options};

// --- Helpers ----------------------------------------------------------------

/// STREAMINFO metadata block (header + 34-byte payload), marked last.
fn streaminfo_block(
    min_block: u16,
    max_block: u16,
    rate: u32,
    channels: u8,
    bits: u8,
    total: u64,
) -> Vec<u8> {
    let mut block = vec![0x80, 0, 0, 34];
    block.extend_from_slice(&min_block.to_be_bytes());
    block.extend_from_slice(&max_block.to_be_bytes());
    block.extend_from_slice(&[0; 6]); // frame sizes unknown
    let packed: u64 =
        (rate as u64) << 44 | ((channels - 1) as u64) << 41 | ((bits - 1) as u64) << 36 | total;
    block.extend_from_slice(&packed.to_be_bytes());
    block.extend_from_slice(&[0; 16]); // MD5 not computed
    block
}

/// The coded number of RFC 9639 §9.1.5.
fn coded_number(v: u64) -> Vec<u8> {
    if v < 0x80 {
        return vec![v as u8];
    }
    let len = (2..=7usize)
        .find(|&len| v < 1u64 << ((7 - len) + 6 * (len - 1)))
        .unwrap();
    let mut out = vec![0u8; len];
    let mut rest = v;
    for byte in out.iter_mut().skip(1).rev() {
        *byte = 0x80 | (rest & 0x3F) as u8;
        rest >>= 6;
    }
    out[0] = (0xFFu16 << (8 - len)) as u8 | rest as u8;
    out
}

/// A complete frame of verbatim subframes holding whole-byte samples
/// (8 or 16 bits), one plane per channel. The subframe header is one
/// byte, so the samples stay byte-aligned.
fn verbatim_frame(variable: bool, number: u64, bits: u8, planes: &[Vec<i32>]) -> Vec<u8> {
    let block = planes[0].len();
    let depth_code = match bits {
        8 => 1u8,
        16 => 4,
        _ => unreachable!(),
    };
    let mut f = vec![
        0xFF,
        0xF8 | variable as u8,
        0x79, // block size in 16 bits, 44.1 kHz
        (planes.len() as u8 - 1) << 4 | depth_code << 1,
    ];
    f.extend_from_slice(&coded_number(number));
    f.extend_from_slice(&((block - 1) as u16).to_be_bytes());
    f.push(crc8(&f));
    for plane in planes {
        f.push(0b0000_0010); // verbatim, no wasted bits
        for &s in plane {
            match bits {
                8 => f.push(s as u8),
                _ => f.extend_from_slice(&(s as i16).to_be_bytes()),
            }
        }
    }
    let c = crc16(&f);
    f.extend_from_slice(&c.to_be_bytes());
    f
}

fn open(file: Vec<u8>) -> Box<dyn Demuxer> {
    let mut reg = ContainerRegistry::new();
    oxideav_flac::register_containers(&mut reg);
    let cursor: Box<dyn ReadSeek> = Box::new(Cursor::new(file));
    reg.open_demuxer("flac", cursor, &NullCodecResolver)
        .expect("open flac")
}

fn packets(demux: &mut Box<dyn Demuxer>) -> Vec<Packet> {
    let mut out = Vec::new();
    loop {
        match demux.next_packet() {
            Ok(p) => out.push(p),
            Err(Error::Eof) => return out,
            Err(e) => panic!("demux error: {e:?}"),
        }
    }
}

fn decode_packet(dec: &mut Box<dyn oxideav_core::Decoder>, p: &Packet) -> Result<Vec<u8>, Error> {
    dec.send_packet(p)?;
    match dec.receive_frame()? {
        Frame::Audio(a) => Ok(a.data.into_iter().next().unwrap()),
        _ => panic!("expected audio"),
    }
}

fn noise(count: usize, bits: u32, mut seed: u64) -> Vec<i32> {
    (0..count)
        .map(|_| {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            ((seed as i64) >> (64 - bits)) as i32
        })
        .collect()
}

/// Little-endian interleaved PCM as the decoder emits it for 16 bits.
fn s16le(planes: &[Vec<i32>]) -> Vec<u8> {
    let mut out = Vec::new();
    for i in 0..planes[0].len() {
        for plane in planes {
            out.extend_from_slice(&(plane[i] as i16).to_le_bytes());
        }
    }
    out
}

// --- Framing ----------------------------------------------------------------

/// A frame whose audio bytes contain complete frame headers with valid
/// CRC-8s is still one frame: the end of a frame is where its CRC-16 is.
#[test]
fn frame_headers_inside_the_audio_do_not_split_a_frame() {
    // An 8-bit mono verbatim frame stores its samples as the raw bytes,
    // so the audio can spell out anything — here, two frame headers.
    let mut fake = vec![0xFFu8, 0xF8, 0x19, 0x02, 0x05];
    fake.push(crc8(&fake));
    let mut fake2 = vec![0xFFu8, 0xF9, 0xC9, 0x18, 0x00];
    fake2.push(crc8(&fake2));
    let mut audio: Vec<u8> = (0..40u8).map(|i| i.wrapping_mul(3)).collect();
    audio.extend_from_slice(&fake);
    audio.extend((0..25u8).map(|i| 0x40 ^ i));
    audio.extend_from_slice(&fake2);
    audio.extend((0..30u8).map(|i| i.wrapping_mul(7)));
    assert!(oxideav_flac::frame::parse_frame_header(&fake).is_ok());
    assert!(oxideav_flac::frame::parse_frame_header(&fake2).is_ok());

    let block = audio.len();
    let planes: Vec<Vec<i32>> = (0..3)
        .map(|f| {
            audio
                .iter()
                .map(|&b| (b as i8 as i32).wrapping_add(f) as i8 as i32)
                .collect()
        })
        .collect();
    // Frame 1 is the one whose samples are exactly `audio`.
    let frames: Vec<Vec<u8>> = [1, 0, 2]
        .iter()
        .enumerate()
        .map(|(n, &p)| verbatim_frame(false, n as u64, 8, &[planes[p].clone()]))
        .collect();
    let mut file = b"fLaC".to_vec();
    file.extend_from_slice(&streaminfo_block(
        block as u16,
        block as u16,
        44_100,
        1,
        8,
        3 * block as u64,
    ));
    for f in &frames {
        file.extend_from_slice(f);
    }

    let mut demux = open(file);
    let mut dec = oxideav_flac::decoder::make_decoder(&demux.streams()[0].params).unwrap();
    let pkts = packets(&mut demux);
    assert_eq!(pkts.len(), 3);
    for (n, (pkt, frame)) in pkts.iter().zip(&frames).enumerate() {
        assert_eq!(&pkt.data, frame, "packet {n}");
        assert_eq!(pkt.pts, Some((n * block) as i64));
        let pcm = decode_packet(&mut dec, pkt).unwrap();
        let want: Vec<u8> = planes[[1, 0, 2][n]]
            .iter()
            .map(|&s| (s as u8).wrapping_add(128))
            .collect();
        assert_eq!(pcm, want, "packet {n}");
    }
}

/// A damaged frame costs that frame only: the demuxer still cuts it at
/// the next frame header, the decoder reports the CRC failure, and every
/// other frame decodes.
#[test]
fn corruption_is_confined_to_the_frame_it_hits() {
    let block = 64usize;
    let planes: Vec<[Vec<i32>; 2]> = (0..8u64)
        .map(|f| [noise(block, 16, f * 2), noise(block, 16, f * 2 + 1)])
        .collect();
    let frames: Vec<Vec<u8>> = planes
        .iter()
        .enumerate()
        .map(|(n, p)| verbatim_frame(false, n as u64, 16, p))
        .collect();
    let head = {
        let mut h = b"fLaC".to_vec();
        h.extend_from_slice(&streaminfo_block(64, 64, 44_100, 2, 16, 8 * 64));
        h
    };

    // (frame, byte offset inside it, what the damage is)
    let cases: [(usize, usize, &str); 5] = [
        (3, 40, "audio byte"),
        (3, frames[3].len() - 1, "CRC-16 byte"),
        (0, 20, "audio byte of the first frame"),
        (7, 30, "audio byte of the last frame"),
        (4, 2, "header byte"),
    ];
    for (hit, offset, what) in cases {
        let mut file = head.clone();
        for (n, f) in frames.iter().enumerate() {
            let mut f = f.clone();
            if n == hit {
                f[offset] ^= 0x04;
            }
            file.extend_from_slice(&f);
        }
        let mut demux = open(file);
        let mut dec = oxideav_flac::decoder::make_decoder(&demux.streams()[0].params).unwrap();
        let mut good = [false; 8];
        let mut failures = 0;
        for pkt in packets(&mut demux) {
            match decode_packet(&mut dec, &pkt) {
                Ok(pcm) => {
                    let n = (pkt.pts.unwrap() / block as i64) as usize;
                    assert_eq!(pcm, s16le(&planes[n]), "{what}: frame {n}");
                    good[n] = true;
                }
                Err(_) => failures += 1,
            }
        }
        // A broken header takes the frame out of the packet sequence (its
        // bytes ride along with the previous frame, whose CRC-16 then
        // cannot be confirmed but whose audio is intact); broken audio or
        // a broken CRC-16 is one failed packet.
        let lost: Vec<usize> = (0..8).filter(|&n| !good[n]).collect();
        assert_eq!(lost, vec![hit], "{what}");
        assert!(failures <= 1, "{what}: {failures} failed packets");
    }
}

/// Bytes that are not frames — before the first frame, between frames,
/// after the last — are skipped, and the frames around them survive.
#[test]
fn junk_between_frames_is_skipped() {
    let block = 32usize;
    let planes: Vec<Vec<i32>> = (0..4u64).map(|f| noise(block, 16, 40 + f)).collect();
    let frames: Vec<Vec<u8>> = planes
        .iter()
        .enumerate()
        .map(|(n, p)| verbatim_frame(false, n as u64, 16, std::slice::from_ref(p)))
        .collect();
    let mut file = b"fLaC".to_vec();
    file.extend_from_slice(&streaminfo_block(32, 32, 44_100, 1, 16, 4 * 32));
    file.extend_from_slice(&[0x00, 0xFF, 0xF8, 0x00, 0x13, 0x37]);
    file.extend_from_slice(&frames[0]);
    file.extend_from_slice(&frames[1]);
    file.extend_from_slice(&[0xFF; 300]);
    file.extend_from_slice(&frames[2]);
    file.extend_from_slice(&frames[3]);
    file.extend_from_slice(b"TAG and other trailing bytes");

    let mut demux = open(file);
    let mut dec = oxideav_flac::decoder::make_decoder(&demux.streams()[0].params).unwrap();
    let mut seen = Vec::new();
    for pkt in packets(&mut demux) {
        if let Ok(pcm) = decode_packet(&mut dec, &pkt) {
            let n = (pkt.pts.unwrap() / block as i64) as usize;
            assert_eq!(pcm, s16le(std::slice::from_ref(&planes[n])));
            seen.push(n);
        }
    }
    assert_eq!(seen, vec![0, 1, 2, 3]);
}

// --- Variable block size ----------------------------------------------------

/// In a variable block size stream the coded number is the number of the
/// frame's first sample (§9.1.5) and the blocks differ in length; only
/// the last may be shorter than 16 samples.
#[test]
fn variable_block_size_stream() {
    let sizes = [16usize, 4096, 17, 255, 256, 1000, 65_535, 16, 33, 5];
    let mut first = 0u64;
    let mut frames = Vec::new();
    let mut planes = Vec::new();
    for (n, &size) in sizes.iter().enumerate() {
        let p = [noise(size, 16, n as u64), noise(size, 16, 100 + n as u64)];
        frames.push((first, verbatim_frame(true, first, 16, &p)));
        planes.push(p);
        first += size as u64;
    }
    let total = first;
    let mut file = b"fLaC".to_vec();
    file.extend_from_slice(&streaminfo_block(16, 65_535, 44_100, 2, 16, total));
    for (_, f) in &frames {
        file.extend_from_slice(f);
    }

    let mut demux = open(file.clone());
    let mut dec = oxideav_flac::decoder::make_decoder(&demux.streams()[0].params).unwrap();
    let pkts = packets(&mut demux);
    assert_eq!(pkts.len(), sizes.len());
    for (n, pkt) in pkts.iter().enumerate() {
        assert_eq!(pkt.pts, Some(frames[n].0 as i64), "frame {n}");
        assert_eq!(pkt.duration, Some(sizes[n] as i64), "frame {n}");
        assert_eq!(pkt.data, frames[n].1, "frame {n}");
        assert_eq!(decode_packet(&mut dec, pkt).unwrap(), s16le(&planes[n]));
    }

    // Seeking without a SEEKTABLE lands on the frame that holds the
    // target sample.
    let mut demux = open(file);
    for (n, &(start, _)) in frames.iter().enumerate() {
        for target in [start, start + sizes[n] as u64 - 1] {
            assert_eq!(demux.seek_to(0, target as i64).unwrap(), start as i64);
            let pkt = demux.next_packet().unwrap();
            assert_eq!(pkt.pts, Some(start as i64));
            assert_eq!(pkt.duration, Some(sizes[n] as i64));
        }
    }
    assert_eq!(
        demux.seek_to(0, i64::MAX).unwrap(),
        frames.last().unwrap().0 as i64
    );
}

/// Sample numbers up to the 36-bit limit of the coded number survive the
/// trip through the demuxer as timestamps.
#[test]
fn sample_numbers_up_to_36_bits() {
    let starts = [0u64, 0x7FFF_FFF0, 0x8000_0000, 0xF_FFFF_FF00];
    let mut file = b"fLaC".to_vec();
    file.extend_from_slice(&streaminfo_block(16, 4096, 44_100, 1, 16, 0));
    let plane = noise(16, 16, 9);
    for &start in &starts {
        file.extend_from_slice(&verbatim_frame(
            true,
            start,
            16,
            std::slice::from_ref(&plane),
        ));
    }
    let mut demux = open(file);
    let pts: Vec<i64> = packets(&mut demux).iter().map(|p| p.pts.unwrap()).collect();
    assert_eq!(pts, starts.map(|s| s as i64));
}

// --- Stream parameters ------------------------------------------------------

/// The decoder's output layout is fixed by STREAMINFO; a frame whose
/// header announces another channel count or bit depth is refused
/// (RFC 9639 §9: "a decoder MAY choose to stop decoding on such a
/// change"), and decoding continues with the next conforming frame.
#[test]
fn frames_that_contradict_streaminfo_are_refused() {
    let stereo = [noise(32, 16, 1), noise(32, 16, 2)];
    let mono = [noise(32, 16, 3)];
    let narrow = [noise(32, 8, 4), noise(32, 8, 5)];
    let mut file = b"fLaC".to_vec();
    file.extend_from_slice(&streaminfo_block(32, 32, 44_100, 2, 16, 0));
    file.extend_from_slice(&verbatim_frame(false, 0, 16, &stereo));
    file.extend_from_slice(&verbatim_frame(false, 1, 16, &mono));
    file.extend_from_slice(&verbatim_frame(false, 2, 8, &narrow));
    file.extend_from_slice(&verbatim_frame(false, 3, 16, &stereo));

    let mut demux = open(file);
    let mut dec = oxideav_flac::decoder::make_decoder(&demux.streams()[0].params).unwrap();
    let pkts = packets(&mut demux);
    assert_eq!(pkts.len(), 4);
    assert_eq!(decode_packet(&mut dec, &pkts[0]).unwrap(), s16le(&stereo));
    assert!(decode_packet(&mut dec, &pkts[1]).is_err());
    assert!(decode_packet(&mut dec, &pkts[2]).is_err());
    assert_eq!(decode_packet(&mut dec, &pkts[3]).unwrap(), s16le(&stereo));
}

// --- Encoder output ---------------------------------------------------------

struct Encoded {
    packets: Vec<Packet>,
    params: CodecParameters,
}

fn encode(
    pcm: &[u8],
    format: SampleFormat,
    channels: u16,
    rate: u32,
    options: FlacEncoderOptions,
) -> Encoded {
    let mut params = CodecParameters::audio(CodecId::new("flac"));
    params.channels = Some(channels);
    params.sample_rate = Some(rate);
    params.sample_format = Some(format);
    let mut enc = make_encoder_with_options(&params, options).expect("encoder");
    let frame_bytes = format.bytes_per_sample() * channels as usize;
    if !pcm.is_empty() {
        enc.send_frame(&Frame::Audio(AudioFrame {
            samples: (pcm.len() / frame_bytes) as u32,
            pts: Some(0),
            data: vec![pcm.to_vec()],
        }))
        .expect("send_frame");
    }
    enc.flush().expect("flush");
    let mut packets = Vec::new();
    loop {
        match enc.receive_packet() {
            Ok(p) => packets.push(p),
            Err(Error::NeedMore) | Err(Error::Eof) => break,
            Err(e) => panic!("receive_packet: {e:?}"),
        }
    }
    Encoded {
        packets,
        params: enc.output_params().clone(),
    }
}

/// Decode `encoded` twice — packet by packet, and as a muxed file through
/// the demuxer — and return the PCM, which must be the same both ways.
fn decode_both_ways(encoded: &Encoded) -> Vec<u8> {
    let mut dec = oxideav_flac::decoder::make_decoder(&encoded.params).unwrap();
    let mut direct = Vec::new();
    // The strict decoder refuses everything RFC 9639 forbids, including
    // what the default decoder tolerates.
    let mut strict = oxideav_flac::decoder::FrameScratch::strict();
    let info = oxideav_flac::metadata::StreamInfo::parse(&encoded.params.extradata[4..38]).unwrap();
    for p in &encoded.packets {
        direct.extend_from_slice(&decode_packet(&mut dec, p).expect("own frame decodes"));
        let shape = strict
            .decode(&p.data, &info)
            .expect("own frame is strictly valid");
        assert_eq!(shape.frame_byte_len, p.data.len());
    }

    let mut file = b"fLaC".to_vec();
    file.extend_from_slice(&encoded.params.extradata);
    for p in &encoded.packets {
        file.extend_from_slice(&p.data);
    }
    // An outside decoder agrees that the stream is valid and that its
    // MD5 matches (skipped when none is installed).
    if !encoded.packets.is_empty() {
        if let Some(accepted) = black_box_accepts(&file) {
            assert!(accepted, "black-box decoder rejected the stream");
        }
    }
    let mut demux = open(file);
    let mut dec = oxideav_flac::decoder::make_decoder(&demux.streams()[0].params).unwrap();
    let mut via_file = Vec::new();
    let pkts = packets(&mut demux);
    assert_eq!(pkts.len(), encoded.packets.len(), "demuxed frame count");
    for (p, original) in pkts.iter().zip(&encoded.packets) {
        assert_eq!(p.data, original.data, "demuxed frame bytes");
        via_file.extend_from_slice(&decode_packet(&mut dec, p).expect("demuxed frame decodes"));
    }
    assert_eq!(direct, via_file);
    direct
}

/// Hand `file` to a black-box FLAC decoder in test mode, which decodes
/// the stream, checks every frame and compares the STREAMINFO MD5.
/// `None` when no such binary is installed (the check is then skipped).
fn black_box_accepts(file: &[u8]) -> Option<bool> {
    use std::process::Command;
    use std::sync::atomic::{AtomicUsize, Ordering};
    static SERIAL: AtomicUsize = AtomicUsize::new(0);
    let probe = Command::new("flac").arg("--version").output().ok()?;
    if !probe.status.success() {
        return None;
    }
    let path = std::env::temp_dir().join(format!(
        "oxideav-flac-stream-edges-{}-{}.flac",
        std::process::id(),
        SERIAL.fetch_add(1, Ordering::Relaxed)
    ));
    std::fs::write(&path, file).ok()?;
    let status = Command::new("flac")
        .args(["--test", "--silent", "--warnings-as-errors"])
        .arg(&path)
        .output();
    let _ = std::fs::remove_file(&path);
    Some(status.ok()?.status.success())
}

/// PCM of `frames` interleaved samples: a few tones, noise, and runs at
/// the extremes of the sample range.
fn test_pcm(format: SampleFormat, channels: u16, frames: usize, seed: u64) -> Vec<u8> {
    let bytes = format.bytes_per_sample();
    let bits = 8 * bytes as u32;
    let hi = (1i64 << (bits - 1)) - 1;
    let lo = -(1i64 << (bits - 1));
    let rnd = noise(frames * channels as usize, 32, seed);
    let mut out = Vec::with_capacity(frames * channels as usize * bytes);
    for i in 0..frames {
        for c in 0..channels as usize {
            let r = rnd[i * channels as usize + c] as i64;
            let phase = (i / 37 + c) % 6;
            let v: i64 = match phase {
                0 => hi,
                1 => lo,
                2 => {
                    if (i + c) % 2 == 0 {
                        hi
                    } else {
                        lo
                    }
                }
                3 => r >> (32 - bits),
                4 => ((i as i64 * 911) % 2001 - 1000) * (hi / 1200) + (r >> 28),
                _ => (r >> 30) << (bits / 2),
            };
            let v = v.clamp(lo, hi);
            match format {
                // Unsigned 8-bit PCM is the signed value plus 128.
                SampleFormat::U8 => out.push((v + 128) as u8),
                _ => out.extend_from_slice(&v.to_le_bytes()[..bytes]),
            }
        }
    }
    out
}

/// Every stream the encoder produces is accepted by this crate's decoder,
/// frame by frame and re-framed by the demuxer, and decodes to the input:
/// block sizes from 16 up, stream lengths that leave a last block of
/// every size from 1 sample, four depths, mono and stereo, levels 0, 5
/// and 8, with the encoder's own verification on.
#[test]
fn encoder_output_decodes_for_every_shape() {
    let formats = [
        SampleFormat::U8,
        SampleFormat::S16,
        SampleFormat::S24,
        SampleFormat::S32,
    ];
    // (block size, total frames)
    let shapes: Vec<(u32, usize)> = (1..=40usize)
        .map(|n| (16u32, n))
        .chain((0..=33usize).map(|extra| (32, 64 + extra)))
        .chain([
            (16, 0),
            (17, 100),
            (192, 500),
            (255, 511),
            (256, 257),
            (1000, 2001),
            (4096, 4097),
            (4096, 8192 + 15),
            (4608, 4608 * 2 + 1),
        ])
        .collect();
    let mut combos = 0;
    for (index, &(block, frames)) in shapes.iter().enumerate() {
        let format = formats[index % formats.len()];
        let channels = 1 + (index / formats.len()) as u16 % 2;
        let level = [0u8, 5, 8][index % 3];
        let pcm = test_pcm(format, channels, frames, index as u64);
        let options = FlacEncoderOptions::level(level)
            .with_block_size(block)
            .with_verify(true);
        let encoded = encode(&pcm, format, channels, 44_100, options);
        assert_eq!(encoded.packets.len(), frames.div_ceil(block as usize));
        let decoded = decode_both_ways(&encoded);
        assert!(
            decoded == pcm,
            "block {block}, {frames} frames, {format:?}, {channels} ch, level {level}"
        );
        combos += 1;
    }
    assert_eq!(combos, shapes.len());
}

/// The same for every depth × channel layout × level on one awkward
/// length, so no combination is left to the rotation above.
#[test]
fn encoder_output_decodes_for_every_depth_layout_and_level() {
    for format in [
        SampleFormat::U8,
        SampleFormat::S16,
        SampleFormat::S24,
        SampleFormat::S32,
    ] {
        for channels in [1u16, 2, 3, 8] {
            for level in 0..=8u8 {
                let frames = 2 * 576 + 7;
                let pcm = test_pcm(
                    format,
                    channels,
                    frames,
                    level as u64 * 31 + channels as u64,
                );
                let options = FlacEncoderOptions::level(level).with_block_size(576);
                let encoded = encode(&pcm, format, channels, 48_000, options);
                assert!(
                    decode_both_ways(&encoded) == pcm,
                    "{format:?}, {channels} ch, level {level}"
                );
            }
        }
    }
}

/// Sample rates that the frame header can only express through one of its
/// "stored at the end of the header" forms, or not at all (§9.1.2,
/// §9.1.7): the stream must still decode, and re-frame, at each.
#[test]
fn encoder_output_decodes_at_uncommon_sample_rates() {
    for rate in [
        1u32, 7, 1000, 11_025, 12_000, 37_800, 44_099, 65_535, 65_536, 65_540, 100_000, 255_000,
        256_000, 384_000, 655_350, 655_351, 1_048_575,
    ] {
        let pcm = test_pcm(SampleFormat::S16, 2, 700, rate as u64);
        let options = FlacEncoderOptions::new()
            .with_block_size(256)
            .with_allow_non_subset_block_size(true);
        let encoded = encode(&pcm, SampleFormat::S16, 2, rate, options);
        assert!(decode_both_ways(&encoded) == pcm, "{rate} Hz");
        // Whatever the header says about the rate agrees with STREAMINFO
        // or defers to it.
        for p in &encoded.packets {
            let header = oxideav_flac::frame::parse_frame_header(&p.data).unwrap();
            assert!(
                header.sample_rate == rate || header.sample_rate == 0,
                "{rate} Hz coded as {}",
                header.sample_rate
            );
        }
    }
}
