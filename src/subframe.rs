//! Subframe decoding: constant, verbatim, fixed-predictor, LPC.
//!
//! Reference: <https://xiph.org/flac/format.html#subframe>

use oxideav_core::{Error, Result};

use crate::bits_ext::BitReaderExt;
use oxideav_core::bits::BitReader;

/// Decode one subframe from the bitstream.
///
/// `bps` is the per-sample bit depth for *this* channel — caller must add 1
/// for the side channel of left-side / right-side / mid-side stereo frames.
///
/// Returns [`Error::Unsupported`] if `bps` exceeds 32. The decoder reads
/// samples through `BitReader::read_i32`, which is bounded at 32-bit
/// reads, so a 33-bit side channel (which arises from a STREAMINFO
/// declaring `bits_per_sample=32` combined with a stereo-decorrelated
/// frame) does not fit in `i32`. The caller decodes that one channel
/// through [`decode_subframe_wide`] instead, which returns `i64`
/// samples; this narrow path stays `i32`-only so the common ≤ 32-bit
/// hot loop is never widened. Rejecting `bps > 32` up front here (rather
/// than panicking inside the bit reader) was first added for the
/// `panic_free_decode` fuzz harness on 2026-05-10.
pub fn decode_subframe(br: &mut BitReader, block_size: u32, bps: u32) -> Result<Vec<i32>> {
    if bps == 0 || bps > 32 {
        return Err(Error::unsupported(format!(
            "FLAC subframe bps {bps} out of supported range 1..=32 \
             (32-bit samples cannot be combined with stereo decorrelation)"
        )));
    }
    let pad = br.read_u32(1)?;
    if pad != 0 {
        return Err(Error::invalid("subframe header pad bit must be zero"));
    }
    let type_code = br.read_u32(6)? as u8;
    let has_wasted = br.read_bit()?;
    let wasted = if has_wasted {
        // Wasted-bit count is unary-encoded — count of leading zeros, plus 1
        // (because at least one wasted bit was indicated).
        br.read_unary()? + 1
    } else {
        0
    };

    if wasted >= bps {
        return Err(Error::invalid(format!("wasted bits {wasted} >= bps {bps}")));
    }
    let effective_bps = bps - wasted;

    let mut samples = match type_code {
        0b000000 => decode_constant(br, block_size, effective_bps)?,
        0b000001 => decode_verbatim(br, block_size, effective_bps)?,
        0b001000..=0b001100 => {
            let order = (type_code & 0x07) as usize;
            decode_fixed(br, block_size, effective_bps, order)?
        }
        0b100000..=0b111111 => {
            let order = ((type_code & 0x1F) + 1) as usize;
            decode_lpc(br, block_size, effective_bps, order)?
        }
        _ => {
            return Err(Error::invalid(format!(
                "reserved FLAC subframe type 0x{type_code:02x}"
            )));
        }
    };

    if wasted > 0 {
        for s in samples.iter_mut() {
            *s = ((*s as i64) << wasted) as i32;
        }
    }
    Ok(samples)
}

fn decode_constant(br: &mut BitReader, block_size: u32, bps: u32) -> Result<Vec<i32>> {
    let v = br.read_i32(bps)?;
    Ok(vec![v; block_size as usize])
}

fn decode_verbatim(br: &mut BitReader, block_size: u32, bps: u32) -> Result<Vec<i32>> {
    let mut s = Vec::with_capacity(block_size as usize);
    for _ in 0..block_size {
        s.push(br.read_i32(bps)?);
    }
    Ok(s)
}

fn decode_fixed(br: &mut BitReader, block_size: u32, bps: u32, order: usize) -> Result<Vec<i32>> {
    let mut samples = Vec::with_capacity(block_size as usize);
    for _ in 0..order {
        samples.push(br.read_i32(bps)?);
    }
    // Decode the residual straight onto the tail of `samples` (no separate
    // residual buffer / copy), then reconstruct in place.
    decode_residual_into(br, &mut samples, block_size, order)?;
    apply_fixed_predictor(&mut samples, order);
    Ok(samples)
}

