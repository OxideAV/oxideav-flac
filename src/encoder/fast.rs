//! Estimate-driven subframe search — the production encoder path for
//! every compression level (issue #12, "Encode is very slow").
//!
//! The exhaustive search in the parent module scores every candidate
//! predictor (5 windows × 12 LPC orders × 5 precisions per channel, for
//! all four stereo layouts) by **writing it out** and counting the bits.
//! That is exact but costs hundreds of full residual + Rice passes per
//! subframe. This module ranks candidates by cheap *estimates* instead and
//! only encodes the winner:
//!
//! * **Stereo layout** (RFC 9639 §4.2): pick independent / left-side /
//!   side-right / mid-side from fixed-predictor residual magnitudes of the
//!   four candidate channels, then search only the two channels the chosen
//!   layout codes (level 8 searches all four in full and compares).
//! * **Fixed predictors** (§9.2.5): one pass computes the residual
//!   magnitude sum of all five orders by successive differencing; only the
//!   cheapest order is residual-coded.
//! * **LPC** (§9.2.6): per analysis window, one autocorrelation and one
//!   Levinson-Durbin recursion yield every order's coefficients *and*
//!   prediction error. The error predicts each order's residual cost, so
//!   only the best few orders (per level) are quantised and coded.
//! * **Partitioned Rice** (§9.2.7): per finest partition one pass gathers
//!   the folded-residual sum and the bit-OR (for the escape width); coarser
//!   orders are pairwise merges. Rice parameters are chosen from the sum in
//!   closed form, then refined exactly for the emitted layout only.
//!
//! Output is a valid streamable-subset FLAC stream under the same caps as
//! the exhaustive search (LPC order ≤ 12 and partition order ≤ 8 unless
//! the caller opted out of the subset).

use super::{
    ApodizationWindow, MAX_LPC_QLP_PRECISION, MIN_LPC_QLP_PRECISION, SubframeScratch,
    finest_legal_partition_order, lpc_precision_for, quantize_lpc_into, wasted_bits,
    window_weights,
};

/// Per-level search configuration for the fast path. Built once per
/// encoder from [`super::FlacEncoderOptions`] by [`super::build_scratch`].
#[derive(Clone, Debug)]
pub(super) struct FastConfig {
    /// LPC order ceiling; 0 restricts the search to FIXED predictors.
    pub max_lpc_order: usize,
    /// Analysis windows; each costs one autocorrelation + recursion.
    pub windows: Vec<ApodizationWindow>,
    /// How many of the best-estimated LPC orders (per window) are
    /// actually quantised and residual-coded.
    pub order_candidates: usize,
    /// Precisions below the block-size heuristic to try per candidate.
    pub precision_span: u32,
    /// Partition-order ceiling (already clamped to the subset bound).
    pub max_partition_order: u32,
    /// Search all four stereo channels in full instead of estimating.
    pub exhaustive_stereo: bool,
}

/// What one coded subframe is. Residual-carrying kinds keep their data in
/// the owning [`Choice`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    Constant(i32),
    Verbatim,
    Fixed(usize),
    Lpc {
        order: usize,
        precision: u32,
        shift: u32,
    },
}

/// One partition's coding in the emitted layout.
#[derive(Clone, Copy, Debug)]
enum Part {
    Rice(u32),
    Escape(u32),
}

/// The chosen coding of one subframe, ready to write.
#[derive(Default)]
pub(super) struct Choice {
    kind: Option<Kind>,
    /// Wasted bits per sample (§9.2.2) shifted out of `samples`.
    wasted: u32,
    /// Effective bits per sample after the wasted-bits shift.
    bps: u32,
    /// Estimated size in bits (used to compare stereo layouts).
    bits: u64,
    /// Shifted input samples (warm-up + verbatim source).
    samples: Vec<i32>,
    /// Quantised LPC coefficients (FLAC order: `[0]` multiplies `s[i-1]`).
    qcoeffs: Vec<i32>,
    /// Prediction residual for FIXED / LPC kinds.
    residual: Vec<i32>,
    /// Rice method (0 = 4-bit parameters, 1 = 5-bit).
    method: u32,
    partition_order: u32,
    parts: Vec<Part>,
}

/// A channel's analysis from the stereo-layout estimate.
#[derive(Default)]
struct Est {
    fixed_order: usize,
    window: Option<ApodizationWindow>,
    reached: usize,
    energy: f64,
    coeffs: Vec<f64>,
    err: Vec<f64>,
}

/// Fast-path reusable state, carried on [`SubframeScratch`].
#[derive(Default)]
pub(super) struct FastScratch {
    /// Candidate residual under evaluation; swapped into a `Choice` when it
    /// wins so neither buffer is ever copied.
    residual: Vec<i32>,
    qcoeffs: Vec<i32>,
    windowed: Vec<f64>,
    autoc: Vec<f64>,
    /// Triangular coefficient store: order `m` lives at `m*(m-1)/2`.
    coeffs: Vec<f64>,
    /// Prediction error after each order (`err[m-1]`).
    err: Vec<f64>,
    /// Windowed signal energy (lag-0 autocorrelation) of the last analysis.
    energy: f64,
    /// Finest-level partition stats (sum of folded residuals, OR of them).
    sums: Vec<u64>,
    ors: Vec<u32>,
    side: Vec<i32>,
    mid: Vec<i32>,
    /// `Σ w²` per `(window, n)`, for the order-ranking variance estimate.
    wsum: Vec<(ApodizationWindow, usize, f64)>,
    /// Per-slot analysis left by the stereo-layout estimate, reused by the
    /// subframe search of the same channel within the same frame.
    est: [Option<Est>; 4],
    /// Recycled `Est` buffers.
    est_pool: Vec<Est>,
    /// Per-window coefficient stores while candidates are collected.
    win_coeffs: Vec<Vec<f64>>,
    /// `(estimated bits, window index, order)` picks across all windows.
    ranked: Vec<(f64, usize, usize)>,
    /// Per-channel results for the frame under construction (up to the
    /// four stereo candidates).
    pub choices: Vec<Choice>,
}

