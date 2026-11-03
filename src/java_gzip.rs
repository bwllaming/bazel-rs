//! The bytes of Java's `GZIPOutputStream` over one `write` of a buffer
//! (java.util.zip: a 10-byte header with MTIME 0, XFL 0 and OS 255, a raw
//! deflate stream from `Deflater(DEFAULT_COMPRESSION, nowrap)`, then the
//! CRC-32 and the size mod 2^32). `Deflater` is zlib: this module ports
//! zlib 1.3's deflate (deflate.c `deflate_slow`, `longest_match`,
//! `fill_window`; trees.c) for exactly the parameters Java uses — level 6
//! (good 8, lazy 16, nice 128, chain 128), windowBits 15, memLevel 8, the
//! default strategy — so the output is byte-identical to the JDK's.
//!
//! Bazel keys a FileWrite over 256 characters by these bytes
//! (CompressedFileWriteAction.computeKey, analysis/actions/
//! FileWriteAction.java:285-370), so `aquery` needs them exactly
//! (crate::aquery_key). Upstream source: madler/zlib v1.3.1.

const W_SIZE: usize = 1 << 15;
const W_MASK: usize = W_SIZE - 1;
const WINDOW_SIZE: usize = 2 * W_SIZE;
const HASH_SIZE: usize = 1 << 15;
const HASH_MASK: u32 = (HASH_SIZE - 1) as u32;
const HASH_SHIFT: u32 = 5;
const MIN_MATCH: usize = 3;
const MAX_MATCH: usize = 258;
const MIN_LOOKAHEAD: usize = MAX_MATCH + MIN_MATCH + 1;
const MAX_DIST: usize = W_SIZE - MIN_LOOKAHEAD;
const WIN_INIT: usize = MAX_MATCH;
const TOO_FAR: usize = 4096;
/// lit_bufsize - 1 symbols per block (memLevel 8: lit_bufsize 16K).
const SYM_END: usize = (1 << 14) - 1;
// configuration_table[6].
const GOOD_MATCH: usize = 8;
const MAX_LAZY: usize = 16;
const NICE_MATCH: usize = 128;
const MAX_CHAIN: u32 = 128;

const LENGTH_CODES: usize = 29;
const LITERALS: usize = 256;
const L_CODES: usize = LITERALS + 1 + LENGTH_CODES;
const D_CODES: usize = 30;
const BL_CODES: usize = 19;
const HEAP_SIZE: usize = 2 * L_CODES + 1;
const MAX_BITS: usize = 15;
const MAX_BL_BITS: usize = 7;
const END_BLOCK: usize = 256;
const REP_3_6: usize = 16;
const REPZ_3_10: usize = 17;
const REPZ_11_138: usize = 18;

const EXTRA_LBITS: [u32; LENGTH_CODES] = [
    0, 0, 0, 0, 0, 0, 0, 0, 1, 1, 1, 1, 2, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 4, 5, 5, 5, 5, 0,
];
const EXTRA_DBITS: [u32; D_CODES] = [
    0, 0, 0, 0, 1, 1, 2, 2, 3, 3, 4, 4, 5, 5, 6, 6, 7, 7, 8, 8, 9, 9, 10, 10, 11, 11, 12, 12, 13,
    13,
];
const EXTRA_BLBITS: [u32; BL_CODES] = [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 2, 3, 7];
const BL_ORDER: [usize; BL_CODES] = [
    16, 17, 18, 0, 8, 7, 9, 6, 10, 5, 11, 4, 12, 3, 13, 2, 14, 1, 15,
];

/// ct_data: `fc` is Freq or Code, `dl` is Dad or Len (C unions).
#[derive(Clone, Copy, Default)]
struct Ct {
    fc: u16,
    dl: u16,
}

/// trees.c's static tables (tr_static_init).
struct Tables {
    static_ltree: [Ct; L_CODES + 2],
    static_dtree: [Ct; D_CODES],
    dist_code: [u8; 512],
    length_code: [u8; MAX_MATCH - MIN_MATCH + 1],
    base_length: [u32; LENGTH_CODES],
    base_dist: [u32; D_CODES],
}

fn bi_reverse(mut code: u32, mut len: u32) -> u32 {
    let mut res = 0;
    loop {
        res |= code & 1;
        code >>= 1;
        res <<= 1;
        len -= 1;
        if len == 0 {
            break;
        }
    }
    res >> 1
}

