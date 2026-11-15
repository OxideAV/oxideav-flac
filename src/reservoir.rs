//! Bit reservoir for subframe decoding.
//!
//! A FLAC subframe is a most-significant-bit-first bitstream (RFC 9639
//! §9.2). The reservoir keeps up to 64 bits of it in one machine word,
//! left-aligned, and tops the word up eight bytes at a time, so the
//! residual decoder can take a whole Rice code word — unary prefix, stop
//! bit and binary suffix — out of the word with one leading-zero count and
//! two shifts instead of bit-by-bit reads.
//!
//! Invariant: the top `have` bits of `word` are the next `have` unread
//! bits of the stream. A bit below them is either zero or a copy of the
//! stream bit that will later occupy that position, so topping the word
//! up by OR-ing in the following bytes never disturbs it.

use oxideav_core::{Error, Result};

/// Number of bits a top-up guarantees while at least eight bytes of input
/// remain: the word then holds between 56 and 63 bits.
const FULL: u32 = 56;

/// The Rice loop tops the word up when it holds fewer bits than this. A
/// code word longer than what is held takes the general path, which tops
/// up again, so the value only trades top-ups against that detour.
const RICE_REFILL_BELOW: u32 = 32;

/// Largest folded residual the format allows. RFC 9639 §9.2.7.3 limits a
/// residual to the range of a 32-bit two's-complement integer without its
/// most negative value, i.e. `-(2^31 - 1) ..= 2^31 - 1`; folding (zigzag)
/// maps that range onto `0 ..= 2^32 - 2`.
const MAX_FOLDED: u64 = 0xFFFF_FFFE;

/// A decoded sample slot: `i32` for every ordinary channel, `i64` for the
/// 33-bit side channel of a 32-bit stereo-decorrelated frame (RFC 9639
/// §4.2, Appendix A.2).
pub trait Sample: Copy + Default {
    /// Store `v`, keeping the low bits that fit the slot.
    fn from_i64(v: i64) -> Self;
    /// The stored value, sign-extended.
    fn to_i64(self) -> i64;
}

impl Sample for i32 {
    #[inline(always)]
    fn from_i64(v: i64) -> Self {
        v as i32
    }
    #[inline(always)]
    fn to_i64(self) -> i64 {
        self as i64
    }
}

impl Sample for i64 {
    #[inline(always)]
    fn from_i64(v: i64) -> Self {
        v
    }
    #[inline(always)]
    fn to_i64(self) -> i64 {
        self
    }
}

/// MSB-first bit reader over a byte slice with a 64-bit reservoir.
#[derive(Clone, Debug)]
pub struct BitReservoir<'a> {
    data: &'a [u8],
    /// Index of the first byte not yet moved into `word`.
    next: usize,
    word: u64,
    have: u32,
}

fn out_of_bits() -> Error {
    Error::invalid("FLAC bitstream: out of bits")
}

impl<'a> BitReservoir<'a> {
    /// Start reading at the first bit of `data`.
    pub fn new(data: &'a [u8]) -> Self {
        Self {
            data,
            next: 0,
            word: 0,
            have: 0,
        }
    }

    /// Bits consumed so far.
    pub fn bit_position(&self) -> u64 {
        self.next as u64 * 8 - self.have as u64
    }

    /// Whole bytes consumed so far (rounded up when inside a byte).
    pub fn byte_position(&self) -> usize {
        self.next - (self.have / 8) as usize
    }

    /// Bits left in the stream.
    pub fn bits_remaining(&self) -> u64 {
        (self.data.len() - self.next) as u64 * 8 + self.have as u64
    }

    /// True when the read position is on a byte boundary.
    pub fn is_byte_aligned(&self) -> bool {
        self.have.is_multiple_of(8)
    }

    /// Skip to the next byte boundary (no-op when already on one).
    pub fn align_to_byte(&mut self) {
        let drop = self.have % 8;
        self.word <<= drop;
        self.have -= drop;
    }