/// Reconstruct the block *in place* by running the §9.2.6 fixed
/// predictor. On entry `samples` holds `[warm-up(order) | residuals]`
/// (the residual coder wrote its output straight onto the tail). Each
/// position `t >= order` is overwritten by
/// `prediction(samples[t-order..t]) + residual[t]`; the predictor only
/// reads *earlier* positions, so overwriting `samples[t]` after reading
/// its window is safe and needs no separate residual buffer.
///
/// Each order's difference equation is unrolled into a branch-free
/// closed form so the per-sample inner loop carries no coefficient
/// iterator, no `samples.len()` recomputation, and — because it is
/// driven by an absolute index `t` with the window `samples[t-order..t]`
/// — no per-access bounds check. Output is bit-identical to the naive
/// `sum(coeff[i] * samples[len-1-i]) + r` form: the coefficients here
/// are the same Table-of-differences constants
/// (`{}`, `{1}`, `{2,-1}`, `{3,-3,1}`, `{4,-6,4,-1}`), just spelled
/// out. All arithmetic stays in `i64` and truncates to `i32` exactly
/// as before.
fn apply_fixed_predictor(samples: &mut [i32], order: usize) {
    let total = samples.len();
    let buf = samples;
    match order {
        // order 0: prediction is 0, so `buf[t]` already *is* the
        // residual — nothing to reconstruct.
        0 => {}
        1 => {
            for t in order..total {
                let p = buf[t - 1] as i64;
                buf[t] = (p + buf[t] as i64) as i32;
            }
        }
        2 => {
            for t in order..total {
                let p = 2 * buf[t - 1] as i64 - buf[t - 2] as i64;
                buf[t] = (p + buf[t] as i64) as i32;
            }
        }
        3 => {
            for t in order..total {
                let p = 3 * buf[t - 1] as i64 - 3 * buf[t - 2] as i64 + buf[t - 3] as i64;
                buf[t] = (p + buf[t] as i64) as i32;
            }
        }
        _ => {
            // order == 4 (the only remaining legal value; §9.2.6).
            for t in order..total {
                let p = 4 * buf[t - 1] as i64 - 6 * buf[t - 2] as i64 + 4 * buf[t - 3] as i64
                    - buf[t - 4] as i64;
                buf[t] = (p + buf[t] as i64) as i32;
            }
        }
    }
}

fn decode_lpc(br: &mut BitReader, block_size: u32, bps: u32, order: usize) -> Result<Vec<i32>> {
    let mut samples = Vec::with_capacity(block_size as usize);
    for _ in 0..order {
        samples.push(br.read_i32(bps)?);
    }
    let qlp_precision_raw = br.read_u32(4)?;
    if qlp_precision_raw == 0xF {
        return Err(Error::invalid("FLAC LPC: invalid qlp precision (0xF)"));
    }
    let qlp_precision = qlp_precision_raw + 1;
    let qlp_shift = br.read_i32(5)?;
    if qlp_shift < 0 {
        return Err(Error::invalid("FLAC LPC: negative qlp_shift not supported"));
    }
    let mut coeffs = Vec::with_capacity(order);
    for _ in 0..order {
        coeffs.push(br.read_i32(qlp_precision)?);
    }
    // Decode the residual straight onto the tail of `samples`, then
    // reconstruct in place (no separate residual buffer / copy).
    decode_residual_into(br, &mut samples, block_size, order)?;
    apply_lpc(&mut samples, &coeffs, qlp_shift as u32);
    Ok(samples)
}