// --- Estimates ------------------------------------------------------------

/// Estimated Rice-coded size of `n` folded residuals whose sum is `sum`,
/// with a single closed-form parameter: `n*(k+1) + Σ(u >> k)`, the shift
/// sum approximated as `(sum - n*(2^k-1)/2) >> k` (uniform low bits).
#[inline]
fn rice_estimate(sum: u64, n: u64, k_max: u32) -> (u64, u32) {
    if n == 0 {
        return (0, 0);
    }
    let mean = sum / n;
    let k0 = if mean == 0 {
        0
    } else {
        (63 - mean.leading_zeros()).min(k_max)
    };
    let mut best = (u64::MAX, 0);
    for k in k0.saturating_sub(1)..=(k0 + 1).min(k_max) {
        let shifted = if k == 0 {
            sum
        } else {
            let low = n * ((1u64 << k) - 1) / 2;
            sum.saturating_sub(low) >> k
        };
        let cost = n * (k as u64 + 1) + shifted;
        if cost < best.0 {
            best = (cost, k);
        }
    }
    best
}

/// Bits per residual sample for an escape partition holding values whose
/// folded forms OR to `or` (two's-complement width of the largest
/// magnitude; at least 1).
#[inline]
fn escape_width(or: u32) -> u32 {
    (33 - (or >> 1).leading_zeros()).max(1)
}

#[inline]
fn fold(r: i32) -> u32 {
    ((r << 1) ^ (r >> 31)) as u32
}

/// Choose the partitioned-Rice method and partition order for `residual`
/// (block of `block_size` samples, `order` warm-up samples) from
/// estimates. Returns `(estimated bits incl. the 6-bit header, method,
/// partition order)`; [`refine_partitions`] later picks exact parameters
/// for the winning candidate only.
fn plan_partitions(
    residual: &[i32],
    block_size: usize,
    order: usize,
    max_po: u32,
    fs: &mut FastScratch,
) -> (u64, u32, u32) {
    let finest = finest_legal_partition_order(block_size, order, max_po);
    let n_fin = 1usize << finest;
    let per = block_size >> finest;
    fs.sums.clear();
    fs.sums.resize(n_fin, 0);
    fs.ors.clear();
    fs.ors.resize(n_fin, 0);
    let mut idx = 0usize;
    for p in 0..n_fin {
        let end = (p + 1) * per - order;
        let mut s = 0u64;
        let mut o = 0u32;
        for &r in &residual[idx..end] {
            let u = fold(r);
            s += u as u64;
            o |= u;
        }
        fs.sums[p] = s;
        fs.ors[p] = o;
        idx = end;
    }

    // Walk from the finest order down, merging pairs in place. Each level
    // is scored under both Rice methods; ties keep the earlier (coarser,
    // 4-bit) candidate because levels are compared with strict `<` in the
    // ascending order the selection loop below replays.
    let mut totals = [[u64::MAX; 16]; 2];
    let mut n_parts = n_fin;
    let mut level = finest as i32;
    loop {
        let psize = (block_size >> level) as u64;
        for (m, (pb, kmax)) in [(4u64, 14u32), (5u64, 30u32)].into_iter().enumerate() {
            let mut total = 0u64;
            for p in 0..n_parts {
                let n = if p == 0 { psize - order as u64 } else { psize };
                let (rice, _) = rice_estimate(fs.sums[p], n, kmax);
                let w = escape_width(fs.ors[p]) as u64;
                let esc = if w <= 31 { 5 + n * w } else { u64::MAX };
                total += pb + rice.min(esc);
            }
            totals[m][level as usize] = total;
        }
        if level == 0 {
            break;
        }
        let half = n_parts / 2;
        for j in 0..half {
            fs.sums[j] = fs.sums[2 * j] + fs.sums[2 * j + 1];
            fs.ors[j] = fs.ors[2 * j] | fs.ors[2 * j + 1];
        }
        n_parts = half;
        level -= 1;
    }
    let mut best = (u64::MAX, 0u32, 0u32);
    for m in 0..2u32 {
        for p in 0..=finest {
            let t = totals[m as usize][p as usize];
            if t < best.0 {
                best = (t, m, p);
            }
        }
    }
    (best.0 + 6, best.1, best.2)
}