fn gen_codes(tree: &mut [Ct], max_code: usize, bl_count: &[u16; MAX_BITS + 1]) {
    let mut next_code = [0u16; MAX_BITS + 1];
    let mut code: u32 = 0;
    for bits in 1..=MAX_BITS {
        code = (code + u32::from(bl_count[bits - 1])) << 1;
        next_code[bits] = code as u16;
    }
    for entry in tree.iter_mut().take(max_code + 1) {
        let len = usize::from(entry.dl);
        if len == 0 {
            continue;
        }
        entry.fc = bi_reverse(u32::from(next_code[len]), len as u32) as u16;
        next_code[len] = next_code[len].wrapping_add(1);
    }
}

impl Tables {
    fn new() -> Self {
        let mut tables = Self {
            static_ltree: [Ct::default(); L_CODES + 2],
            static_dtree: [Ct::default(); D_CODES],
            dist_code: [0; 512],
            length_code: [0; MAX_MATCH - MIN_MATCH + 1],
            base_length: [0; LENGTH_CODES],
            base_dist: [0; D_CODES],
        };
        let mut length = 0usize;
        let mut code = 0usize;
        while code < LENGTH_CODES - 1 {
            tables.base_length[code] = length as u32;
            for _ in 0..(1 << EXTRA_LBITS[code]) {
                tables.length_code[length] = code as u8;
                length += 1;
            }
            code += 1;
        }
        tables.length_code[length - 1] = code as u8;
        let mut dist = 0usize;
        code = 0;
        while code < 16 {
            tables.base_dist[code] = dist as u32;
            for _ in 0..(1 << EXTRA_DBITS[code]) {
                tables.dist_code[dist] = code as u8;
                dist += 1;
            }
            code += 1;
        }
        dist >>= 7;
        while code < D_CODES {
            tables.base_dist[code] = (dist << 7) as u32;
            for _ in 0..(1 << (EXTRA_DBITS[code] - 7)) {
                tables.dist_code[256 + dist] = code as u8;
                dist += 1;
            }
            code += 1;
        }
        let mut bl_count = [0u16; MAX_BITS + 1];
        for (n, entry) in tables.static_ltree.iter_mut().enumerate() {
            let len = match n {
                0..=143 => 8,
                144..=255 => 9,
                256..=279 => 7,
                _ => 8,
            };
            entry.dl = len;
            bl_count[usize::from(len)] += 1;
        }
        gen_codes(&mut tables.static_ltree, L_CODES + 1, &bl_count);
        for (n, entry) in tables.static_dtree.iter_mut().enumerate() {
            entry.dl = 5;
            entry.fc = bi_reverse(n as u32, 5) as u16;
        }
        tables
    }

    fn d_code(&self, dist: usize) -> usize {
        usize::from(if dist < 256 {
            self.dist_code[dist]
        } else {
            self.dist_code[256 + (dist >> 7)]
        })
    }
}

/// static_tree_desc.
#[derive(Clone, Copy)]
enum Desc {
    Literal,
    Distance,
    BitLength,
}

/// The Huffman construction state of trees.c (heap, depth, bl_count and
/// the block's bit lengths).
struct Huffman {
    heap: [usize; 2 * L_CODES + 1],
    heap_len: usize,
    heap_max: usize,
    depth: [u8; 2 * L_CODES + 1],
    bl_count: [u16; MAX_BITS + 1],
    opt_len: u64,
    static_len: u64,
}

fn smaller(tree: &[Ct], n: usize, m: usize, depth: &[u8]) -> bool {
    tree[n].fc < tree[m].fc || (tree[n].fc == tree[m].fc && depth[n] <= depth[m])
}

impl Huffman {
    fn pqdownheap(&mut self, tree: &[Ct], mut k: usize) {
        let v = self.heap[k];
        let mut j = k << 1;
        while j <= self.heap_len {
            if j < self.heap_len && smaller(tree, self.heap[j + 1], self.heap[j], &self.depth) {
                j += 1;
            }
            if smaller(tree, v, self.heap[j], &self.depth) {
                break;
            }
            self.heap[k] = self.heap[j];
            k = j;
            j <<= 1;
        }
        self.heap[k] = v;
    }

