//! Shared helpers for the black-box matrices: deterministic signals,
//! WAVE writing / reading, this crate's decode and encode paths, and
//! the outside tools run as opaque processes.
//!
//! Sample planes are `Vec<Vec<i32>>`, one vector per channel, holding
//! values in the stream's own bit depth (a 12-bit plane holds
//! `-2048..=2047`). On the PCM side every depth is carried
//! left-justified in whole bytes, the way a WAVE file and
//! `oxideav_core::SampleFormat` both do it.

#![allow(dead_code)]

use std::io::Cursor;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicU64, Ordering};

use oxideav_core::{
    AudioFrame, CodecId, CodecParameters, ContainerRegistry, Demuxer, Error, Frame,
    NullCodecResolver, ReadSeek, SampleFormat,
};
use oxideav_flac::encoder::FlacEncoderOptions;

/// True when `tool` starts and exits successfully with `arg`.
pub fn have(tool: &str, arg: &str) -> bool {
    matches!(Command::new(tool).arg(arg).output(), Ok(o) if o.status.success())
}

/// The black-box `flac` tool, release 1.4 or later (earlier ones do not
/// take 32-bit samples, which the matrices include).
pub fn have_flac() -> bool {
    let Ok(probe) = Command::new("flac").arg("--version").output() else {
        return false;
    };
    let banner = String::from_utf8_lossy(&probe.stdout).into_owned();
    let mut version = banner
        .split_whitespace()
        .nth(1)
        .unwrap_or("")
        .split('.')
        .map(|part| part.parse::<u32>().unwrap_or(0));
    probe.status.success() && (version.next(), version.next()) >= (Some(1), Some(4))
}

pub fn have_metaflac() -> bool {
    have("metaflac", "--version")
}

pub fn have_ffmpeg() -> bool {
    have("ffmpeg", "-version")
}

pub fn have_sox() -> bool {
    have("sox", "--version")
}

/// A scratch directory that is removed when dropped.
pub struct Scratch {
    dir: PathBuf,
}