/// Exact per-partition parameters for the emitted layout: the true cost of
/// Rice parameter k is n*(k+1) + Σ(u >> k), evaluated around the
/// closed-form estimate, against the escape coding (§9.2.7.1).
fn refine_partitions(
    residual: &[i32],
    block_size: usize,
    order: usize,
    method: u32,
    po: u32,
    parts_out: &mut Vec<Part>,
) {
    let kmax = if method == 0 { 14 } else { 30 };
    let n_parts = 1usize << po;
    let psize = block_size >> po;
    parts_out.clear();
    let mut idx = 0usize;
    for p in 0..n_parts {
        let end = (p + 1) * psize - order;
        let slice = &residual[idx..end];
        idx = end;
        let n = slice.len() as u64;
        let mut sum = 0u64;
        let mut or = 0u32;
        for &r in slice {
            let u = fold(r);
            sum += u as u64;
            or |= u;
        }
        let (_, k0) = rice_estimate(sum, n, kmax);
        let mut best_k = (u64::MAX, k0);
        for k in k0.saturating_sub(1)..=(k0 + 1).min(kmax) {
            let mut c = n * (k as u64 + 1);
            for &r in slice {
                c += (fold(r) >> k) as u64;
            }
            if c < best_k.0 {
                best_k = (c, k);
            }
        }
        let w = escape_width(or);
        let esc = if w <= 31 { 5 + n * w as u64 } else { u64::MAX };
        if esc < best_k.0 {
            parts_out.push(Part::Escape(w));
        } else {
            parts_out.push(Part::Rice(best_k.1));
        }
    }
}

// --- Predictors -----------------------------------------------------------

/// Residual magnitude sums of fixed orders 0..=4 over samples `4..n`
/// (common start so the five sums compare like for like), via successive
/// differences (RFC 9639 Table 20's derivation column).
fn fixed_magnitudes(s: &[i32]) -> [u64; 5] {
    let mut sums = [0u64; 5];
    if s.len() <= 4 {
        return sums;
    }
    let mut d1p = s[3] as i64 - s[2] as i64;
    let mut d2p = d1p - (s[2] as i64 - s[1] as i64);
    let mut d3p = d2p - ((s[2] as i64 - s[1] as i64) - (s[1] as i64 - s[0] as i64));
    let mut prev = s[3] as i64;
    for &x in &s[4..] {
        let x = x as i64;
        let d1 = x - prev;
        let d2 = d1 - d1p;
        let d3 = d2 - d2p;
        let d4 = d3 - d3p;
        sums[0] += x.unsigned_abs();
        sums[1] += d1.unsigned_abs();
        sums[2] += d2.unsigned_abs();
        sums[3] += d3.unsigned_abs();
        sums[4] += d4.unsigned_abs();
        prev = x;
        d1p = d1;
        d2p = d2;
        d3p = d3;
    }
    sums
}

/// Best fixed order by magnitude and its rough Rice cost estimate.
fn best_fixed_estimate(s: &[i32], bps: u32) -> (usize, u64) {
    let n = s.len();
    if n <= 4 {
        return (0, n as u64 * bps as u64);
    }
    let sums = fixed_magnitudes(s);
    let mut best = 0usize;
    for o in 1..5 {
        if sums[o] < sums[best] {
            best = o;
        }
    }
    let m = (n - 4) as u64;
    let (est, _) = rice_estimate(2 * sums[best], m, 30);
    (best, est * n as u64 / m + best as u64 * bps as u64)
}

/// Fill `out` with the fixed-order residual; `false` when a residual leaves
/// the §9.2.7.3 range.
fn fixed_residual(s: &[i32], order: usize, out: &mut Vec<i32>) -> bool {
    out.clear();
    out.reserve(s.len() - order);
    let mut ok = true;
    let mut push = |r: i64| {
        ok &= r > i32::MIN as i64 && r <= i32::MAX as i64;
        out.push(r as i32);
    };
    let n = s.len();
    match order {
        0 => s.iter().for_each(|&x| push(x as i64)),
        1 => (1..n).for_each(|i| push(s[i] as i64 - s[i - 1] as i64)),
        2 => (2..n).for_each(|i| push(s[i] as i64 - 2 * s[i - 1] as i64 + s[i - 2] as i64)),
        3 => (3..n).for_each(|i| {
            push(s[i] as i64 - 3 * s[i - 1] as i64 + 3 * s[i - 2] as i64 - s[i - 3] as i64)
        }),
        _ => (4..n).for_each(|i| {
            push(
                s[i] as i64 - 4 * s[i - 1] as i64 + 6 * s[i - 2] as i64 - 4 * s[i - 3] as i64
                    + s[i - 4] as i64,
            )
        }),
    }
    ok
}

/// LPC residual `s[i] - (Σ q[j]·s[i-1-j]) >> shift`; `false` on a residual
/// outside the §9.2.7.3 range. Orders up to 12 (every subset order at
/// ≤ 48 kHz) run a fully unrolled kernel.
fn lpc_residual(s: &[i32], q: &[i32], shift: u32, out: &mut Vec<i32>) -> bool {
    macro_rules! fixed_n {
        ($($n:literal)*) => {
            match q.len() {
                $($n => return lpc_residual_n::<$n>(s, q, shift, out),)*
                _ => {}
            }
        };
    }
    fixed_n!(1 2 3 4 5 6 7 8 9 10 11 12);
    lpc_residual_any(s, q, shift, out)
}