/// Reconstruct the block *in place* by running the §9.2.5 quantised LPC
/// predictor. On entry `samples` holds `[warm-up(order) | residuals]`
/// (the residual coder wrote its output straight onto the tail). Each
/// position `t >= order` is overwritten by
/// `(inner_product(samples[t-order..t], coeffs) >> qlp_shift)
/// wrapping_add residual[t]`; the inner product reads only *earlier*
/// positions, so overwriting `samples[t]` after computing it is safe
/// and needs no separate residual buffer.
///
/// The per-sample inner product runs over the fixed-length window
/// `buf[t-order..t]` against a same-length reversed coefficient view.
/// Reversing the coefficients once up front (into a small stack buffer;
/// FLAC LPC order is 1..=32) turns the naive `coeffs[i] *
/// samples[len-1-i]` (a descending index) into two forward-scanned
/// equal-length slices, which the compiler can walk without
/// re-deriving `samples.len()` or bounds-checking each access. Output
/// is bit-identical to the naive form: the same products are summed in
/// `i64`, the same `>> qlp_shift` is applied, and the same
/// `wrapping_add` truncation to `i32` closes each sample.
fn apply_lpc(samples: &mut [i32], coeffs: &[i32], qlp_shift: u32) {
    let order = coeffs.len();
    let total = samples.len();
    let buf = samples;

    // Reversed coefficients: `rc[j]` multiplies the sample `order-1-j`
    // steps back, so the window `buf[t-order..t]` (ascending in time)
    // pairs element-for-element with `rc`. The FLAC LPC order is 1..=32
    // (§9.2.5, subframe type `0b1xxxxx` → order `(code & 0x1F) + 1`), so
    // a 32-entry stack buffer always holds the reversed row; the
    // `min(32)` keeps the copy bounded even if a caller ever passed a
    // longer coefficient slice.
    let mut rc_stack = [0i32; 32];
    let n = order.min(32);
    for (j, &c) in coeffs.iter().rev().take(n).enumerate() {
        rc_stack[j] = c;
    }
    let rc = &rc_stack[..n];

    for t in order..total {
        let window = &buf[t - order..t];
        let mut pred: i64 = 0;
        for (&s, &c) in window.iter().zip(rc) {
            pred += (c as i64) * (s as i64);
        }
        let predicted = (pred >> qlp_shift) as i32;
        buf[t] = predicted.wrapping_add(buf[t]);
    }
}

/// Decode the §9.2.7 partitioned Rice residual, appending the
/// `block_size - predictor_order` residual values directly onto the tail
/// of `samples` (which already holds the `predictor_order` warm-up
/// samples). Writing straight into the reconstruction buffer avoids a
/// separate residual `Vec` allocation and the copy the split
/// `decode_residual` + `apply_*` form performed on every subframe.
///
/// The residual arithmetic is unchanged: Rice `(q << k) | r` folded via
/// the standard zig-zag `(u >> 1)` / `-(u >> 1) - 1` mapping, and the
/// escape partition's raw `raw_bps`-wide signed reads.
fn decode_residual_into(
    br: &mut BitReader,
    samples: &mut Vec<i32>,
    block_size: u32,
    predictor_order: usize,
) -> Result<()> {
    let method = br.read_u32(2)?;
    let (param_bits, escape_marker) = match method {
        0 => (4u32, 15u32),
        1 => (5u32, 31u32),
        _ => return Err(Error::invalid("reserved FLAC residual coding method")),
    };
    let partition_order = br.read_u32(4)?;
    let n_partitions = 1u32 << partition_order;
    if block_size % n_partitions != 0 {
        return Err(Error::invalid(
            "FLAC residual: block_size not divisible by partition count",
        ));
    }
    let partition_size = block_size / n_partitions;
    if partition_size as usize <= predictor_order {
        // First partition would have zero or negative samples — invalid.
        return Err(Error::invalid(
            "FLAC residual: first partition smaller than predictor order",
        ));
    }

    let total = block_size as usize - predictor_order;
    samples.reserve(total);
    for p in 0..n_partitions {
        let n_samples = if p == 0 {
            partition_size as usize - predictor_order
        } else {
            partition_size as usize
        };
        let k = br.read_u32(param_bits)?;
        if k == escape_marker {
            // "Escape" partition: 5 bits of raw bps then n_samples raw signed values.
            let raw_bps = br.read_u32(5)?;
            for _ in 0..n_samples {
                samples.push(br.read_i32(raw_bps)?);
            }
        } else {
            for _ in 0..n_samples {
                let q = br.read_unary()?;
                let r = br.read_u32(k)?;
                let unsigned = ((q as u64) << k) | (r as u64);
                let signed: i64 = if unsigned & 1 == 0 {
                    (unsigned >> 1) as i64
                } else {
                    -((unsigned >> 1) as i64) - 1
                };
                samples.push(signed as i32);
            }
        }
    }
    Ok(())
}

