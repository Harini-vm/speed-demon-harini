//! Speed Demon 2026, Track D (Traverse). README.md explains the approach.
//!
//! 1. mmap graph.txt and parse the edge lines in parallel, one chunk per thread.
//! 2. Build a compressed sparse row (CSR) graph: out-degree counts, a prefix sum, and a
//!    parallel scatter of (head, weight) pairs. Self-loops are dropped: they never shorten
//!    a path. Parallel and duplicate edges stay; Dijkstra relaxes the lighter one first.
//!    Then renumber the nodes in breadth-first order for memory locality.
//! 3. Answer the queries in parallel, one Dijkstra per query that stops when the target is
//!    settled. Trivial cases are answered without a search: s == t is 0, and a source with
//!    no out-edges or a target with no in-edges (other than s == t) is unreachable.
//! 4. Write the answers in input order.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering::Relaxed};
use std::sync::Mutex;
use std::thread;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn mallopt(param: i32, value: i32) -> i32;
    fn prctl(option: i32, ...) -> i32;
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

/// Reads a decimal number at `*i` and steps past it and the one separator after it.
#[inline(always)]
fn number(buf: &[u8], i: &mut usize) -> u64 {
    let mut v = 0;
    while buf[*i] >= b'0' {
        v = v * 10 + u64::from(buf[*i] - b'0');
        *i += 1;
    }
    *i += 1;
    v
}

const ONES: u64 = 0x0101_0101_0101_0101;

#[inline(always)]
fn word(buf: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(buf[i..i + 8].try_into().unwrap())
}

/// SWAR: the index of the first byte of `w` below `n` (a space or LF for n = b'0'), or 8.
/// The borrow can flag a false byte only above a real one, so the lowest flag is exact.
#[inline(always)]
fn first_below(w: u64, n: u8) -> usize {
    ((w.wrapping_sub(ONES * u64::from(n)) & !w & (ONES << 7)).trailing_zeros() / 8) as usize
}

/// `number` without a loop per digit: the separator is found a word at a time, and the
/// last (up to) eight digits are converted at once, as one word ending at the separator
/// with the bytes before the number masked to zero digits (pairs, quads, then octets).
#[inline(always)]
fn number_fast(buf: &[u8], i: &mut usize) -> u64 {
    let q = *i;
    if q < 8 || q + 16 > buf.len() {
        return number(buf, i); // too close to either end for the word loads
    }
    let k = first_below(word(buf, q), b'0');
    let end = if k < 8 {
        q + k
    } else {
        q + 8 + first_below(word(buf, q + 8), b'0')
    };
    let keep = (end - q).min(8);
    let mask = u64::MAX << (8 * (8 - keep));
    let d = (word(buf, end - 8) & mask) - (ONES * u64::from(b'0') & mask);
    let v = (d * 10 + (d >> 8)) & 0x00FF_00FF_00FF_00FF;
    let v = (v * 100 + (v >> 16)) & 0x0000_FFFF_0000_FFFF;
    let low = (v * 10_000 + (v >> 32)) & 0xFFFF_FFFF;
    let high = buf[q..end - keep]
        .iter()
        .fold(0, |a, &c| a * 10 + u64::from(c - b'0'));
    *i = end + 1;
    high * 100_000_000 + low
}

/// Splits `buf` into `n` pieces that each end just after a newline.
fn split_lines(buf: &[u8], n: usize) -> Vec<&[u8]> {
    let mut cuts = vec![0];
    for i in 1..=n {
        let at = (buf.len() * i / n).max(*cuts.last().unwrap());
        cuts.push(
            buf[at..]
                .iter()
                .position(|&c| c == b'\n')
                .map_or(buf.len(), |k| at + k + 1),
        );
    }
    cuts.windows(2).map(|w| &buf[w[0]..w[1]]).collect()
}