fn lpc_residual_n<const N: usize>(s: &[i32], q: &[i32], shift: u32, out: &mut Vec<i32>) -> bool {
    let mut rev = [0i64; N];
    for (j, &c) in q.iter().enumerate() {
        rev[N - 1 - j] = c as i64;
    }
    out.clear();
    out.reserve(s.len() - N);
    let mut ok = true;
    for (i, &x) in s.iter().enumerate().skip(N) {
        let hist: &[i32; N] = s[i - N..i].try_into().expect("window of N");
        let mut acc = 0i64;
        for j in 0..N {
            acc += rev[j] * hist[j] as i64;
        }
        let r = x as i64 - (acc >> shift);
        ok &= r > i32::MIN as i64 && r <= i32::MAX as i64;
        out.push(r as i32);
    }
    ok
}

fn lpc_residual_any(s: &[i32], q: &[i32], shift: u32, out: &mut Vec<i32>) -> bool {
    let order = q.len();
    out.clear();
    out.reserve(s.len() - order);
    // Reverse the coefficients so the dot product walks the history window
    // `s[i-order..i]` forwards (contiguous, vectorisable).
    let mut rev = [0i64; 32];
    for (j, &c) in q.iter().enumerate() {
        rev[order - 1 - j] = c as i64;
    }
    let rev = &rev[..order];
    let mut ok = true;
    for i in order..s.len() {
        let hist = &s[i - order..i];
        let mut acc = 0i64;
        for (&c, &x) in rev.iter().zip(hist) {
            acc += c * x as i64;
        }
        let r = s[i] as i64 - (acc >> shift);
        ok &= r > i32::MIN as i64 && r <= i32::MAX as i64;
        out.push(r as i32);
    }
    ok
}

/// Window + autocorrelate + Levinson-Durbin at every order up to `max`.
/// Fills `fs.coeffs` (triangular, predictor convention) and `fs.err`;
/// returns the highest order solved.
fn lpc_analysis(
    s: &[i32],
    max: usize,
    window: ApodizationWindow,
    cache: &mut Vec<super::WindowWeights>,
    fs: &mut FastScratch,
) -> usize {
    let n = s.len();
    let max = max.min(n.saturating_sub(1));
    if max == 0 {
        return 0;
    }
    let weights = window_weights(cache, window, n);
    fs.windowed.clear();
    fs.windowed
        .extend(s.iter().zip(weights).map(|(&x, &w)| x as f64 * w));
    let x = &fs.windowed;

    // Four interleaved partial sums per lag: independent accumulator
    // lanes pipeline (and vectorise) instead of serialising every product
    // on one f64 add chain.
    fs.autoc.clear();
    fs.autoc.resize(max + 1, 0.0);
    let ac = &mut fs.autoc[..];
    for (lag, out) in ac.iter_mut().enumerate() {
        let a = &x[lag..];
        let b = &x[..n - lag];
        let mut lanes = [0f64; 4];
        let (ca, ra) = a.as_chunks::<4>();
        let (cb, rb) = b.as_chunks::<4>();
        for (qa, qb) in ca.iter().zip(cb) {
            for l in 0..4 {
                lanes[l] += qa[l] * qb[l];
            }
        }
        let mut tail = 0f64;
        for (&p, &q) in ra.iter().zip(rb) {
            tail += p * q;
        }
        *out = (lanes[0] + lanes[1]) + (lanes[2] + lanes[3]) + tail;
    }
    if ac[0].is_nan() || ac[0] <= 0.0 || !ac[0].is_finite() {
        return 0;
    }
    fs.energy = ac[0];

    fs.coeffs.clear();
    fs.coeffs.resize(max * (max + 1) / 2, 0.0);
    fs.err.clear();
    let mut cur = [0f64; 33];
    let mut err = ac[0];
    let mut reached = 0;
    for m in 1..=max {
        // Reflection coefficient for order m (predictor sign convention:
        // prediction = Σ c_j·x[n-j]).
        let mut acc = ac[m];
        for j in 1..m {
            acc -= cur[j] * ac[m - j];
        }
        if err <= 1e-12 * ac[0] {
            break;
        }
        let k = acc / err;
        let mut next = cur;
        for j in 1..m {
            next[j] = cur[j] - k * cur[m - j];
        }
        next[m] = k;
        cur = next;
        err *= 1.0 - k * k;
        if !err.is_finite() || err <= 0.0 {
            break;
        }
        let base = m * (m - 1) / 2;
        fs.coeffs[base..base + m].copy_from_slice(&cur[1..=m]);
        fs.err.push(err);
        reached = m;
    }
    reached
}

/// Estimated cost of each LPC order `1..=reached` after [`lpc_analysis`]:
/// per-sample residual variance ≈ err / Σw², and a Laplacian residual of
/// variance σ² Rice-codes at about ½·log2(σ²) + 1.9 bits; each order also
/// pays its warm-up samples plus coefficients (`per_coeff_bits` each).
fn rank_orders(
    fs: &mut FastScratch,
    reached: usize,
    n: usize,
    precision: u32,
    bps: u32,
    window: ApodizationWindow,
    cache: &mut Vec<super::WindowWeights>,
) -> Vec<(f64, usize)> {
    let wsum = match fs.wsum.iter().find(|e| e.0 == window && e.1 == n) {
        Some(e) => e.2,
        None => {
            let v: f64 = window_weights(cache, window, n).iter().map(|w| w * w).sum();
            fs.wsum.push((window, n, v));
            v
        }
    };
    // Coefficient quantisation adds its own error on top of the ideal
    // predictor's: each of the m coefficients is off by up to half a step
    // of 2^-shift (uniform, variance 2^-2·shift / 12) and multiplies a
    // sample of power σx². On highly predictable signals (pure tones) this
    // term, not the recursion's error, decides which order wins.
    let power = fs.energy / wsum;
    let max_q = ((1i64 << (precision - 1)) - 1) as f64;
    (1..=reached)
        .map(|m| {
            let base = m * (m - 1) / 2;
            let cmax = fs.coeffs[base..base + m]
                .iter()
                .fold(0f64, |a, &c| a.max(c.abs()));
            let shift = if cmax > 0.0 {
                (max_q / cmax).log2().floor().clamp(0.0, 15.0)
            } else {
                15.0
            };
            let qvar = m as f64 * power * (-2.0 * shift).exp2() / 12.0;
            let var = (fs.err[m - 1] / wsum + qvar).max(1e-9);
            let per = (0.5 * var.log2() + 1.9).max(1.0);
            let cost = per * (n - m) as f64 + (m as u32 * (precision + bps)) as f64;
            (cost, m)
        })
        .collect()
}