// --- Wide (33-bit) subframe path ----------------------------------------
//
// The side channel of a 32-bit stereo-decorrelated frame carries samples
// one bit wider than the declared depth (RFC 9639 §4.2 / Appendix A.2 —
// "the side channel needs one extra bit of bit depth, as the subtraction
// can produce sample values twice as large as the maximum possible in any
// given bit depth"). At the 32-bit ceiling that is 33 bits, which does not
// fit in `i32`, so these mirror the narrow path with `i64` storage. The
// arithmetic is identical (same predictor difference equations, same
// zig-zag Rice residual, same escape partitions); only the sample width
// grows. All products stay well inside `i64`: a 33-bit sample times a
// ≤ 15-bit coefficient is ≤ 48 bits, summed over ≤ 32 taps is ≤ 53 bits.

/// Decode one 33-bit-wide subframe, returning `i64` samples.
///
/// Used only for the side channel of a 32-bit stereo-decorrelated frame;
/// `bps` must be exactly 33 (STREAMINFO bps 32 + the +1 side bit). Any
/// other width is a caller bug and returns [`Error::Unsupported`].
pub fn decode_subframe_wide(br: &mut BitReader, block_size: u32, bps: u32) -> Result<Vec<i64>> {
    if bps != 33 {
        return Err(Error::unsupported(format!(
            "FLAC wide subframe bps {bps} (the wide path handles only the \
             33-bit side channel of a 32-bit decorrelated frame)"
        )));
    }
    let pad = br.read_u32(1)?;
    if pad != 0 {
        return Err(Error::invalid("subframe header pad bit must be zero"));
    }
    let type_code = br.read_u32(6)? as u8;
    let has_wasted = br.read_bit()?;
    let wasted = if has_wasted { br.read_unary()? + 1 } else { 0 };
    if wasted >= bps {
        return Err(Error::invalid(format!("wasted bits {wasted} >= bps {bps}")));
    }
    let effective_bps = bps - wasted;

    let mut samples = match type_code {
        0b000000 => {
            let v = br.read_i64(effective_bps)?;
            vec![v; block_size as usize]
        }
        0b000001 => {
            let mut s = Vec::with_capacity(block_size as usize);
            for _ in 0..block_size {
                s.push(br.read_i64(effective_bps)?);
            }
            s
        }
        0b001000..=0b001100 => {
            let order = (type_code & 0x07) as usize;
            decode_fixed_wide(br, block_size, effective_bps, order)?
        }
        0b100000..=0b111111 => {
            let order = ((type_code & 0x1F) + 1) as usize;
            decode_lpc_wide(br, block_size, effective_bps, order)?
        }
        _ => {
            return Err(Error::invalid(format!(
                "reserved FLAC subframe type 0x{type_code:02x}"
            )));
        }
    };

    if wasted > 0 {
        for s in samples.iter_mut() {
            *s <<= wasted;
        }
    }
    Ok(samples)
}

fn decode_fixed_wide(
    br: &mut BitReader,
    block_size: u32,
    bps: u32,
    order: usize,
) -> Result<Vec<i64>> {
    let mut samples = Vec::with_capacity(block_size as usize);
    for _ in 0..order {
        samples.push(br.read_i64(bps)?);
    }
    decode_residual_into_wide(br, &mut samples, block_size, order)?;
    apply_fixed_predictor_wide(&mut samples, order);
    Ok(samples)
}