    fn gen_bitlen(&mut self, tree: &mut [Ct], max_code: usize, desc: Desc, tables: &Tables) {
        let (stree, extra, base, max_length): (Option<&[Ct]>, &[u32], usize, usize) = match desc {
            Desc::Literal => (
                Some(&tables.static_ltree[..]),
                &EXTRA_LBITS,
                LITERALS + 1,
                MAX_BITS,
            ),
            Desc::Distance => (Some(&tables.static_dtree[..]), &EXTRA_DBITS, 0, MAX_BITS),
            Desc::BitLength => (None, &EXTRA_BLBITS, 0, MAX_BL_BITS),
        };
        self.bl_count = [0; MAX_BITS + 1];
        tree[self.heap[self.heap_max]].dl = 0;
        let mut overflow: i32 = 0;
        for h in self.heap_max + 1..HEAP_SIZE {
            let n = self.heap[h];
            let mut bits = usize::from(tree[usize::from(tree[n].dl)].dl) + 1;
            if bits > max_length {
                bits = max_length;
                overflow += 1;
            }
            tree[n].dl = bits as u16;
            if n > max_code {
                continue;
            }
            self.bl_count[bits] += 1;
            let xbits = if n >= base { extra[n - base] } else { 0 } as u64;
            let f = u64::from(tree[n].fc);
            self.opt_len = self
                .opt_len
                .wrapping_add(f.wrapping_mul(bits as u64 + xbits));
            if let Some(stree) = stree {
                self.static_len = self
                    .static_len
                    .wrapping_add(f.wrapping_mul(u64::from(stree[n].dl) + xbits));
            }
        }
        if overflow == 0 {
            return;
        }
        loop {
            let mut bits = max_length - 1;
            while self.bl_count[bits] == 0 {
                bits -= 1;
            }
            self.bl_count[bits] -= 1;
            self.bl_count[bits + 1] += 2;
            self.bl_count[max_length] -= 1;
            overflow -= 2;
            if overflow <= 0 {
                break;
            }
        }
        let mut h = HEAP_SIZE;
        for bits in (1..=max_length).rev() {
            let mut n = self.bl_count[bits];
            while n != 0 {
                h -= 1;
                let m = self.heap[h];
                if m > max_code {
                    continue;
                }
                if usize::from(tree[m].dl) != bits {
                    let delta = (bits as u64).wrapping_sub(u64::from(tree[m].dl));
                    self.opt_len = self
                        .opt_len
                        .wrapping_add(delta.wrapping_mul(u64::from(tree[m].fc)));
                    tree[m].dl = bits as u16;
                }
                n -= 1;
            }
        }
    }

    /// build_tree: returns max_code.
    fn build_tree(&mut self, tree: &mut [Ct], desc: Desc, tables: &Tables) -> usize {
        let (stree, elems): (Option<&[Ct]>, usize) = match desc {
            Desc::Literal => (Some(&tables.static_ltree[..]), L_CODES),
            Desc::Distance => (Some(&tables.static_dtree[..]), D_CODES),
            Desc::BitLength => (None, BL_CODES),
        };
        let mut max_code: isize = -1;
        self.heap_len = 0;
        self.heap_max = HEAP_SIZE;
        #[allow(clippy::needless_range_loop)] // trees.c's indexing, kept verbatim
        for n in 0..elems {
            if tree[n].fc != 0 {
                self.heap_len += 1;
                self.heap[self.heap_len] = n;
                max_code = n as isize;
                self.depth[n] = 0;
            } else {
                tree[n].dl = 0;
            }
        }
        while self.heap_len < 2 {
            let node = if max_code < 2 {
                max_code += 1;
                max_code as usize
            } else {
                0
            };
            self.heap_len += 1;
            self.heap[self.heap_len] = node;
            tree[node].fc = 1;
            self.depth[node] = 0;
            self.opt_len = self.opt_len.wrapping_sub(1);
            if let Some(stree) = stree {
                self.static_len = self.static_len.wrapping_sub(u64::from(stree[node].dl));
            }
        }
        let max_code = max_code as usize;
        for n in (1..=self.heap_len / 2).rev() {
            self.pqdownheap(tree, n);
        }
        let mut node = elems;
        loop {
            let n = self.heap[1];
            self.heap[1] = self.heap[self.heap_len];
            self.heap_len -= 1;
            self.pqdownheap(tree, 1);
            let m = self.heap[1];
            self.heap_max -= 1;
            self.heap[self.heap_max] = n;
            self.heap_max -= 1;
            self.heap[self.heap_max] = m;
            tree[node].fc = tree[n].fc.wrapping_add(tree[m].fc);
            self.depth[node] = self.depth[n].max(self.depth[m]) + 1;
            tree[n].dl = node as u16;
            tree[m].dl = node as u16;
            self.heap[1] = node;
            node += 1;
            self.pqdownheap(tree, 1);
            if self.heap_len < 2 {
                break;
            }
        }
        self.heap_max -= 1;
        self.heap[self.heap_max] = self.heap[1];
        self.gen_bitlen(tree, max_code, desc, tables);
        let bl_count = self.bl_count;
        gen_codes(tree, max_code, &bl_count);
        max_code
    }
}

