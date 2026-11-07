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
    let k0 = floor_log2_mean(sum, n).min(k_max);
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

/// `floor(log2(floor(sum / n)))`, or 0 when `sum < n`, without a
/// division: it is the largest `k` with `n·2^k ≤ sum`, which is either the
/// difference of the two bit lengths or one less.
#[inline]
fn floor_log2_mean(sum: u64, n: u64) -> u32 {
    debug_assert!(n > 0);
    if sum < n {
        return 0;
    }
    let k = n.leading_zeros() - sum.leading_zeros();
    // `n << k` has the same bit length as `sum`, so it cannot overflow.
    if (n << k) <= sum { k } else { k - 1 }
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
    // The 5-bit method (parameters up to 30) can only win where some
    // partition wants a parameter above 14. Every partition's closed-form
    // parameter is floor(log2(mean)), and a merged partition's mean never
    // exceeds the larger of its halves', so if every finest partition has
    // mean < 2^13 both methods try the same parameters at every level,
    // and the 5-bit method then costs exactly one bit more per partition:
    // it can never win, and its scoring is skipped.
    let wide = (0..n_fin).any(|p| {
        let n = if p == 0 { per - order } else { per } as u64;
        fs.sums[p] >= n << 13
    });

    // Walk from the finest order down, merging pairs in place. Each level
    // is scored under both Rice methods; ties keep the earlier (coarser,
    // 4-bit) candidate because levels are compared with strict `<` in the
    // ascending order the selection loop below replays.
    let mut totals = [[u64::MAX; 16]; 2];
    let mut n_parts = n_fin;
    let mut level = finest as i32;
    loop {
        let psize = (block_size >> level) as u64;
        let methods: &[(u64, u32)] = if wide {
            &[(4, 14), (5, 30)]
        } else {
            &[(4, 14)]
        };
        for (m, &(pb, kmax)) in methods.iter().enumerate() {
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

/// [`fixed_magnitudes`] for samples of at most 27 bits: the order-4
/// difference is then bounded by `16·2^26 = 2^30`, so every difference is
/// evaluated exactly in 32-bit lanes, directly from Table 20's polynomials
/// (no loop-carried state, so the loop vectorises). Same sums.
fn fixed_magnitudes_narrow(s: &[i32]) -> [u64; 5] {
    let mut sums = [0u64; 5];
    if s.len() <= 4 {
        return sums;
    }
    for w in s.windows(5) {
        let &[x4, x3, x2, x1, x0] = w else {
            unreachable!("window of 5")
        };
        let d1 = x0 - x1;
        let d2 = d1 - (x1 - x2);
        let d3 = x0 - 3 * x1 + 3 * x2 - x3;
        let d4 = x0 - 4 * x1 + 6 * x2 - 4 * x3 + x4;
        sums[0] += x0.unsigned_abs() as u64;
        sums[1] += d1.unsigned_abs() as u64;
        sums[2] += d2.unsigned_abs() as u64;
        sums[3] += d3.unsigned_abs() as u64;
        sums[4] += d4.unsigned_abs() as u64;
    }
    sums
}

/// Best fixed order by magnitude and its rough Rice cost estimate.
fn best_fixed_estimate(s: &[i32], bps: u32) -> (usize, u64) {
    let n = s.len();
    if n <= 4 {
        return (0, n as u64 * bps as u64);
    }
    let sums = if bps <= 27 {
        fixed_magnitudes_narrow(s)
    } else {
        fixed_magnitudes(s)
    };
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

/// [`fixed_residual`] for samples of at most 27 bits: every fixed
/// residual is then bounded by `16·2^26 = 2^30`, so it is computed exactly
/// in 32-bit lanes and always inside the §9.2.7.3 range.
fn fixed_residual_narrow(s: &[i32], order: usize, out: &mut Vec<i32>) {
    out.clear();
    out.resize(s.len() - order, 0);
    let tail = &s[order..];
    match order {
        0 => out.copy_from_slice(s),
        1 => {
            for ((o, &x0), w) in out.iter_mut().zip(tail).zip(s.windows(1)) {
                *o = x0 - w[0];
            }
        }
        2 => {
            for ((o, &x0), w) in out.iter_mut().zip(tail).zip(s.windows(2)) {
                *o = x0 - 2 * w[1] + w[0];
            }
        }
        3 => {
            for ((o, &x0), w) in out.iter_mut().zip(tail).zip(s.windows(3)) {
                *o = x0 - 3 * w[2] + 3 * w[1] - w[0];
            }
        }
        _ => {
            for ((o, &x0), w) in out.iter_mut().zip(tail).zip(s.windows(4)) {
                *o = x0 - 4 * w[3] + 6 * w[2] - 4 * w[1] + w[0];
            }
        }
    }
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
///
/// When `max|s| · (1 + Σ|q|) < 2^31` every partial dot product and every
/// residual fits an `i32` exactly, so the kernel runs in 32-bit lanes
/// (and no residual can leave the §9.2.7.3 range); otherwise it
/// accumulates in 64 bits. Both produce the same residual.
fn lpc_residual(s: &[i32], q: &[i32], shift: u32, out: &mut Vec<i32>) -> bool {
    let peak = s.iter().fold(0u32, |m, &x| m.max(x.unsigned_abs())) as u64;
    let qsum: u64 = q.iter().map(|&c| c.unsigned_abs() as u64).sum();
    let narrow = peak * (1 + qsum) < 1u64 << 31;
    macro_rules! fixed_n {
        ($($n:literal)*) => {
            match q.len() {
                $($n => return if narrow {
                    lpc_residual_n32::<$n>(s, q, shift, out)
                } else {
                    lpc_residual_n::<$n>(s, q, shift, out)
                },)*
                _ => {}
            }
        };
    }
    fixed_n!(1 2 3 4 5 6 7 8 9 10 11 12);
    lpc_residual_any(s, q, shift, out)
}

/// Order-`N` kernel in 32-bit arithmetic; the caller guarantees
/// `max|s| · (1 + Σ|q|) < 2^31`, which bounds every intermediate and the
/// residual strictly inside `(-2^31, 2^31)`.
fn lpc_residual_n32<const N: usize>(s: &[i32], q: &[i32], shift: u32, out: &mut Vec<i32>) -> bool {
    let mut rev = [0i32; N];
    for (j, &c) in q.iter().enumerate() {
        rev[N - 1 - j] = c;
    }
    out.clear();
    out.resize(s.len() - N, 0);
    for (o, (win, &x)) in out.iter_mut().zip(s.windows(N).zip(&s[N..])) {
        let hist: &[i32; N] = win.try_into().expect("window of N");
        let mut acc = 0i32;
        for j in 0..N {
            acc += rev[j] * hist[j];
        }
        *o = x - (acc >> shift);
    }
    true
}

/// Order-`N` kernel with 64-bit accumulation (32×32→64 products).
fn lpc_residual_n<const N: usize>(s: &[i32], q: &[i32], shift: u32, out: &mut Vec<i32>) -> bool {
    let mut rev = [0i32; N];
    for (j, &c) in q.iter().enumerate() {
        rev[N - 1 - j] = c;
    }
    out.clear();
    out.resize(s.len() - N, 0);
    let mut ok = true;
    for (o, (win, &x)) in out.iter_mut().zip(s.windows(N).zip(&s[N..])) {
        let hist: &[i32; N] = win.try_into().expect("window of N");
        let mut acc = 0i64;
        for j in 0..N {
            acc += rev[j] as i64 * hist[j] as i64;
        }
        let r = x as i64 - (acc >> shift);
        ok &= r > i32::MIN as i64 && r <= i32::MAX as i64;
        *o = r as i32;
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

/// Autocorrelation `ac[lag] = Σ_i x[i + lag]·x[i]` for every lag in
/// `0..ac.len()`.
///
/// Each lag sums into four interleaved lanes (lane `l` takes the products
/// at `i ≡ l (mod 4)`, in increasing `i`), then adds a scalar tail: the
/// independent lanes pipeline instead of serialising every product on one
/// add chain. Four lags are evaluated per pass over `x`, sharing the loads
/// of the unshifted operand and giving sixteen independent accumulators;
/// every lag still performs exactly the same additions in the same order
/// as a lag-at-a-time loop would, so the result is bit-identical to it.
fn autocorrelate(x: &[f64], ac: &mut [f64]) {
    let n = x.len();
    let lags = ac.len();
    debug_assert!(lags <= n);
    // Finish lag `k` from chunk `c0` on: remaining full chunks, then the
    // tail, onto the lane sums accumulated so far.
    let finish = |k: usize, c0: usize, mut lanes: [f64; 4]| -> f64 {
        let len = n - k;
        let full = len / 4;
        for c in c0..full {
            for l in 0..4 {
                lanes[l] += x[k + 4 * c + l] * x[4 * c + l];
            }
        }
        let mut tail = 0f64;
        for i in 4 * full..len {
            tail += x[k + i] * x[i];
        }
        (lanes[0] + lanes[1]) + (lanes[2] + lanes[3]) + tail
    };
    let mut g = 0;
    while g + 4 <= lags {
        // Chunks every lag of the group has in full.
        let common = (n - (g + 3)) / 4;
        let mut lanes = [[0f64; 4]; 4];
        let (head, _) = x[..4 * common].as_chunks::<4>();
        for (c, b) in head.iter().enumerate() {
            let at = 4 * c + g;
            let a: &[f64; 7] = x[at..at + 7].try_into().expect("7 samples");
            for d in 0..4 {
                for l in 0..4 {
                    lanes[d][l] += a[d + l] * b[l];
                }
            }
        }
        for d in 0..4 {
            ac[g + d] = finish(g + d, common, lanes[d]);
        }
        g += 4;
    }
    for k in g..lags {
        ac[k] = finish(k, 0, [0f64; 4]);
    }
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

    fs.autoc.clear();
    fs.autoc.resize(max + 1, 0.0);
    autocorrelate(x, &mut fs.autoc);
    let ac = &fs.autoc[..];
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
    let fixed_ok = if bps <= 27 {
        fixed_residual_narrow(&s, fixed_order, &mut fs.residual);
        true
    } else {
        fixed_residual(&s, fixed_order, &mut fs.residual)
    };
    if fixed_ok {
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

/// MSB-first bit writer. The pending bits sit left-aligned in a 64-bit
/// accumulator; every write stores the whole accumulator at the current
/// byte position and advances by the number of completed bytes, so there
/// is no data-dependent branch per codeword. The bytes past the cursor are
/// scratch that later stores overwrite; [`Bits::finish`] cuts them off.
pub(super) struct Bits {
    out: Vec<u8>,
    /// Byte position of the first incomplete byte.
    pos: usize,
    /// Pending bits, left-aligned (bit 63 is the next bit of the stream).
    acc: u64,
    /// Valid bits in `acc` (always < 8 between calls).
    n: u32,
}

impl Bits {
    /// Continue after `prefix`, which must be byte-aligned bytes.
    pub fn after(prefix: Vec<u8>) -> Self {
        let pos = prefix.len();
        let mut out = prefix;
        out.resize(pos + 4096, 0);
        Bits {
            out,
            pos,
            acc: 0,
            n: 0,
        }
    }

    #[inline(always)]
    fn put(&mut self, v: u32, bits: u32) {
        debug_assert!(bits <= 32 && self.n < 8);
        if self.pos + 8 > self.out.len() {
            self.grow();
        }
        let v = (v as u64) & ((1u64 << bits) - 1);
        // `n + bits <= 39`, so the shift is at least 25 — except for an
        // empty write at `n == 0`, where `v` is 0 and the wrapped shift is
        // harmless.
        self.acc |= v.wrapping_shl(64 - self.n - bits);
        self.n += bits;
        let at = self.pos;
        self.out[at..at + 8].copy_from_slice(&self.acc.to_be_bytes());
        let whole = self.n >> 3;
        self.pos += whole as usize;
        self.acc = self.acc.wrapping_shl(whole * 8);
        self.n &= 7;
    }

    #[cold]
    fn grow(&mut self) {
        let len = (self.out.len() * 2).max(self.pos + 64);
        self.out.resize(len, 0);
    }

    #[inline]
    fn put_signed(&mut self, v: i32, bits: u32) {
        self.put(v as u32, bits);
    }

    #[inline(always)]
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
        // The incomplete byte was already stored by the last write, with
        // zero bits below the valid ones.
        let end = self.pos + usize::from(self.n > 0);
        self.out.truncate(end);
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
    fn narrow_fixed_magnitudes_match_the_wide_ones() {
        let mut st = 0x9E37_79B9u32;
        for bits in [8u32, 16, 17, 24, 25, 27] {
            let half = 1i64 << (bits - 1);
            let s: Vec<i32> = (0..1000)
                .map(|i| {
                    st ^= st << 13;
                    st ^= st >> 17;
                    st ^= st << 5;
                    // Alternate full-scale extremes with noise: the worst
                    // case for the order-4 difference.
                    if i % 7 < 3 {
                        if i % 2 == 0 { half - 1 } else { -half }
                    } else {
                        (st as i64).rem_euclid(2 * half) - half
                    }
                })
                .map(|v| v as i32)
                .collect();
            assert_eq!(
                fixed_magnitudes_narrow(&s),
                fixed_magnitudes(&s),
                "{bits} bits"
            );
        }
    }

    #[test]
    fn narrow_fixed_residual_matches_the_wide_one() {
        let half = 1i32 << 26;
        let s: Vec<i32> = (0..600)
            .map(|i| match i % 5 {
                0 | 2 => half - 1,
                1 | 3 => -half,
                _ => (i * 7919) % half - half / 2,
            })
            .collect();
        for order in 0..=4 {
            let (mut a, mut b) = (Vec::new(), Vec::new());
            fixed_residual_narrow(&s, order, &mut a);
            assert!(fixed_residual(&s, order, &mut b));
            assert_eq!(a, b, "order {order}");
        }
    }

    #[test]
    fn floor_log2_mean_matches_division() {
        let mut st = 12345u32;
        for _ in 0..20_000 {
            st ^= st << 13;
            st ^= st >> 17;
            st ^= st << 5;
            let n = 1 + (st % 5000) as u64;
            let sum = ((st as u64) << (st % 29)) ^ (st as u64 >> 3);
            let mean = sum / n;
            let want = if mean == 0 {
                0
            } else {
                63 - mean.leading_zeros()
            };
            assert_eq!(floor_log2_mean(sum, n), want, "sum {sum} n {n}");
        }
        for n in 1..300u64 {
            for sum in 0..2000u64 {
                let mean = sum / n;
                let want = if mean == 0 {
                    0
                } else {
                    63 - mean.leading_zeros()
                };
                assert_eq!(floor_log2_mean(sum, n), want);
            }
        }
    }

    #[test]
    fn narrow_lpc_kernel_matches_the_wide_one() {
        let mut st = 777u32;
        let s: Vec<i32> = (0..2000)
            .map(|_| {
                st ^= st << 13;
                st ^= st >> 17;
                st ^= st << 5;
                (st as i32) >> 16
            })
            .collect();
        let q = [1200, -900, 700, -500, 300, -100, 50, -25];
        let (mut a, mut b) = (Vec::new(), Vec::new());
        assert!(lpc_residual_n32::<8>(&s, &q, 10, &mut a));
        assert!(lpc_residual_n::<8>(&s, &q, 10, &mut b));
        assert_eq!(a, b);
        assert!(lpc_residual(&s, &q, 10, &mut a));
        assert_eq!(a, b);
    }

    #[test]
    fn autocorrelation_matches_lag_at_a_time() {
        let x: Vec<f64> = (0..1037)
            .map(|i| ((i * 7919) % 211) as f64 - 105.3)
            .collect();
        for lags in [1usize, 3, 4, 5, 8, 13, 33] {
            let mut ac = vec![0f64; lags];
            autocorrelate(&x, &mut ac);
            for (lag, &v) in ac.iter().enumerate() {
                let a = &x[lag..];
                let b = &x[..x.len() - lag];
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
                let want = (lanes[0] + lanes[1]) + (lanes[2] + lanes[3]) + tail;
                assert_eq!(v.to_bits(), want.to_bits(), "lag {lag} of {lags}");
            }
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
