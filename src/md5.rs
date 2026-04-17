//! Minimal pure-Rust MD5 (RFC 1321).
//!
//! Used only to populate the STREAMINFO signature field. MD5 is broken
//! as a cryptographic primitive — do **not** reuse this for anything
//! security-sensitive.

pub struct Md5 {
    state: [u32; 4],
    buffer: [u8; 64],
    buffer_len: usize,
    bits_hi: u32,
    bits_lo: u32,
}

impl Md5 {
    pub fn new() -> Self {
        Self {
            state: [0x6745_2301, 0xefcd_ab89, 0x98ba_dcfe, 0x1032_5476],
            buffer: [0u8; 64],
            buffer_len: 0,
            bits_hi: 0,
            bits_lo: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        let added = (data.len() as u64) << 3;
        let added_lo = added as u32;
        let added_hi = (added >> 32) as u32;
        let (new_lo, carry) = self.bits_lo.overflowing_add(added_lo);
        self.bits_lo = new_lo;
        self.bits_hi = self
            .bits_hi
            .wrapping_add(added_hi)
            .wrapping_add(carry as u32);

        if self.buffer_len > 0 {
            let need = 64 - self.buffer_len;
            if data.len() < need {
                self.buffer[self.buffer_len..self.buffer_len + data.len()].copy_from_slice(data);
                self.buffer_len += data.len();
                return;
            }
            let (head, rest) = data.split_at(need);
            self.buffer[self.buffer_len..].copy_from_slice(head);
            let block = self.buffer;
            process_block(&mut self.state, &block);
            self.buffer_len = 0;
            data = rest;
        }
        while data.len() >= 64 {
            let (chunk, rest) = data.split_at(64);
            let mut block = [0u8; 64];
            block.copy_from_slice(chunk);
            process_block(&mut self.state, &block);
            data = rest;
        }
        if !data.is_empty() {
            self.buffer[..data.len()].copy_from_slice(data);
            self.buffer_len = data.len();
        }
    }

    pub fn finalize(mut self) -> [u8; 16] {
        let bits_lo = self.bits_lo;
        let bits_hi = self.bits_hi;
        let mut pad = [0u8; 64];
        pad[0] = 0x80;
        let pad_len = if self.buffer_len < 56 {
            56 - self.buffer_len
        } else {
            120 - self.buffer_len
        };
        self.update(&pad[..pad_len]);
        let mut tail = [0u8; 8];
        tail[0..4].copy_from_slice(&bits_lo.to_le_bytes());
        tail[4..8].copy_from_slice(&bits_hi.to_le_bytes());
        self.update(&tail);
        debug_assert_eq!(self.buffer_len, 0);
        let mut out = [0u8; 16];
        for (i, w) in self.state.iter().enumerate() {
            out[i * 4..i * 4 + 4].copy_from_slice(&w.to_le_bytes());
        }
        out
    }
}

impl Default for Md5 {
    fn default() -> Self {
        Self::new()
    }
}

fn process_block(state: &mut [u32; 4], block: &[u8; 64]) {
    let mut x = [0u32; 16];
    for i in 0..16 {
        x[i] = u32::from_le_bytes([
            block[i * 4],
            block[i * 4 + 1],
            block[i * 4 + 2],
            block[i * 4 + 3],
        ]);
    }
    let [mut a, mut b, mut c, mut d] = *state;

    macro_rules! round {
        ($f:expr, $a:ident, $b:ident, $c:ident, $d:ident, $xi:expr, $s:expr, $ac:expr) => {
            $a = $b.wrapping_add(
                $a.wrapping_add($f($b, $c, $d))
                    .wrapping_add($xi)
                    .wrapping_add($ac)
                    .rotate_left($s),
            );
        };
    }

    let f = |b: u32, c: u32, d: u32| (b & c) | (!b & d);
    let g = |b: u32, c: u32, d: u32| (b & d) | (c & !d);
    let h = |b: u32, c: u32, d: u32| b ^ c ^ d;
    let i_fn = |b: u32, c: u32, d: u32| c ^ (b | !d);

    // Round 1.
    round!(f, a, b, c, d, x[0], 7, 0xd76a_a478);
    round!(f, d, a, b, c, x[1], 12, 0xe8c7_b756);
    round!(f, c, d, a, b, x[2], 17, 0x2420_70db);
    round!(f, b, c, d, a, x[3], 22, 0xc1bd_ceee);
    round!(f, a, b, c, d, x[4], 7, 0xf57c_0faf);
    round!(f, d, a, b, c, x[5], 12, 0x4787_c62a);
    round!(f, c, d, a, b, x[6], 17, 0xa830_4613);
    round!(f, b, c, d, a, x[7], 22, 0xfd46_9501);
    round!(f, a, b, c, d, x[8], 7, 0x6980_98d8);
    round!(f, d, a, b, c, x[9], 12, 0x8b44_f7af);
    round!(f, c, d, a, b, x[10], 17, 0xffff_5bb1);
    round!(f, b, c, d, a, x[11], 22, 0x895c_d7be);
    round!(f, a, b, c, d, x[12], 7, 0x6b90_1122);
    round!(f, d, a, b, c, x[13], 12, 0xfd98_7193);
    round!(f, c, d, a, b, x[14], 17, 0xa679_438e);
    round!(f, b, c, d, a, x[15], 22, 0x49b4_0821);
    // Round 2.
    round!(g, a, b, c, d, x[1], 5, 0xf61e_2562);
    round!(g, d, a, b, c, x[6], 9, 0xc040_b340);
    round!(g, c, d, a, b, x[11], 14, 0x265e_5a51);
    round!(g, b, c, d, a, x[0], 20, 0xe9b6_c7aa);
    round!(g, a, b, c, d, x[5], 5, 0xd62f_105d);
    round!(g, d, a, b, c, x[10], 9, 0x0244_1453);
    round!(g, c, d, a, b, x[15], 14, 0xd8a1_e681);
    round!(g, b, c, d, a, x[4], 20, 0xe7d3_fbc8);
    round!(g, a, b, c, d, x[9], 5, 0x21e1_cde6);
    round!(g, d, a, b, c, x[14], 9, 0xc337_07d6);
    round!(g, c, d, a, b, x[3], 14, 0xf4d5_0d87);
    round!(g, b, c, d, a, x[8], 20, 0x455a_14ed);
    round!(g, a, b, c, d, x[13], 5, 0xa9e3_e905);
    round!(g, d, a, b, c, x[2], 9, 0xfcef_a3f8);
    round!(g, c, d, a, b, x[7], 14, 0x676f_02d9);
    round!(g, b, c, d, a, x[12], 20, 0x8d2a_4c8a);
    // Round 3.
    round!(h, a, b, c, d, x[5], 4, 0xfffa_3942);
    round!(h, d, a, b, c, x[8], 11, 0x8771_f681);
    round!(h, c, d, a, b, x[11], 16, 0x6d9d_6122);
    round!(h, b, c, d, a, x[14], 23, 0xfde5_380c);
    round!(h, a, b, c, d, x[1], 4, 0xa4be_ea44);
    round!(h, d, a, b, c, x[4], 11, 0x4bde_cfa9);
    round!(h, c, d, a, b, x[7], 16, 0xf6bb_4b60);
    round!(h, b, c, d, a, x[10], 23, 0xbebf_bc70);
    round!(h, a, b, c, d, x[13], 4, 0x289b_7ec6);
    round!(h, d, a, b, c, x[0], 11, 0xeaa1_27fa);
    round!(h, c, d, a, b, x[3], 16, 0xd4ef_3085);
    round!(h, b, c, d, a, x[6], 23, 0x0488_1d05);
    round!(h, a, b, c, d, x[9], 4, 0xd9d4_d039);
    round!(h, d, a, b, c, x[12], 11, 0xe6db_99e5);
    round!(h, c, d, a, b, x[15], 16, 0x1fa2_7cf8);
    round!(h, b, c, d, a, x[2], 23, 0xc4ac_5665);
    // Round 4.
    round!(i_fn, a, b, c, d, x[0], 6, 0xf429_2244);
    round!(i_fn, d, a, b, c, x[7], 10, 0x432a_ff97);
    round!(i_fn, c, d, a, b, x[14], 15, 0xab94_23a7);
    round!(i_fn, b, c, d, a, x[5], 21, 0xfc93_a039);
    round!(i_fn, a, b, c, d, x[12], 6, 0x655b_59c3);
    round!(i_fn, d, a, b, c, x[3], 10, 0x8f0c_cc92);
    round!(i_fn, c, d, a, b, x[10], 15, 0xffef_f47d);
    round!(i_fn, b, c, d, a, x[1], 21, 0x8584_5dd1);
    round!(i_fn, a, b, c, d, x[8], 6, 0x6fa8_7e4f);
    round!(i_fn, d, a, b, c, x[15], 10, 0xfe2c_e6e0);
    round!(i_fn, c, d, a, b, x[6], 15, 0xa301_4314);
    round!(i_fn, b, c, d, a, x[13], 21, 0x4e08_11a1);
    round!(i_fn, a, b, c, d, x[4], 6, 0xf753_7e82);
    round!(i_fn, d, a, b, c, x[11], 10, 0xbd3a_f235);
    round!(i_fn, c, d, a, b, x[2], 15, 0x2ad7_d2bb);
    round!(i_fn, b, c, d, a, x[9], 21, 0xeb86_d391);

    state[0] = state[0].wrapping_add(a);
    state[1] = state[1].wrapping_add(b);
    state[2] = state[2].wrapping_add(c);
    state[3] = state[3].wrapping_add(d);
}

pub fn compute(data: &[u8]) -> [u8; 16] {
    let mut m = Md5::new();
    m.update(data);
    m.finalize()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8; 16]) -> String {
        let mut s = String::with_capacity(32);
        for b in bytes {
            s.push_str(&format!("{:02x}", b));
        }
        s
    }

    #[test]
    fn md5_empty() {
        assert_eq!(hex(&compute(b"")), "d41d8cd98f00b204e9800998ecf8427e");
    }

    #[test]
    fn md5_short() {
        assert_eq!(hex(&compute(b"abc")), "900150983cd24fb0d6963f7d28e17f72");
    }

    #[test]
    fn md5_longer() {
        assert_eq!(
            hex(&compute(b"The quick brown fox jumps over the lazy dog")),
            "9e107d9d372bb6826bd81d3542a419d6"
        );
    }

    #[test]
    fn md5_streamed_matches_oneshot() {
        let data = b"The quick brown fox jumps over the lazy dog";
        let one = compute(data);
        let mut m = Md5::new();
        for chunk in data.chunks(7) {
            m.update(chunk);
        }
        assert_eq!(m.finalize(), one);
    }

    #[test]
    fn md5_block_boundary_lengths() {
        // Exercise the branch where final padding crosses a block boundary
        // (inputs of length 55 / 56 / 63 / 64 / 65 stress the edge cases).
        for &len in &[0usize, 1, 55, 56, 57, 63, 64, 65, 119, 120, 121] {
            let data: Vec<u8> = (0..len as u8).collect();
            let oneshot = compute(&data);
            let mut m = Md5::new();
            for chunk in data.chunks(3) {
                m.update(chunk);
            }
            assert_eq!(m.finalize(), oneshot, "len={len}");
        }
    }
}