/// Cheap size estimate of one channel for the stereo-layout decision: the
/// better of the best fixed predictor's estimate and, when LPC is enabled,
/// the best LPC order's error-based estimate under the first window.
fn channel_estimate(
    s: &[i32],
    bps: u32,
    slot: usize,
    cfg: &FastConfig,
    scratch: &mut SubframeScratch,
) -> u64 {
    let n = s.len();
    let mut fs = std::mem::take(&mut scratch.fast_state);
    let mut est = fs.est_pool.pop().unwrap_or_default();
    let (fixed_order, fixed) = best_fixed_estimate(s, bps);
    est.fixed_order = fixed_order;
    est.window = None;
    let max_lpc = cfg.max_lpc_order.min(n.saturating_sub(1));
    let mut bits = fixed;
    if let Some(&window) = cfg.windows.first()
        && max_lpc > 0
        && !s.iter().all(|&x| x == s[0])
    {
        let reached = lpc_analysis(s, max_lpc, window, &mut scratch.window_cache, &mut fs);
        let precision = lpc_precision_for(n);
        let lpc = rank_orders(
            &mut fs,
            reached,
            n,
            precision,
            bps,
            window,
            &mut scratch.window_cache,
        )
        .into_iter()
        .map(|(c, _)| c)
        .fold(f64::INFINITY, f64::min);
        if lpc.is_finite() {
            bits = fixed.min((lpc + 9.0) as u64);
        }
        if reached > 0 {
            est.window = Some(window);
            est.reached = reached;
            est.energy = fs.energy;
            std::mem::swap(&mut est.coeffs, &mut fs.coeffs);
            std::mem::swap(&mut est.err, &mut fs.err);
        }
    }
    if let Some(old) = fs.est[slot].replace(est) {
        fs.est_pool.push(old);
    }
    scratch.fast_state = fs;
    bits
}

// --- Subframe search ------------------------------------------------------

/// Search one channel and write the result into `fs.choices[slot]`.
pub(super) fn search_subframe(
    samples: &[i32],
    bps: u32,
    slot: usize,
    cfg: &FastConfig,
    scratch: &mut SubframeScratch,
) {
    let mut fs = std::mem::take(&mut scratch.fast_state);
    if fs.choices.len() <= slot {
        fs.choices.resize_with(slot + 1, Choice::default);
    }
    let mut ch = std::mem::take(&mut fs.choices[slot]);
    let mut est = fs.est.get_mut(slot).and_then(Option::take);
    search_into(samples, bps, cfg, scratch, &mut fs, &mut ch, est.as_mut());
    if let Some(e) = est {
        fs.est_pool.push(e);
    }
    fs.choices[slot] = ch;
    scratch.fast_state = fs;
}

