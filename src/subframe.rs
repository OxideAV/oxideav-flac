//! Subframe decoding: constant, verbatim, fixed predictor, linear
//! predictor, and the partitioned Rice coded residual (RFC 9639 §9.2).
//!
//! Everything decodes straight into a caller-supplied sample slice, so a
//! frame decoder that keeps its planes between frames allocates nothing
//! per frame. The slice element is [`Sample`]: `i32` for ordinary
//! channels, `i64` for the 33-bit side channel of a 32-bit
//! stereo-decorrelated frame (RFC 9639 §4.2, Appendix A.2). Predictions
//! are always accumulated in 64 bits (Appendix A.3: a 33-bit sample times
//! a 15-bit coefficient summed over 32 taps needs 53 bits), with wrapping
//! arithmetic so that a stream which breaks the format's value limits
//! yields garbage samples rather than an overflow trap.

use oxideav_core::{Error, Result};

use crate::reservoir::{BitReservoir, Sample};

/// Largest linear-predictor order (RFC 9639 §9.2.1, Table 19).
const MAX_LPC_ORDER: usize = 32;

/// Decode one subframe of `block_size` samples at `bps` bits per sample
/// (`1..=32`) into a new vector.
///
/// `bps` is the bit depth of *this* subframe: the caller adds one for the
/// side channel of a left-side, side-right or mid-side frame. A 33-bit
/// side channel does not fit `i32` and goes through
/// [`decode_subframe_wide`]; this function reports
/// [`Error::Unsupported`] for it.
pub fn decode_subframe(br: &mut BitReservoir, block_size: u32, bps: u32) -> Result<Vec<i32>> {
    if bps == 0 || bps > 32 {
        return Err(Error::unsupported(format!(
            "FLAC subframe bps {bps} out of supported range 1..=32"
        )));
    }
    let mut samples = vec![0i32; block_size as usize];
    decode_subframe_into(br, &mut samples, bps)?;
    Ok(samples)
}

/// Decode one 33-bit subframe — the side channel of a 32-bit
/// stereo-decorrelated frame — into a new `i64` vector. Any other width
/// is a caller bug and returns [`Error::Unsupported`].
pub fn decode_subframe_wide(br: &mut BitReservoir, block_size: u32, bps: u32) -> Result<Vec<i64>> {
    if bps != 33 {
        return Err(Error::unsupported(format!(
            "FLAC wide subframe bps {bps} (the wide path handles only the \
             33-bit side channel of a 32-bit decorrelated frame)"
        )));
    }
    let mut samples = vec![0i64; block_size as usize];
    decode_subframe_into(br, &mut samples, bps)?;
    Ok(samples)
}

/// Decode one subframe into `out`; the block size is `out.len()`.
///
/// `bps` must be `1..=32` for an `i32` slice and `1..=33` for an `i64`
/// slice. On error the contents of `out` are unspecified.
pub fn decode_subframe_into<S: Sample>(
    br: &mut BitReservoir,
    out: &mut [S],
    bps: u32,
) -> Result<()> {
    debug_assert!((1..=33).contains(&bps));
    // §9.2.1: one zero bit, six type bits, the wasted-bits flag.
    let header = br.read_u32(8)?;
    if header & 0x80 != 0 {
        return Err(Error::invalid(
            "FLAC subframe: first header bit must be zero",
        ));
    }
    let type_code = (header >> 1) & 0x3F;
    // §9.2.2: the flag is followed by (wasted bits - 1) in unary.
    let wasted = if header & 1 != 0 {
        br.read_unary()?.saturating_add(1)
    } else {
        0
    };
    if wasted >= bps {
        return Err(Error::invalid(format!(
            "FLAC subframe: {wasted} wasted bits leave no sample bits at depth {bps}"
        )));
    }
    let depth = bps - wasted;

    match type_code {
        0b000000 => {
            let v = S::from_i64(br.read_signed(depth)?);
            out.fill(v);
        }
        0b000001 => br.read_signed_run(depth, out)?,
        0b001000..=0b001100 => {
            let order = (type_code - 8) as usize;
            read_warm_up(br, out, order, depth)?;
            decode_residual(br, out, order)?;
            restore_fixed(out, order);
        }
        0b100000..=0b111111 => {
            let order = (type_code - 31) as usize;
            read_warm_up(br, out, order, depth)?;
            // §9.2.6: (precision - 1) in 4 bits, 0b1111 forbidden.
            let precision = br.read_u32(4)? + 1;
            if precision == 16 {
                return Err(Error::invalid(
                    "FLAC linear predictor: forbidden coefficient precision 0b1111",
                ));
            }
            // A 5-bit two's-complement shift that MUST NOT be negative.
            let shift = br.read_u32(5)?;
            if shift & 0x10 != 0 {
                return Err(Error::invalid(
                    "FLAC linear predictor: negative prediction shift",
                ));
            }
            let mut coefficients = [0i32; MAX_LPC_ORDER];
            br.read_signed_run(precision, &mut coefficients[..order])?;
            decode_residual(br, out, order)?;
            restore_linear(out, &coefficients[..order], shift);
        }
        _ => {
            return Err(Error::invalid(format!(
                "FLAC subframe: reserved type 0b{type_code:06b}"
            )));
        }
    }

    if wasted > 0 {
        // §9.2.2: pad with `wasted` zero bits by shifting left.
        for s in out.iter_mut() {
            *s = S::from_i64(s.to_i64() << wasted);
        }
    }
    Ok(())
}