impl Scratch {
    pub fn new(tag: &str) -> Self {
        static NEXT: AtomicU64 = AtomicU64::new(0);
        let n = NEXT.fetch_add(1, Ordering::Relaxed);
        let mut dir = std::env::temp_dir();
        dir.push(format!("oxideav-flac-{tag}-{}-{n}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        Scratch { dir }
    }

    pub fn path(&self, name: &str) -> PathBuf {
        self.dir.join(name)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// --- signals ----------------------------------------------------------

pub struct Rng(pub u64);

impl Rng {
    pub fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    /// Uniform in `lo..=hi`.
    pub fn range(&mut self, lo: i64, hi: i64) -> i64 {
        let span = (hi - lo + 1) as u64;
        lo + (self.next() % span) as i64
    }
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Signal {
    /// Two sines plus a little noise: what a linear predictor likes.
    Tone,
    /// Uniform noise over the whole range: forces verbatim subframes.
    Noise,
    /// All zero.
    Silence,
    /// One non-zero value throughout.
    Dc,
    /// Alternating runs at the two extremes of the range.
    Extremes,
    /// A tone whose low `n` bits are zero.
    Wasted(u32),
    /// Tone on the left, the same tone on the right.
    Same,
    /// Tone on the left, its negation on the right.
    Inverted,
    /// Tone on the left, silence on the right.
    LeftOnly,
    /// Silence on the left, tone on the right.
    RightOnly,
    /// Full-scale opposite-sign noise bursts: the side channel uses its
    /// extra bit.
    WideSide,
    /// Quiet noise with rare full-scale clicks: residuals whose
    /// partitions differ wildly.
    Clicks,
    /// A slow ramp: fixed predictors of low order code it exactly.
    Ramp,
}

pub fn limits(bps: u32) -> (i64, i64) {
    (-(1i64 << (bps - 1)), (1i64 << (bps - 1)) - 1)
}

/// `channels` planes of `n` samples at `bps` bits.
pub fn signal(kind: Signal, bps: u32, channels: usize, n: usize, seed: u64) -> Vec<Vec<i32>> {
    let (lo, hi) = limits(bps);
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15 ^ seed.wrapping_mul(0xD134_2543_DE82_EF95) | 1);
    let amp = hi as f64 * 0.7;
    let tone = |i: usize, c: usize, rng: &mut Rng| -> i64 {
        let t = i as f64;
        let v = amp * 0.6 * (t * 0.031 * (c as f64 * 0.37 + 1.0)).sin()
            + amp * 0.3 * (t * 0.0047 * (c as f64 + 1.0)).sin();
        let dither = if bps > 6 { rng.range(-2, 2) } else { 0 };
        (v as i64 + dither).clamp(lo, hi)
    };
    let mut planes = vec![Vec::with_capacity(n); channels];
    for i in 0..n {
        let base = tone(i, 0, &mut rng);
        for (c, plane) in planes.iter_mut().enumerate() {
            let v = match kind {
                Signal::Tone => {
                    if c == 0 {
                        base
                    } else {
                        tone(i, c, &mut rng)
                    }
                }
                Signal::Noise => rng.range(lo, hi),
                Signal::Silence => 0,
                Signal::Dc => (hi / 3).max(1) - c as i64,
                Signal::Extremes => {
                    if (i / 5 + c) % 2 == 0 {
                        hi
                    } else {
                        lo
                    }
                }
                Signal::Wasted(k) => {
                    let k = k.min(bps - 1);
                    (tone(i, c, &mut rng) >> k) << k
                }
                Signal::Same => base,
                Signal::Inverted => {
                    if c % 2 == 0 {
                        base
                    } else {
                        (-base).clamp(lo, hi)
                    }
                }
                Signal::LeftOnly => {
                    if c == 0 {
                        base
                    } else {
                        0
                    }
                }
                Signal::RightOnly => {
                    if c == 1 {
                        base
                    } else {
                        0
                    }
                }
                Signal::WideSide => {
                    let burst = (i / 64) % 3;
                    let mag = match burst {
                        0 => hi,
                        1 => rng.range(hi / 2, hi),
                        _ => rng.range(0, hi),
                    };
                    if c % 2 == 0 { mag } else { (-mag - 1).max(lo) }
                }
                Signal::Clicks => {
                    if rng.next().is_multiple_of(997) {
                        if rng.next() & 1 == 0 { hi } else { lo }
                    } else {
                        rng.range(-3, 3).clamp(lo, hi)
                    }
                }
                Signal::Ramp => {
                    let span = (hi - lo) as usize + 1;
                    lo + ((i * (c + 1)) % span) as i64
                }
            };
            plane.push(v as i32);
        }
    }
    planes
}

// --- PCM byte layouts ---------------------------------------------------

/// Bytes each sample occupies on the PCM side.
pub fn container_bytes(bps: u32) -> usize {
    bps.div_ceil(8) as usize
}

/// The sample format this crate uses for a stream depth.
pub fn format_for(bps: u32) -> SampleFormat {
    match bps {
        0..=8 => SampleFormat::U8,
        9..=16 => SampleFormat::S16,
        17..=24 => SampleFormat::S24,
        _ => SampleFormat::S32,
    }
}

/// Interleave planes as WAVE sample data: little-endian, left-justified
/// in whole bytes, one-byte samples unsigned with an offset of 128.
pub fn pack(planes: &[Vec<i32>], bps: u32) -> Vec<u8> {
    let width = container_bytes(bps);
    let shift = width as u32 * 8 - bps;
    let n = planes.first().map_or(0, Vec::len);
    let mut out = Vec::with_capacity(n * planes.len() * width);
    for i in 0..n {
        for plane in planes {
            let v = (plane[i] as i64) << shift;
            if width == 1 {
                out.push((v + 128) as u8);
            } else {
                out.extend_from_slice(&v.to_le_bytes()[..width]);
            }
        }
    }
    out
}

/// The inverse of [`pack`].
pub fn unpack(bytes: &[u8], bps: u32, channels: usize) -> Vec<Vec<i32>> {
    let width = container_bytes(bps);
    let shift = width as u32 * 8 - bps;
    let mut planes = vec![Vec::new(); channels];
    for (i, chunk) in bytes.chunks_exact(width).enumerate() {
        let v = if width == 1 {
            chunk[0] as i64 - 128
        } else {
            let mut raw = [0u8; 8];
            raw[..width].copy_from_slice(chunk);
            let unsigned = i64::from_le_bytes(raw);
            let bits = width as u32 * 8;
            (unsigned << (64 - bits)) >> (64 - bits)
        };
        planes[i % channels].push((v >> shift) as i32);
    }
    planes
}

/// A WAVE file image (extensible format, so any depth and channel count
/// is described exactly). `extra_chunks` go between `fmt ` and `data`.
pub fn wav_bytes(planes: &[Vec<i32>], bps: u32, rate: u32, extra_chunks: &[u8]) -> Vec<u8> {
    let channels = planes.len() as u16;
    let width = container_bytes(bps) as u16;
    let data = pack(planes, bps);
    let mut fmt = Vec::new();
    fmt.extend_from_slice(&0xFFFEu16.to_le_bytes());
    fmt.extend_from_slice(&channels.to_le_bytes());
    fmt.extend_from_slice(&rate.to_le_bytes());
    fmt.extend_from_slice(&(rate.wrapping_mul(channels as u32 * width as u32)).to_le_bytes());
    fmt.extend_from_slice(&(channels * width).to_le_bytes());
    fmt.extend_from_slice(&(width * 8).to_le_bytes());
    fmt.extend_from_slice(&22u16.to_le_bytes());
    fmt.extend_from_slice(&(bps as u16).to_le_bytes());
    fmt.extend_from_slice(&0u32.to_le_bytes()); // no speaker positions
    fmt.extend_from_slice(&[
        0x01, 0x00, 0x00, 0x00, 0x00, 0x00, 0x10, 0x00, 0x80, 0x00, 0x00, 0xAA, 0x00, 0x38, 0x9B,
        0x71,
    ]);
    let mut body = Vec::new();
    body.extend_from_slice(b"WAVEfmt ");
    body.extend_from_slice(&(fmt.len() as u32).to_le_bytes());
    body.extend_from_slice(&fmt);
    body.extend_from_slice(extra_chunks);
    body.extend_from_slice(b"data");
    body.extend_from_slice(&(data.len() as u32).to_le_bytes());
    body.extend_from_slice(&data);
    if data.len() % 2 == 1 {
        body.push(0);
    }
    let mut out = Vec::with_capacity(body.len() + 8);
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    out
}

/// The `data` chunk of a WAVE file image.
pub fn wav_data(file: &[u8]) -> Option<&[u8]> {
    if file.len() < 12 || &file[..4] != b"RIFF" || &file[8..12] != b"WAVE" {
        return None;
    }
    let mut at = 12;
    while at + 8 <= file.len() {
        let len = u32::from_le_bytes(file[at + 4..at + 8].try_into().unwrap()) as usize;
        let start = at + 8;
        let end = (start + len).min(file.len());
        if &file[at..at + 4] == b"data" {
            return Some(&file[start..end]);
        }
        at = start + len + (len & 1);
    }
    None
}

// --- this crate ---------------------------------------------------------

pub fn open_demuxer(file: Vec<u8>) -> Result<Box<dyn Demuxer>, String> {
    let mut reg = ContainerRegistry::new();
    oxideav_flac::register_containers(&mut reg);
    let cursor: Box<dyn ReadSeek> = Box::new(Cursor::new(file));
    reg.open_demuxer("flac", cursor, &NullCodecResolver)
        .map_err(|e| format!("open: {e}"))
}

/// What this crate made of a `.flac` file image.
pub struct Decoded {
    /// Interleaved PCM exactly as the decoder delivered it.
    pub pcm: Vec<u8>,
    pub format: SampleFormat,
    pub channels: usize,
    pub rate: u32,
    /// Presentation time of the first delivered packet.
    pub first_pts: Option<i64>,
    pub packets: usize,
}

/// Drain `demux` through a fresh decoder.
pub fn drain(demux: &mut dyn Demuxer) -> Result<Decoded, String> {
    let params = demux.streams()[0].params.clone();
    let format = params.sample_format.ok_or("no sample format")?;
    let mut dec = oxideav_flac::decoder::make_decoder(&params).map_err(|e| format!("dec: {e}"))?;
    let mut pcm = Vec::new();
    let mut first_pts = None;
    let mut packets = 0;
    loop {
        let pkt = match demux.next_packet() {
            Ok(p) => p,
            Err(Error::Eof) => break,
            Err(e) => return Err(format!("demux: {e}")),
        };
        if packets == 0 {
            first_pts = pkt.pts;
        }
        packets += 1;
        dec.send_packet(&pkt).map_err(|e| format!("send: {e}"))?;
        match dec.receive_frame() {
            Ok(Frame::Audio(a)) => pcm.extend_from_slice(&a.data[0]),
            Ok(_) => return Err("non-audio frame".into()),
            Err(e) => return Err(format!("decode (packet {packets}): {e}")),
        }
    }
    Ok(Decoded {
        pcm,
        format,
        channels: params.channels.unwrap_or(0) as usize,
        rate: params.sample_rate.unwrap_or(0),
        first_pts,
        packets,
    })
}

pub fn decode_file(file: &[u8]) -> Result<Decoded, String> {
    let mut demux = open_demuxer(file.to_vec())?;
    drain(demux.as_mut())
}

/// Encode planes with this crate and mux them into a `.flac` image.
/// `bps` is the stream depth; the PCM handed to the encoder is the
/// [`pack`] layout in [`format_for`]`(bps)`.
pub fn encode_file(
    planes: &[Vec<i32>],
    bps: u32,
    rate: u32,
    options: FlacEncoderOptions,
    chunk: usize,
) -> Result<Vec<u8>, String> {
    let (extradata, packets) = encode_packets(planes, bps, rate, options, chunk)?;
    let mut out = Vec::new();
    out.extend_from_slice(b"fLaC");
    out.extend_from_slice(&extradata);
    for p in &packets {
        out.extend_from_slice(&p.data);
    }
    Ok(out)
}

pub fn encoder_params(channels: usize, bps: u32, rate: u32) -> CodecParameters {
    let mut params = CodecParameters::audio(CodecId::new("flac"));
    params.channels = Some(channels as u16);
    params.sample_rate = Some(rate);
    params.sample_format = Some(format_for(bps));
    params
}

/// The finalised metadata chain and the frame packets.
pub fn encode_packets(
    planes: &[Vec<i32>],
    bps: u32,
    rate: u32,
    options: FlacEncoderOptions,
    chunk: usize,
) -> Result<(Vec<u8>, Vec<oxideav_core::Packet>), String> {
    let params = encoder_params(planes.len(), bps, rate);
    let mut enc = oxideav_flac::encoder::make_encoder_with_options(&params, options)
        .map_err(|e| format!("make_encoder: {e}"))?;
    let pcm = pack(planes, bps);
    let stride = container_bytes(bps) * planes.len();
    let n = planes.first().map_or(0, Vec::len);
    let mut packets = Vec::new();
    let mut at = 0;
    while at < n {
        let take = chunk.max(1).min(n - at);
        let frame = AudioFrame {
            samples: take as u32,
            pts: Some(at as i64),
            data: vec![pcm[at * stride..(at + take) * stride].to_vec()],
        };
        enc.send_frame(&Frame::Audio(frame))
            .map_err(|e| format!("send_frame: {e}"))?;
        while let Ok(p) = enc.receive_packet() {
            packets.push(p);
        }
        at += take;
    }
    enc.flush().map_err(|e| format!("flush: {e}"))?;
    while let Ok(p) = enc.receive_packet() {
        packets.push(p);
    }
    Ok((enc.output_params().extradata.clone(), packets))
}

// --- outside tools ------------------------------------------------------

/// Run a tool; `Err` carries its diagnostics.
pub fn run(tool: &str, args: &[String]) -> Result<Vec<u8>, String> {
    let out = Command::new(tool)
        .args(args)
        .output()
        .map_err(|e| format!("spawn {tool}: {e}"))?;
    if out.status.success() {
        Ok(out.stderr)
    } else {
        Err(format!(
            "{tool} {}: {}",
            args.join(" "),
            String::from_utf8_lossy(&out.stderr).trim()
        ))
    }
}

pub fn s(path: &Path) -> String {
    path.to_string_lossy().into_owned()
}

/// Decode a `.flac` with the black-box decoder, warnings fatal, and
/// return the WAVE sample data.
pub fn tool_decode(scratch: &Scratch, flac_path: &Path) -> Result<Vec<u8>, String> {
    let wav = scratch.path("tool-decode.wav");
    run(
        "flac",
        &[
            "-s".into(),
            "-w".into(),
            "-d".into(),
            "-f".into(),
            "-o".into(),
            s(&wav),
            s(flac_path),
        ],
    )?;
    let bytes = std::fs::read(&wav).map_err(|e| format!("read decoded wav: {e}"))?;
    let _ = std::fs::remove_file(&wav);
    wav_data(&bytes)
        .map(<[u8]>::to_vec)
        .ok_or_else(|| "black-box decoder wrote no data chunk".to_string())
}

/// `flac -t` with warnings fatal.
pub fn tool_test(flac_path: &Path) -> Result<(), String> {
    let diag = run(
        "flac",
        &["-s".into(), "-w".into(), "-t".into(), s(flac_path)],
    )?;
    if diag.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "flac -t said: {}",
            String::from_utf8_lossy(&diag).trim()
        ))
    }
}

/// Tally for one family of cases.
#[derive(Default)]
pub struct Tally {
    pub run: usize,
    pub failures: Vec<String>,
}

impl Tally {
    pub fn record(&mut self, name: &str, outcome: Result<(), String>) {
        self.run += 1;
        if let Err(e) = outcome {
            self.failures.push(format!("{name}: {e}"));
        }
    }