/// Runs `f` on every item, one thread each, and returns the results in order.
fn par_map<I: Send, T: Send>(items: Vec<I>, f: impl Fn(I) -> T + Sync) -> Vec<T> {
    thread::scope(|sc| {
        let f = &f;
        let handles: Vec<_> = items.into_iter().map(|x| sc.spawn(move || f(x))).collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    })
}

/// A raw pointer that threads may share when they provably write disjoint indices.
#[derive(Clone, Copy)]
struct SharedPtr<T>(*mut T);
unsafe impl<T> Send for SharedPtr<T> {}
unsafe impl<T> Sync for SharedPtr<T> {}
impl<T> SharedPtr<T> {
    /// Writes element `i`. The caller guarantees `i` is in bounds and that no other thread
    /// writes the same element.
    fn set(self, i: usize, v: T) {
        // SAFETY: in bounds and unshared, per the caller's guarantee.
        unsafe { *self.0.add(i) = v }
    }
}

/// An edge in 6 bytes: its other end and its weight. A weight of 65,535 or more is stored
/// as 65,535 and looked up in the graph's `big` list (about 50 edges at full scale).
#[derive(Clone, Copy)]
#[repr(C, packed)]
struct Edge {
    v: u32,
    w: u16,
}

/// `n` zero edges, allocated zeroed: `vec![edge(0, 0); n]` would write each one in turn,
/// while zeroed memory is committed only as the threads fill it.
fn zero_edges(n: usize) -> Vec<Edge> {
    let layout = std::alloc::Layout::array::<Edge>(n).unwrap();
    if layout.size() == 0 {
        return Vec::new();
    }
    // SAFETY: an all-zero Edge is a valid Edge, the pointer comes from the global allocator
    // with this exact layout (align 1), and length and capacity are both n.
    unsafe {
        let ptr = std::alloc::alloc_zeroed(layout).cast::<Edge>();
        assert!(!ptr.is_null(), "out of memory");
        Vec::from_raw_parts(ptr, n, n)
    }
}

/// A stored weight of BIG stands for one of BIG or more, kept in the graph's `big` list.
/// ABSENT marks a missing reverse edge in `Back::rw`.
const BIG: u16 = u16::MAX - 1;
const ABSENT: u16 = u16::MAX;

fn edge(v: u32, w: u32) -> Edge {
    Edge {
        v,
        w: w.min(u32::from(BIG)) as u16,
    }
}

/// The graph in CSR form: the edges of `u` are `adj[off[u]..off[u + 1]]`. `big` holds
/// (u, v, weight) for every edge whose weight did not fit, sorted.
struct Graph {
    off: Vec<u32>,
    adj: Vec<Edge>,
    big: Vec<(u32, u32, u32)>,
}

impl Graph {
    /// The edges of `u` as (other end, weight).
    #[inline(always)]
    fn edges(&self, u: u32) -> impl Iterator<Item = (u32, u64)> + '_ {
        let list = &self.adj[self.off[u as usize] as usize..self.off[u as usize + 1] as usize];
        list.iter().map(move |&e| {
            let (v, w) = (e.v, e.w);
            (
                v,
                if w != BIG {
                    u64::from(w)
                } else {
                    self.big_weight(u, v)
                },
            )
        })
    }

    /// A stored weight of BIG from `u` to `v`: the smallest such edge's real weight
    /// (parallel edges may share the pair; any of their weights is a real edge).
    #[cold]
    fn big_weight(&self, u: u32, v: u32) -> u64 {
        let i = self.big.partition_point(|&(a, b, _)| (a, b) < (u, v));
        let same = self.big[i..]
            .iter()
            .take_while(|&&(a, b, _)| (a, b) == (u, v));
        u64::from(same.map(|e| e.2).min().unwrap())
    }
}

/// A bucket holds the edges whose tail is in one run of 65,536 consecutive node ids: small
/// enough for its counts to stay in L2, big enough for its vectors to be mmap-backed.
const SHIFT: u32 = 16;

