//! memcmp/bcmp/memcpy/memmove through Rust's slice operations, checked against
//! byte loops. On musl targets these are bazel-rs's own src/musl_string.c
//! (build.rs); elsewhere the system libc's, which must agree all the same.

use std::cmp::Ordering;
use std::hint::black_box;

struct XorShift(u64);

impl XorShift {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

fn reference_cmp(a: &[u8], b: &[u8]) -> Ordering {
    for (x, y) in a.iter().zip(b) {
        if x != y {
            return x.cmp(y);
        }
    }
    a.len().cmp(&b.len())
}

/// Lengths that cross every size class of the implementation (0..=32 bytes,
/// the 32-byte loop up to 512, the large-copy path above it).
fn length(rng: &mut XorShift, round: usize) -> usize {
    match round % 4 {
        0 => rng.below(40),
        1 => rng.below(140),
        2 => rng.below(600),
        _ => rng.below(2100),
    }
}

#[test]
fn compare_matches_byte_loops() {
    let mut rng = XorShift(0x9e37_79b9_7f4a_7c15);
    let mut a = vec![0u8; 2200];
    let mut b = vec![0u8; 2200];
    for round in 0..40_000 {
        let n = length(&mut rng, round);
        let (oa, ob) = (rng.below(64), rng.below(64));
        for i in 0..n {
            // A small alphabet makes long equal prefixes likely.
            a[oa + i] = (rng.next() & 3) as u8;
            b[ob + i] = a[oa + i];
        }
        if n > 0 && rng.below(4) != 0 {
            let p = rng.below(n);
            b[ob + p] = (rng.next() & 0xff) as u8;
        }
        let x = black_box(&a[oa..oa + n]);
        let y = black_box(&b[ob..ob + n]);
        assert_eq!(x.cmp(y), reference_cmp(x, y), "memcmp, n={n}");
        assert_eq!(x == y, reference_cmp(x, y).is_eq(), "bcmp, n={n}");
        // Different lengths: the common prefix decides, then the length.
        let m = n.saturating_sub(rng.below(3));
        let z = black_box(&b[ob..ob + m]);
        assert_eq!(x.cmp(z), reference_cmp(x, z), "memcmp prefix, n={n} m={m}");
    }
}

#[test]
fn copies_match_byte_loops() {
    let mut rng = XorShift(0x2545_f491_4f6c_dd1d);
    let mut buf = vec![0u8; 2400];
    let mut expected = vec![0u8; 2400];
    let mut dst = vec![0u8; 2400];
    for round in 0..40_000 {
        buf.iter_mut().for_each(|byte| *byte = rng.next() as u8);
        expected.copy_from_slice(&buf);
        let n = length(&mut rng, round);
        let (src, to) = (rng.below(2400 - n + 1), rng.below(2400 - n + 1));
        // memmove: overlapping in both directions, or not at all. The
        // reference copies byte by byte (black_box: never turned into a
        // memmove call itself).
        for i in 0..n {
            expected[to + i] = black_box(buf[src + i]);
        }
        black_box(&mut buf).copy_within(src..src + n, to);
        assert!(buf == expected, "memmove src={src} to={to} n={n}");
        // memcpy: disjoint buffers.
        dst.iter_mut().for_each(|byte| *byte = 0);
        black_box(&mut dst[to..to + n]).copy_from_slice(black_box(&buf[src..src + n]));
        assert!(
            dst[to..to + n] == buf[src..src + n]
                && dst[..to].iter().all(|&b| b == 0)
                && dst[to + n..].iter().all(|&b| b == 0),
            "memcpy src={src} to={to} n={n}"
        );
    }
}