    /// Print the count and fail the test when anything failed.
    pub fn finish(self, family: &str) {
        eprintln!(
            "{family}: {}/{} cases pass",
            self.run - self.failures.len(),
            self.run
        );
        if !self.failures.is_empty() {
            let shown: Vec<&str> = self.failures.iter().map(String::as_str).take(40).collect();
            panic!(
                "{family}: {} of {} cases failed:\n{}",
                self.failures.len(),
                self.run,
                shown.join("\n")
            );
        }
    }
}

// --- census ---------------------------------------------------------------

/// What a set of streams actually contained, summed over a whole
/// matrix, so a test can say which parts of the format were exercised
/// rather than assume it.
#[derive(Default)]
pub struct Census {
    pub frames: u64,
    pub constant: u64,
    pub verbatim: u64,
    pub fixed_orders: [u64; 5],
    pub linear_orders: [u64; 32],
    pub precisions: [u64; 16],
    pub shifts: [u64; 32],
    pub partition_orders: [u64; 16],
    pub four_bit_parameters: u64,
    pub five_bit_parameters: u64,
    pub escaped_partitions: u64,
    pub wasted_bits: [u64; 32],
    pub independent: [u64; 9],
    pub left_side: u64,
    pub right_side: u64,
    pub mid_side: u64,
    /// Frames whose side channel left the 32-bit range.
    pub side_beyond_32_bits: u64,
    pub depths: [u64; 32],
    pub variable_block_frames: u64,
}

impl Census {
    /// Add every frame of a `.flac` image that starts with STREAMINFO.
    pub fn take(&mut self, file: &[u8]) -> Result<(), String> {
        use oxideav_flac::decoder::{FrameScratch, SubframeKind};
        use oxideav_flac::frame::{BlockingStrategy, ChannelAssignment, parse_frame_header};
        let census = self;
        let info = oxideav_flac::metadata::StreamInfo::parse(file.get(8..42).ok_or("short file")?)
            .map_err(|e| e.to_string())?;
        let mut demux = open_demuxer(file.to_vec())?;
        let mut scratch = FrameScratch::new();
        loop {
            let pkt = match demux.next_packet() {
                Ok(p) => p,
                Err(Error::Eof) => break,
                Err(e) => return Err(e.to_string()),
            };
            let header = parse_frame_header(&pkt.data).map_err(|e| e.to_string())?;
            let shape = scratch
                .decode(&pkt.data, &info)
                .map_err(|e| e.to_string())?;
            census.frames += 1;
            census.depths[shape.bits_per_sample as usize - 1] += 1;
            if header.blocking_strategy == BlockingStrategy::Variable {
                census.variable_block_frames += 1;
            }
            let decorrelated = match header.channels {
                ChannelAssignment::Independent(n) => {
                    census.independent[n as usize] += 1;
                    false
                }
                ChannelAssignment::LeftSide => {
                    census.left_side += 1;
                    true
                }
                ChannelAssignment::RightSide => {
                    census.right_side += 1;
                    true
                }
                ChannelAssignment::MidSide => {
                    census.mid_side += 1;
                    true
                }
            };
            if decorrelated && shape.bits_per_sample == 32 {
                let planes = scratch.planes();
                let beyond = planes[0].iter().zip(&planes[1]).any(|(&l, &r)| {
                    let side = l as i64 - r as i64;
                    side != side as i32 as i64
                });
                census.side_beyond_32_bits += beyond as u64;
            }
            for sub in &scratch.subframes()[..shape.channels] {
                census.wasted_bits[sub.wasted_bits as usize] += 1;
                match sub.kind {
                    SubframeKind::Constant => census.constant += 1,
                    SubframeKind::Verbatim => census.verbatim += 1,
                    SubframeKind::Fixed => census.fixed_orders[sub.order as usize] += 1,
                    SubframeKind::Linear => {
                        census.linear_orders[sub.order as usize - 1] += 1;
                        census.precisions[sub.precision as usize] += 1;
                        census.shifts[sub.shift as usize] += 1;
                    }
                    _ => {}
                }
                if sub.parameter_bits != 0 {
                    census.partition_orders[sub.partition_order as usize] += 1;
                    if sub.parameter_bits == 4 {
                        census.four_bit_parameters += 1;
                    } else {
                        census.five_bit_parameters += 1;
                    }
                    census.escaped_partitions += sub.escaped_partitions as u64;
                }
            }
        }
        Ok(())
    }