fn build_graph(text: &[u8], threads: usize) -> Graph {
    let mut i = 0;
    let n = number(text, &mut i) as usize;
    let m = number(text, &mut i) as usize;
    assert!(m < u32::MAX as usize, "edge count must fit in u32");
    let nb = (n >> SHIFT) + 1;
    // Parse straight into buckets by tail; self-loops never shorten a path, so drop them.
    // An entry is [tail's low 16 bits | stored weight << 16, head]: the bucket holds the
    // tail's high bits. The few weights of BIG or more also go to `big` here.
    // Transparent huge pages are off while the buckets fill (PR_SET_THP_DISABLE = 41):
    // the buckets' mmaps sit side by side, and a 2 MB page would commit their unused room.
    // SAFETY: prctl only sets a flag of this process.
    unsafe { prctl(41, 1, 0, 0, 0) };
    let big = Mutex::new(Vec::new());
    let parts = par_map(split_lines(&text[i..], threads), |chunk| {
        // Every bucket gets its own capacity (`vec![v; nb]` would clone empty ones), about
        // 18 bytes of input per edge line: at full scale each is then above the mmap
        // threshold, so freeing it returns the memory instead of leaving it in this
        // thread's malloc arena.
        let expect = chunk.len() / 18 / nb;
        let mut buckets: Vec<Vec<[u32; 2]>> = (0..nb)
            .map(|_| Vec::with_capacity(expect + expect / 4 + 16))
            .collect();
        let mut j = 0;
        while j < chunk.len() {
            let [u, v, w] = [0; 3].map(|_| number_fast(chunk, &mut j) as u32);
            if u != v {
                let e = edge(v, w);
                buckets[(u >> SHIFT) as usize].push([u & 0xFFFF | u32::from(e.w) << 16, v]);
                if e.w == BIG {
                    big.lock().unwrap().push((u, v, w)); // about 50 edges at full scale
                }
            }
        }
        buckets
    });
    // SAFETY: as above; the graph arrays from here on are randomly read, where huge pages
    // save TLB misses.
    unsafe { prctl(41, 0, 0, 0, 0) };
    let mut base = vec![0usize; nb + 1];
    for b in 0..nb {
        base[b + 1] = base[b] + parts.iter().map(|p| p[b].len()).sum::<usize>();
    }
    // Regroup by bucket, so each bucket's lists can be freed as soon as it is built.
    let mut by_bucket: Vec<Vec<Vec<[u32; 2]>>> = (0..nb).map(|_| Vec::new()).collect();
    for part in parts {
        by_bucket
            .iter_mut()
            .zip(part)
            .for_each(|(b, list)| b.push(list));
    }
    let by_bucket: Vec<Mutex<Vec<Vec<[u32; 2]>>>> = by_bucket.into_iter().map(Mutex::new).collect();

    // Counting sort, one bucket at a time: its degree counts fit in L1, and each bucket owns
    // a disjoint range of `off` and of `adj`, so no atomics are needed.
    let mut off = vec![0u32; n + 1];
    off[n] = base[nb] as u32;
    let mut adj = zero_edges(base[nb]);
    let (offp, adjp) = (SharedPtr(off.as_mut_ptr()), SharedPtr(adj.as_mut_ptr()));
    let next = AtomicUsize::new(0);
    par_map(vec![(); threads], |()| {
        let mut cursor = vec![0u32; 1 << SHIFT];
        loop {
            let b = next.fetch_add(1, Relaxed);
            if b >= nb {
                break;
            }
            let lists = std::mem::take(&mut *by_bucket[b].lock().unwrap());
            let (lo, edges) = (b << SHIFT, || lists.iter().flatten());
            cursor.fill(0);
            edges().for_each(|&[x, _]| cursor[x as usize & 0xFFFF] += 1);
            let mut c = base[b] as u32;
            for k in 0..(lo + (1 << SHIFT)).min(n) - lo {
                offp.set(lo + k, c); // nodes lo.. belong to this bucket alone
                (cursor[k], c) = (c, c + cursor[k]);
            }
            for &[x, v] in edges() {
                let (k, w) = (x as usize & 0xFFFF, (x >> 16) as u16);
                adjp.set(cursor[k] as usize, Edge { v, w }); // so do its edge slots
                cursor[k] += 1;
            }
        }
    });
    let mut big = big.into_inner().unwrap();
    big.sort_unstable();
    Graph { off, adj, big }
}

