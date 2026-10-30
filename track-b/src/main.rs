//! Speed Demon 2026, Track B (Index). README.md explains the approach.
//!
//! 1. Read the queries first and intern their distinct terms in one shared table. Only
//!    those terms are ever indexed; every other token just separates positions.
//! 2. Compile every distinct phrase into one Aho-Corasick automaton over term ids.
//! 3. Each thread takes a contiguous range of documents and walks it in batches of 4,096.
//!    It tokenizes each document once: phrase matches come straight out of the automaton,
//!    and BOOL terms collect (term, local doc) pairs. After a batch it counting-sorts the
//!    pairs into a tiny index of u16 doc lists, evaluates every BOOL query on it, and adds
//!    the answer to its running count, id sum and first ten ids.
//! 4. Thread ranges are in id order, so the totals merge by adding counts and sums and
//!    taking the first ten ids from the earliest ranges.

use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::thread;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
}

/// Maps a whole file read-only. The harness pre-warms /data into the page cache.
fn map(path: &str) -> &'static [u8] {
    let file = File::open(path).unwrap_or_else(|e| panic!("open {path}: {e}"));
    let len = file.metadata().expect("stat").len() as usize;
    if len == 0 {
        return &[];
    }
    // SAFETY: a read-only private mapping (PROT_READ = 1, MAP_PRIVATE = 2) of a file nobody
    // modifies while we run. It stays mapped until the process exits.
    unsafe {
        let ptr = mmap(std::ptr::null_mut(), len, 1, 2, file.as_raw_fd(), 0);
        assert!(ptr as isize != -1, "mmap {path} failed");
        std::slice::from_raw_parts(ptr, len)
    }
}

/// Documents per batch: local ids fit in u16, and a batch's index stays small.
const BATCH: usize = 4096;
/// A (bool term, local doc) pair packs into one u32: the doc takes the low SHIFT bits.
const SHIFT: u32 = BATCH.trailing_zeros();
const NONE: u32 = u32::MAX;

/// Spec 2.2 as a table: the lowercase byte for [A-Za-z0-9], 0 for every separator.
const fn token_bytes() -> [u8; 256] {
    let mut t = [0u8; 256];
    let mut b = 0;
    while b < 256 {
        let c = b as u8;
        t[b] = match c {
            b'a'..=b'z' | b'0'..=b'9' => c,
            b'A'..=b'Z' => c + 32,
            _ => 0,
        };
        b += 1;
    }
    t
}
const TOKEN: [u8; 256] = token_bytes();

const LOWER: u64 = 0x2020_2020_2020_2020;

/// A token of at most 32 bytes as four little-endian words, zero past its end.
type Words = [u64; 4];

#[inline(always)]
fn hash(w: &Words, len: usize) -> u64 {
    const K: u64 = 0x9E37_79B9_7F4A_7C15;
    let h = w
        .iter()
        .fold(len as u64, |h, &x| (h ^ x).wrapping_mul(K).rotate_left(31));
    h.wrapping_mul(K)
}

/// Eight bytes of `buf` at `i`, zero past its end.
#[inline(always)]
fn load8(buf: &[u8], i: usize) -> u64 {
    if i + 8 <= buf.len() {
        u64::from_le_bytes(buf[i..i + 8].try_into().unwrap())
    } else {
        let mut t = [0u8; 8];
        t[..buf.len().saturating_sub(i)].copy_from_slice(&buf[i.min(buf.len())..]);
        u64::from_le_bytes(t)
    }
}

/// The lowercased words of the token `buf[start..start + len]` (len <= 32). Inside a token
/// every byte is a letter or a digit, and OR-ing 0x20 lowercases letters and keeps digits.
#[inline(always)]
fn token_words(buf: &[u8], start: usize, len: usize) -> Words {
    let mut w = [0u64; 4];
    for (k, x) in w.iter_mut().enumerate().take(len.div_ceil(8)) {
        let n = (len - 8 * k).min(8);
        let keep = if n == 8 {
            u64::MAX
        } else {
            (1u64 << (8 * n)) - 1
        };
        *x = (load8(buf, start + 8 * k) | LOWER) & keep;
    }
    w
}

/// One query term: its key, and what the scan needs to know about it, in one record so a
/// hit costs one cache line.
#[derive(Clone, Copy)]
struct Term {
    words: Words,
    len: usize,
    bool_index: u32, // dense index among BOOL terms, or NONE
    in_phrase: bool,
}