/// `i64` twin of [`apply_fixed_predictor`] — same difference-equation
/// constants, widened storage. See that function for the derivation.
///
/// All arithmetic is `wrapping_*`, mirroring the narrow path's `as i32`
/// truncation: a valid 33-bit stream keeps every reconstructed sample and
/// predictor sum well inside `i64` (a 33-bit value times the ≤ 6 fixed
/// coefficient is ≤ 36 bits), so no wrap ever occurs and the output is
/// exact; a hostile/invalid stream that would otherwise grow a sample past
/// `i64::MAX` wraps instead of panicking (debug overflow check), preserving
/// the crate-wide panic-freedom contract exercised by the fuzz harnesses.
fn apply_fixed_predictor_wide(samples: &mut [i64], order: usize) {
    let total = samples.len();
    let buf = samples;
    match order {
        0 => {}
        1 => {
            for t in order..total {
                buf[t] = buf[t].wrapping_add(buf[t - 1]);
            }
        }
        2 => {
            for t in order..total {
                let p = buf[t - 1].wrapping_mul(2).wrapping_sub(buf[t - 2]);
                buf[t] = buf[t].wrapping_add(p);
            }
        }
        3 => {
            for t in order..total {
                let p = buf[t - 1]
                    .wrapping_mul(3)
                    .wrapping_sub(buf[t - 2].wrapping_mul(3))
                    .wrapping_add(buf[t - 3]);
                buf[t] = buf[t].wrapping_add(p);
            }
        }
        _ => {
            for t in order..total {
                let p = buf[t - 1]
                    .wrapping_mul(4)
                    .wrapping_sub(buf[t - 2].wrapping_mul(6))
                    .wrapping_add(buf[t - 3].wrapping_mul(4))
                    .wrapping_sub(buf[t - 4]);
                buf[t] = buf[t].wrapping_add(p);
            }
        }
    }
}

fn decode_lpc_wide(
    br: &mut BitReader,
    block_size: u32,
    bps: u32,
    order: usize,
) -> Result<Vec<i64>> {
    let mut samples = Vec::with_capacity(block_size as usize);
    for _ in 0..order {
        samples.push(br.read_i64(bps)?);
    }
    let qlp_precision_raw = br.read_u32(4)?;
    if qlp_precision_raw == 0xF {
        return Err(Error::invalid("FLAC LPC: invalid qlp precision (0xF)"));
    }
    let qlp_precision = qlp_precision_raw + 1;
    let qlp_shift = br.read_i32(5)?;
    if qlp_shift < 0 {
        return Err(Error::invalid("FLAC LPC: negative qlp_shift not supported"));
    }
    let mut coeffs = Vec::with_capacity(order);
    for _ in 0..order {
        coeffs.push(br.read_i32(qlp_precision)? as i64);
    }
    decode_residual_into_wide(br, &mut samples, block_size, order)?;
    apply_lpc_wide(&mut samples, &coeffs, qlp_shift as u32);
    Ok(samples)
}

/// `i64` twin of [`apply_lpc`]. The inner product accumulates in `i64`
/// exactly as the narrow path does (which already promoted to `i64`), so
/// this is bit-identical to the ≤ 32-bit form on any sample that fits both
/// widths; the only difference is the reconstructed samples themselves are
/// stored `i64`-wide. Arithmetic is `wrapping_*` for the same reason as
/// [`apply_fixed_predictor_wide`]: a valid 33-bit stream never wraps (a
/// 33-bit sample times a ≤ 15-bit coefficient summed over ≤ 32 taps is
/// ≤ 53 bits), while a hostile stream wraps rather than panicking.
fn apply_lpc_wide(samples: &mut [i64], coeffs: &[i64], qlp_shift: u32) {
    let order = coeffs.len();
    let total = samples.len();
    let buf = samples;
    for t in order..total {
        let window = &buf[t - order..t];
        let mut pred: i64 = 0;
        for (&s, &c) in window.iter().zip(coeffs.iter().rev()) {
            pred = pred.wrapping_add(c.wrapping_mul(s));
        }
        buf[t] = buf[t].wrapping_add(pred >> qlp_shift);
    }
}