/// LSB-first bit output (bi_buf/bi_valid; emitting whole bytes early is
/// the same stream).
#[derive(Default)]
struct Bits {
    out: Vec<u8>,
    acc: u64,
    count: u32,
}

impl Bits {
    fn send(&mut self, value: u32, length: u32) {
        self.acc |= u64::from(value & ((1u32 << length) - 1)) << self.count;
        self.count += length;
        while self.count >= 8 {
            self.out.push(self.acc as u8);
            self.acc >>= 8;
            self.count -= 8;
        }
    }

    fn code(&mut self, c: usize, tree: &[Ct]) {
        self.send(u32::from(tree[c].fc), u32::from(tree[c].dl));
    }

    fn windup(&mut self) {
        if self.count > 0 {
            self.out.push(self.acc as u8);
        }
        self.acc = 0;
        self.count = 0;
    }
}

struct Deflate<'a> {
    input: &'a [u8],
    next_in: usize,
    window: Vec<u8>,
    prev: Vec<u16>,
    head: Vec<u16>,
    ins_h: u32,
    block_start: i64,
    match_length: usize,
    prev_match: usize,
    match_available: bool,
    strstart: usize,
    match_start: usize,
    lookahead: usize,
    prev_length: usize,
    insert: usize,
    high_water: usize,
    dyn_ltree: [Ct; HEAP_SIZE],
    dyn_dtree: [Ct; 2 * D_CODES + 1],
    bl_tree: [Ct; 2 * BL_CODES + 1],
    huffman: Huffman,
    syms: Vec<(u16, u8)>,
    bits: Bits,
    tables: Tables,
}

impl<'a> Deflate<'a> {
    fn new(input: &'a [u8]) -> Self {
        let mut deflate = Self {
            input,
            next_in: 0,
            window: vec![0; WINDOW_SIZE],
            prev: vec![0; W_SIZE],
            head: vec![0; HASH_SIZE],
            ins_h: 0,
            block_start: 0,
            match_length: MIN_MATCH - 1,
            prev_match: 0,
            match_available: false,
            strstart: 0,
            match_start: 0,
            lookahead: 0,
            prev_length: MIN_MATCH - 1,
            insert: 0,
            high_water: 0,
            dyn_ltree: [Ct::default(); HEAP_SIZE],
            dyn_dtree: [Ct::default(); 2 * D_CODES + 1],
            bl_tree: [Ct::default(); 2 * BL_CODES + 1],
            huffman: Huffman {
                heap: [0; 2 * L_CODES + 1],
                heap_len: 0,
                heap_max: 0,
                depth: [0; 2 * L_CODES + 1],
                bl_count: [0; MAX_BITS + 1],
                opt_len: 0,
                static_len: 0,
            },
            syms: Vec::with_capacity(SYM_END),
            bits: Bits::default(),
            tables: Tables::new(),
        };
        deflate.init_block();
        deflate
    }

    fn init_block(&mut self) {
        for entry in &mut self.dyn_ltree[..L_CODES] {
            entry.fc = 0;
        }
        for entry in &mut self.dyn_dtree[..D_CODES] {
            entry.fc = 0;
        }
        for entry in &mut self.bl_tree[..BL_CODES] {
            entry.fc = 0;
        }
        self.dyn_ltree[END_BLOCK].fc = 1;
        self.huffman.opt_len = 0;
        self.huffman.static_len = 0;
        self.syms.clear();
    }

    fn update_hash(&mut self, c: u8) {
        self.ins_h = ((self.ins_h << HASH_SHIFT) ^ u32::from(c)) & HASH_MASK;
    }

    /// INSERT_STRING: the previous head of the string's hash chain.
    fn insert_string(&mut self, at: usize) -> usize {
        self.update_hash(self.window[at + MIN_MATCH - 1]);
        let head = self.head[self.ins_h as usize];
        self.prev[at & W_MASK] = head;
        self.head[self.ins_h as usize] = at as u16;
        usize::from(head)
    }

    fn slide_hash(&mut self) {
        for entry in self.head.iter_mut().chain(self.prev.iter_mut()) {
            let m = usize::from(*entry);
            *entry = if m >= W_SIZE { (m - W_SIZE) as u16 } else { 0 };
        }
    }