/// Read the `order` unencoded warm-up samples that open a predicted
/// subframe (§9.2.5, §9.2.6). A predictor needs more samples than its
/// order to have anything to predict, so an order that does not leave at
/// least one residual sample is rejected here, before anything is
/// written past the block.
fn read_warm_up<S: Sample>(
    br: &mut BitReservoir,
    out: &mut [S],
    order: usize,
    depth: u32,
) -> Result<()> {
    if order >= out.len() {
        return Err(Error::invalid(format!(
            "FLAC subframe: predictor order {order} needs a block larger than {} samples",
            out.len()
        )));
    }
    br.read_signed_run(depth, &mut out[..order])
}

/// Decode the coded residual (§9.2.7) of a predicted subframe into
/// `out[order..]`.
///
/// The residual is cut into `2^partition order` partitions of
/// `block size >> partition order` samples each, the first one shortened
/// by the predictor order. Two MUSTs bound the partition order: the block
/// size is evenly divisible by the partition count, and
/// `block size >> partition order` is larger than the predictor order —
/// so the first partition always holds at least one residual sample, and
/// a stream where it would hold none (or fewer) is invalid.
fn decode_residual<S: Sample>(br: &mut BitReservoir, out: &mut [S], order: usize) -> Result<()> {
    let block_size = out.len();
    // Two method bits and four partition-order bits.
    let lead = br.read_u32(6)?;
    let (parameter_bits, escape) = match lead >> 4 {
        0b00 => (4u32, 0b1111u32),
        0b01 => (5, 0b11111),
        _ => return Err(Error::invalid("FLAC residual: reserved coding method")),
    };
    let partition_order = lead & 0xF;
    let partition_len = block_size >> partition_order;
    if partition_len << partition_order != block_size {
        return Err(Error::invalid(format!(
            "FLAC residual: block size {block_size} is not divisible by 2^{partition_order} partitions"
        )));
    }
    if partition_len <= order {
        return Err(Error::invalid(format!(
            "FLAC residual: partition of {partition_len} samples is not larger than predictor order {order}"
        )));
    }

    let mut rest = &mut out[order..];
    let mut len = partition_len - order;
    for _ in 0..1usize << partition_order {
        let (partition, tail) = rest.split_at_mut(len);
        rest = tail;
        len = partition_len;
        let parameter = br.read_u32(parameter_bits)?;
        if parameter == escape {
            // §9.2.7.1: 5 bits of width, then that many bits per sample;
            // width 0 means every residual in the partition is zero.
            let width = br.read_u32(5)?;
            br.read_signed_run(width, partition)?;
        } else {
            br.read_rice_run(parameter, partition)?;
        }
    }
    Ok(())
}