/// Renumbers the nodes in breadth-first order, so nodes that are close in the network
/// are close in memory, and gives the queries their new ids too. The input ids
/// are a random relabelling, which makes nearly every edge a cache miss; after this, a
/// Dijkstra ball lives in a few compact ranges of `off`, `adj` and `dist`.
///
/// Every thread grows regions from roots in its own slice of the old ids, claiming nodes
/// with an atomic flag, and the threads' orders are then laid out one after another. A
/// region is a breadth-first ball of at most REGION nodes, about an 8 x 8 patch of the
/// map, and the next region starts where the last one stopped, so a search's ball covers
/// whole patches instead of thin slices of long breadth-first rings.
const REGION: usize = 64;

/// Starts loading the cache line at `p` (a hint: any address is safe).
#[inline(always)]
fn prefetch<T>(p: *const T) {
    #[cfg(target_arch = "x86_64")]
    // SAFETY: a prefetch never faults, whatever the address.
    unsafe {
        std::arch::x86_64::_mm_prefetch(p.cast(), std::arch::x86_64::_MM_HINT_T0)
    };
}

/// How many nodes ahead the walks over `order` prefetch: the old ids are random, so each
/// node's offsets and edge list are cache misses, and these overlap them.
const AHEAD: usize = 16;

fn relabel(mut g: Graph, queries: &mut [(u32, u32)], threads: usize) -> Graph {
    let n = g.off.len() - 1;
    let claimed: Vec<AtomicBool> = (0..n).map(|_| AtomicBool::new(false)).collect();
    let claim = |x: usize| !claimed[x].load(Relaxed) && !claimed[x].swap(true, Relaxed);
    let orders = par_map((0..threads).collect(), |t| {
        let mut order: Vec<u32> = Vec::with_capacity(n / threads + n / threads / 4);
        let mut seeds = std::collections::VecDeque::new();
        for root in n * t / threads..n * (t + 1) / threads {
            seeds.push_back(root as u32);
            while let Some(seed) = seeds.pop_front() {
                if !claim(seed as usize) {
                    continue;
                }
                let (start, mut head) = (order.len(), order.len());
                order.push(seed);
                while head < order.len() {
                    let u = order[head] as usize;
                    if let Some(&x) = order.get(head + 1) {
                        prefetch(g.adj.as_ptr().wrapping_add(g.off[x as usize] as usize));
                    }
                    head += 1;
                    for &Edge { v, .. } in &g.adj[g.off[u] as usize..g.off[u + 1] as usize] {
                        if order.len() - start < REGION {
                            if claim(v as usize) {
                                prefetch(&g.off[v as usize]);
                                order.push(v);
                            }
                        } else if !claimed[v as usize].load(Relaxed) {
                            seeds.push_back(v); // the next region starts at this one's edge
                        }
                    }
                }
            }
        }
        order
    });
    drop(claimed);
    let order: Vec<u32> = orders.concat();
    drop(orders);
    // new_id[old] and each node's degree, in parallel: both are random reads or writes
    // over 1.74 million nodes, too slow as one thread's loop.
    let mut new_id = vec![0u32; n];
    let mut off = vec![0u32; n + 1];
    let (idp, degp) = (SharedPtr(new_id.as_mut_ptr()), SharedPtr(off.as_mut_ptr()));
    par_map((0..threads).collect(), |t| {
        for k in n * t / threads..n * (t + 1) / threads {
            if let Some(&x) = order.get(k + AHEAD) {
                prefetch(&g.off[x as usize]);
            }
            let u = order[k] as usize; // order is a permutation, so u is this thread's alone
            idp.set(u, k as u32);
            degp.set(k + 1, g.off[u + 1] - g.off[u]);
        }
    });
    for k in 0..n {
        off[k + 1] += off[k];
    }
    // The old graph's heads, the big list and the queries get their new ids in place, so
    // `new_id` is freed before the new graph exists: the copy is the run's memory peak.
    let id = |x: u32| new_id[x as usize];
    let size = g.adj.len().div_ceil(threads).max(1);
    par_map(g.adj.chunks_mut(size).collect(), |part| {
        part.iter_mut().for_each(|e| e.v = id(e.v));
    });
    g.big.iter_mut().for_each(|e| (e.0, e.1) = (id(e.0), id(e.1)));
    g.big.sort_unstable();
    queries.iter_mut().for_each(|q| *q = (id(q.0), id(q.1)));
    drop(new_id);
    // Copy each node's edge list to its new place, in parallel over ranges of new ids.
    let mut adj = zero_edges(g.adj.len());
    let mut rest: &mut [Edge] = &mut adj;
    let mut jobs = Vec::new();
    for t in 0..threads {
        let (k0, k1) = (n * t / threads, n * (t + 1) / threads);
        let (mine, tail) = std::mem::take(&mut rest).split_at_mut((off[k1] - off[k0]) as usize);
        rest = tail;
        jobs.push((k0, k1, mine));
    }
    par_map(jobs, |(k0, k1, mine)| {
        let mut slots = mine.iter_mut();
        for k in k0..k1 {
            if let Some(&x) = order.get(k + AHEAD) {
                prefetch(&g.off[x as usize]);
            }
            if let Some(&x) = order.get(k + AHEAD / 2) {
                prefetch(g.adj.as_ptr().wrapping_add(g.off[x as usize] as usize));
            }
            let u = order[k];
            for &e in &g.adj[g.off[u as usize] as usize..g.off[u as usize + 1] as usize] {
                *slots.next().unwrap() = e;
            }
        }
    });
    Graph { off, adj, big: g.big }
}