fn search_into(
    input: &[i32],
    bps_in: u32,
    cfg: &FastConfig,
    scratch: &mut SubframeScratch,
    fs: &mut FastScratch,
    ch: &mut Choice,
    est: Option<&mut Est>,
) {
    let n = input.len();
    ch.parts.clear();
    if input.iter().all(|&x| x == input[0]) {
        ch.kind = Some(Kind::Constant(input[0]));
        ch.wasted = 0;
        ch.bps = bps_in;
        ch.bits = 8 + bps_in as u64;
        return;
    }
    let wasted = wasted_bits(input).min(bps_in.saturating_sub(1));
    ch.samples.clear();
    ch.samples.extend(input.iter().map(|&x| x >> wasted));
    ch.wasted = wasted;
    let bps = bps_in - wasted;
    ch.bps = bps;
    // The layout estimate analysed the unshifted channel; its results only
    // carry over when there was nothing to shift.
    let mut est = est.filter(|_| wasted == 0);
    let header = 8 + if wasted > 0 { wasted as u64 } else { 0 };
    let s = std::mem::take(&mut ch.samples);

    ch.kind = Some(Kind::Verbatim);
    ch.bits = header + n as u64 * bps as u64;

    // FIXED: code only the order with the smallest residual magnitude.
    let fixed_order = match &est {
        Some(e) => e.fixed_order,
        None => best_fixed_estimate(&s, bps).0,
    };
    let fixed_order = fixed_order.min(n - 1);
    if fixed_residual(&s, fixed_order, &mut fs.residual) {
        let res = std::mem::take(&mut fs.residual);
        let (rbits, m, po) = plan_partitions(&res, n, fixed_order, cfg.max_partition_order, fs);
        fs.residual = res;
        let bits = header + fixed_order as u64 * bps as u64 + rbits;
        if bits < ch.bits {
            ch.bits = bits;
            ch.kind = Some(Kind::Fixed(fixed_order));
            ch.method = m;
            ch.partition_order = po;
            std::mem::swap(&mut ch.residual, &mut fs.residual);
        }
    }

    // LPC.
    let max_lpc = cfg.max_lpc_order.min(n.saturating_sub(1));
    if max_lpc >= 1 {
        let ceiling = lpc_precision_for(n).min(MAX_LPC_QLP_PRECISION);
        let floor = ceiling
            .saturating_sub(cfg.precision_span)
            .max(MIN_LPC_QLP_PRECISION);
        // Analyse every window and collect each window's best
        // `order_candidates` orders, then residual-code those. Each window's
        // coefficients are parked in `fs.win_coeffs[wi]` meanwhile.
        fs.ranked.clear();
        if fs.win_coeffs.len() < cfg.windows.len() {
            fs.win_coeffs.resize_with(cfg.windows.len(), Vec::new);
        }
        for (wi, &window) in cfg.windows.iter().enumerate() {
            let cached = est
                .as_deref_mut()
                .filter(|e| wi == 0 && e.window == Some(window));
            let reached = match cached {
                Some(e) => {
                    std::mem::swap(&mut e.coeffs, &mut fs.coeffs);
                    std::mem::swap(&mut e.err, &mut fs.err);
                    fs.energy = e.energy;
                    e.window = None;
                    e.reached
                }
                None => lpc_analysis(&s, max_lpc, window, &mut scratch.window_cache, fs),
            };
            if reached == 0 {
                continue;
            }
            // Rank within the window only: the recursion's error is not
            // comparable across windows (a heavier taper discards more
            // energy), so each window contributes its own best orders.
            let mut ranked = rank_orders(
                fs,
                reached,
                n,
                ceiling,
                bps,
                window,
                &mut scratch.window_cache,
            );
            ranked.sort_by(|a, b| a.0.total_cmp(&b.0).then(a.1.cmp(&b.1)));
            fs.ranked.extend(
                ranked
                    .into_iter()
                    .take(cfg.order_candidates.max(1))
                    .map(|(cost, order)| (cost, wi, order)),
            );
            std::mem::swap(&mut fs.win_coeffs[wi], &mut fs.coeffs);
        }
        let picks: Vec<(usize, usize)> = fs.ranked.iter().map(|&(_, wi, o)| (wi, o)).collect();
        for (wi, order) in picks {
            let base = order * (order - 1) / 2;
            for precision in floor..=ceiling {
                let coeffs = &fs.win_coeffs[wi][base..base + order];
                let Some(shift) = quantize_lpc_into(coeffs, precision, &mut fs.qcoeffs) else {
                    continue;
                };
                let q = std::mem::take(&mut fs.qcoeffs);
                if lpc_residual(&s, &q, shift, &mut fs.residual) {
                    let res = std::mem::take(&mut fs.residual);
                    let (rbits, m, po) =
                        plan_partitions(&res, n, order, cfg.max_partition_order, fs);
                    fs.residual = res;
                    let bits = header
                        + order as u64 * bps as u64
                        + 9
                        + order as u64 * precision as u64
                        + rbits;
                    if bits < ch.bits {
                        ch.bits = bits;
                        ch.kind = Some(Kind::Lpc {
                            order,
                            precision,
                            shift,
                        });
                        ch.method = m;
                        ch.partition_order = po;
                        std::mem::swap(&mut ch.residual, &mut fs.residual);
                        ch.qcoeffs.clear();
                        ch.qcoeffs.extend_from_slice(&q);
                    }
                }
                fs.qcoeffs = q;
            }
        }
    }
    let order = match ch.kind {
        Some(Kind::Fixed(o)) => Some(o),
        Some(Kind::Lpc { order, .. }) => Some(order),
        _ => None,
    };
    if let Some(order) = order {
        refine_partitions(
            &ch.residual,
            n,
            order,
            ch.method,
            ch.partition_order,
            &mut ch.parts,
        );
    }
    ch.samples = s;
}