/// `i64` twin of [`decode_residual_into`]. Identical partition walk and
/// zig-zag mapping; the decoded residual is stored `i64`-wide.
fn decode_residual_into_wide(
    br: &mut BitReader,
    samples: &mut Vec<i64>,
    block_size: u32,
    predictor_order: usize,
) -> Result<()> {
    let method = br.read_u32(2)?;
    let (param_bits, escape_marker) = match method {
        0 => (4u32, 15u32),
        1 => (5u32, 31u32),
        _ => return Err(Error::invalid("reserved FLAC residual coding method")),
    };
    let partition_order = br.read_u32(4)?;
    let n_partitions = 1u32 << partition_order;
    if block_size % n_partitions != 0 {
        return Err(Error::invalid(
            "FLAC residual: block_size not divisible by partition count",
        ));
    }
    let partition_size = block_size / n_partitions;
    if partition_size as usize <= predictor_order {
        return Err(Error::invalid(
            "FLAC residual: first partition smaller than predictor order",
        ));
    }

    let total = block_size as usize - predictor_order;
    samples.reserve(total);
    for p in 0..n_partitions {
        let n_samples = if p == 0 {
            partition_size as usize - predictor_order
        } else {
            partition_size as usize
        };
        let k = br.read_u32(param_bits)?;
        if k == escape_marker {
            let raw_bps = br.read_u32(5)?;
            for _ in 0..n_samples {
                samples.push(br.read_i64(raw_bps)?);
            }
        } else {
            for _ in 0..n_samples {
                let q = br.read_unary()?;
                let r = br.read_u32(k)?;
                let unsigned = ((q as u64) << k) | (r as u64);
                let signed: i64 = if unsigned & 1 == 0 {
                    (unsigned >> 1) as i64
                } else {
                    -((unsigned >> 1) as i64) - 1
                };
                samples.push(signed);
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression test for the panic the `panic_free_decode` fuzz
    /// harness found on 2026-05-10 — a STREAMINFO declaring
    /// `bits_per_sample=32` combined with a stereo-decorrelated frame
    /// asks for a 33-bit side-channel sample, which `read_i32` can't
    /// represent. We now reject `bps > 32` (and `bps == 0`) at
    /// `decode_subframe` entry rather than panicking inside the
    /// bit reader.
    #[test]
    fn decode_subframe_rejects_bps_above_32_without_panic() {
        let bytes = [0u8; 16];
        let mut br = BitReader::new(&bytes);
        let err = decode_subframe(&mut br, 4096, 33).expect_err("bps 33 must be rejected");
        match err {
            Error::Unsupported(_) => {}
            other => panic!("expected Error::Unsupported, got {other:?}"),
        }
    }

    #[test]
    fn decode_subframe_rejects_bps_zero_without_panic() {
        let bytes = [0u8; 16];
        let mut br = BitReader::new(&bytes);
        let err = decode_subframe(&mut br, 4096, 0).expect_err("bps 0 must be rejected");
        assert!(matches!(err, Error::Unsupported(_)));
    }

    // --- 33-bit wide side-channel path (RFC 9639 §4.2 / A.2) ------------
    use oxideav_core::bits::BitWriter;

    /// Encode a chosen residual through the zig-zag + Rice mapping the
    /// wide decoder inverts (`decode_residual_into_wide`), so a test can
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
        let mut br = BitReader::new(&bytes);
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
        let mut br = BitReader::new(&bytes);
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
        let mut br = BitReader::new(&bytes);
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
        let mut br = BitReader::new(&bytes);
        let s = decode_subframe_wide(&mut br, 6, 33).unwrap();
        assert_eq!(s, expected);
    }

    #[test]
    fn wide_rejects_non_33_bps() {
        let bytes = [0u8; 8];
        let mut br = BitReader::new(&bytes);
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
        apply_fixed_predictor_wide(&mut buf, 4);
    }

    /// Same contract for the wide LPC reconstruction: a maximal
    /// coefficient against maximal samples must wrap, not panic.
    #[test]
    fn wide_lpc_predictor_wraps_without_panic() {
        let mut buf = vec![i64::MAX; 4096];
        let coeffs = vec![i64::from(i16::MAX); 32]; // wider than any real qlp coeff
        apply_lpc_wide(&mut buf, &coeffs, 0);
    }
}