/// A radix heap: a priority queue for keys that never go below the last key popped, as in
/// Dijkstra. An entry sits in bucket `bits(key ^ last)`. Popping refills bucket 0 from the
/// lowest non-empty bucket, and each entry moves down at most 32 times in total. Keys are
/// u32: a search whose distances would leave u32 goes to `shortest_wide` instead.
struct RadixHeap {
    last: u32,
    len: usize,
    buckets: Vec<Vec<(u32, u32)>>,
}

impl RadixHeap {
    fn new() -> Self {
        RadixHeap {
            last: 0,
            len: 0,
            buckets: vec![Vec::new(); 33],
        }
    }

    #[inline(always)]
    fn push(&mut self, key: u32, v: u32) {
        self.len += 1;
        self.buckets[32 - (key ^ self.last).leading_zeros() as usize].push((key, v));
    }

    /// The smallest key, or None when empty. Bucket 0 holds only keys equal to `last`.
    fn min(&mut self) -> Option<u32> {
        if self.buckets[0].is_empty() {
            let i = (1..33).find(|&i| !self.buckets[i].is_empty())?;
            let mut items = std::mem::take(&mut self.buckets[i]);
            self.last = items.iter().map(|e| e.0).min().unwrap();
            for &(key, v) in &items {
                self.buckets[32 - (key ^ self.last).leading_zeros() as usize].push((key, v));
            }
            items.clear();
            self.buckets[i] = items; // keep the capacity
        }
        Some(self.last)
    }

    fn pop(&mut self) -> Option<(u32, u32)> {
        self.min()?;
        self.len -= 1;
        self.buckets[0].pop()
    }

    fn clear(&mut self) {
        self.buckets.iter_mut().for_each(Vec::clear);
        self.last = 0;
        self.len = 0;
    }
}

/// The edges a search follows from `u`, as (other end, weight), passed to `f`.
trait Adj {
    fn each(&self, u: u32, f: impl FnMut(u32, u64));
}

impl Adj for Graph {
    #[inline(always)]
    fn each(&self, u: u32, mut f: impl FnMut(u32, u64)) {
        self.edges(u).for_each(|(v, w)| f(v, w));
    }
}