/// The query vocabulary: term -> dense id. Each slot holds the top 32 bits of the term's
/// hash and its id, and keys are compared as four u64s. In front of the table sits a
/// 256 KB filter with one bit per hash value: about a fifth of corpus tokens are not query
/// terms, and a clear bit turns them away without probing a cold slot.
struct Terms {
    filter: Vec<u64>,
    slots: Vec<u64>, // hash tag | id + 1; 0 = empty
    keys: Vec<Term>,
}

const FILTER_BITS: u32 = 21;

impl Terms {
    fn new() -> Self {
        Terms {
            filter: vec![0; 1 << (FILTER_BITS - 6)],
            slots: vec![0; 1 << 18],
            keys: Vec::new(),
        }
    }

    #[inline(always)]
    fn get(&self, w: &Words, len: usize) -> u32 {
        let h = hash(w, len);
        let f = (h >> (64 - FILTER_BITS)) as usize;
        if self.filter[f / 64] & (1 << (f % 64)) == 0 {
            return NONE;
        }
        let (mask, tag) = (self.slots.len() - 1, h & !0xFFFF_FFFF);
        let mut s = h as usize & mask;
        loop {
            let e = self.slots[s];
            if e == 0 {
                return NONE;
            }
            let id = (e & 0xFFFF_FFFF) as usize - 1;
            if e & !0xFFFF_FFFF == tag && self.keys[id].len == len && self.keys[id].words == *w {
                return id as u32;
            }
            s = (s + 1) & mask;
        }
    }

    /// The id of query term `t`, inserting it if new.
    fn insert(&mut self, t: &[u8]) -> u32 {
        let w = token_words(t, 0, t.len());
        let found = self.get(&w, t.len());
        if found != NONE {
            return found;
        }
        assert!(
            self.keys.len() * 2 < self.slots.len(),
            "too many query terms"
        );
        let h = hash(&w, t.len());
        let f = (h >> (64 - FILTER_BITS)) as usize;
        self.filter[f / 64] |= 1 << (f % 64);
        let mut s = h as usize & (self.slots.len() - 1);
        while self.slots[s] != 0 {
            s = (s + 1) & (self.slots.len() - 1);
        }
        self.keys.push(Term {
            words: w,
            len: t.len(),
            bool_index: NONE,
            in_phrase: false,
        });
        self.slots[s] = (h & !0xFFFF_FFFF) | self.keys.len() as u64;
        self.keys.len() as u32 - 1
    }
}

/// Bit k is set if `buf[i + k]` is a token byte ([A-Za-z0-9]); 0 past the end of `buf`.
#[inline(always)]
fn class_mask(buf: &[u8], i: usize) -> u64 {
    if i + 64 > buf.len() {
        return class_mask_tail(buf, i);
    }
    #[cfg(target_feature = "avx2")]
    {
        use std::arch::x86_64::*;
        // SAFETY: AVX2 is enabled at compile time and the 64 bytes are inside `buf`.
        unsafe {
            let half = |off: usize| {
                let v = _mm256_loadu_si256(buf.as_ptr().add(i + off).cast());
                let l = _mm256_or_si256(v, _mm256_set1_epi8(0x20));
                // Signed compares: bytes >= 0x80 are negative, so never letters or digits.
                let alpha = _mm256_and_si256(
                    _mm256_cmpgt_epi8(l, _mm256_set1_epi8(0x60)),
                    _mm256_cmpgt_epi8(_mm256_set1_epi8(0x7B), l),
                );
                let digit = _mm256_and_si256(
                    _mm256_cmpgt_epi8(v, _mm256_set1_epi8(0x2F)),
                    _mm256_cmpgt_epi8(_mm256_set1_epi8(0x3A), v),
                );
                _mm256_movemask_epi8(_mm256_or_si256(alpha, digit)) as u32
            };
            u64::from(half(0)) | u64::from(half(32)) << 32
        }
    }
    #[cfg(not(target_feature = "avx2"))]
    {
        buf[i..i + 64]
            .iter()
            .enumerate()
            .fold(0, |m, (k, &c)| m | u64::from(TOKEN[c as usize] != 0) << k)
    }
}

#[cold]
fn class_mask_tail(buf: &[u8], i: usize) -> u64 {
    buf[i.min(buf.len())..]
        .iter()
        .take(64)
        .enumerate()
        .fold(0, |m, (k, &c)| m | u64::from(TOKEN[c as usize] != 0) << k)
}