/// Add the fixed predictor of §9.2.5 (Table 20) back onto the residual.
/// On entry `buf` is `[warm-up (order) | residual]`; on return it is the
/// decoded signal. The last `order` decoded samples are carried in
/// locals, so the loop touches each slot exactly once.
fn restore_fixed<S: Sample>(buf: &mut [S], order: usize) {
    let (warm, rest) = buf.split_at_mut(order);
    let w = |i: usize| warm[i].to_i64();
    match order {
        // Prediction 0: the residual is the signal.
        0 => {}
        1 => {
            let mut a = w(0);
            for s in rest {
                *s = S::from_i64(s.to_i64().wrapping_add(a));
                a = s.to_i64();
            }
        }
        2 => {
            let (mut b, mut a) = (w(0), w(1));
            for s in rest {
                let p = a.wrapping_mul(2).wrapping_sub(b);
                *s = S::from_i64(s.to_i64().wrapping_add(p));
                (b, a) = (a, s.to_i64());
            }
        }
        3 => {
            let (mut c, mut b, mut a) = (w(0), w(1), w(2));
            for s in rest {
                let p = a.wrapping_sub(b).wrapping_mul(3).wrapping_add(c);
                *s = S::from_i64(s.to_i64().wrapping_add(p));
                (c, b, a) = (b, a, s.to_i64());
            }
        }
        _ => {
            let (mut d, mut c, mut b, mut a) = (w(0), w(1), w(2), w(3));
            for s in rest {
                let p = a
                    .wrapping_add(c)
                    .wrapping_mul(4)
                    .wrapping_sub(b.wrapping_mul(6))
                    .wrapping_sub(d);
                *s = S::from_i64(s.to_i64().wrapping_add(p));
                (d, c, b, a) = (c, b, a, s.to_i64());
            }
        }
    }
}

/// Add the linear predictor of §9.2.6 back onto the residual. On entry
/// `buf` is `[warm-up (order) | residual]`. `coefficients[0]` multiplies
/// the sample directly before the predicted one, `coefficients[1]` the
/// one before that, and so on; the sum is shifted right arithmetically by
/// `shift` and added to the residual.
///
/// One loop is compiled per order (1..=32), each over a fixed-length
/// window, so the inner product has a constant trip count.
fn restore_linear<S: Sample>(buf: &mut [S], coefficients: &[i32], shift: u32) {
    macro_rules! by_order {
        ($($n:literal)*) => {
            match coefficients.len() {
                $($n => restore_linear_n::<S, $n>(buf, coefficients, shift),)*
                _ => unreachable!("linear predictor order is 1..=32"),
            }
        };
    }
    by_order!(1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16
              17 18 19 20 21 22 23 24 25 26 27 28 29 30 31 32);
}