/// The in-edges of every node, for the backward search, without a second graph. Road
/// segments are nearly all two-way, so in-edge y -> x is usually the reverse of an
/// out-edge x -> y: `rw[j]` holds the weight of the reverse of edge `j` (the lightest one
/// if parallel), or ABSENT. The few others (one-way streets) are in `extra` as sorted
/// (head, tail, weight), flagged per head in `has_extra`.
struct Back<'a> {
    g: &'a Graph,
    rw: Vec<u16>,
    extra: Vec<(u32, u32, u32)>,
    has_extra: Vec<u64>,
    start: Vec<u32>, // extra[start[b]..start[b + 1]] holds the heads in block b of 64 nodes
}

impl<'a> Back<'a> {
    /// After the renumbering, y's list is near x's in memory, so finding x in it is cheap.
    fn new(g: &'a Graph, threads: usize) -> Self {
        let n = g.off.len() - 1;
        let mut rw = vec![0u16; g.adj.len()];
        let dst = SharedPtr(rw.as_mut_ptr());
        let ranges: Vec<_> = (0..threads)
            .map(|t| n * t / threads..n * (t + 1) / threads)
            .collect();
        let found = par_map(ranges, |r| {
            let mut extra = Vec::new();
            for x in r {
                for j in g.off[x] as usize..g.off[x + 1] as usize {
                    let Edge { v: y, w } = g.adj[j];
                    let back = &g.adj[g.off[y as usize] as usize..g.off[y as usize + 1] as usize];
                    let mut rev = ABSENT; // BIG < ABSENT, so the min is the lightest reverse
                    back.iter().for_each(|e| rev = if e.v == x as u32 { rev.min(e.w) } else { rev });
                    dst.set(j, rev); // edge j is in this thread's range
                    if rev == ABSENT {
                        let w = if w != BIG { u64::from(w) } else { g.big_weight(x as u32, y) };
                        extra.push((y, x as u32, w as u32));
                    }
                }
            }
            extra
        });
        // Counting sort by block of 64 heads; a lookup scans its block's few entries.
        let (mut has_extra, mut start) = (vec![0u64; n / 64 + 1], vec![0u32; n / 64 + 2]);
        for &(y, _, _) in found.iter().flatten() {
            has_extra[y as usize / 64] |= 1 << (y % 64);
            start[y as usize / 64 + 1] += 1;
        }
        (0..start.len() - 1).for_each(|b| start[b + 1] += start[b]);
        let mut cursor = start.clone();
        let mut extra = vec![(0, 0, 0); start[n / 64 + 1] as usize];
        for &e in found.iter().flatten() {
            extra[cursor[e.0 as usize / 64] as usize] = e;
            cursor[e.0 as usize / 64] += 1;
        }
        Back { g, rw, extra, has_extra, start }
    }

    fn has_in(&self, x: u32) -> bool {
        let mut any = false;
        self.each(x, |_, _| any = true);
        any
    }
}

impl Adj for Back<'_> {
    #[inline(always)]
    fn each(&self, x: u32, mut f: impl FnMut(u32, u64)) {
        let g = self.g;
        let (a, b) = (g.off[x as usize] as usize, g.off[x as usize + 1] as usize);
        for (e, &rw) in g.adj[a..b].iter().zip(&self.rw[a..b]) {
            if rw != ABSENT {
                let y = e.v;
                f(y, if rw != BIG { u64::from(rw) } else { g.big_weight(y, x) });
            }
        }
        if self.has_extra[x as usize / 64] >> (x % 64) & 1 != 0 {
            let b = x as usize / 64;
            for &(h, y, w) in &self.extra[self.start[b] as usize..self.start[b + 1] as usize] {
                if h == x {
                    f(y, u64::from(w));
                }
            }
        }
    }
}

/// Node v's distance lives on page `v >> PAGE_BITS` of a small pool, so a search uses
/// memory for the pages its nodes fall on, not for the whole graph.
const PAGE_BITS: usize = 8;
const PAGE: usize = 1 << PAGE_BITS;