    fn fill_window(&mut self) {
        loop {
            let mut more = WINDOW_SIZE - self.lookahead - self.strstart;
            if self.strstart >= W_SIZE + MAX_DIST {
                self.window.copy_within(W_SIZE..W_SIZE + (W_SIZE - more), 0);
                self.match_start = self.match_start.wrapping_sub(W_SIZE);
                self.strstart -= W_SIZE;
                self.block_start -= W_SIZE as i64;
                if self.insert > self.strstart {
                    self.insert = self.strstart;
                }
                self.slide_hash();
                more += W_SIZE;
            }
            let available = self.input.len() - self.next_in;
            if available == 0 {
                break;
            }
            let n = available.min(more);
            let at = self.strstart + self.lookahead;
            self.window[at..at + n].copy_from_slice(&self.input[self.next_in..self.next_in + n]);
            self.next_in += n;
            self.lookahead += n;
            if self.lookahead + self.insert >= MIN_MATCH {
                let mut at = self.strstart - self.insert;
                self.ins_h = u32::from(self.window[at]);
                self.update_hash(self.window[at + 1]);
                while self.insert > 0 {
                    self.update_hash(self.window[at + MIN_MATCH - 1]);
                    self.prev[at & W_MASK] = self.head[self.ins_h as usize];
                    self.head[self.ins_h as usize] = at as u16;
                    at += 1;
                    self.insert -= 1;
                    if self.lookahead + self.insert < MIN_MATCH {
                        break;
                    }
                }
            }
            if !(self.lookahead < MIN_LOOKAHEAD && self.next_in < self.input.len()) {
                break;
            }
        }
        if self.high_water < WINDOW_SIZE {
            let curr = self.strstart + self.lookahead;
            if self.high_water < curr {
                let init = (WINDOW_SIZE - curr).min(WIN_INIT);
                self.window[curr..curr + init].fill(0);
                self.high_water = curr + init;
            } else if self.high_water < curr + WIN_INIT {
                let init = (curr + WIN_INIT - self.high_water).min(WINDOW_SIZE - self.high_water);
                self.window[self.high_water..self.high_water + init].fill(0);
                self.high_water += init;
            }
        }
    }

    fn longest_match(&mut self, mut cur_match: usize) -> usize {
        let mut chain_length = MAX_CHAIN;
        let scan = self.strstart;
        let mut best_len = self.prev_length;
        let mut nice_match = NICE_MATCH;
        let limit = self.strstart.saturating_sub(MAX_DIST);
        let strend = self.strstart + MAX_MATCH;
        let window = &self.window;
        let mut scan_end1 = window[scan + best_len - 1];
        let mut scan_end = window[scan + best_len];
        if self.prev_length >= GOOD_MATCH {
            chain_length >>= 2;
        }
        if nice_match > self.lookahead {
            nice_match = self.lookahead;
        }
        loop {
            let m = cur_match;
            if window[m + best_len] == scan_end
                && window[m + best_len - 1] == scan_end1
                && window[m] == window[scan]
                && window[m + 1] == window[scan + 1]
            {
                // scan[2] == match[2] by the hash; compare from offset 3 up
                // to MAX_MATCH (the unrolled loop checks strend every 8th
                // byte, on the boundary strend falls on).
                let mut s = scan + 2;
                let mut mm = m + 2;
                loop {
                    s += 1;
                    mm += 1;
                    if window[s] != window[mm] || s >= strend {
                        break;
                    }
                }
                let len = MAX_MATCH - (strend - s);
                if len > best_len {
                    self.match_start = cur_match;
                    best_len = len;
                    if len >= nice_match {
                        break;
                    }
                    scan_end1 = window[scan + best_len - 1];
                    scan_end = window[scan + best_len];
                }
            }
            cur_match = usize::from(self.prev[cur_match & W_MASK]);
            if cur_match <= limit {
                break;
            }
            chain_length -= 1;
            if chain_length == 0 {
                break;
            }
        }
        best_len.min(self.lookahead)
    }

    fn tally_lit(&mut self, c: u8) -> bool {
        self.syms.push((0, c));
        self.dyn_ltree[usize::from(c)].fc += 1;
        self.syms.len() == SYM_END
    }

    fn tally_dist(&mut self, dist: usize, len: usize) -> bool {
        self.syms.push((dist as u16, len as u8));
        let code = usize::from(self.tables.length_code[len]);
        self.dyn_ltree[code + LITERALS + 1].fc += 1;
        let d = self.tables.d_code(dist - 1);
        self.dyn_dtree[d].fc += 1;
        self.syms.len() == SYM_END
    }

    fn flush_block(&mut self, last: bool) {
        let buf = (self.block_start >= 0).then_some(self.block_start as usize);
        let stored_len = (self.strstart as i64 - self.block_start) as u64;
        self.tr_flush_block(buf, stored_len, last);
        self.block_start = self.strstart as i64;
    }