#[inline(always)]
fn restore_linear_n<S: Sample, const N: usize>(buf: &mut [S], coefficients: &[i32], shift: u32) {
    // Oldest-first copy of the coefficients, so `taps[j]` pairs with
    // `window[j]` when the window is in time order.
    let mut taps = [0i64; N];
    for (tap, &c) in taps.iter_mut().zip(coefficients.iter().rev()) {
        *tap = c as i64;
    }
    for t in N..buf.len() {
        let (past, current) = buf.split_at_mut(t);
        let window: &[S; N] = past[t - N..].try_into().expect("window of N samples");
        let mut sum = 0i64;
        for (tap, s) in taps.iter().zip(window) {
            sum = sum.wrapping_add(tap.wrapping_mul(s.to_i64()));
        }
        let slot = &mut current[0];
        *slot = S::from_i64(slot.to_i64().wrapping_add(sum >> shift));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The narrow entry point holds `i32` samples, so a 33-bit side
    /// channel (and a zero depth) is refused up front.
    #[test]
    fn decode_subframe_rejects_bps_above_32_without_panic() {
        let bytes = [0u8; 16];
        let mut br = BitReservoir::new(&bytes);
        let err = decode_subframe(&mut br, 4096, 33).expect_err("bps 33 must be rejected");
        match err {
            Error::Unsupported(_) => {}
            other => panic!("expected Error::Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn decode_subframe_rejects_bps_zero_without_panic() {
        let bytes = [0u8; 16];
        let mut br = BitReservoir::new(&bytes);
        let err = decode_subframe(&mut br, 4096, 0).expect_err("bps 0 must be rejected");
        assert!(matches!(err, Error::Unsupported(_)));
    }

    // --- 33-bit wide side-channel path (RFC 9639 §4.2 / A.2) ------------
    use oxideav_core::bits::BitWriter;

    /// Encode a chosen residual through the zig-zag + Rice mapping the
    /// wide decoder inverts (`decode_residual`), so a test can
    /// build a Rice-coded partition by hand. `-((u>>1))-1` for odd is the
    /// inverse of this `2r`/`-2r-1` fold.
    fn write_rice(w: &mut BitWriter, r: i64, k: u32) {
        let unsigned: u64 = if r >= 0 {
            (r as u64) << 1
        } else {
            ((-(r + 1)) as u64) << 1 | 1
        };
        let q = (unsigned >> k) as u32;
        let rem = (unsigned & ((1u64 << k) - 1)) as u32;
        w.write_unary(q);
        if k > 0 {
            w.write_u32(rem, k);
        }
    }

    /// The wide CONSTANT subframe carries one 33-bit value that a 32-bit
    /// read would misinterpret as negative — proves the widening matters.
    #[test]
    fn wide_constant_decodes_full_33bit_value() {
        // 3_000_000_000 has bit 31 set; a 32-bit signed read would render
        // it as a negative number. As a 33-bit value it is positive.
        let val: i64 = 3_000_000_000;
        let mut w = BitWriter::new();
        w.write_u32(0, 1); // pad
        w.write_u32(0b000000, 6); // CONSTANT
        w.write_u32(0, 1); // no wasted bits
        w.write_u64(val as u64, 33);
        w.align_to_byte();
        let bytes = w.finish();
        let mut br = BitReservoir::new(&bytes);
        let s = decode_subframe_wide(&mut br, 6, 33).unwrap();
        assert_eq!(s, vec![val; 6]);
    }

    #[test]
    fn wide_verbatim_roundtrips_extreme_33bit_samples() {
        // Values spanning the full 33-bit signed range.
        let vals: [i64; 5] = [
            0,
            4_294_967_295,
            -4_294_967_296,
            2_500_000_000,
            -2_500_000_000,
        ];
        let mut w = BitWriter::new();
        w.write_u32(0, 1);
        w.write_u32(0b000001, 6); // VERBATIM
        w.write_u32(0, 1);
        for &v in &vals {
            w.write_u64(v as u64, 33);
        }
        w.align_to_byte();
        let bytes = w.finish();
        let mut br = BitReservoir::new(&bytes);
        let s = decode_subframe_wide(&mut br, vals.len() as u32, 33).unwrap();
        assert_eq!(s, vals.to_vec());
    }

    #[test]
    fn wide_fixed_order2_reconstructs_via_escape_residual() {
        // Second-order fixed: sample[t] = 2*s[t-1] - s[t-2] + residual[t].
        // Warm-up samples sit near the 33-bit ceiling so the reconstruction
        // genuinely exercises i64 storage.
        let w0: i64 = 4_000_000_000;
        let w1: i64 = 4_100_000_000;
        let residuals: [i64; 4] = [10, -20, 30, -40];
        // Manually reconstruct the expected signal.
        let mut expected = vec![w0, w1];
        for &r in &residuals {
            let t = expected.len();
            let p = 2 * expected[t - 1] - expected[t - 2];
            expected.push(p + r);
        }

        let mut w = BitWriter::new();
        w.write_u32(0, 1);
        w.write_u32(0b001010, 6); // FIXED order 2
        w.write_u32(0, 1);
        w.write_u64(w0 as u64, 33);
        w.write_u64(w1 as u64, 33);
        // Residual: method 0 (4-bit params), partition order 0, escape (k=15).
        w.write_u32(0, 2); // method
        w.write_u32(0, 4); // partition order
        w.write_u32(15, 4); // escape marker
        w.write_u32(20, 5); // raw_bps = 20 (comfortably holds the residuals)
        for &r in &residuals {
            w.write_u32((r as i32) as u32 & 0xF_FFFF, 20);
        }
        w.align_to_byte();
        let bytes = w.finish();
        let mut br = BitReservoir::new(&bytes);
        let s = decode_subframe_wide(&mut br, 6, 33).unwrap();
        assert_eq!(s, expected);
    }

    #[test]
    fn wide_lpc_order1_reconstructs_via_rice_residual() {
        // First-order LPC: pred = (coeff * s[t-1]) >> shift; add residual.
        let coeff: i64 = 3; // qlp precision 4 bits holds -8..7
        let shift: u32 = 1;
        let w0: i64 = 2_000_000_000;
        let residuals: [i64; 5] = [5, -7, 11, -3, 0];
        let mut expected = vec![w0];
        for &r in &residuals {
            let prev = *expected.last().unwrap();
            let pred = (coeff * prev) >> shift;
            expected.push(pred + r);
        }

        let mut w = BitWriter::new();
        w.write_u32(0, 1);
        w.write_u32(0b100000, 6); // LPC order 1 ((0 & 0x1F) + 1)
        w.write_u32(0, 1); // no wasted
        w.write_u64(w0 as u64, 33); // warm-up
        w.write_u32(4 - 1, 4); // qlp precision 4 → field value precision-1
        w.write_u32(shift, 5); // qlp shift
        w.write_u32(coeff as u32 & 0xF, 4); // one 4-bit coefficient
        // Residual: method 0, partition order 0, Rice param k=3.
        w.write_u32(0, 2);
        w.write_u32(0, 4);
        w.write_u32(3, 4); // k
        for &r in &residuals {
            write_rice(&mut w, r, 3);
        }
        w.align_to_byte();
        let bytes = w.finish();
        let mut br = BitReservoir::new(&bytes);
        let s = decode_subframe_wide(&mut br, 6, 33).unwrap();
        assert_eq!(s, expected);
    }

    #[test]
    fn wide_rejects_non_33_bps() {
        let bytes = [0u8; 8];
        let mut br = BitReservoir::new(&bytes);
        assert!(matches!(
            decode_subframe_wide(&mut br, 16, 34),
            Err(Error::Unsupported(_))
        ));
    }

    /// A hostile stream can drive the 4th-order fixed reconstruction past
    /// `i64::MAX` (the sample sequence grows ~ `t^4`). Without wrapping
    /// arithmetic that overflow panics in a debug/fuzz build; the wrapping
    /// predictor must complete instead. Exercises the panic-freedom
    /// contract at the predictor directly (the widest block a §9.1 header
    /// can encode).
    #[test]
    fn wide_fixed_predictor_order4_wraps_without_panic() {
        let mut buf = vec![i64::MAX; 65535];
        restore_fixed(&mut buf, 4);
    }

    /// Same contract for the wide LPC reconstruction: a maximal
    /// coefficient against maximal samples must wrap, not panic.
    #[test]
    fn wide_lpc_predictor_wraps_without_panic() {
        let mut buf = vec![i64::MAX; 4096];
        let coeffs = [i32::MAX; 32]; // wider than any real coefficient
        restore_linear(&mut buf, &coeffs, 0);
    }

    /// Wasted-bits on the wide path: the unary-coded count reduces the
    /// effective read width, and the final left-shift restores the low
    /// zero bits. Values are chosen divisible by `2^wasted` so the shift
    /// round-trips exactly, and near the 33-bit ceiling so the shifted-out
    /// magnitude genuinely needs the wide storage.
    #[test]
    fn wide_verbatim_honours_wasted_bits() {
        let wasted = 2u32;
        let eff = 33 - wasted; // 31-bit stored samples
        let vals: [i64; 3] = [4_000_000_000, -2_000_000_004, 8];
        for &v in &vals {
            assert_eq!(v % (1 << wasted), 0, "test value must be divisible");
        }
        let mut w = BitWriter::new();
        w.write_u32(0, 1); // pad
        w.write_u32(0b000001, 6); // VERBATIM
        w.write_u32(1, 1); // has wasted bits
        w.write_unary(wasted - 1); // unary count → wasted = count + 1
        for &v in &vals {
            w.write_u64((v >> wasted) as u64, eff);
        }
        w.align_to_byte();
        let bytes = w.finish();
        let mut br = BitReservoir::new(&bytes);
        let s = decode_subframe_wide(&mut br, vals.len() as u32, 33).unwrap();
        assert_eq!(s, vals.to_vec());
    }

    /// Wide FIXED order-4 correctness (the panic test only proved the
    /// order-4 predictor wraps, not that it reconstructs the right values).
    #[test]
    fn wide_fixed_order4_reconstructs_via_escape_residual() {
        let warmup: [i64; 4] = [4_000_000_000, 4_100_000_000, 4_150_000_000, 4_180_000_000];
        let residuals: [i64; 4] = [3, -5, 7, -2];
        let mut expected = warmup.to_vec();
        for &r in &residuals {
            let t = expected.len();
            let p =
                4 * expected[t - 1] - 6 * expected[t - 2] + 4 * expected[t - 3] - expected[t - 4];
            expected.push(p + r);
        }

        let mut w = BitWriter::new();
        w.write_u32(0, 1);
        w.write_u32(0b001100, 6); // FIXED order 4
        w.write_u32(0, 1);
        for &v in &warmup {
            w.write_u64(v as u64, 33);
        }
        w.write_u32(0, 2); // residual method 0
        w.write_u32(0, 4); // partition order 0
        w.write_u32(15, 4); // escape marker
        w.write_u32(8, 5); // raw_bps = 8
        for &r in &residuals {
            w.write_u32((r as i32) as u32 & 0xFF, 8);
        }
        w.align_to_byte();
        let bytes = w.finish();
        let mut br = BitReservoir::new(&bytes);
        let s = decode_subframe_wide(&mut br, 8, 33).unwrap();
        assert_eq!(s, expected);
    }
}