/// One direction of a search. `dist` holds distance + 1, with 0 for "not reached", in pages
/// handed out on first write: `table[v >> PAGE_BITS]` is the page's place in `pages`, and
/// page 0 is all zeros, standing in for every page not yet handed out. After the BFS
/// renumbering a search touches few pages, so the pool stays small, and resetting the
/// touched entries leaves every page zeroed for the next query. u32 distances: one that
/// would not fit sets `wide`, and the query is re-run by `shortest_wide`.
struct Side {
    table: Vec<u32>,
    pages: Vec<u32>,
    used: usize,
    touched: Vec<u32>,
    heap: RadixHeap,
}

impl Side {
    fn new(n: usize) -> Self {
        Side {
            table: vec![0; (n >> PAGE_BITS) + 1],
            pages: vec![0; PAGE],
            used: 1,
            touched: Vec::new(),
            heap: RadixHeap::new(),
        }
    }

    #[inline(always)]
    fn dist(&self, v: u32) -> u32 {
        let v = v as usize;
        self.pages[(self.table[v >> PAGE_BITS] as usize) << PAGE_BITS | (v & (PAGE - 1))]
    }

    #[inline(always)]
    fn slot(&mut self, v: u32) -> &mut u32 {
        let v = v as usize;
        let page = &mut self.table[v >> PAGE_BITS];
        if *page == 0 {
            *page = self.used as u32;
            self.used += 1;
            if self.pages.len() < self.used << PAGE_BITS {
                self.pages.resize(self.used << PAGE_BITS, 0);
            }
        }
        &mut self.pages[(*page as usize) << PAGE_BITS | (v & (PAGE - 1))]
    }

    fn start(&mut self, x: u32) {
        *self.slot(x) = 1;
        self.touched.push(x);
        self.heap.push(0, x);
    }

    fn reset(&mut self) {
        for &v in &self.touched {
            let page = self.table[v as usize >> PAGE_BITS] as usize;
            self.pages[page << PAGE_BITS | (v as usize & (PAGE - 1))] = 0;
        }
        for &v in &self.touched {
            self.table[v as usize >> PAGE_BITS] = 0;
        }
        self.touched.clear();
        self.heap.clear();
        self.used = 1;
    }

    /// Settles one node and relaxes its edges in `g`. Every relaxed edge that reaches a
    /// node the other side has labelled is a complete s-t path; `best` keeps the shortest.
    fn step(&mut self, g: &impl Adj, other: &Side, best: &mut u64, wide: &mut bool) {
        let (d, u) = self.heap.pop().unwrap();
        let d = u64::from(d);
        if d + 1 > u64::from(self.dist(u)) {
            return; // stale entry
        }
        g.each(u, |v, w| {
            let nd = d + w;
            if nd >= u64::from(u32::MAX) {
                *wide = true;
                return;
            }
            let o = other.dist(v);
            if o != 0 {
                *best = (*best).min(nd + u64::from(o) - 1);
            }
            let dv = self.slot(v);
            if *dv == 0 || nd + 1 < u64::from(*dv) {
                let new = *dv == 0;
                *dv = nd as u32 + 1;
                if new {
                    self.touched.push(v);
                }
                self.heap.push(nd as u32, v);
            }
        });
    }
}

/// Bidirectional Dijkstra: forward from `s` in `g`, backward from `t` in `r`, always
/// growing the side with fewer queued entries (growing by smaller key would let one side
/// flood the graph when the other starts with an edge of weight near 2^32). Once the two
/// next keys add up to at least the best path seen, no shorter path can exist.
/// Returns None if a distance left u32, else Some(the distance, or None if unreachable).
fn shortest(
    g: &Graph,
    r: &Back,
    (f, b): &mut (Side, Side),
    s: u32,
    t: u32,
) -> Option<Option<u64>> {
    let (mut best, mut wide) = (u64::MAX, false);
    f.start(s);
    b.start(t);
    while let (Some(kf), Some(kb)) = (f.heap.min(), b.heap.min()) {
        if wide || u64::from(kf) + u64::from(kb) >= best {
            break;
        }
        if f.heap.len <= b.heap.len {
            f.step(g, b, &mut best, &mut wide);
        } else {
            b.step(r, f, &mut best, &mut wide);
        }
    }
    f.reset();
    b.reset();
    (!wide).then_some((best != u64::MAX).then_some(best))
}