    /// Top the word up. Afterwards it holds at least [`FULL`] bits, or
    /// every remaining bit of the stream when fewer are left.
    #[inline(always)]
    fn top_up(&mut self) {
        if self.have >= FULL {
            return;
        }
        if self.next + 8 <= self.data.len() {
            let chunk = &self.data[self.next..self.next + 8];
            let incoming = u64::from_be_bytes(chunk.try_into().expect("8-byte chunk"));
            self.word |= incoming >> self.have;
            // As many whole bytes as fit while staying at or below 63
            // bits: `(63 - have) / 8` of them, which leaves `56 + have % 8`
            // bits. The part of `incoming` past those bytes stays in the
            // word as look-ahead (see the invariant).
            self.next += ((63 - self.have) >> 3) as usize;
            self.have |= 56;
        } else {
            self.top_up_tail();
        }
    }

    #[cold]
    fn top_up_tail(&mut self) {
        while self.have <= 56 && self.next < self.data.len() {
            self.word |= (self.data[self.next] as u64) << (56 - self.have);
            self.have += 8;
            self.next += 1;
        }
    }

    /// Read `n` bits, `n <= 56`, as an unsigned number.
    #[inline(always)]
    fn take(&mut self, n: u32) -> Result<u64> {
        debug_assert!(n <= FULL);
        if n == 0 {
            return Ok(0);
        }
        if self.have < n {
            self.top_up();
            if self.have < n {
                return Err(out_of_bits());
            }
        }
        let v = self.word >> (64 - n);
        self.word <<= n;
        self.have -= n;
        Ok(v)
    }

    /// Read `n` bits (`0..=32`) as an unsigned number.
    #[inline]
    pub fn read_u32(&mut self, n: u32) -> Result<u32> {
        debug_assert!(n <= 32);
        Ok(self.take(n)? as u32)
    }

    /// Read `n` bits (`0..=64`) as an unsigned number.
    #[inline]
    pub fn read_u64(&mut self, n: u32) -> Result<u64> {
        debug_assert!(n <= 64);
        if n <= 32 {
            return self.take(n);
        }
        let hi = self.take(n - 32)?;
        let lo = self.take(32)?;
        Ok((hi << 32) | lo)
    }

    /// Read `n` bits (`0..=64`) as a two's-complement signed number. Zero
    /// bits read as the value zero (the 0-bit escaped partition of
    /// RFC 9639 §9.2.7.1).
    #[inline]
    pub fn read_signed(&mut self, n: u32) -> Result<i64> {
        if n == 0 {
            return Ok(0);
        }
        let raw = self.read_u64(n)? as i64;
        let unused = 64 - n;
        Ok((raw << unused) >> unused)
    }

    /// Read one bit.
    #[inline]
    pub fn read_bit(&mut self) -> Result<bool> {
        Ok(self.take(1)? != 0)
    }

    /// Read a unary number: the count of zero bits before the next one
    /// bit, which is consumed too.
    pub fn read_unary(&mut self) -> Result<u32> {
        let mut count: u32 = 0;
        loop {
            self.top_up();
            if self.have == 0 {
                return Err(out_of_bits());
            }
            let zeros = self.word.leading_zeros();
            if zeros < self.have {
                count = count
                    .checked_add(zeros)
                    .ok_or_else(|| Error::invalid("FLAC bitstream: unary run too long"))?;
                // `zeros <= 63`, shifted in two steps so 63 + 1 is legal.
                self.word = (self.word << zeros) << 1;
                self.have -= zeros + 1;
                return Ok(count);
            }
            // Every held bit is zero; drop them and look further.
            count = count
                .checked_add(self.have)
                .ok_or_else(|| Error::invalid("FLAC bitstream: unary run too long"))?;
            self.word = self.word.checked_shl(self.have).unwrap_or(0);
            self.have = 0;
        }
    }