    fn scan_tree(&mut self, literal: bool, max_code: usize) {
        let tree: &mut [Ct] = if literal {
            &mut self.dyn_ltree
        } else {
            &mut self.dyn_dtree
        };
        let mut prevlen: i32 = -1;
        let mut nextlen = i32::from(tree[0].dl);
        let mut count = 0;
        let (mut max_count, mut min_count) = if nextlen == 0 { (138, 3) } else { (7, 4) };
        tree[max_code + 1].dl = 0xffff;
        for n in 0..=max_code {
            let curlen = nextlen;
            nextlen = i32::from(tree[n + 1].dl);
            count += 1;
            if count < max_count && curlen == nextlen {
                continue;
            } else if count < min_count {
                self.bl_tree[curlen as usize].fc += count as u16;
            } else if curlen != 0 {
                if curlen != prevlen {
                    self.bl_tree[curlen as usize].fc += 1;
                }
                self.bl_tree[REP_3_6].fc += 1;
            } else if count <= 10 {
                self.bl_tree[REPZ_3_10].fc += 1;
            } else {
                self.bl_tree[REPZ_11_138].fc += 1;
            }
            count = 0;
            prevlen = curlen;
            (max_count, min_count) = if nextlen == 0 {
                (138, 3)
            } else if curlen == nextlen {
                (6, 3)
            } else {
                (7, 4)
            };
        }
    }

    fn send_tree(&mut self, literal: bool, max_code: usize) {
        let tree: &[Ct] = if literal {
            &self.dyn_ltree
        } else {
            &self.dyn_dtree
        };
        let bl = &self.bl_tree;
        let bits = &mut self.bits;
        let mut prevlen: i32 = -1;
        let mut nextlen = i32::from(tree[0].dl);
        let mut count: i32 = 0;
        let (mut max_count, mut min_count) = if nextlen == 0 { (138, 3) } else { (7, 4) };
        for n in 0..=max_code {
            let curlen = nextlen;
            nextlen = i32::from(tree[n + 1].dl);
            count += 1;
            if count < max_count && curlen == nextlen {
                continue;
            } else if count < min_count {
                while count != 0 {
                    bits.code(curlen as usize, bl);
                    count -= 1;
                }
            } else if curlen != 0 {
                if curlen != prevlen {
                    bits.code(curlen as usize, bl);
                    count -= 1;
                }
                bits.code(REP_3_6, bl);
                bits.send((count - 3) as u32, 2);
            } else if count <= 10 {
                bits.code(REPZ_3_10, bl);
                bits.send((count - 3) as u32, 3);
            } else {
                bits.code(REPZ_11_138, bl);
                bits.send((count - 11) as u32, 7);
            }
            count = 0;
            prevlen = curlen;
            (max_count, min_count) = if nextlen == 0 {
                (138, 3)
            } else if curlen == nextlen {
                (6, 3)
            } else {
                (7, 4)
            };
        }
    }

    fn compress_block(&mut self, dynamic: bool) {
        let (ltree, dtree): (&[Ct], &[Ct]) = if dynamic {
            (&self.dyn_ltree, &self.dyn_dtree)
        } else {
            (&self.tables.static_ltree, &self.tables.static_dtree)
        };
        let tables = &self.tables;
        let bits = &mut self.bits;
        for &(dist, lc) in &self.syms {
            let mut lc = u32::from(lc);
            if dist == 0 {
                bits.code(lc as usize, ltree);
            } else {
                let code = usize::from(tables.length_code[lc as usize]);
                bits.code(code + LITERALS + 1, ltree);
                let extra = EXTRA_LBITS[code];
                if extra != 0 {
                    lc -= tables.base_length[code];
                    bits.send(lc, extra);
                }
                let mut dist = u32::from(dist) - 1;
                let code = tables.d_code(dist as usize);
                bits.code(code, dtree);
                let extra = EXTRA_DBITS[code];
                if extra != 0 {
                    dist -= tables.base_dist[code];
                    bits.send(dist, extra);
                }
            }
        }
        bits.code(END_BLOCK, ltree);
    }

    fn stored_block(&mut self, buf: usize, stored_len: u64, last: bool) {
        self.bits.send(u32::from(last), 3);
        self.bits.windup();
        let len = stored_len as u16;
        self.bits.out.extend_from_slice(&len.to_le_bytes());
        self.bits.out.extend_from_slice(&(!len).to_le_bytes());
        self.bits
            .out
            .extend_from_slice(&self.window[buf..buf + stored_len as usize]);
    }

