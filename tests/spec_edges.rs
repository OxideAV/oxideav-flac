//! Edge cases of the frame structure (RFC 9639 §9), one hand-built frame
//! at a time.
//!
//! Every frame here is assembled bit by bit from the layout tables of the
//! specification — not by this crate's encoder — and the expected samples
//! come from an independent model in the test (plain `i128` arithmetic
//! over the definitions of §9.2.5 / §9.2.6 / §4.2). A case is either a
//! stream the specification allows, which must decode to exactly those
//! samples, or one it forbids, which must be refused.

use oxideav_core::bits::BitWriter;
use oxideav_flac::crc::{crc8, crc16};
use oxideav_flac::decoder::decode_frame_channels;
use oxideav_flac::frame::{BlockingStrategy, ChannelAssignment, parse_frame_header};
use oxideav_flac::metadata::StreamInfo;

// --- Frame assembly ---------------------------------------------------------

/// The coded number of §9.1.5, Table 18.
fn coded_number(v: u64) -> Vec<u8> {
    assert!(v < 1 << 36);
    if v < 0x80 {
        return vec![v as u8];
    }
    // Sequence length: the first byte holds `7 - len` bits, every other
    // byte 6.
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

#[derive(Clone, Copy)]
enum BlockCode {
    /// One of the table codes 1..=5, 8..=15.
    Table(u8),
    /// Code 6: block size minus one in 8 bits after the coded number.
    Byte(u32),
    /// Code 7: block size minus one in 16 bits after the coded number.
    Word(u32),
}

#[derive(Clone, Copy)]
enum RateCode {
    /// Codes 0..=11 and the forbidden 15: nothing stored after the header.
    Table(u8),
    /// Code 12: kHz in 8 bits.
    KiloHertz(u8),
    /// Code 13: Hz in 16 bits.
    Hertz(u16),
    /// Code 14: Hz / 10 in 16 bits.
    DecaHertz(u16),
}

#[derive(Clone, Copy)]
struct Header {
    variable: bool,
    block: BlockCode,
    rate: RateCode,
    channel_code: u8,
    depth_code: u8,
    reserved_bit: bool,
    number: u64,
}

impl Header {
    /// Fixed block size stream, frame 0, 44.1 kHz.
    fn new(block_size: u32, channel_code: u8, depth_code: u8) -> Self {
        Header {
            variable: false,
            block: BlockCode::Word(block_size),
            rate: RateCode::Table(9),
            channel_code,
            depth_code,
            reserved_bit: false,
            number: 0,
        }
    }

    /// Everything before the CRC-8.
    fn bytes_without_crc(&self) -> Vec<u8> {
        let (block_code, block_tail): (u8, Vec<u8>) = match self.block {
            BlockCode::Table(c) => (c, vec![]),
            BlockCode::Byte(size) => (6, vec![(size - 1) as u8]),
            BlockCode::Word(size) => (7, ((size - 1) as u16).to_be_bytes().to_vec()),
        };
        let (rate_code, rate_tail): (u8, Vec<u8>) = match self.rate {
            RateCode::Table(c) => (c, vec![]),
            RateCode::KiloHertz(v) => (12, vec![v]),
            RateCode::Hertz(v) => (13, v.to_be_bytes().to_vec()),
            RateCode::DecaHertz(v) => (14, v.to_be_bytes().to_vec()),
        };
        let mut out = vec![
            0xFF,
            0xF8 | self.variable as u8,
            block_code << 4 | rate_code,
            self.channel_code << 4 | self.depth_code << 1 | self.reserved_bit as u8,
        ];
        out.extend_from_slice(&coded_number(self.number));
        out.extend_from_slice(&block_tail);
        out.extend_from_slice(&rate_tail);
        out
    }

    fn bytes(&self) -> Vec<u8> {
        let mut out = self.bytes_without_crc();
        out.push(crc8(&out));
        out
    }
}

/// Header, subframes written by `body`, zero padding, CRC-16.
fn frame(header: &Header, body: impl FnOnce(&mut BitWriter)) -> Vec<u8> {
    let mut w = BitWriter::new();
    body(&mut w);
    w.align_to_byte();
    let mut out = header.bytes();
    out.extend_from_slice(&w.finish());
    let c = crc16(&out);
    out.extend_from_slice(&c.to_be_bytes());
    out
}

fn put_signed(w: &mut BitWriter, v: i64, bits: u32) {
    if bits == 0 {
        return;
    }
    let mask = if bits == 64 {
        u64::MAX
    } else {
        (1u64 << bits) - 1
    };
    w.write_u64(v as u64 & mask, bits);
}

/// Subframe header (§9.2.1): zero bit, type, wasted-bits flag and count.
fn put_subframe_header(w: &mut BitWriter, type_code: u32, wasted: u32) {
    w.write_u32(0, 1);
    w.write_u32(type_code, 6);
    if wasted == 0 {
        w.write_u32(0, 1);
    } else {
        w.write_u32(1, 1);
        w.write_unary(wasted - 1);
    }
}

fn put_constant(w: &mut BitWriter, value: i64, bits: u32) {
    put_subframe_header(w, 0b000000, 0);
    put_signed(w, value, bits);
}

fn put_verbatim(w: &mut BitWriter, samples: &[i64], bits: u32) {
    put_subframe_header(w, 0b000001, 0);
    for &s in samples {
        put_signed(w, s, bits);
    }
}

/// One Rice code word (§9.2.7.2).
fn put_rice(w: &mut BitWriter, residual: i64, k: u32) {
    let folded: u64 = if residual >= 0 {
        (residual as u64) << 1
    } else {
        ((-(residual + 1)) as u64) << 1 | 1
    };
    w.write_unary((folded >> k) as u32);
    if k > 0 {
        w.write_u64(folded & ((1u64 << k) - 1), k);
    }
}

#[derive(Clone)]
enum Partition {
    Rice(u32, Vec<i64>),
    Escape(u32, Vec<i64>),
}

/// Coded residual (§9.2.7): method, partition order, partitions.
fn put_residual(w: &mut BitWriter, method: u32, partition_order: u32, partitions: &[Partition]) {
    w.write_u32(method, 2);
    w.write_u32(partition_order, 4);
    let parameter_bits = if method == 0 { 4 } else { 5 };
    for partition in partitions {
        match partition {
            Partition::Rice(k, residuals) => {
                w.write_u32(*k, parameter_bits);
                for &r in residuals {
                    put_rice(w, r, *k);
                }
            }
            Partition::Escape(width, residuals) => {
                w.write_u32((1 << parameter_bits) - 1, parameter_bits);
                w.write_u32(*width, 5);
                for &r in residuals {
                    put_signed(w, r, *width);
                }
            }
        }
    }
}

/// Fixed predictor subframe (§9.2.5).
// One argument per field of the subframe layout.
#[allow(clippy::too_many_arguments)]
fn put_fixed(
    w: &mut BitWriter,
    order: usize,
    warm_up: &[i64],
    bits: u32,
    wasted: u32,
    method: u32,
    partition_order: u32,
    partitions: &[Partition],
) {
    assert_eq!(warm_up.len(), order);
    put_subframe_header(w, 0b001000 + order as u32, wasted);
    for &s in warm_up {
        put_signed(w, s, bits);
    }
    put_residual(w, method, partition_order, partitions);
}

/// Linear predictor subframe (§9.2.6).
// One argument per field of the subframe layout.
#[allow(clippy::too_many_arguments)]
fn put_linear(
    w: &mut BitWriter,
    warm_up: &[i64],
    bits: u32,
    precision: u32,
    shift: u32,
    coefficients: &[i64],
    method: u32,
    partition_order: u32,
    partitions: &[Partition],
) {
    let order = coefficients.len();
    assert_eq!(warm_up.len(), order);
    put_subframe_header(w, 0b100000 + order as u32 - 1, 0);
    for &s in warm_up {
        put_signed(w, s, bits);
    }
    w.write_u32(precision - 1, 4);
    w.write_u32(shift, 5);
    for &c in coefficients {
        put_signed(w, c, precision);
    }
    put_residual(w, method, partition_order, partitions);
}

// --- Models -----------------------------------------------------------------

fn all_residuals(partitions: &[Partition]) -> Vec<i64> {
    partitions
        .iter()
        .flat_map(|p| match p {
            Partition::Rice(_, r) | Partition::Escape(_, r) => r.clone(),
        })
        .collect()
}

/// §9.2.5, Table 20.
fn model_fixed(order: usize, warm_up: &[i64], residuals: &[i64]) -> Vec<i64> {
    let taps: &[i128] = match order {
        0 => &[],
        1 => &[1],
        2 => &[2, -1],
        3 => &[3, -3, 1],
        4 => &[4, -6, 4, -1],
        _ => unreachable!(),
    };
    let mut out: Vec<i128> = warm_up.iter().map(|&s| s as i128).collect();
    for &r in residuals {
        let n = out.len();
        let prediction: i128 = taps
            .iter()
            .enumerate()
            .map(|(j, &t)| t * out[n - 1 - j])
            .sum();
        out.push(prediction + r as i128);
    }
    out.into_iter().map(|s| s as i64).collect()
}

/// §9.2.6: coefficient `j` multiplies the sample `j + 1` steps back.
fn model_linear(warm_up: &[i64], coefficients: &[i64], shift: u32, residuals: &[i64]) -> Vec<i64> {
    let mut out: Vec<i128> = warm_up.iter().map(|&s| s as i128).collect();
    for &r in residuals {
        let n = out.len();
        let sum: i128 = coefficients
            .iter()
            .enumerate()
            .map(|(j, &c)| c as i128 * out[n - 1 - j])
            .sum();
        out.push((sum >> shift) + r as i128);
    }
    out.into_iter().map(|s| s as i64).collect()
}

fn stream_info(channels: u8, bits_per_sample: u8) -> StreamInfo {
    StreamInfo {
        min_block_size: 16,
        max_block_size: 65_535,
        min_frame_size: 0,
        max_frame_size: 0,
        sample_rate: 44_100,
        channels,
        bits_per_sample,
        total_samples: 0,
        md5: [0; 16],
    }
}

fn decode(frame: &[u8], channels: u8, bits: u8) -> oxideav_core::Result<Vec<Vec<i32>>> {
    let decoded = decode_frame_channels(frame, &stream_info(channels, bits))?;
    assert_eq!(decoded.frame_byte_len, frame.len());
    Ok(decoded.channels)
}

fn as_i32(samples: &[i64]) -> Vec<i32> {
    samples
        .iter()
        .map(|&s| i32::try_from(s).expect("model sample fits 32 bits"))
        .collect()
}

/// Deterministic residual-sized values in `-(2^(bits-1)) .. 2^(bits-1)`.
fn noise(count: usize, bits: u32, mut seed: u64) -> Vec<i64> {
    (0..count)
        .map(|_| {
            seed = seed
                .wrapping_mul(6_364_136_223_846_793_005)
                .wrapping_add(1_442_695_040_888_963_407);
            if bits == 0 {
                0
            } else {
                (seed as i64) >> (64 - bits)
            }
        })
        .collect()
}

/// A mono 16-bit frame holding one fixed-predictor subframe.
fn fixed_frame(
    block_size: u32,
    order: usize,
    method: u32,
    partition_order: u32,
    partitions: &[Partition],
) -> (Vec<u8>, Vec<i32>) {
    let warm_up: Vec<i64> = (0..order as i64).map(|i| 100 - 37 * i).collect();
    let bytes = frame(&Header::new(block_size, 0, 4), |w| {
        put_fixed(
            w,
            order,
            &warm_up,
            16,
            0,
            method,
            partition_order,
            partitions,
        )
    });
    let model = model_fixed(order, &warm_up, &all_residuals(partitions));
    (bytes, as_i32(&model))
}

/// A bounded 16-bit test signal: a triangle wave with a little noise on
/// top, within ±2100.
fn signal(count: usize, seed: u64) -> Vec<i64> {
    noise(count, 6, seed)
        .into_iter()
        .enumerate()
        .map(|(i, n)| ((i as i64 * 7) % 4001 - 2000).abs() - 1000 + n)
        .collect()
}

/// What an encoder sends for `signal` under the fixed predictor of
/// `order` (Table 20): each sample minus its prediction.
fn fixed_residuals(signal: &[i64], order: usize) -> Vec<i64> {
    let taps: &[i64] = [&[][..], &[1], &[2, -1], &[3, -3, 1], &[4, -6, 4, -1]][order];
    (order..signal.len())
        .map(|t| {
            let p: i128 = taps
                .iter()
                .enumerate()
                .map(|(j, &c)| c as i128 * signal[t - 1 - j] as i128)
                .sum();
            i64::try_from(signal[t] as i128 - p).unwrap()
        })
        .collect()
}

/// The same for a linear predictor (§9.2.6).
fn linear_residuals(signal: &[i64], coefficients: &[i64], shift: u32) -> Vec<i64> {
    (coefficients.len()..signal.len())
        .map(|t| {
            let sum: i128 = coefficients
                .iter()
                .enumerate()
                .map(|(j, &c)| c as i128 * signal[t - 1 - j] as i128)
                .sum();
            i64::try_from(signal[t] as i128 - (sum >> shift)).unwrap()
        })
        .collect()
}

/// Cut the residual of a block into `2^partition_order` Rice partitions
/// with parameter `k`; the first is shorter by the predictor order.
fn rice_split(residuals: &[i64], order: usize, partition_order: u32, k: u32) -> Vec<Partition> {
    let per = (residuals.len() + order) >> partition_order;
    let mut rest = residuals;
    (0..1usize << partition_order)
        .map(|p| {
            let (part, tail) = rest.split_at(if p == 0 { per - order } else { per });
            rest = tail;
            Partition::Rice(k, part.to_vec())
        })
        .collect()
}

/// A mono 16-bit frame carrying [`signal`] through a fixed predictor and
/// Rice partitions, with the signal it must decode to.
fn fixed_signal_frame(
    block_size: u32,
    order: usize,
    method: u32,
    partition_order: u32,
    k: u32,
) -> (Vec<u8>, Vec<i32>) {
    let sig = signal(block_size as usize, block_size as u64 + order as u64);
    let parts = rice_split(&fixed_residuals(&sig, order), order, partition_order, k);
    let bytes = frame(&Header::new(block_size, 0, 4), |w| {
        put_fixed(
            w,
            order,
            &sig[..order],
            16,
            0,
            method,
            partition_order,
            &parts,
        )
    });
    (bytes, as_i32(&sig))
}

// --- Partition orders -------------------------------------------------------

#[test]
fn partition_order_zero_and_fifteen() {
    // Order 0: one partition holds the whole residual.
    let (bytes, want) = fixed_signal_frame(4096, 2, 0, 0, 5);
    assert_eq!(decode(&bytes, 1, 16).unwrap(), vec![want]);

    // Order 15, the largest the 4-bit field can express: 32768 partitions
    // of one sample each, which only predictor order 0 allows.
    for method in [0, 1] {
        let (bytes, want) = fixed_signal_frame(32_768, 0, method, 15, 9);
        assert_eq!(decode(&bytes, 1, 16).unwrap(), vec![want]);
    }

    // Every partition order a 4096-sample block admits with a 4th-order
    // predictor: 4096 >> order must exceed 4, so 0..=9 (§9.2.7).
    for partition_order in 0..=9 {
        let (bytes, want) = fixed_signal_frame(4096, 4, 0, partition_order, 6);
        assert_eq!(
            decode(&bytes, 1, 16).unwrap(),
            vec![want],
            "partition order {partition_order}"
        );
    }
}

#[test]
fn partition_count_must_divide_the_block() {
    // "only partition order 0 is allowed for all odd block sizes".
    for (block_size, partition_order) in [(4097u32, 1u32), (17, 1), (18, 2), (192, 7), (65_535, 15)]
    {
        let per = (block_size >> partition_order) as usize;
        let mut parts: Vec<Partition> = (0..1usize << partition_order)
            .map(|_| Partition::Rice(0, vec![0; per]))
            .collect();
        // Put the remainder in the last partition so every sample of the
        // block is present in the bitstream.
        let extra = block_size as usize - (per << partition_order);
        if let Some(Partition::Rice(_, r)) = parts.last_mut() {
            r.extend(std::iter::repeat_n(0, extra));
        }
        let (bytes, _) = fixed_frame(block_size, 0, 0, partition_order, &parts);
        assert!(
            decode(&bytes, 1, 16).is_err(),
            "{block_size} samples in 2^{partition_order} partitions"
        );
    }
    // The same odd block sizes are fine with partition order 0.
    for block_size in [17u32, 4097, 65_535] {
        let (bytes, want) = fixed_signal_frame(block_size, 1, 0, 0, 5);
        assert_eq!(decode(&bytes, 1, 16).unwrap(), vec![want]);
    }
}

// --- The first partition and the predictor order ----------------------------

/// §9.2.7: "The partition order also MUST be such that the (block size >>
/// partition order) is larger than the predictor order." The first
/// partition therefore always carries at least one residual sample.
#[test]
fn first_partition_must_be_larger_than_the_predictor_order() {
    // One residual sample in the first partition: the smallest legal case.
    for (block_size, order, partition_order) in [
        (2u32, 1usize, 0u32),
        (5, 4, 0),
        (16, 3, 2),
        (4096, 4, 9),
        (40, 4, 3),
    ] {
        let (bytes, want) = fixed_signal_frame(block_size, order, 0, partition_order, 6);
        assert_eq!(
            decode(&bytes, 1, 16).unwrap(),
            vec![want],
            "block {block_size} order {order} partition order {partition_order}"
        );
    }

    // Zero residual samples in the first partition (partition length
    // equal to the predictor order): forbidden. The partition is written
    // out completely — parameter and no code words — so the only thing
    // wrong with the frame is the rule itself.
    for (block_size, order, partition_order) in [
        (4096u32, 4usize, 10u32),
        (16, 4, 2),
        (8, 2, 2),
        (64, 1, 6),
        (4, 4, 0),
        (1, 1, 0),
    ] {
        let per = (block_size >> partition_order) as usize;
        assert_eq!(per, order);
        let parts: Vec<Partition> = (0..1usize << partition_order)
            .map(|p| Partition::Rice(2, vec![1; if p == 0 { 0 } else { per }]))
            .collect();
        let warm_up = vec![0i64; order];
        let bytes = frame(&Header::new(block_size, 0, 4), |w| {
            put_fixed(w, order, &warm_up, 16, 0, 0, partition_order, &parts)
        });
        assert!(
            decode(&bytes, 1, 16).is_err(),
            "block {block_size} order {order} partition order {partition_order}"
        );
    }

    // Predictor order above the partition length: the first partition
    // would have a negative sample count.
    for (block_size, order, partition_order) in [(16u32, 4usize, 3u32), (4096, 3, 11), (3, 4, 0)] {
        let per = (block_size >> partition_order) as usize;
        assert!(per < order);
        let parts: Vec<Partition> = (0..1usize << partition_order)
            .map(|p| Partition::Rice(2, vec![1; if p == 0 { 0 } else { per }]))
            .collect();
        let warm_up = vec![0i64; order.min(block_size as usize)];
        let bytes = frame(&Header::new(block_size, 0, 4), |w| {
            put_subframe_header(w, 0b001000 + order as u32, 0);
            for &s in &warm_up {
                put_signed(w, s, 16);
            }
            put_residual(w, 0, partition_order, &parts);
        });
        assert!(
            decode(&bytes, 1, 16).is_err(),
            "block {block_size} order {order} partition order {partition_order}"
        );
    }
}

#[test]
fn linear_predictor_order_against_block_size() {
    // Order 32, the largest: needs a block of at least 33 samples.
    let coefficients: Vec<i64> = (0..32).map(|i| if i % 2 == 0 { 3 } else { -2 }).collect();
    let warm_up = signal(32, 5);
    for block_size in [33u32, 64, 4096] {
        let sig = signal(block_size as usize, 5);
        let parts = vec![Partition::Rice(9, linear_residuals(&sig, &coefficients, 4))];
        let bytes = frame(&Header::new(block_size, 0, 4), |w| {
            put_linear(w, &sig[..32], 16, 5, 4, &coefficients, 0, 0, &parts)
        });
        let model = model_linear(&sig[..32], &coefficients, 4, &all_residuals(&parts));
        assert_eq!(model, sig);
        assert_eq!(decode(&bytes, 1, 16).unwrap(), vec![as_i32(&sig)]);
    }
    // A 32-sample block has no residual left; smaller ones cannot even
    // hold the warm-up.
    for block_size in [32u32, 31, 16, 1] {
        let bytes = frame(&Header::new(block_size, 0, 4), |w| {
            put_subframe_header(w, 0b111111, 0);
            for &s in warm_up.iter().take(block_size as usize) {
                put_signed(w, s, 16);
            }
            w.write_u32(4, 4);
            w.write_u32(4, 5);
            for &c in &coefficients {
                put_signed(w, c, 5);
            }
            put_residual(w, 0, 0, &[Partition::Rice(0, vec![])]);
        });
        assert!(decode(&bytes, 1, 16).is_err(), "block {block_size}");
    }
    // Order 1 with every coefficient precision 1..=15, the most negative
    // coefficient of each, and shifts across the range 0..=15.
    let sig = signal(48, 77);
    for precision in 1..=15u32 {
        for shift in [0u32, 1, 7, 15] {
            let c = [-(1i64 << (precision - 1))];
            let parts = vec![Partition::Rice(20, linear_residuals(&sig, &c, shift))];
            let bytes = frame(&Header::new(48, 0, 4), |w| {
                put_linear(w, &sig[..1], 16, precision, shift, &c, 1, 0, &parts)
            });
            assert_eq!(
                decode(&bytes, 1, 16).unwrap(),
                vec![as_i32(&sig)],
                "precision {precision} shift {shift}"
            );
        }
    }
}

#[test]
fn linear_predictor_forbidden_fields() {
    let build = |precision_field: u32, shift_field: u32| {
        frame(&Header::new(16, 0, 4), |w| {
            put_subframe_header(w, 0b100000, 0);
            put_signed(w, 5, 16);
            w.write_u32(precision_field, 4);
            w.write_u32(shift_field, 5);
            // Enough bits for a coefficient of any precision.
            w.write_u32(0, 16);
            put_residual(w, 0, 0, &[Partition::Escape(0, vec![0; 15])]);
        })
    };
    // Precision field 0b1111 is forbidden (Table 22).
    assert!(decode(&build(0b1111, 0), 1, 16).is_err());
    // The shift is two's complement but MUST NOT be negative.
    for shift in 16..32 {
        assert!(decode(&build(3, shift), 1, 16).is_err(), "shift {shift}");
    }
}

// --- Escaped partitions -----------------------------------------------------

#[test]
fn escaped_partitions_of_every_width() {
    for method in [0u32, 1] {
        for width in 0..=31u32 {
            // Fixed order 0, so the residual is the signal; 32-bit frame
            // so that every width up to 31 bits fits the sample range.
            let residuals = noise(16, width, 1000 + width as u64);
            let mut extremes = residuals.clone();
            if width > 0 {
                extremes[3] = -(1i64 << (width - 1));
                extremes[9] = (1i64 << (width - 1)) - 1;
            }
            let parts = vec![Partition::Escape(width, extremes.clone())];
            let bytes = frame(&Header::new(16, 0, 7), |w| {
                put_fixed(w, 0, &[], 32, 0, method, 0, &parts)
            });
            let got = decode(&bytes, 1, 32).unwrap();
            assert_eq!(
                got,
                vec![as_i32(&extremes)],
                "method {method} width {width}"
            );
            if width == 0 {
                // "all residual samples in that partition have a value
                // of 0 and … no bits are used to store the samples".
                assert!(got[0].iter().all(|&s| s == 0));
            }
        }
    }
}

#[test]
fn zero_width_escape_takes_no_sample_bits() {
    // Four partitions: 0-bit escape, Rice, 0-bit escape, 5-bit escape. A
    // decoder that read even one bit for a 0-bit partition would be out
    // of step for everything after it.
    let parts = vec![
        Partition::Escape(0, vec![0; 6]),
        Partition::Rice(2, noise(8, 4, 3)),
        Partition::Escape(0, vec![0; 8]),
        Partition::Escape(5, noise(8, 5, 4)),
    ];
    let (bytes, want) = fixed_frame(32, 2, 0, 2, &parts);
    assert_eq!(decode(&bytes, 1, 16).unwrap(), vec![want]);
    // The whole coded residual of an all-zero-escape subframe is the six
    // lead bits plus nine bits per partition.
    let parts: Vec<Partition> = (0..4)
        .map(|p| Partition::Escape(0, vec![0; if p == 0 { 7 } else { 8 }]))
        .collect();
    let (bytes, want) = fixed_frame(32, 1, 0, 2, &parts);
    let header_len = Header::new(32, 0, 4).bytes().len();
    let bits: usize = 8 + 16 + 6 + 4 * 9;
    assert_eq!(bytes.len(), header_len + bits.div_ceil(8) + 2);
    assert_eq!(decode(&bytes, 1, 16).unwrap(), vec![want]);
}

// --- Rice parameters --------------------------------------------------------

#[test]
fn rice_parameters_at_the_limits_of_both_methods() {
    // Method 0: 4-bit parameters 0..=14, 0b1111 is the escape code.
    for k in 0..=14u32 {
        let residuals = noise(16, k + 2, 50 + k as u64);
        let parts = vec![Partition::Rice(k, residuals.clone())];
        let bytes = frame(&Header::new(16, 0, 7), |w| {
            put_fixed(w, 0, &[], 32, 0, 0, 0, &parts)
        });
        assert_eq!(
            decode(&bytes, 1, 32).unwrap(),
            vec![as_i32(&residuals)],
            "4-bit k = {k}"
        );
    }
    // Method 1: 5-bit parameters 0..=30, 0b11111 is the escape code. 15,
    // the 4-bit escape code, is an ordinary parameter here.
    for k in 0..=30u32 {
        let residuals = noise(16, (k + 2).min(31), 90 + k as u64);
        let parts = vec![Partition::Rice(k, residuals.clone())];
        let bytes = frame(&Header::new(16, 0, 7), |w| {
            put_fixed(w, 0, &[], 32, 0, 1, 0, &parts)
        });
        assert_eq!(
            decode(&bytes, 1, 32).unwrap(),
            vec![as_i32(&residuals)],
            "5-bit k = {k}"
        );
    }
}

#[test]
fn reserved_residual_coding_methods() {
    for method in [2u32, 3] {
        let bytes = frame(&Header::new(16, 0, 4), |w| {
            put_subframe_header(w, 0b001000, 0);
            w.write_u32(method, 2);
            w.write_u32(0, 4);
            w.write_u32(0, 5);
            for _ in 0..16 {
                w.write_u32(1, 1);
            }
        });
        assert!(decode(&bytes, 1, 16).is_err(), "method {method}");
    }
}

/// §9.2.7.3: a residual is within `-(2^31 - 1) ..= 2^31 - 1`.
#[test]
fn residual_value_limit() {
    let build = |residual: i64, k: u32| {
        frame(&Header::new(16, 0, 7), |w| {
            let mut residuals = vec![0i64; 16];
            residuals[7] = residual;
            put_fixed(w, 0, &[], 32, 0, 1, 0, &[Partition::Rice(k, residuals)])
        })
    };
    for k in [30u32, 29, 24] {
        for residual in [i32::MAX as i64, -(i32::MAX as i64)] {
            let got = decode(&build(residual, k), 1, 32).unwrap();
            assert_eq!(got[0][7] as i64, residual, "k = {k}");
        }
        // The most negative 32-bit value is excluded, and so is anything
        // beyond the range.
        for residual in [i32::MIN as i64, i32::MAX as i64 + 1, 1 << 33, -(1 << 33)] {
            assert!(
                decode(&build(residual, k), 1, 32).is_err(),
                "residual {residual} with k = {k}"
            );
        }
    }
}

// --- Wasted bits ------------------------------------------------------------

#[test]
fn wasted_bits_on_every_subframe_type() {
    for wasted in [1u32, 2, 7, 15] {
        let depth = 16 - wasted;
        let scale = |v: &[i64]| -> Vec<i32> { v.iter().map(|&s| (s << wasted) as i32).collect() };

        // Constant.
        let value = -(1i64 << (depth - 1));
        let bytes = frame(&Header::new(20, 0, 4), |w| {
            put_subframe_header(w, 0b000000, wasted);
            put_signed(w, value, depth);
        });
        assert_eq!(decode(&bytes, 1, 16).unwrap(), vec![scale(&[value; 20])]);

        // Verbatim.
        let samples = noise(20, depth, 3);
        let bytes = frame(&Header::new(20, 0, 4), |w| {
            put_subframe_header(w, 0b000001, wasted);
            for &s in &samples {
                put_signed(w, s, depth);
            }
        });
        assert_eq!(decode(&bytes, 1, 16).unwrap(), vec![scale(&samples)]);

        // Fixed predictor: warm-up samples are stored at the reduced
        // depth too, and the shift is applied after prediction. The
        // signal stays in {-1, 0}, which every depth down to one bit
        // holds.
        let reduced: Vec<i64> = (0..20).map(|i| -((i * i + i / 3) % 2)).collect();
        let parts = vec![Partition::Escape(2, fixed_residuals(&reduced, 1))];
        let bytes = frame(&Header::new(20, 0, 4), |w| {
            put_fixed(w, 1, &reduced[..1], depth, wasted, 0, 0, &parts)
        });
        assert_eq!(
            decode(&bytes, 1, 16).unwrap(),
            vec![scale(&reduced)],
            "{wasted}"
        );
    }
}

#[test]
fn wasted_bits_must_leave_a_sample_bit() {
    // 15 wasted bits of 16 leave one-bit samples: allowed.
    let bytes = frame(&Header::new(4, 0, 4), |w| {
        put_subframe_header(w, 0b000001, 15);
        for s in [0i64, -1, -1, 0] {
            put_signed(w, s, 1);
        }
    });
    assert_eq!(
        decode(&bytes, 1, 16).unwrap(),
        vec![vec![0, -32_768, -32_768, 0]]
    );
    // 16 or more: the resulting depth is not larger than zero.
    for wasted in [16u32, 17, 40, 300] {
        let bytes = frame(&Header::new(4, 0, 4), |w| {
            put_subframe_header(w, 0b000001, wasted);
            w.write_u32(0, 32);
        });
        assert!(decode(&bytes, 1, 16).is_err(), "{wasted} wasted bits");
    }
    // A wasted-bits run that never ends.
    let mut bytes = Header::new(4, 0, 4).bytes();
    bytes.push(0b0000_0011);
    bytes.extend_from_slice(&[0u8; 64]);
    let c = crc16(&bytes);
    bytes.extend_from_slice(&c.to_be_bytes());
    assert!(decode_frame_channels(&bytes, &stream_info(1, 16)).is_err());
}

/// §9.2.2: with stereo decorrelation the padding happens "on the
/// subframes before restoring the channels to left and right", and the
/// side subframe has one more bit to begin with.
#[test]
fn wasted_bits_are_restored_before_decorrelation() {
    let left: Vec<i64> = vec![1000, -2000, 32_760, -32_768, 8, 0];
    let right: Vec<i64> = vec![-8, 1992, -32_768, 32_760, 0, 16];
    let side: Vec<i64> = left.iter().zip(&right).map(|(l, r)| l - r).collect();
    let mid: Vec<i64> = left.iter().zip(&right).map(|(l, r)| (l + r) >> 1).collect();
    // Every value is a multiple of four, and so is every mid value.
    for v in left.iter().chain(&right).chain(&side).chain(&mid) {
        assert_eq!(v % 4, 0);
    }
    let reduced = |v: &[i64]| -> Vec<i64> { v.iter().map(|s| s >> 2).collect() };
    let put = |w: &mut BitWriter, v: &[i64], bits: u32| {
        put_subframe_header(w, 0b000001, 2);
        for &s in &reduced(v) {
            put_signed(w, s, bits - 2);
        }
    };
    let want = vec![as_i32(&left), as_i32(&right)];
    // Left-side, side-right, mid-side: channel codes 8, 9, 10.
    let bytes = frame(&Header::new(6, 8, 4), |w| {
        put(w, &left, 16);
        put(w, &side, 17);
    });
    assert_eq!(decode(&bytes, 2, 16).unwrap(), want);
    let bytes = frame(&Header::new(6, 9, 4), |w| {
        put(w, &side, 17);
        put(w, &right, 16);
    });
    assert_eq!(decode(&bytes, 2, 16).unwrap(), want);
    let bytes = frame(&Header::new(6, 10, 4), |w| {
        put(w, &mid, 16);
        put(w, &side, 17);
    });
    assert_eq!(decode(&bytes, 2, 16).unwrap(), want);
    // The two subframes need not agree on the count.
    let bytes = frame(&Header::new(6, 8, 4), |w| {
        put_verbatim(w, &left, 16);
        put(w, &side, 17);
    });
    assert_eq!(decode(&bytes, 2, 16).unwrap(), want);
}

// --- Side channel: one more bit, up to 33 -----------------------------------

#[test]
fn side_channel_is_one_bit_wider_at_every_depth() {
    for (depth_code, bits) in [(1u8, 8u32), (2, 12), (4, 16), (5, 20), (6, 24), (7, 32)] {
        let hi = (1i64 << (bits - 1)) - 1;
        let lo = -(1i64 << (bits - 1));
        // Full-scale opposite channels: the side needs `bits + 1` bits.
        let left = vec![hi, lo, hi, lo, 0, -1, 1, hi, lo, 3];
        let right = vec![lo, hi, hi, lo, -1, 0, -1, 0, -1, -4];
        let side: Vec<i64> = left.iter().zip(&right).map(|(l, r)| l - r).collect();
        let mid: Vec<i64> = left.iter().zip(&right).map(|(l, r)| (l + r) >> 1).collect();
        let want = vec![as_i32(&left), as_i32(&right)];
        let n = left.len() as u32;

        let bytes = frame(&Header::new(n, 8, depth_code), |w| {
            put_verbatim(w, &left, bits);
            put_verbatim(w, &side, bits + 1);
        });
        assert_eq!(decode(&bytes, 2, bits as u8).unwrap(), want, "{bits} L/S");
        let bytes = frame(&Header::new(n, 9, depth_code), |w| {
            put_verbatim(w, &side, bits + 1);
            put_verbatim(w, &right, bits);
        });
        assert_eq!(decode(&bytes, 2, bits as u8).unwrap(), want, "{bits} S/R");
        let bytes = frame(&Header::new(n, 10, depth_code), |w| {
            put_verbatim(w, &mid, bits);
            put_verbatim(w, &side, bits + 1);
        });
        assert_eq!(decode(&bytes, 2, bits as u8).unwrap(), want, "{bits} M/S");

        // The side subframe modelled by a predictor with Rice and escaped
        // partitions, constant, and with wasted bits.
        let warm_up = side[..2].to_vec();
        let model_residuals: Vec<i64> = (2..side.len())
            .map(|t| side[t] - (2 * side[t - 1] - side[t - 2]))
            .collect();
        if model_residuals.iter().all(|r| r.abs() < 1 << 31) {
            let width = 64
                - model_residuals
                    .iter()
                    .map(|r| r.unsigned_abs())
                    .max()
                    .unwrap()
                    .leading_zeros()
                + 1;
            let parts = if width <= 31 {
                vec![Partition::Escape(width, model_residuals.clone())]
            } else {
                vec![Partition::Rice(30, model_residuals.clone())]
            };
            let bytes = frame(&Header::new(n, 8, depth_code), |w| {
                put_verbatim(w, &left, bits);
                put_fixed(w, 2, &warm_up, bits + 1, 0, 1, 0, &parts);
            });
            assert_eq!(
                decode(&bytes, 2, bits as u8).unwrap(),
                want,
                "{bits} modelled"
            );
        }
        let bytes = frame(&Header::new(4, 9, depth_code), |w| {
            put_constant(w, hi - lo, bits + 1);
            put_constant(w, lo, bits);
        });
        assert_eq!(
            decode(&bytes, 2, bits as u8).unwrap(),
            vec![vec![hi as i32; 4], vec![lo as i32; 4]],
            "{bits} constant side"
        );
    }
}

/// A 33-bit side subframe whose reconstruction needs more than 32 bits of
/// accumulator on every step: Rice-coded residual, fourth-order fixed
/// predictor and a 32-tap linear predictor (Appendix A.3).
#[test]
fn side_channel_of_33_bits_with_predictors() {
    let n = 96usize;
    // 32 taps that pass the previous sample through almost unchanged:
    // 31/32 of it plus 1/32 of the one before, and 30 more taps that
    // cancel in pairs on a slow signal.
    let coefficients: Vec<i64> = (0..32)
        .map(|i| match i {
            0 => 31,
            1 => 1,
            _ => 1 - 2 * (i % 2),
        })
        .collect();
    let shift = 5;
    // The channels sit at opposite ends of the 32-bit range, so the side
    // is near ±2^32 — outside 32 bits — for the whole block; once with
    // each sign.
    for sign in [1i64, -1] {
        let left: Vec<i64> = (0..n as i64)
            .map(|i| sign * (i32::MAX as i64 - 1000 * i - (i * i) % 777))
            .collect();
        let right: Vec<i64> = (0..n as i64)
            .map(|i| sign * (i32::MIN as i64 + 1 + 313 * i + (i * 37) % 91))
            .collect();
        let side: Vec<i64> = left.iter().zip(&right).map(|(l, r)| l - r).collect();
        let mid: Vec<i64> = left.iter().zip(&right).map(|(l, r)| (l + r) >> 1).collect();
        assert!(side.iter().all(|&s| s.abs() > i32::MAX as i64));
        let want = vec![as_i32(&left), as_i32(&right)];

        let residuals = linear_residuals(&side, &coefficients, shift);
        assert!(residuals.iter().all(|r| r.abs() < 1 << 31));
        let parts = vec![Partition::Rice(16, residuals)];
        let bytes = frame(&Header::new(n as u32, 10, 7), |w| {
            put_verbatim(w, &mid, 32);
            put_linear(w, &side[..32], 33, 6, shift, &coefficients, 1, 0, &parts);
        });
        assert_eq!(decode(&bytes, 2, 32).unwrap(), want, "sign {sign}");
    }

    // First-order fixed predictor on a slowly moving 33-bit side.
    let left: Vec<i64> = (0..n as i64).map(|i| i32::MAX as i64 - i * i).collect();
    let right: Vec<i64> = (0..n as i64).map(|i| i32::MIN as i64 + 3 * i).collect();
    let side: Vec<i64> = left.iter().zip(&right).map(|(l, r)| l - r).collect();
    let want = vec![as_i32(&left), as_i32(&right)];
    for order in 0..=4usize {
        let residuals = fixed_residuals(&side, order);
        if residuals.iter().any(|r| r.abs() >= 1 << 31) {
            // Order 0 of a 33-bit signal: not representable (Appendix
            // A.4), an encoder has to choose another model.
            assert_eq!(order, 0);
            continue;
        }
        let parts = vec![Partition::Rice(12, residuals)];
        for (channel_code, side_first) in [(8u8, false), (9, true)] {
            let bytes = frame(&Header::new(n as u32, channel_code, 7), |w| {
                if side_first {
                    put_fixed(w, order, &side[..order], 33, 0, 1, 0, &parts);
                    put_verbatim(w, &right, 32);
                } else {
                    put_verbatim(w, &left, 32);
                    put_fixed(w, order, &side[..order], 33, 0, 1, 0, &parts);
                }
            });
            assert_eq!(decode(&bytes, 2, 32).unwrap(), want, "order {order}");
        }
    }

    // A side sample that no pair of 32-bit samples can produce.
    let bytes = frame(&Header::new(4, 8, 7), |w| {
        put_verbatim(w, &[0, 0, 0, 0], 32);
        // 2^32 - 1 followed by +1 through a first-order predictor.
        put_fixed(
            w,
            1,
            &[(1 << 32) - 1],
            33,
            0,
            0,
            0,
            &[Partition::Rice(0, vec![1, 0, 0])],
        );
    });
    assert!(decode(&bytes, 2, 32).is_err());
}

// --- 32-bit samples and 64-bit accumulation ---------------------------------

#[test]
fn full_scale_32_bit_samples_through_every_predictor() {
    let n = 64usize;
    // A signal that sits at both ends of the 32-bit range.
    let signal: Vec<i64> = (0..n as i64)
        .map(|i| match i % 4 {
            0 => i32::MAX as i64 - i,
            1 => i32::MAX as i64 - 5 * i,
            2 => i32::MIN as i64 + i * i,
            _ => i32::MIN as i64 + 3,
        })
        .collect();
    let want = vec![as_i32(&signal)];

    // Linear predictors whose inner product is far outside 32 bits: 15-bit
    // coefficients at both extremes, orders 1, 2, 8 and 32.
    for order in [1usize, 2, 8, 32] {
        for (precision, shift) in [(15u32, 14u32), (15, 15), (12, 11), (2, 0)] {
            let top = (1i64 << (precision - 1)) - 1;
            let coefficients: Vec<i64> = (0..order)
                .map(|j| if j % 2 == 0 { top } else { -top - 1 })
                .collect();
            let residuals: Vec<i64> = (order..n)
                .map(|t| {
                    let sum: i128 = coefficients
                        .iter()
                        .enumerate()
                        .map(|(j, &c)| c as i128 * signal[t - 1 - j] as i128)
                        .sum();
                    (signal[t] as i128 - (sum >> shift)) as i64
                })
                .collect();
            if residuals.iter().any(|r| r.abs() >= 1 << 31) {
                continue;
            }
            let parts = vec![Partition::Rice(30, residuals)];
            let bytes = frame(&Header::new(n as u32, 0, 7), |w| {
                put_linear(
                    w,
                    &signal[..order],
                    32,
                    precision,
                    shift,
                    &coefficients,
                    1,
                    0,
                    &parts,
                )
            });
            assert_eq!(
                decode(&bytes, 1, 32).unwrap(),
                want,
                "order {order} precision {precision} shift {shift}"
            );
        }
    }

    // Fixed predictors on a smooth full-scale ramp: the prediction of
    // order 4 reaches 15 times full scale before the residual is added.
    let ramp: Vec<i64> = (0..n as i64)
        .map(|i| i32::MIN as i64 + i * 60_000_000)
        .collect();
    for order in 0..=4usize {
        let taps: &[i64] = [&[][..], &[1], &[2, -1], &[3, -3, 1], &[4, -6, 4, -1]][order];
        let residuals: Vec<i64> = (order..n)
            .map(|t| {
                let p: i128 = taps
                    .iter()
                    .enumerate()
                    .map(|(j, &c)| c as i128 * ramp[t - 1 - j] as i128)
                    .sum();
                (ramp[t] as i128 - p) as i64
            })
            .collect();
        if residuals.iter().any(|r| r.abs() >= 1 << 31) {
            continue;
        }
        let parts = vec![Partition::Rice(if order == 0 { 30 } else { 10 }, residuals)];
        let bytes = frame(&Header::new(n as u32, 0, 7), |w| {
            put_fixed(w, order, &ramp[..order], 32, 0, 1, 0, &parts)
        });
        assert_eq!(
            decode(&bytes, 1, 32).unwrap(),
            vec![as_i32(&ramp)],
            "fixed order {order}"
        );
    }
}

// --- Subframe header --------------------------------------------------------

#[test]
fn reserved_subframe_types_and_the_zero_bit() {
    for type_code in (0b000010..=0b000111).chain(0b001101..=0b011111) {
        let bytes = frame(&Header::new(16, 0, 4), |w| {
            put_subframe_header(w, type_code, 0);
            w.write_u32(0, 32);
            w.write_u32(0, 32);
        });
        assert!(decode(&bytes, 1, 16).is_err(), "type {type_code:#08b}");
    }
    // "The first bit of the header MUST be 0".
    let bytes = frame(&Header::new(16, 0, 4), |w| {
        w.write_u32(1, 1);
        w.write_u32(0b000000, 6);
        w.write_u32(0, 1);
        put_signed(w, 0, 16);
    });
    assert!(decode(&bytes, 1, 16).is_err());
    // Every defined type code is accepted.
    for order in 0..=4usize {
        let parts = vec![Partition::Escape(0, vec![0; 16 - order])];
        let (bytes, want) = fixed_frame(16, order, 0, 0, &parts);
        assert_eq!(decode(&bytes, 1, 16).unwrap(), vec![want]);
    }
    for order in 1..=32usize {
        let coefficients = vec![0i64; order];
        let warm_up = noise(order, 12, order as u64);
        let parts = vec![Partition::Escape(3, noise(64 - order, 3, 1))];
        let bytes = frame(&Header::new(64, 0, 4), |w| {
            put_linear(w, &warm_up, 16, 1, 0, &coefficients, 0, 0, &parts)
        });
        let model = model_linear(&warm_up, &coefficients, 0, &all_residuals(&parts));
        assert_eq!(
            decode(&bytes, 1, 16).unwrap(),
            vec![as_i32(&model)],
            "linear order {order}"
        );
    }
}

// --- Block sizes ------------------------------------------------------------

#[test]
fn every_block_size_code() {
    // Table 14: code → block size.
    let table: [(u8, u32); 13] = [
        (1, 192),
        (2, 576),
        (3, 1152),
        (4, 2304),
        (5, 4608),
        (8, 256),
        (9, 512),
        (10, 1024),
        (11, 2048),
        (12, 4096),
        (13, 8192),
        (14, 16_384),
        (15, 32_768),
    ];
    for (code, size) in table {
        let mut h = Header::new(0, 0, 4);
        h.block = BlockCode::Table(code);
        let parsed = parse_frame_header(&h.bytes()).unwrap();
        assert_eq!(parsed.block_size, size, "code {code}");
        let bytes = frame(&h, |w| put_constant(w, -7, 16));
        assert_eq!(
            decode(&bytes, 1, 16).unwrap(),
            vec![vec![-7; size as usize]]
        );
    }
    // Code 0 is reserved.
    let mut h = Header::new(0, 0, 4);
    h.block = BlockCode::Table(0);
    assert!(parse_frame_header(&h.bytes()).is_err());

    // Code 6 (8 bits) covers 1..=256 and code 7 (16 bits) 1..=65535.
    for size in 1..=256u32 {
        let mut h = Header::new(0, 0, 4);
        h.block = BlockCode::Byte(size);
        assert_eq!(parse_frame_header(&h.bytes()).unwrap().block_size, size);
    }
    for size in 1..=65_535u32 {
        let h = Header::new(size, 0, 4);
        assert_eq!(parse_frame_header(&h.bytes()).unwrap().block_size, size);
    }
    // "A value of 65535 (corresponding to a block size of 65536) is
    // forbidden".
    let h = Header::new(65_536, 0, 4);
    assert_eq!(&h.bytes_without_crc()[5..7], &[0xFF, 0xFF]);
    assert!(parse_frame_header(&h.bytes()).is_err());
}

#[test]
fn blocks_of_one_to_sixty_five_thousand_samples_decode() {
    let sizes = [
        1u32, 2, 3, 4, 5, 15, 16, 17, 31, 32, 33, 191, 192, 193, 255, 256, 257, 4095, 4096, 4097,
        32_767, 32_768, 32_769, 65_534, 65_535,
    ];
    for size in sizes {
        let n = size as usize;
        // Verbatim stereo (independent channels).
        let a = noise(n, 16, size as u64);
        let b = noise(n, 16, size as u64 + 1);
        let bytes = frame(&Header::new(size, 1, 4), |w| {
            put_verbatim(w, &a, 16);
            put_verbatim(w, &b, 16);
        });
        assert_eq!(
            decode(&bytes, 2, 16).unwrap(),
            vec![as_i32(&a), as_i32(&b)],
            "{size} samples"
        );
        // The largest predictor the block admits, residual in one Rice
        // partition.
        let order = 4.min(n - 1);
        let (bytes, want) = fixed_signal_frame(size, order, 0, 0, 7);
        assert_eq!(decode(&bytes, 1, 16).unwrap(), vec![want], "{size} samples");
        // Through the 8-bit block size field where it fits.
        if size <= 256 {
            let mut h = Header::new(size, 0, 4);
            h.block = BlockCode::Byte(size);
            let bytes = frame(&h, |w| put_verbatim(w, &a, 16));
            assert_eq!(decode(&bytes, 1, 16).unwrap(), vec![as_i32(&a)]);
        }
    }
}

// --- Sample rate codes ------------------------------------------------------

#[test]
fn every_sample_rate_code() {
    let table: [(u8, u32); 12] = [
        (0, 0), // only stored in STREAMINFO
        (1, 88_200),
        (2, 176_400),
        (3, 192_000),
        (4, 8_000),
        (5, 16_000),
        (6, 22_050),
        (7, 24_000),
        (8, 32_000),
        (9, 44_100),
        (10, 48_000),
        (11, 96_000),
    ];
    for (code, rate) in table {
        let mut h = Header::new(192, 0, 4);
        h.rate = RateCode::Table(code);
        assert_eq!(parse_frame_header(&h.bytes()).unwrap().sample_rate, rate);
    }
    let mut h = Header::new(192, 0, 4);
    h.rate = RateCode::Table(15);
    assert!(
        parse_frame_header(&h.bytes()).is_err(),
        "code 15 is forbidden"
    );

    // The three forms stored at the end of the header, at their limits.
    for khz in [0u8, 1, 96, 255] {
        h.rate = RateCode::KiloHertz(khz);
        let parsed = parse_frame_header(&h.bytes()).unwrap();
        assert_eq!(parsed.sample_rate, khz as u32 * 1000);
    }
    for hz in [0u16, 1, 11_025, 44_100, 65_535] {
        h.rate = RateCode::Hertz(hz);
        assert_eq!(
            parse_frame_header(&h.bytes()).unwrap().sample_rate,
            hz as u32
        );
    }
    for dahz in [0u16, 1, 4_410, 38_400, 65_535] {
        h.rate = RateCode::DecaHertz(dahz);
        let parsed = parse_frame_header(&h.bytes()).unwrap();
        assert_eq!(parsed.sample_rate, dahz as u32 * 10);
    }
}

/// §9.1.6 / §9.1.7: the uncommon block size follows the coded number and
/// the uncommon sample rate follows the uncommon block size.
#[test]
fn uncommon_block_size_precedes_uncommon_sample_rate() {
    for (block, block_len) in [
        (BlockCode::Table(12), 0usize),
        (BlockCode::Byte(200), 1),
        (BlockCode::Word(0x1234 + 1), 2),
    ] {
        for (rate, rate_len, hz) in [
            (RateCode::Table(9), 0usize, 44_100u32),
            (RateCode::KiloHertz(0x31), 1, 49_000),
            (RateCode::Hertz(0xABCD), 2, 0xABCD),
            (RateCode::DecaHertz(0x5678), 2, 0x5678 * 10),
        ] {
            for number in [0u64, 0x7F, 0x80, 0x7FFF_FFFF] {
                let mut h = Header::new(0, 1, 6);
                h.block = block;
                h.rate = rate;
                h.number = number;
                let bytes = h.bytes();
                let parsed = parse_frame_header(&bytes).unwrap();
                let want_block = match block {
                    BlockCode::Table(_) => 4096,
                    BlockCode::Byte(s) | BlockCode::Word(s) => s,
                };
                assert_eq!(parsed.block_size, want_block);
                assert_eq!(parsed.sample_rate, hz);
                assert_eq!(parsed.coded_number, number);
                assert_eq!(parsed.bits_per_sample, 24);
                assert_eq!(parsed.channels, ChannelAssignment::Independent(2));
                let want_len = 4 + coded_number(number).len() + block_len + rate_len + 1;
                assert_eq!(parsed.header_byte_len, want_len);
                assert_eq!(bytes.len(), want_len);
                // Every proper prefix asks for more bytes instead of
                // failing: a header split across two reads is not a bad
                // header.
                for cut in 0..bytes.len() {
                    assert!(
                        matches!(
                            parse_frame_header(&bytes[..cut]),
                            Err(oxideav_core::Error::NeedMore)
                        ),
                        "prefix of {cut} bytes"
                    );
                }
            }
        }
    }
}

// --- Coded number -----------------------------------------------------------

#[test]
fn coded_number_at_the_limits_of_every_length() {
    // Table 18: first and last number of each sequence length.
    let limits: [(u64, u64, usize); 7] = [
        (0x0, 0x7F, 1),
        (0x80, 0x7FF, 2),
        (0x800, 0xFFFF, 3),
        (0x1_0000, 0x1F_FFFF, 4),
        (0x20_0000, 0x3FF_FFFF, 5),
        (0x400_0000, 0x7FFF_FFFF, 6),
        (0x8000_0000, 0xF_FFFF_FFFF, 7),
    ];
    for (first, last, len) in limits {
        for number in [first, first + 1, (first + last) / 2, last - 1, last] {
            assert_eq!(coded_number(number).len(), len);
            let mut h = Header::new(192, 0, 4);
            h.variable = true;
            h.number = number;
            let parsed = parse_frame_header(&h.bytes()).unwrap();
            assert_eq!(parsed.coded_number, number);
            assert_eq!(parsed.blocking_strategy, BlockingStrategy::Variable);
            // A sample number is the first sample of the frame.
            assert_eq!(parsed.first_sample(4096), number);

            // "When a frame number is encoded, the value MUST NOT be
            // larger than what fits a value of 31 bits unencoded or 6
            // bytes encoded."
            h.variable = false;
            let parsed = parse_frame_header(&h.bytes());
            if len <= 6 {
                let parsed = parsed.unwrap();
                assert_eq!(parsed.blocking_strategy, BlockingStrategy::Fixed);
                assert_eq!(parsed.first_sample(4096), number * 4096);
            } else {
                assert!(parsed.is_err(), "frame number {number:#x}");
            }
        }
    }
    // The worked example of §9.1.5: 51 billion samples.
    assert_eq!(
        coded_number(51_000_000_000),
        [0xFE, 0xAF, 0x9F, 0xB5, 0xA3, 0xB8, 0x80]
    );
}

#[test]
fn malformed_coded_numbers() {
    let with_number = |number: &[u8]| {
        let mut bytes = vec![0xFF, 0xF9, 0x19, 0x08];
        bytes.extend_from_slice(number);
        bytes.push(crc8(&bytes));
        bytes
    };
    assert!(parse_frame_header(&with_number(&[0x00])).is_ok());
    // A continuation byte or 0xFF cannot start a number.
    for lead in (0x80..=0xBFu8).chain([0xFF]) {
        let bytes = with_number(&[lead, 0x80, 0x80, 0x80, 0x80, 0x80, 0x80]);
        assert!(
            matches!(
                parse_frame_header(&bytes),
                Err(oxideav_core::Error::InvalidData(_))
            ),
            "lead {lead:#04x}"
        );
    }
    // Every following byte must be 0b10xxxxxx.
    for len in 2..=7usize {
        let lead = (0xFFu16 << (8 - len)) as u8;
        for bad_at in 1..len {
            for bad in [0x00u8, 0x7F, 0xC0, 0xFF] {
                let mut number = vec![0x80u8; len];
                number[0] = lead;
                number[bad_at] = bad;
                assert!(
                    matches!(
                        parse_frame_header(&with_number(&number)),
                        Err(oxideav_core::Error::InvalidData(_))
                    ),
                    "length {len}, byte {bad_at} = {bad:#04x}"
                );
            }
        }
    }
}

// --- Fixed header fields ----------------------------------------------------

#[test]
fn channel_and_bit_depth_codes() {
    for code in 0..=7u8 {
        let h = Header::new(16, code, 4);
        let parsed = parse_frame_header(&h.bytes()).unwrap();
        assert_eq!(parsed.channels, ChannelAssignment::Independent(code + 1));
        // One constant subframe per channel.
        let bytes = frame(&h, |w| {
            for c in 0..=code as i64 {
                put_constant(w, 100 * c - 300, 16);
            }
        });
        let want: Vec<Vec<i32>> = (0..=code as i32).map(|c| vec![100 * c - 300; 16]).collect();
        assert_eq!(decode(&bytes, code + 1, 16).unwrap(), want);
    }
    for (code, want) in [
        (8u8, ChannelAssignment::LeftSide),
        (9, ChannelAssignment::RightSide),
        (10, ChannelAssignment::MidSide),
    ] {
        let parsed = parse_frame_header(&Header::new(16, code, 4).bytes()).unwrap();
        assert_eq!(parsed.channels, want);
    }
    for code in 11..=15u8 {
        assert!(parse_frame_header(&Header::new(16, code, 4).bytes()).is_err());
    }

    for (code, bits) in [(1u8, 8u8), (2, 12), (4, 16), (5, 20), (6, 24), (7, 32)] {
        let h = Header::new(8, 0, code);
        assert_eq!(
            parse_frame_header(&h.bytes()).unwrap().bits_per_sample,
            bits
        );
        let hi = (1i64 << (bits - 1)) - 1;
        let samples = [hi, -hi - 1, 0, -1, 1, hi, -hi - 1, 2];
        let bytes = frame(&h, |w| put_verbatim(w, &samples, bits as u32));
        assert_eq!(decode(&bytes, 1, bits).unwrap(), vec![as_i32(&samples)]);
    }
    assert!(parse_frame_header(&Header::new(8, 0, 3).bytes()).is_err());
    // Code 0: the depth is only in STREAMINFO, which allows 4..=32.
    for bits in [4u8, 5, 7, 13, 17, 23, 31, 32] {
        let h = Header::new(8, 0, 0);
        assert_eq!(parse_frame_header(&h.bytes()).unwrap().bits_per_sample, 0);
        let hi = (1i64 << (bits - 1)) - 1;
        let samples = [hi, -hi - 1, 0, -1, 1, hi, -hi - 1, 2];
        let bytes = frame(&h, |w| put_verbatim(w, &samples, bits as u32));
        let decoded = decode_frame_channels(&bytes, &stream_info(1, bits)).unwrap();
        assert_eq!(decoded.bits_per_sample, bits as u32);
        assert_eq!(decoded.channels, vec![as_i32(&samples)]);
    }
}

#[test]
fn sync_code_and_reserved_bits() {
    let good = Header::new(192, 0, 4).bytes();
    assert!(parse_frame_header(&good).is_ok());
    // The reserved bit after the bit depth "MUST be zero".
    let mut h = Header::new(192, 0, 4);
    h.reserved_bit = true;
    assert!(parse_frame_header(&h.bytes()).is_err());
    // The 15-bit sync code is 0b111111111111100: nothing else will do.
    for second in [0xFAu8, 0xFB, 0xFC, 0xF0, 0x00, 0xFF] {
        let mut bytes = good.clone();
        bytes[1] = second;
        let last = bytes.len() - 1;
        bytes[last] = crc8(&bytes[..last]);
        assert!(
            parse_frame_header(&bytes).is_err(),
            "second byte {second:#04x}"
        );
    }
    let mut bytes = good.clone();
    bytes[0] = 0xFE;
    assert!(parse_frame_header(&bytes).is_err());
}

// --- Checksums --------------------------------------------------------------

#[test]
fn header_crc8_covers_every_bit() {
    let mut h = Header::new(0x1234, 1, 6);
    h.rate = RateCode::Hertz(0x8421);
    h.number = 0x12345;
    let good = h.bytes();
    assert!(parse_frame_header(&good).is_ok());
    for bit in 0..good.len() * 8 {
        let mut bytes = good.clone();
        bytes[bit / 8] ^= 0x80 >> (bit % 8);
        assert!(parse_frame_header(&bytes).is_err(), "bit {bit}");
    }
}

#[test]
fn frame_crc16_covers_every_bit() {
    let samples = noise(24, 16, 9);
    let good = frame(&Header::new(24, 0, 4), |w| put_verbatim(w, &samples, 16));
    assert_eq!(decode(&good, 1, 16).unwrap(), vec![as_i32(&samples)]);
    for bit in 0..good.len() * 8 {
        let mut bytes = good.clone();
        bytes[bit / 8] ^= 0x80 >> (bit % 8);
        assert!(
            decode_frame_channels(&bytes, &stream_info(1, 16)).is_err(),
            "bit {bit}"
        );
    }
    // A frame cut short anywhere is an error, not a partial block.
    for cut in 0..good.len() {
        assert!(decode_frame_channels(&good[..cut], &stream_info(1, 16)).is_err());
    }
    // Bytes after the frame are not part of it.
    let mut longer = good.clone();
    longer.extend_from_slice(&[0xAA; 9]);
    let decoded = decode_frame_channels(&longer, &stream_info(1, 16)).unwrap();
    assert_eq!(decoded.frame_byte_len, good.len());
}

/// §9.3: the subframes are followed by zero bits up to the byte boundary;
/// the count depends on where the last subframe ends.
#[test]
fn padding_to_the_byte_boundary_for_every_bit_offset() {
    for bits in 4..=11u32 {
        // One 3-sample verbatim subframe of `bits`-bit samples: the body
        // is `8 + 3 * bits` bits long, covering every offset modulo 8.
        let hi = (1i64 << (bits - 1)) - 1;
        let samples = [hi, -hi - 1, -1];
        let bytes = frame(&Header::new(3, 0, 0), |w| put_verbatim(w, &samples, bits));
        let decoded = decode_frame_channels(&bytes, &stream_info(1, bits as u8)).unwrap();
        assert_eq!(decoded.channels, vec![as_i32(&samples)]);
        assert_eq!(decoded.frame_byte_len, bytes.len());
    }
}