    /// Decode `out.len()` Rice code words with parameter `k` (`0..=30`)
    /// into `out` (RFC 9639 §9.2.7.2): unary most-significant part, `k`
    /// binary least-significant bits, zigzag unfolding. A code word whose
    /// value is outside the residual limit of §9.2.7.3 is an error.
    pub fn read_rice_run<S: Sample>(&mut self, k: u32, out: &mut [S]) -> Result<()> {
        debug_assert!(k <= 30);
        // The reservoir lives in locals for the duration of the run and
        // is written back around the general path and at the end.
        let data = self.data;
        let (mut word, mut have, mut next) = (self.word, self.have, self.next);
        for slot in out {
            if have < RICE_REFILL_BELOW && next + 8 <= data.len() {
                let chunk = &data[next..next + 8];
                word |= u64::from_be_bytes(chunk.try_into().expect("8-byte chunk")) >> have;
                next += ((63 - have) >> 3) as usize;
                have |= 56;
            }
            // 64 for an all-zero word, which then fails the length test.
            let zeros = word.leading_zeros();
            let length = zeros + 1 + k;
            let folded = if length <= have.min(63) {
                // The whole code word is held: prefix, stop bit, suffix.
                // The suffix is the `k` bits after the stop bit; the two
                // step shifts keep `k == 0` legal.
                let low = ((word << zeros) << 1 >> 1) >> (63 - k);
                word <<= length;
                have -= length;
                ((zeros as u64) << k) | low
            } else {
                (self.word, self.have, self.next) = (word, have, next);
                let folded = self.rice_word_spanning(k);
                (word, have, next) = (self.word, self.have, self.next);
                folded?
            };
            if folded > MAX_FOLDED {
                (self.word, self.have, self.next) = (word, have, next);
                return Err(Error::invalid(
                    "FLAC residual: value outside the 32-bit residual range",
                ));
            }
            // Even: half. Odd: half with every bit flipped.
            let half = (folded >> 1) as i64;
            *slot = S::from_i64(half ^ -((folded & 1) as i64));
        }
        (self.word, self.have, self.next) = (word, have, next);
        Ok(())
    }

    /// A Rice code word that is not held in one piece: a long unary
    /// prefix, a nearly empty word, or the tail of the input.
    #[cold]
    fn rice_word_spanning(&mut self, k: u32) -> Result<u64> {
        let high = self.read_unary()? as u64;
        let low = self.take(k)?;
        Ok((high << k) | low)
    }

    /// Read `out.len()` two's-complement numbers of `n` bits each
    /// (`0..=33`): verbatim samples, warm-up samples, predictor
    /// coefficients and escaped partitions.
    pub fn read_signed_run<S: Sample>(&mut self, n: u32, out: &mut [S]) -> Result<()> {
        debug_assert!(n <= FULL);
        if n == 0 {
            out.fill(S::default());
            return Ok(());
        }
        let unused = 64 - n;
        for slot in out {
            let raw = self.take(n)? as i64;
            *slot = S::from_i64((raw << unused) >> unused);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reference bit source: one bit at a time from the same bytes.
    struct Slow<'a> {
        data: &'a [u8],
        pos: usize,
    }
    impl Slow<'_> {
        fn bit(&mut self) -> Option<u64> {
            let byte = *self.data.get(self.pos / 8)?;
            let b = (byte >> (7 - self.pos % 8)) & 1;
            self.pos += 1;
            Some(b as u64)
        }
        fn bits(&mut self, n: u32) -> Option<u64> {
            let mut v = 0;
            for _ in 0..n {
                v = (v << 1) | self.bit()?;
            }
            Some(v)
        }
        fn unary(&mut self) -> Option<u64> {
            let mut n = 0;
            while self.bit()? == 0 {
                n += 1;
            }
            Some(n)
        }
    }