/// Aho-Corasick over term ids: reports every distinct phrase that ends at a position.
struct Automaton {
    root: Vec<u32>, // the start state's transitions, dense by term id (0 = none)
    /// Other states' transitions: edges[edge_start[s]..edge_start[s + 1]] are state s's
    /// (term, next) pairs sorted by term. Most states have one to three, so a lookup reads
    /// one cache line instead of probing a large hash table.
    edge_start: Vec<u32>,
    edges: Vec<(u32, u32)>,
    fail: Vec<u32>,
    /// Per state: the phrase ending exactly here (or NONE), and the nearest state on the
    /// fail chain that ends a phrase (or NONE).
    out: Vec<(u32, u32)>,
}

impl Automaton {
    #[inline(always)]
    fn edge(&self, s: u32, t: u32) -> u32 {
        let e = &self.edges
            [self.edge_start[s as usize] as usize..self.edge_start[s as usize + 1] as usize];
        match e.binary_search_by_key(&t, |x| x.0) {
            Ok(i) => e[i].1,
            Err(_) => NONE,
        }
    }

    /// The state after reading term `t` in state `s` (0 is the root).
    #[inline(always)]
    fn step(&self, mut s: u32, t: u32) -> u32 {
        loop {
            if s == 0 {
                return self.root[t as usize];
            }
            let n = self.edge(s, t);
            if n != NONE {
                return n;
            }
            s = self.fail[s as usize];
        }
    }

    fn build(phrases: &[Vec<u32>], n_terms: usize) -> Self {
        let mut children: Vec<Vec<(u32, u32)>> = vec![Vec::new()];
        let mut pattern = vec![NONE];
        for (p, terms) in phrases.iter().enumerate() {
            let mut s = 0;
            for &t in terms {
                s = match children[s].iter().find(|c| c.0 == t) {
                    Some(&(_, n)) => n as usize,
                    None => {
                        children.push(Vec::new());
                        pattern.push(NONE);
                        let n = children.len() - 1;
                        children[s].push((t, n as u32));
                        n
                    }
                };
            }
            pattern[s] = p as u32;
        }
        let n = children.len();
        let mut a = Automaton {
            root: vec![0; n_terms],
            edge_start: vec![0; n + 1],
            edges: Vec::new(),
            fail: vec![0; n],
            out: pattern.iter().map(|&p| (p, NONE)).collect(),
        };
        for &(t, c) in &children[0] {
            a.root[t as usize] = c;
        }
        for (s, cs) in children.iter_mut().enumerate().skip(1) {
            cs.sort_unstable();
            a.edges.extend_from_slice(cs);
            a.edge_start[s + 1] = a.edges.len() as u32;
        }
        // Breadth-first: a child's fail state is where its parent's fail state goes on t.
        let mut queue: Vec<u32> = children[0].iter().map(|c| c.1).collect();
        let mut head = 0;
        while head < queue.len() {
            let r = queue[head] as usize;
            head += 1;
            for &(t, u) in &children[r] {
                let f = if r == 0 { 0 } else { a.step(a.fail[r], t) };
                a.fail[u as usize] = f;
                let (fp, fl) = a.out[f as usize];
                a.out[u as usize].1 = if fp != NONE { f } else { fl };
                queue.push(u);
            }
        }
        a
    }
}

/// Masks of the bit positions whose index has bit k set: the sum of the set positions of
/// a word x is the sum over k of popcount(x & MASK[k]) << k.
const POSITION_BITS: [u64; 6] = [
    0xAAAA_AAAA_AAAA_AAAA,
    0xCCCC_CCCC_CCCC_CCCC,
    0xF0F0_F0F0_F0F0_F0F0,
    0xFF00_FF00_FF00_FF00,
    0xFFFF_0000_FFFF_0000,
    0xFFFF_FFFF_0000_0000,
];

/// One thread's partial answers. Every distinct BOOL query and every distinct phrase has
/// a slot with (count, id sum). `firsts` holds (slot, id) for the first ten ids of each
/// slot within this thread's documents, appended in id order: a short list, instead of
/// ten ids reserved for every slot in every thread.
struct Answers {
    totals: Vec<(u32, u64)>,
    firsts: Vec<(u32, u32)>,
}

impl Answers {
    #[inline(always)]
    fn add(&mut self, slot: usize, doc: u32) {
        let t = &mut self.totals[slot];
        if t.0 < 10 {
            self.firsts.push((slot as u32, doc));
        }
        t.0 += 1;
        t.1 += u64::from(doc);
    }