/// Pick the stereo layout and search the subframes it needs. Returns the
/// channel-assignment code (RFC 9639 Table 13: 1 independent, 8 left-side,
/// 9 side-right, 10 mid-side) and the `fs.choices` slots to emit in order.
pub(super) fn search_frame(
    channels: &[Vec<i32>],
    bps: u32,
    cfg: &FastConfig,
    scratch: &mut SubframeScratch,
) -> (u32, Vec<usize>) {
    let n_ch = channels.len();
    if n_ch != 2 || bps >= 32 {
        // Decorrelation needs a bps+1 side channel; 32-bit input stays
        // independent (the side channel would need 33 bits).
        for (i, c) in channels.iter().enumerate() {
            search_subframe(c, bps, i, cfg, scratch);
        }
        return ((n_ch - 1) as u32, (0..n_ch).collect());
    }
    let (l, r) = (&channels[0], &channels[1]);
    let mut fs = std::mem::take(&mut scratch.fast_state);
    fs.side.clear();
    fs.mid.clear();
    for (&a, &b) in l.iter().zip(r) {
        fs.side.push(a - b);
        fs.mid.push(((a as i64 + b as i64) >> 1) as i32);
    }
    let side = std::mem::take(&mut fs.side);
    let mid = std::mem::take(&mut fs.mid);
    scratch.fast_state = fs;

    // Slots: 0 = L, 1 = R, 2 = S, 3 = M.
    let mode = if cfg.exhaustive_stereo {
        search_subframe(l, bps, 0, cfg, scratch);
        search_subframe(r, bps, 1, cfg, scratch);
        search_subframe(&side, bps + 1, 2, cfg, scratch);
        search_subframe(&mid, bps, 3, cfg, scratch);
        let b: Vec<u64> = scratch.fast_state.choices[..4]
            .iter()
            .map(|c| c.bits)
            .collect();
        pick_mode([b[0], b[1], b[2], b[3]])
    } else {
        let est = [
            channel_estimate(l, bps, 0, cfg, scratch),
            channel_estimate(r, bps, 1, cfg, scratch),
            channel_estimate(&side, bps + 1, 2, cfg, scratch),
            channel_estimate(&mid, bps, 3, cfg, scratch),
        ];
        let mode = pick_mode(est);
        for &slot in mode_slots(mode) {
            let (src, depth) = match slot {
                0 => (l.as_slice(), bps),
                1 => (r.as_slice(), bps),
                2 => (side.as_slice(), bps + 1),
                _ => (mid.as_slice(), bps),
            };
            search_subframe(src, depth, slot, cfg, scratch);
        }
        let fs = &mut scratch.fast_state;
        for slot in 0..4 {
            if let Some(e) = fs.est[slot].take() {
                fs.est_pool.push(e);
            }
        }
        mode
    };
    scratch.fast_state.side = side;
    scratch.fast_state.mid = mid;
    (mode, mode_slots(mode).to_vec())
}

fn pick_mode(b: [u64; 4]) -> u32 {
    let mut best = (b[0] + b[1], 1u32);
    for (bits, code) in [(b[0] + b[2], 8u32), (b[2] + b[1], 9), (b[3] + b[2], 10)] {
        if bits < best.0 {
            best = (bits, code);
        }
    }
    best.1
}

fn mode_slots(mode: u32) -> &'static [usize] {
    match mode {
        8 => &[0, 2],
        9 => &[2, 1],
        10 => &[3, 2],
        _ => &[0, 1],
    }
}

// --- Bit output -----------------------------------------------------------

/// MSB-first bit writer with a 64-bit accumulator that flushes 32 bits at
/// a time — the residual writer is the hottest output loop.
pub(super) struct Bits {
    pub out: Vec<u8>,
    acc: u64,
    /// Valid bits in the low end of `acc` (always < 32 between calls).
    n: u32,
}

impl Bits {
    /// Continue after `prefix`, which must be byte-aligned bytes.
    pub fn after(prefix: Vec<u8>) -> Self {
        Bits {
            out: prefix,
            acc: 0,
            n: 0,
        }
    }

    #[inline]
    fn put(&mut self, v: u32, bits: u32) {
        debug_assert!(bits <= 32);
        if bits == 0 {
            return;
        }
        let v = (v as u64) & ((1u64 << bits) - 1);
        self.acc = (self.acc << bits) | v;
        self.n += bits;
        if self.n >= 32 {
            self.n -= 32;
            // Bits above `n + 32` are stale high bits already emitted; the
            // `as u32` truncation discards them.
            self.out
                .extend_from_slice(&((self.acc >> self.n) as u32).to_be_bytes());
        }
    }

    #[inline]
    fn put_signed(&mut self, v: i32, bits: u32) {
        self.put(v as u32, bits);
    }

    #[inline]
    fn put_rice(&mut self, u: u32, k: u32) {
        let q = u >> k;
        if q + 1 + k <= 32 {
            // q zeros, a 1, then the k low bits — one write.
            let low = if k == 0 { 0 } else { u & ((1u32 << k) - 1) };
            self.put((1u32 << k) | low, q + 1 + k);
        } else {
            let mut z = q;
            while z >= 32 {
                self.put(0, 32);
                z -= 32;
            }
            self.put(0, z);
            self.put(1, 1);
            if k > 0 {
                self.put(u & ((1u32 << k) - 1), k);
            }
        }
    }

    /// Zero-pad to a byte boundary and flush; returns the bytes.
    pub fn finish(mut self) -> Vec<u8> {
        let pad = (8 - self.n % 8) % 8;
        self.put(0, pad);
        while self.n >= 8 {
            self.n -= 8;
            self.out.push((self.acc >> self.n) as u8);
        }
        self.out
    }
}