    fn noise(len: usize, mut seed: u32, zero_bias: bool) -> Vec<u8> {
        (0..len)
            .map(|_| {
                seed ^= seed << 13;
                seed ^= seed >> 17;
                seed ^= seed << 5;
                let b = (seed >> 11) as u8;
                if zero_bias && seed & 0x300 != 0 { 0 } else { b }
            })
            .collect()
    }

    #[test]
    fn fixed_width_reads_match_bitwise_reference() {
        let data = noise(4096, 0xC0FF_EE11, false);
        let mut fast = BitReservoir::new(&data);
        let mut slow = Slow {
            data: &data,
            pos: 0,
        };
        let mut n = 0u32;
        loop {
            n = (n * 7 + 3) % 65; // 0..=64, every width visited
            let want = slow.bits(n);
            match want {
                Some(v) => {
                    assert_eq!(fast.read_u64(n).unwrap(), v, "width {n}");
                    assert_eq!(fast.bit_position(), slow.pos as u64);
                }
                None => {
                    assert!(fast.read_u64(n).is_err());
                    break;
                }
            }
        }
    }

    #[test]
    fn signed_reads_sign_extend_at_every_width() {
        for n in 1..=64u32 {
            let data = [0xFFu8; 16];
            let mut r = BitReservoir::new(&data);
            assert_eq!(r.read_signed(n).unwrap(), -1, "width {n}");
            let mut top = [0u8; 16];
            top[0] = 0x80;
            let mut r = BitReservoir::new(&top);
            let want = if n == 64 {
                i64::MIN
            } else {
                -(1i64 << (n - 1))
            };
            assert_eq!(r.read_signed(n).unwrap(), want, "width {n}");
            let mut low = [0xFFu8; 16];
            low[0] = 0x7F;
            let mut r = BitReservoir::new(&low);
            let want = if n == 64 {
                i64::MAX
            } else {
                (1i64 << (n - 1)) - 1
            };
            assert_eq!(r.read_signed(n).unwrap(), want, "width {n}");
        }
        let mut r = BitReservoir::new(&[]);
        assert_eq!(r.read_signed(0).unwrap(), 0);
        assert!(r.read_signed(1).is_err());
    }

    #[test]
    fn unary_and_rice_match_bitwise_reference() {
        for k in 0..=30u32 {
            let data = noise(3000, 0x9E37_79B9 ^ k, k % 3 == 0);
            let mut slow = Slow {
                data: &data,
                pos: 0,
            };
            let mut want = Vec::new();
            // Decode with the reference until it runs dry or leaves the
            // residual range.
            loop {
                let before = slow.pos;
                let word = slow
                    .unary()
                    .and_then(|q| slow.bits(k).map(|low| (q << k) | low));
                let Some(folded) = word.filter(|&f| f <= MAX_FOLDED) else {
                    slow.pos = before;
                    break;
                };
                let half = (folded >> 1) as i64;
                want.push(if folded & 1 == 0 { half } else { -half - 1 });
            }
            let mut got = vec![0i64; want.len()];
            let mut fast = BitReservoir::new(&data);
            fast.read_rice_run(k, &mut got).unwrap();
            assert_eq!(got, want, "k = {k}");
            assert_eq!(fast.bit_position(), slow.pos as u64, "k = {k}");
            // The narrow slot keeps the same values (they fit in 32 bits).
            let mut narrow = vec![0i32; want.len()];
            BitReservoir::new(&data)
                .read_rice_run(k, &mut narrow)
                .unwrap();
            assert!(narrow.iter().zip(&want).all(|(&a, &b)| a as i64 == b));
        }
    }