/// Plain Dijkstra with 64-bit distances in a hash map, for the rare query whose distances
/// do not fit in u32 (paths over edges of weight near 2^32).
fn shortest_wide(g: &Graph, s: u32, t: u32) -> Option<u64> {
    let mut dist = HashMap::from([(s, 0u64)]);
    let mut heap = BinaryHeap::from([Reverse((0u64, s))]);
    while let Some(Reverse((d, u))) = heap.pop() {
        if d > dist[&u] {
            continue; // stale entry
        }
        if u == t {
            return Some(d);
        }
        for (v, w) in g.edges(u) {
            let nd = d + w;
            if dist.get(&v).map_or(true, |&x| nd < x) {
                dist.insert(v, nd);
                heap.push(Reverse((nd, v)));
            }
        }
    }
    None
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = args.get(1).map_or("/data", String::as_str);
    let out_dir = args.get(2).map_or("/out", String::as_str);
    let threads = thread::available_parallelism().map_or(1, |n| n.get());
    // Serve every allocation of 128 KiB or more with its own mmap (M_MMAP_THRESHOLD = -3),
    // so freeing the parse buckets and the first graph returns them to the OS. Otherwise
    // glibc keeps them in per-thread arenas and they count towards peak memory.
    // SAFETY: mallopt only tunes the allocator; it is called before any thread starts.
    unsafe { mallopt(-3, 128 * 1024) };
    let qtext = map(&format!("{data_dir}/queries.txt"));
    let mut queries = Vec::new();
    let mut i = 0;
    while i < qtext.len() {
        let s = number(qtext, &mut i) as u32;
        queries.push((s, number(qtext, &mut i) as u32));
    }
    let graph = build_graph(map(&format!("{data_dir}/graph.txt")), threads);
    let g = relabel(graph, &mut queries, threads);
    let rev = Back::new(&g, threads);

    // Queries sorted by source and handed out one at a time: the threads then all work in
    // one moving part of the renumbered map, which the shared cache holds, and no thread is
    // left finishing a run of long searches alone. Answers: i64, -1 unreachable.
    let mut by_source: Vec<usize> = (0..queries.len()).collect();
    by_source.sort_unstable_by_key(|&q| queries[q].0);
    let answers: Vec<AtomicI64> = queries.iter().map(|_| AtomicI64::new(-1)).collect();
    let (next, n) = (AtomicUsize::new(0), g.off.len() - 1);
    par_map(vec![(); threads], |()| {
        let mut sides: Option<(Side, Side)> = None;
        while let Some(&q) = by_source.get(next.fetch_add(1, Relaxed)) {
            let (s, t) = queries[q];
            let answer = if s == t {
                Some(0)
            } else if g.off[s as usize] == g.off[s as usize + 1] || !rev.has_in(t) {
                None // no way out of s, or no way into t
            } else {
                let sides = sides.get_or_insert_with(|| (Side::new(n), Side::new(n)));
                shortest(&g, &rev, sides, s, t).unwrap_or_else(|| shortest_wide(&g, s, t))
            };
            answers[q].store(answer.map_or(-1, |d| d as i64), Relaxed);
        }
    });

    let mut out = Vec::with_capacity(queries.len() * 8);
    for a in &answers {
        out.extend_from_slice(a.load(Relaxed).to_string().as_bytes());
        out.push(b'\n');
    }
    File::create(format!("{out_dir}/distances.txt"))
        .and_then(|mut f| f.write_all(&out))
        .expect("write distances.txt");
}