/// Write the subframe in `choice` (RFC 9639 §9.2.1–§9.2.7).
pub(super) fn emit(choice: &Choice, w: &mut Bits) {
    let kind = choice.kind.expect("searched subframe");
    let type_code = match kind {
        Kind::Constant(_) => 0,
        Kind::Verbatim => 1,
        Kind::Fixed(o) => 0b001000 | o as u32,
        Kind::Lpc { order, .. } => 0b100000 | (order as u32 - 1),
    };
    w.put(type_code, 7); // leading zero bit + 6-bit type
    if choice.wasted == 0 {
        w.put(0, 1);
    } else {
        // Flag, then (wasted - 1) zeros and a terminating one.
        w.put(1, 1);
        w.put_rice(choice.wasted - 1, 0);
    }
    let bps = choice.bps;
    match kind {
        Kind::Constant(v) => w.put_signed(v, bps),
        Kind::Verbatim => {
            for &x in &choice.samples {
                w.put_signed(x, bps);
            }
        }
        Kind::Fixed(order) => {
            for &x in &choice.samples[..order] {
                w.put_signed(x, bps);
            }
            emit_residual(choice, order, w);
        }
        Kind::Lpc {
            order,
            precision,
            shift,
        } => {
            for &x in &choice.samples[..order] {
                w.put_signed(x, bps);
            }
            w.put(precision - 1, 4);
            w.put(shift, 5);
            for &q in &choice.qcoeffs {
                w.put_signed(q, precision);
            }
            emit_residual(choice, order, w);
        }
    }
}

fn emit_residual(choice: &Choice, order: usize, w: &mut Bits) {
    let pb = if choice.method == 0 { 4 } else { 5 };
    let escape = (1u32 << pb) - 1;
    w.put(choice.method, 2);
    w.put(choice.partition_order, 4);
    let block = choice.samples.len();
    let psize = block >> choice.partition_order;
    let mut idx = 0usize;
    for (p, part) in choice.parts.iter().enumerate() {
        let end = (p + 1) * psize - order;
        let slice = &choice.residual[idx..end];
        idx = end;
        match *part {
            Part::Rice(k) => {
                w.put(k, pb);
                for &r in slice {
                    w.put_rice(fold(r), k);
                }
            }
            Part::Escape(width) => {
                w.put(escape, pb);
                w.put(width, 5);
                for &r in slice {
                    w.put_signed(r, width);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rice_estimate_tracks_exact_cost() {
        // On a geometric-ish spread the closed-form estimate lands within a
        // couple of percent of the exact best-k cost.
        let mut state = 0x1234_5678u32;
        let mut us = Vec::new();
        for _ in 0..4096 {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let r = (state as i32) >> 22;
            us.push(fold(r));
        }
        let sum: u64 = us.iter().map(|&u| u as u64).sum();
        let exact = (0..=30u32)
            .map(|k| {
                us.len() as u64 * (k as u64 + 1) + us.iter().map(|&u| (u >> k) as u64).sum::<u64>()
            })
            .min()
            .unwrap();
        let (est, _) = rice_estimate(sum, us.len() as u64, 30);
        let diff = est.abs_diff(exact) as f64 / exact as f64;
        assert!(diff < 0.03, "estimate {est} vs exact {exact}");
    }

    #[test]
    fn bits_writer_matches_core_bitwriter() {
        use oxideav_core::bits::BitWriter;
        let mut a = Bits::after(vec![0xAB]);
        let mut b = BitWriter::new();
        b.write_u32(0xAB, 8);
        let mut state = 7u32;
        for i in 0..5000u32 {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let width = 1 + (state >> 27);
            let v = state.rotate_left(i % 32);
            a.put(v, width);
            b.write_u32(v, width);
            if i % 7 == 0 {
                let k = i % 9;
                let u = (state >> 20) & 0x3FF;
                a.put_rice(u, k);
                b.write_unary(u >> k);
                if k > 0 {
                    b.write_u32(u & ((1 << k) - 1), k);
                }
            }
        }
        // Long unary run crosses the 32-bit split path.
        a.put_rice(100, 2);
        b.write_unary(25);
        b.write_u32(0, 2);
        b.align_to_byte();
        assert_eq!(a.finish(), b.into_bytes());
    }

    #[test]
    fn fixed_magnitudes_match_direct_residuals() {
        let s: Vec<i32> = (0..300).map(|i| ((i * i * 37) % 2001) - 1000).collect();
        let sums = fixed_magnitudes(&s);
        for (order, &sum) in sums.iter().enumerate() {
            let mut res = Vec::new();
            assert!(fixed_residual(&s, order, &mut res));
            // Residual index i corresponds to sample i + order; compare over
            // samples 4.. only.
            let direct: u64 = res[4 - order..]
                .iter()
                .map(|r| r.unsigned_abs() as u64)
                .sum();
            assert_eq!(sum, direct, "order {order}");
        }
    }

    #[test]
    fn partition_planner_splits_nonstationary_residual() {
        // Quiet first half, loud second half: a single Rice parameter wastes
        // bits on one side, so the planner must pick partition order >= 1.
        let mut res = Vec::new();
        let mut state = 99u32;
        for i in 0..4096 {
            state ^= state << 13;
            state ^= state >> 17;
            state ^= state << 5;
            let amp = if i < 2048 { 3 } else { 3000 };
            res.push((state % (2 * amp + 1)) as i32 - amp as i32);
        }
        let mut fs = FastScratch::default();
        let (_, _, po) = plan_partitions(&res, 4096, 0, 8, &mut fs);
        assert!(po >= 1, "partition order {po}");
    }

    #[test]
    fn residual_of_i32_min_is_rejected() {
        // RFC 9639 §9.2.7.3: a residual equal to -2^31 is not representable.
        let s = [0, i32::MIN];
        let mut out = Vec::new();
        assert!(!fixed_residual(&s, 0, &mut out));
        assert!(!fixed_residual(&s, 1, &mut out));
        assert!(fixed_residual(&[0, i32::MIN + 1], 1, &mut out));
    }
}