    #[test]
    fn unary_runs_longer_than_the_reservoir() {
        for zeros in [0usize, 1, 7, 8, 56, 57, 63, 64, 65, 127, 128, 1000] {
            let mut data = vec![0u8; zeros / 8 + 2];
            data[zeros / 8] = 0x80 >> (zeros % 8);
            let mut r = BitReservoir::new(&data);
            assert_eq!(r.read_unary().unwrap(), zeros as u32, "{zeros} zeros");
            assert_eq!(r.bit_position(), zeros as u64 + 1);
        }
        // No terminating one bit: an error, not a hang.
        assert!(BitReservoir::new(&[0u8; 100]).read_unary().is_err());
        assert!(BitReservoir::new(&[]).read_unary().is_err());
    }

    #[test]
    fn rice_rejects_values_outside_the_residual_range() {
        // k = 30: folded = (q << 30) | low. q = 3 with all-ones low bits
        // is 2^32 - 1 (the excluded most negative value); q = 4 is larger.
        let mut out = [0i32; 1];
        // 0b0001 then thirty 1 bits.
        let data = [0x1F, 0xFF, 0xFF, 0xFF, 0xC0, 0, 0, 0, 0, 0];
        assert!(
            BitReservoir::new(&data)
                .read_rice_run(30, &mut out)
                .is_err()
        );
        // Same prefix, low bits all ones but the last: 2^32 - 2, the
        // largest legal folded value, which unfolds to 2^31 - 1.
        let data = [0x1F, 0xFF, 0xFF, 0xFF, 0x80, 0, 0, 0, 0, 0];
        BitReservoir::new(&data)
            .read_rice_run(30, &mut out)
            .unwrap();
        assert_eq!(out[0], i32::MAX);
        // 2^32 - 3 unfolds to -(2^31 - 1).
        let data = [0x1F, 0xFF, 0xFF, 0xFF, 0x40, 0, 0, 0, 0, 0];
        BitReservoir::new(&data)
            .read_rice_run(30, &mut out)
            .unwrap();
        assert_eq!(out[0], -i32::MAX);
        // One more zero in the prefix (q = 4): 2^32 or more, also in the
        // wide slot.
        let mut out = [0i64; 1];
        let data = [0x08, 0, 0, 0, 0, 0, 0, 0, 0, 0];
        assert!(
            BitReservoir::new(&data)
                .read_rice_run(30, &mut out)
                .is_err()
        );
    }

    #[test]
    fn alignment_and_positions() {
        let data = [0xA5, 0x5A, 0xFF, 0x00, 0x12, 0x34, 0x56, 0x78, 0x9A, 0xBC];
        let mut r = BitReservoir::new(&data);
        assert!(r.is_byte_aligned());
        assert_eq!(r.byte_position(), 0);
        assert_eq!(r.read_u32(3).unwrap(), 0b101);
        assert!(!r.is_byte_aligned());
        r.align_to_byte();
        assert_eq!(r.byte_position(), 1);
        assert_eq!(r.bit_position(), 8);
        r.align_to_byte();
        assert_eq!(r.byte_position(), 1);
        assert_eq!(r.read_u32(8).unwrap(), 0x5A);
        assert_eq!(r.bits_remaining(), 64);
        assert_eq!(r.read_u64(64).unwrap(), 0xFF00_1234_5678_9ABC);
        assert_eq!(r.byte_position(), data.len());
        assert!(r.read_bit().is_err());
    }

    #[test]
    fn signed_runs_cover_zero_width_and_wide_slots() {
        let data = noise(600, 77, false);
        for n in 0..=33u32 {
            let mut slow = Slow {
                data: &data,
                pos: 0,
            };
            let count = 100;
            let want: Vec<i64> = (0..count)
                .map(|_| {
                    let raw = slow.bits(n).unwrap() as i64;
                    if n == 0 {
                        0
                    } else {
                        (raw << (64 - n)) >> (64 - n)
                    }
                })
                .collect();
            let mut got = vec![7i64; count];
            let mut r = BitReservoir::new(&data);
            r.read_signed_run(n, &mut got).unwrap();
            assert_eq!(got, want, "width {n}");
            assert_eq!(r.bit_position(), n as u64 * count as u64);
        }
    }
}