    fn tr_flush_block(&mut self, buf: Option<usize>, stored_len: u64, last: bool) {
        let tables = &self.tables;
        let l_max = self
            .huffman
            .build_tree(&mut self.dyn_ltree, Desc::Literal, tables);
        let d_max = self
            .huffman
            .build_tree(&mut self.dyn_dtree, Desc::Distance, tables);
        // build_bl_tree.
        self.scan_tree(true, l_max);
        self.scan_tree(false, d_max);
        let tables = &self.tables;
        self.huffman
            .build_tree(&mut self.bl_tree, Desc::BitLength, tables);
        let mut max_blindex = BL_CODES - 1;
        while max_blindex >= 3 {
            if self.bl_tree[BL_ORDER[max_blindex]].dl != 0 {
                break;
            }
            max_blindex -= 1;
        }
        self.huffman.opt_len = self
            .huffman
            .opt_len
            .wrapping_add(3 * (max_blindex as u64 + 1) + 5 + 5 + 4);
        let mut opt_lenb = self.huffman.opt_len.wrapping_add(3 + 7) >> 3;
        let static_lenb = self.huffman.static_len.wrapping_add(3 + 7) >> 3;
        if static_lenb <= opt_lenb {
            opt_lenb = static_lenb;
        }
        match buf {
            Some(buf) if stored_len + 4 <= opt_lenb => self.stored_block(buf, stored_len, last),
            _ if static_lenb == opt_lenb => {
                self.bits.send(2 + u32::from(last), 3);
                self.compress_block(false);
            }
            _ => {
                self.bits.send(4 + u32::from(last), 3);
                let (lcodes, dcodes, blcodes) = (l_max + 1, d_max + 1, max_blindex + 1);
                self.bits.send((lcodes - 257) as u32, 5);
                self.bits.send((dcodes - 1) as u32, 5);
                self.bits.send((blcodes - 4) as u32, 4);
                for &order in BL_ORDER.iter().take(blcodes) {
                    let len = u32::from(self.bl_tree[order].dl);
                    self.bits.send(len, 3);
                }
                self.send_tree(true, lcodes - 1);
                self.send_tree(false, dcodes - 1);
                self.compress_block(true);
            }
        }
        self.init_block();
        if last {
            self.bits.windup();
        }
    }

    /// deflate_slow to the end of the input (Z_FINISH).
    fn run(mut self) -> Vec<u8> {
        loop {
            if self.lookahead < MIN_LOOKAHEAD {
                self.fill_window();
                if self.lookahead == 0 {
                    break;
                }
            }
            let mut hash_head = 0;
            if self.lookahead >= MIN_MATCH {
                hash_head = self.insert_string(self.strstart);
            }
            self.prev_length = self.match_length;
            self.prev_match = self.match_start;
            self.match_length = MIN_MATCH - 1;
            if hash_head != 0
                && self.prev_length < MAX_LAZY
                && self.strstart - hash_head <= MAX_DIST
            {
                self.match_length = self.longest_match(hash_head);
                if self.match_length == MIN_MATCH
                    && self.strstart.wrapping_sub(self.match_start) > TOO_FAR
                {
                    self.match_length = MIN_MATCH - 1;
                }
            }
            if self.prev_length >= MIN_MATCH && self.match_length <= self.prev_length {
                let max_insert = self.strstart + self.lookahead - MIN_MATCH;
                let bflush = self.tally_dist(
                    self.strstart - 1 - self.prev_match,
                    self.prev_length - MIN_MATCH,
                );
                self.lookahead -= self.prev_length - 1;
                self.prev_length -= 2;
                loop {
                    self.strstart += 1;
                    if self.strstart <= max_insert {
                        self.insert_string(self.strstart);
                    }
                    self.prev_length -= 1;
                    if self.prev_length == 0 {
                        break;
                    }
                }
                self.match_available = false;
                self.match_length = MIN_MATCH - 1;
                self.strstart += 1;
                if bflush {
                    self.flush_block(false);
                }
            } else if self.match_available {
                let bflush = self.tally_lit(self.window[self.strstart - 1]);
                if bflush {
                    self.flush_block(false);
                }
                self.strstart += 1;
                self.lookahead -= 1;
            } else {
                self.match_available = true;
                self.strstart += 1;
                self.lookahead -= 1;
            }
        }
        if self.match_available {
            self.tally_lit(self.window[self.strstart - 1]);
            self.match_available = false;
        }
        self.flush_block(true);
        self.bits.out
    }
}

/// zlib's raw deflate stream of `data` at Java's default parameters.
pub fn raw_deflate(data: &[u8]) -> Vec<u8> {
    Deflate::new(data).run()
}

