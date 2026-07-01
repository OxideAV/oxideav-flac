//! Subframe decoding: constant, verbatim, fixed-predictor, LPC.
//!
//! Reference: <https://xiph.org/flac/format.html#subframe>

use oxideav_core::{Error, Result};

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
/// frame) cannot currently be decoded losslessly. Reject up front
/// rather than panicking inside the bit reader. Caught by the
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
    let residual = decode_residual(br, block_size, order)?;
    apply_fixed_predictor(&mut samples, &residual, order);
    Ok(samples)
}

/// Reconstruct the block by running the §9.2.6 fixed predictor over the
/// decoded residual. `samples` enters holding exactly `order` warm-up
/// samples and is grown by `residual.len()` reconstructed samples.
///
/// Each order's difference equation is unrolled into a branch-free
/// closed form so the per-sample inner loop carries no coefficient
/// iterator, no `samples.len()` recomputation, and — because the
/// output buffer is pre-extended to its final length and driven by an
/// absolute index `t` with the predictor window `samples[t-order..t]`
/// — no per-access bounds check. Output is bit-identical to the naive
/// `sum(coeff[i] * samples[len-1-i]) + r` form: the coefficients here
/// are the same Table-of-differences constants
/// (`{}`, `{1}`, `{2,-1}`, `{3,-3,1}`, `{4,-6,4,-1}`), just spelled
/// out. All arithmetic stays in `i64` and truncates to `i32` exactly
/// as before.
fn apply_fixed_predictor(samples: &mut Vec<i32>, residual: &[i32], order: usize) {
    let warm = samples.len();
    debug_assert_eq!(warm, order, "fixed predictor warm-up length");
    let total = warm + residual.len();
    samples.resize(total, 0);
    let buf = &mut samples[..];
    match order {
        0 => {
            // Prediction is 0: the reconstructed sample *is* the residual.
            for (t, &r) in (warm..total).zip(residual) {
                buf[t] = r;
            }
        }
        1 => {
            for (t, &r) in (warm..total).zip(residual) {
                let p = buf[t - 1] as i64;
                buf[t] = (p + r as i64) as i32;
            }
        }
        2 => {
            for (t, &r) in (warm..total).zip(residual) {
                let p = 2 * buf[t - 1] as i64 - buf[t - 2] as i64;
                buf[t] = (p + r as i64) as i32;
            }
        }
        3 => {
            for (t, &r) in (warm..total).zip(residual) {
                let p = 3 * buf[t - 1] as i64 - 3 * buf[t - 2] as i64 + buf[t - 3] as i64;
                buf[t] = (p + r as i64) as i32;
            }
        }
        _ => {
            // order == 4 (the only remaining legal value; §9.2.6).
            for (t, &r) in (warm..total).zip(residual) {
                let p = 4 * buf[t - 1] as i64 - 6 * buf[t - 2] as i64 + 4 * buf[t - 3] as i64
                    - buf[t - 4] as i64;
                buf[t] = (p + r as i64) as i32;
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
    let residual = decode_residual(br, block_size, order)?;
    apply_lpc(&mut samples, &residual, &coeffs, qlp_shift as u32);
    Ok(samples)
}

/// Reconstruct the block by running the §9.2.5 quantised LPC predictor
/// over the decoded residual. `samples` enters holding exactly
/// `coeffs.len()` warm-up samples and is grown by `residual.len()`
/// reconstructed samples.
///
/// The output buffer is pre-extended to its final length so the
/// per-sample inner product runs over the fixed-length window
/// `buf[t-order..t]` against a same-length reversed coefficient view.
/// Reversing the coefficients once up front (into a small stack
/// buffer for the common `order <= 32` case) turns the naive
/// `coeffs[i] * samples[len-1-i]` (a descending index) into two
/// forward-scanned equal-length slices, which the compiler can walk
/// without re-deriving `samples.len()` or bounds-checking each access.
/// Output is bit-identical to the naive form: the same products are
/// summed in `i64`, the same `>> qlp_shift` is applied, and the same
/// `wrapping_add` truncation to `i32` closes each sample.
fn apply_lpc(samples: &mut Vec<i32>, residual: &[i32], coeffs: &[i32], qlp_shift: u32) {
    let order = coeffs.len();
    let warm = samples.len();
    debug_assert_eq!(warm, order, "LPC warm-up length");
    let total = warm + residual.len();
    samples.resize(total, 0);
    let buf = &mut samples[..];

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

    for (t, &r) in (warm..total).zip(residual) {
        let window = &buf[t - order..t];
        let mut pred: i64 = 0;
        for (&s, &c) in window.iter().zip(rc) {
            pred += (c as i64) * (s as i64);
        }
        let predicted = (pred >> qlp_shift) as i32;
        buf[t] = predicted.wrapping_add(r);
    }
}

fn decode_residual(
    br: &mut BitReader,
    block_size: u32,
    predictor_order: usize,
) -> Result<Vec<i32>> {
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
    let mut residual = Vec::with_capacity(total);
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
                residual.push(br.read_i32(raw_bps)?);
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
                residual.push(signed as i32);
            }
        }
    }
    Ok(residual)
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
}