    /// One line listing what was seen.
    pub fn describe(&self) -> String {
        let census = self;
        format!(
            "{} frames; depths {}; constant {} verbatim {}; fixed orders {}; \
         linear orders {}; precisions {}; shifts {}; partition orders {}; \
         4-bit / 5-bit Rice parameters {} / {}; escaped partitions {}; wasted bits {}; \
         independent channels {}; left-side {} side-right {} mid-side {}; \
         side beyond 32 bits {}; variable-block frames {}",
            census.frames,
            seen(&census.depths, 1),
            census.constant,
            census.verbatim,
            seen(&census.fixed_orders, 0),
            seen(&census.linear_orders, 1),
            seen(&census.precisions, 0),
            seen(&census.shifts, 0),
            seen(&census.partition_orders, 0),
            census.four_bit_parameters,
            census.five_bit_parameters,
            census.escaped_partitions,
            seen(&census.wasted_bits, 0),
            seen(&census.independent, 0),
            census.left_side,
            census.right_side,
            census.mid_side,
            census.side_beyond_32_bits,
            census.variable_block_frames,
        )
    }
}

/// The indices with a non-zero count, numbered from `base`.
fn seen(counts: &[u64], base: usize) -> String {
    let list: Vec<String> = counts
        .iter()
        .enumerate()
        .filter(|&(_, &n)| n != 0)
        .map(|(i, _)| (i + base).to_string())
        .collect();
    list.join(",")
}

// --- muxing -------------------------------------------------------------

/// A writer whose bytes can be read back after the muxer drops its
/// boxed handle.
#[derive(Clone, Default)]
pub struct SharedCursor {
    inner: std::sync::Arc<std::sync::Mutex<Cursor<Vec<u8>>>>,
}

impl SharedCursor {
    pub fn bytes(&self) -> Vec<u8> {
        self.inner.lock().unwrap().get_ref().clone()
    }
}

impl std::io::Write for SharedCursor {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.inner.lock().unwrap().write(buf)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl std::io::Seek for SharedCursor {
    fn seek(&mut self, pos: std::io::SeekFrom) -> std::io::Result<u64> {
        self.inner.lock().unwrap().seek(pos)
    }
}

/// Mux encoder output into a `.flac` image through this crate's muxer.
pub fn mux_file(
    extradata: &[u8],
    channels: usize,
    bps: u32,
    rate: u32,
    packets: &[oxideav_core::Packet],
    options: oxideav_flac::container::FlacMuxerOptions,
) -> Result<Vec<u8>, String> {
    let mut params = encoder_params(channels, bps, rate);
    params.extradata = extradata.to_vec();
    let stream = oxideav_core::StreamInfo {
        index: 0,
        time_base: oxideav_core::TimeBase::new(1, rate as i64),
        duration: None,
        start_time: Some(0),
        params,
    };
    let shared = SharedCursor::default();
    let writer: Box<dyn oxideav_core::WriteSeek> = Box::new(shared.clone());
    let mut muxer = oxideav_flac::container::open_muxer_with_options(
        writer,
        std::slice::from_ref(&stream),
        options,
    )
    .map_err(|e| format!("open_muxer: {e}"))?;
    muxer.write_header().map_err(|e| format!("header: {e}"))?;
    for p in packets {
        muxer.write_packet(p).map_err(|e| format!("packet: {e}"))?;
    }
    muxer.write_trailer().map_err(|e| format!("trailer: {e}"))?;
    drop(muxer);
    Ok(shared.bytes())
}