/// `new GZIPOutputStream(out).write(data); close()` (java.util.zip).
pub fn gzip(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x1f, 0x8b, 8, 0, 0, 0, 0, 0, 0, 0xff];
    out.extend_from_slice(&raw_deflate(data));
    let mut crc = flate2::Crc::new();
    crc.update(data);
    out.extend_from_slice(&crc.sum().to_le_bytes());
    out.extend_from_slice(&(data.len() as u32).to_le_bytes());
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    fn sha(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }

    /// vectors.py's generators: an LCG over `modulus` symbols, and words.
    fn lcg(n: usize, seed: u64, modulus: u64) -> Vec<u8> {
        let mut x = seed;
        (0..n)
            .map(|_| {
                x = (x * 1103515245 + 12345) & 0x7fff_ffff;
                ((x >> 16) % modulus) as u8
            })
            .collect()
    }

    fn words(n: usize, seed: u64) -> Vec<u8> {
        let list: [&[u8]; 12] = [
            b"alpha",
            b"beta",
            b"gamma",
            b"delta",
            b"epsilon",
            b"zeta",
            b"eta",
            b"theta",
            b"\n",
            b" ",
            b"iota",
            b"kappa_lambda_mu",
        ];
        let mut x = seed;
        let mut out = Vec::new();
        while out.len() < n {
            x = (x * 1103515245 + 12345) & 0x7fff_ffff;
            out.extend_from_slice(list[((x >> 16) % 12) as usize]);
        }
        out.truncate(n);
        out
    }

    fn samples() -> Vec<(&'static str, Vec<u8>)> {
        let mut lines = "line %d\n".repeat(20);
        lines.push_str(
            &(0..300)
                .map(|i| format!("entry-{i}"))
                .collect::<Vec<_>>()
                .join("\n"),
        );
        vec![
            ("c257", vec![b'c'; 257]),
            ("lines", lines.into_bytes()),
            ("random", lcg(100_000, 7, 256)),
            ("dna", lcg(300_000, 11, 4)),
            ("words", words(500_000, 3)),
            ("sparse", lcg(200_000, 5, 2)),
        ]
    }

    #[test]
    fn round_trips() {
        for (name, data) in samples() {
            let compressed = gzip(&data);
            let mut decoded = Vec::new();
            flate2::read::GzDecoder::new(&compressed[..])
                .read_to_end(&mut decoded)
                .unwrap();
            assert_eq!(decoded, data, "{name}");
        }
    }

    /// SHA-256 of zlib 1.3's raw deflate of the samples (Python's
    /// `zlib.compressobj(6, DEFLATED, -15, 8)`: the zlib behind the JDK's
    /// Deflater; tests/fixtures/aquery/depsets/vectors.py).
    #[test]
    fn zlib_identical() {
        let expected = [
            "e785f359cdebae4891d1a81cfcccfda0b0620e3e150c83afddac655fa5882c41",
            "df4af2996d80957e6efc8cead06f2f99d8278bc041773b01f25cdec4f5be682d",
            "8a3397d0bdd969a8d2a0a0bed52d4caaa716dabd6e721e54db1e262ec9d17eaa",
            "6a747400bfbb2ad8dcf6ad0ec02192f25b4f8e0fb0075bb5e277567504a37ac4",
            "7b6c2e13d4b9c84208f1bdf69f493293c58105f032bd28932473036a0650e96d",
            "1a32e1029d14efeff08be484d58124e568872fa230b64210017ff1cd67e9e0e8",
        ];
        for ((name, data), digest) in samples().into_iter().zip(expected) {
            assert_eq!(sha(&raw_deflate(&data)), digest, "{name}");
        }
    }

    /// Differential check against zlib over `vectors.py DIR N`'s pairs
    /// (`NNN.in`, `NNN.deflate`): `BAZEL_RS_ZLIB_VECTORS=DIR cargo test
    /// --lib java_gzip -- --ignored`.
    #[test]
    #[ignore]
    fn differential() {
        let dir = std::env::var("BAZEL_RS_ZLIB_VECTORS").expect("BAZEL_RS_ZLIB_VECTORS");
        let mut checked = 0;
        for entry in std::fs::read_dir(&dir).unwrap() {
            let path = entry.unwrap().path();
            if path.extension().is_some_and(|ext| ext == "in") {
                let data = std::fs::read(&path).unwrap();
                let expected = std::fs::read(path.with_extension("deflate")).unwrap();
                assert!(raw_deflate(&data) == expected, "{}", path.display());
                checked += 1;
            }
        }
        assert!(checked > 0);
    }
}