    /// Adds every document of a batch bitmap whose bit 0 is document `base`: counts and
    /// sums by popcount, a word at a time; ids one by one only while fewer than ten.
    fn add_bits(&mut self, slot: usize, base: u32, bits: &[u64]) {
        for (w, &x) in bits.iter().enumerate().filter(|(_, &x)| x != 0) {
            let first = u64::from(base) + w as u64 * 64; // the document of bit 0
            let t = &mut self.totals[slot];
            let mut low = x;
            while t.0 < 10 && low != 0 {
                self.firsts.push((
                    slot as u32,
                    (first + u64::from(low.trailing_zeros())) as u32,
                ));
                t.0 += 1;
                low &= low - 1;
            }
            t.0 += low.count_ones(); // the bits not already counted above
            let positions: u64 = POSITION_BITS
                .iter()
                .enumerate()
                .map(|(k, &m)| u64::from((x & m).count_ones()) << k)
                .sum();
            t.1 += first * u64::from(x.count_ones()) + positions;
        }
    }
}

/// A query line's answer: a distinct BOOL query, or a distinct phrase.
enum Query {
    Bool(usize),
    Phrase(usize),
}

/// Splits `buf` into `n` pieces that each end just after a newline.
fn split_lines(buf: &[u8], n: usize) -> Vec<&[u8]> {
    let mut out = Vec::with_capacity(n);
    let mut start = 0;
    for i in 1..=n {
        let mut end = (buf.len() * i / n).max(start);
        while end < buf.len() && (end == 0 || buf[end - 1] != b'\n') {
            end += 1;
        }
        out.push(&buf[start..end]);
        start = end;
    }
    out
}

/// Shared, read-only inputs to every worker.
struct Ctx<'a> {
    terms: &'a Terms,
    automaton: &'a Automaton,
    bool_queries: &'a [Vec<(u8, u32)>], // (operator, bool-term index); first operator unused
    n_bool_terms: usize,
    n_phrases: usize,
}

/// One thread's documents: the partial answers of every slot. BOOL queries use slots
/// 0.., phrases the slots after them.
fn work(ctx: &Ctx, chunk: &[u8]) -> Answers {
    let phrase_slot = ctx.bool_queries.len();
    let mut ans = Answers {
        totals: vec![(0, 0); phrase_slot + ctx.n_phrases],
        firsts: Vec::new(),
    };
    let mut phrase_last = vec![NONE; ctx.n_phrases];
    let mut term_last = vec![NONE; ctx.n_bool_terms];
    let mut pairs: Vec<u32> = Vec::new(); // bool-term index << SHIFT | local doc
    let mut start = vec![0u32; ctx.n_bool_terms + 1];
    let mut docs: Vec<u16> = Vec::new();
    let (mut bits, mut keep) = ([0u64; BATCH / 64], [0u64; BATCH / 64]);
    let mut dense: Vec<[u64; BATCH / 64]> = Vec::new();
    let mut dense_of = vec![NONE; ctx.n_bool_terms];

    let mut p = 0;
    while p < chunk.len() {
        // Tokenize up to BATCH documents.
        pairs.clear();
        let mut base = NONE;
        let mut n_docs = 0;
        while p < chunk.len() && n_docs < BATCH {
            let tab = p + chunk[p..].iter().position(|&c| c == b'\t').unwrap();
            let doc = chunk[p..tab]
                .iter()
                .fold(0u32, |v, &c| v * 10 + u32::from(c - b'0'));
            if base == NONE {
                base = doc;
            }
            let local = (doc - base) as u16;
            let end = tab + 1 + chunk[tab + 1..].iter().position(|&c| c == b'\n').unwrap();
            let mut state = 0u32;
            let mut on_token = |start: usize, len: usize| {
                let t = if len <= 32 {
                    ctx.terms.get(&token_words(chunk, start, len), len)
                } else {
                    NONE
                };
                if t == NONE {
                    state = 0;
                    return;
                }
                let term = &ctx.terms.keys[t as usize];
                let b = term.bool_index;
                if b != NONE && term_last[b as usize] != doc {
                    term_last[b as usize] = doc;
                    pairs.push(b << SHIFT | u32::from(local));
                }
                state = if term.in_phrase {
                    ctx.automaton.step(state, t)
                } else {
                    0
                };
                let (mut ph, mut next) = ctx.automaton.out[state as usize];
                loop {
                    if ph != NONE && phrase_last[ph as usize] != doc {
                        phrase_last[ph as usize] = doc;
                        ans.add(phrase_slot + ph as usize, doc);
                    }
                    if next == NONE {
                        break;
                    }
                    (ph, next) = ctx.automaton.out[next as usize];
                }
            };
            // Token runs from 64-byte class masks: a run that reaches a block's end goes on
            // in the next block.
            let mut i = tab + 1;
            let mut run = usize::MAX;
            while i < end {
                let mut m = class_mask(chunk, i);
                if end - i < 64 {
                    m &= (1u64 << (end - i)) - 1;
                }
                let mut k = 0;
                while k < 64 {
                    if run == usize::MAX {
                        let rest = m >> k;
                        if rest == 0 {
                            break;
                        }
                        k += rest.trailing_zeros();
                        run = i + k as usize;
                    }
                    let ones = (!(m >> k)).trailing_zeros();
                    if k + ones >= 64 {
                        break; // the token continues in the next block
                    }
                    k += ones;
                    on_token(run, i + k as usize - run);
                    run = usize::MAX;
                }
                i += 64;
            }
            if run != usize::MAX {
                on_token(run, end - run);
            }
            n_docs += 1;
            p = end + 1;
        }

        // Counting sort of the pairs: docs[start[b]..start[b + 1]] are term b's local ids,
        // ascending because documents were read in order.
        start.fill(0);
        for &x in &pairs {
            let b = x >> SHIFT;
            start[b as usize + 1] += 1;
        }
        for b in 0..ctx.n_bool_terms {
            start[b + 1] += start[b];
        }
        docs.resize(pairs.len(), 0);
        for &x in &pairs {
            let (b, d) = (x >> SHIFT, (x & (BATCH as u32 - 1)) as u16);
            docs[start[b as usize] as usize] = d;
            start[b as usize] += 1;
        }
        for b in (0..ctx.n_bool_terms).rev() {
            start[b + 1] = start[b];
        }
        start[0] = 0;
        let list = |b: u32| -> &[u16] {
            if b == NONE {
                &[]
            } else {
                &docs[start[b as usize] as usize..start[b as usize + 1] as usize]
            }
        };

        // Terms in at least 64 of the batch's documents also get a bitmap, built once per
        // batch: combining one with the running set is then 64 word operations.
        dense.clear();
        for b in 0..ctx.n_bool_terms {
            let l = list(b as u32);
            dense_of[b] = NONE;
            if l.len() >= 64 {
                dense_of[b] = dense.len() as u32;
                let mut m = [0u64; BATCH / 64];
                l.iter().for_each(|&d| m[d as usize / 64] |= 1 << (d % 64));
                dense.push(m);
            }
        }

        // Every BOOL query, strictly left to right, on this batch. The running set is a
        // bitmap over the batch (64 words, in L1): OR sets a list's bits, NOT clears them,
        // AND keeps the list's bits that are set, each one a branch-free bit operation.
        for (slot, steps) in ctx.bool_queries.iter().enumerate() {
            // AND and NOT keep an empty set empty, so an empty start skips to the first OR.
            let mut k = 1;
            if list(steps[0].1).is_empty() {
                while k < steps.len() && steps[k].0 != b'O' {
                    k += 1;
                }
                if k == steps.len() {
                    continue;
                }
            }
            match steps[0].1 {
                b if b != NONE && dense_of[b as usize] != NONE => {
                    bits = dense[dense_of[b as usize] as usize]
                }
                b => {
                    bits.fill(0);
                    list(b)
                        .iter()
                        .for_each(|&d| bits[d as usize / 64] |= 1 << (d % 64));
                }
            }
            for &(op, b) in &steps[k..] {
                if b != NONE && dense_of[b as usize] != NONE {
                    let m = &dense[dense_of[b as usize] as usize];
                    for (x, &y) in bits.iter_mut().zip(m) {
                        *x = match op {
                            b'O' => *x | y,
                            b'N' => *x & !y,
                            _ => *x & y,
                        };
                    }
                    continue;
                }
                let docs = list(b);
                match op {
                    b'O' => docs
                        .iter()
                        .for_each(|&d| bits[d as usize / 64] |= 1 << (d % 64)),
                    b'N' => docs
                        .iter()
                        .for_each(|&d| bits[d as usize / 64] &= !(1 << (d % 64))),
                    _ => {
                        keep.fill(0);
                        docs.iter()
                            .for_each(|&d| keep[d as usize / 64] |= 1 << (d % 64));
                        bits.iter_mut().zip(&keep).for_each(|(x, &y)| *x &= y);
                    }
                }
            }
            ans.add_bits(slot, base, &bits);
        }
    }
    ans
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = args.get(1).map_or("/data", String::as_str);
    let out_dir = args.get(2).map_or("/out", String::as_str);
    let threads = thread::available_parallelism().map_or(1, |n| n.get());

    // Queries: intern every term; give BOOL terms a dense index; number distinct phrases.
    let qtext = map(&format!("{data_dir}/queries.txt"));
    let mut terms = Terms::new();
    let mut n_bool_terms = 0;
    let mut phrases: Vec<Vec<u32>> = Vec::new();
    let mut phrase_ids: std::collections::HashMap<Vec<u32>, u32> = std::collections::HashMap::new();
    let mut queries = Vec::new();
    let mut bool_queries: Vec<Vec<(u8, u32)>> = Vec::new();
    let mut bool_seen: std::collections::HashMap<Vec<(u8, u32)>, usize> =
        std::collections::HashMap::new();
    for line in qtext.split(|&c| c == b'\n').filter(|l| !l.is_empty()) {
        let (kind, rest) = line.split_at(line.iter().position(|&c| c == b' ').unwrap());
        let words: Vec<&[u8]> = rest[1..].split(|&c| c == b' ').collect();
        if kind == b"BOOL" {
            let mut steps = Vec::new();
            for (k, w) in words.iter().enumerate().step_by(2) {
                let t = terms.insert(w) as usize;
                let term = &mut terms.keys[t];
                if term.bool_index == NONE {
                    term.bool_index = n_bool_terms;
                    n_bool_terms += 1;
                }
                steps.push((if k == 0 { b'O' } else { words[k - 1][0] }, term.bool_index));
            }
            let n = bool_queries.len();
            let slot = *bool_seen.entry(steps.clone()).or_insert(n);
            if slot == n {
                bool_queries.push(steps); // repeated lines share one answer
            }
            queries.push(Query::Bool(slot));
        } else {
            let inner = &rest[2..rest.len() - 1]; // strip ` "` and `"`
            let seq: Vec<u32> = inner
                .split(|&c| c == b' ')
                .map(|w| terms.insert(w))
                .collect();
            seq.iter()
                .for_each(|&t| terms.keys[t as usize].in_phrase = true);
            let n = phrase_ids.len() as u32;
            let p = *phrase_ids.entry(seq.clone()).or_insert(n);
            if p == n {
                phrases.push(seq);
            }
            queries.push(Query::Phrase(p as usize));
        }
    }
    let automaton = Automaton::build(&phrases, terms.keys.len());
    let ctx = Ctx {
        terms: &terms,
        automaton: &automaton,
        bool_queries: &bool_queries,
        n_bool_terms: n_bool_terms as usize,
        n_phrases: phrases.len(),
    };

    let docs = map(&format!("{data_dir}/docs.txt"));
    assert!(
        n_bool_terms < 1 << (32 - SHIFT),
        "too many BOOL terms to pack"
    );
    let parts: Vec<Answers> = thread::scope(|sc| {
        let handles: Vec<_> = split_lines(docs, threads)
            .into_iter()
            .map(|chunk| {
                let ctx = &ctx;
                sc.spawn(move || work(ctx, chunk))
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // Merge in thread order, which is id order: counts and sums add, and each slot's
    // first ten ids come from the earliest threads (each thread's list is in id order).
    let n_slots = bool_queries.len() + phrases.len();
    let mut totals = vec![(0u32, 0u64); n_slots];
    let mut firsts = vec![(0usize, [0u32; 10]); n_slots];
    for part in &parts {
        for (t, &(c, s)) in totals.iter_mut().zip(&part.totals) {
            (t.0, t.1) = (t.0 + c, t.1 + s);
        }
        for &(slot, doc) in &part.firsts {
            let f = &mut firsts[slot as usize];
            if f.0 < 10 {
                f.1[f.0] = doc;
                f.0 += 1;
            }
        }
    }
    let mut out = Vec::with_capacity(queries.len() * 32);
    for query in &queries {
        let slot = match *query {
            Query::Bool(b) => b,
            Query::Phrase(p) => bool_queries.len() + p,
        };
        write!(out, "{} {}", totals[slot].0, totals[slot].1).unwrap();
        for d in &firsts[slot].1[..firsts[slot].0] {
            write!(out, " {d}").unwrap();
        }
        out.push(b'\n');
    }
    File::create(format!("{out_dir}/results.txt"))
        .and_then(|mut f| f.write_all(&out))
        .expect("write results.txt");
}
