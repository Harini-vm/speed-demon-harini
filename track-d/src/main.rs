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
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicUsize, Ordering::Relaxed};
use std::thread;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn mallopt(param: i32, value: i32) -> i32;
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

/// A raw pointer that threads may share when they provably write disjoint indices.
#[derive(Clone, Copy)]
struct SharedPtr<T>(*mut T);
unsafe impl<T> Send for SharedPtr<T> {}
unsafe impl<T> Sync for SharedPtr<T> {}

/// The graph in CSR form: the out-edges of `u` are `adj[off[u]..off[u + 1]]`.
struct Graph {
    off: Vec<u32>,
    adj: Vec<(u32, u32)>,
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
    let parts: Vec<Vec<Vec<[u32; 3]>>> = thread::scope(|sc| {
        let handles: Vec<_> = split_lines(&text[i..], threads)
            .into_iter()
            .map(|chunk| {
                sc.spawn(move || {
                    let expect = chunk.len() / 18 / nb; // ~18 bytes per edge line
                    let mut buckets = vec![Vec::with_capacity(expect + expect / 4 + 16); nb];
                    let mut j = 0;
                    while j < chunk.len() {
                        let u = number_fast(chunk, &mut j) as u32;
                        let v = number_fast(chunk, &mut j) as u32;
                        let w = number_fast(chunk, &mut j) as u32;
                        if u != v {
                            buckets[(u >> SHIFT) as usize].push([u, v, w]);
                        }
                    }
                    buckets
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut base = vec![0usize; nb + 1];
    for b in 0..nb {
        base[b + 1] = base[b] + parts.iter().map(|p| p[b].len()).sum::<usize>();
    }

    // Counting sort, one bucket at a time: its degree counts fit in L1, and each bucket owns
    // a disjoint range of `off` and of `adj`, so no atomics are needed.
    let mut off = vec![0u32; n + 1];
    off[n] = base[nb] as u32;
    let mut adj = vec![(0u32, 0u32); base[nb]];
    let (offp, adjp) = (SharedPtr(off.as_mut_ptr()), SharedPtr(adj.as_mut_ptr()));
    let next = AtomicUsize::new(0);
    thread::scope(|sc| {
        for _ in 0..threads {
            let (parts, base, next) = (&parts, &base, &next);
            sc.spawn(move || {
                let (offp, adjp) = (offp, adjp);
                let mut cursor = vec![0u32; 1 << SHIFT];
                loop {
                    let b = next.fetch_add(1, Relaxed);
                    if b >= nb {
                        break;
                    }
                    let lo = b << SHIFT;
                    let hi = (lo + (1 << SHIFT)).min(n);
                    cursor.fill(0);
                    for &[u, _, _] in parts.iter().flat_map(|p| &p[b]) {
                        cursor[u as usize - lo] += 1;
                    }
                    let mut c = base[b] as u32;
                    for k in 0..hi - lo {
                        // SAFETY: nodes lo..hi belong to this bucket alone.
                        unsafe { *offp.0.add(lo + k) = c };
                        (cursor[k], c) = (c, c + cursor[k]);
                    }
                    for &[u, v, w] in parts.iter().flat_map(|p| &p[b]) {
                        let k = &mut cursor[u as usize - lo];
                        // SAFETY: slots base[b]..base[b + 1] belong to this bucket alone.
                        unsafe { *adjp.0.add(*k as usize) = (v, w) };
                        *k += 1;
                    }
                }
            });
        }
    });
    Graph { off, adj }
}

/// Renumbers the nodes in breadth-first order, so nodes that are close in the network
/// are close in memory, and returns the renumbered graph with `new_id[old]`. The input ids
/// are a random relabelling, which makes nearly every edge a cache miss; after this, a
/// Dijkstra ball lives in a few compact ranges of `off`, `adj` and `dist`.
///
/// Every thread grows breadth-first regions from roots in its own slice of the old ids,
/// claiming nodes with an atomic flag; the regions are then laid out one after another.
fn relabel(g: Graph, threads: usize) -> (Graph, Vec<u32>) {
    let n = g.off.len() - 1;
    let claimed: Vec<AtomicBool> = (0..n).map(|_| AtomicBool::new(false)).collect();
    let claim = |x: usize| !claimed[x].load(Relaxed) && !claimed[x].swap(true, Relaxed);
    let orders: Vec<Vec<u32>> = thread::scope(|sc| {
        let handles: Vec<_> = (0..threads)
            .map(|t| {
                let (g, claim) = (&g, &claim);
                sc.spawn(move || {
                    let mut order: Vec<u32> = Vec::with_capacity(n / threads + n / threads / 4);
                    for root in n * t / threads..n * (t + 1) / threads {
                        if !claim(root) {
                            continue;
                        }
                        let mut head = order.len();
                        order.push(root as u32);
                        while head < order.len() {
                            let u = order[head] as usize;
                            head += 1;
                            for &(v, _) in &g.adj[g.off[u] as usize..g.off[u + 1] as usize] {
                                if claim(v as usize) {
                                    order.push(v);
                                }
                            }
                        }
                    }
                    order
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    drop(claimed);
    let order: Vec<u32> = orders.concat();
    drop(orders);
    // new_id[old] and each node's degree, in parallel: both are random reads or writes
    // over 1.74 million nodes, too slow as one thread's loop.
    let mut new_id = vec![0u32; n];
    let mut off = vec![0u32; n + 1];
    let (idp, degp) = (SharedPtr(new_id.as_mut_ptr()), SharedPtr(off.as_mut_ptr()));
    thread::scope(|sc| {
        for t in 0..threads {
            let (order, g) = (&order, &g);
            sc.spawn(move || {
                let (idp, degp) = (idp, degp);
                for k in n * t / threads..n * (t + 1) / threads {
                    let u = order[k] as usize;
                    // SAFETY: k (and so u, since order is a permutation) belongs to this
                    // thread alone.
                    unsafe {
                        *idp.0.add(u) = k as u32;
                        *degp.0.add(k + 1) = g.off[u + 1] - g.off[u];
                    }
                }
            });
        }
    });
    for k in 0..n {
        off[k + 1] += off[k];
    }
    // Copy each node's edge list to its new place, in parallel over ranges of new ids.
    let mut adj = vec![(0u32, 0u32); g.adj.len()];
    let mut rest: &mut [(u32, u32)] = &mut adj;
    thread::scope(|sc| {
        for t in 0..threads {
            let (k0, k1) = (n * t / threads, n * (t + 1) / threads);
            let (mine, tail) = std::mem::take(&mut rest).split_at_mut((off[k1] - off[k0]) as usize);
            rest = tail;
            let (g, order, new_id, off) = (&g, &order, &new_id, &off);
            sc.spawn(move || {
                for k in k0..k1 {
                    let u = order[k] as usize;
                    let src = &g.adj[g.off[u] as usize..g.off[u + 1] as usize];
                    let base = (off[k] - off[k0]) as usize;
                    for (slot, &(v, w)) in mine[base..base + src.len()].iter_mut().zip(src) {
                        *slot = (new_id[v as usize], w);
                    }
                }
            });
        }
    });
    (Graph { off, adj }, new_id)
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

/// The reverse graph: the in-edges of `v` as (tail, weight), for the backward search.
fn reverse(g: &Graph, threads: usize) -> Graph {
    let n = g.off.len() - 1;
    let indeg: Vec<AtomicU32> = (0..n).map(|_| AtomicU32::new(0)).collect();
    let ranges = |t: usize| n * t / threads..n * (t + 1) / threads;
    thread::scope(|sc| {
        for t in 0..threads {
            let (indeg, range) = (&indeg, ranges(t));
            sc.spawn(move || {
                for &(v, _) in &g.adj[g.off[range.start] as usize..g.off[range.end] as usize] {
                    indeg[v as usize].fetch_add(1, Relaxed);
                }
            });
        }
    });
    let mut off = vec![0u32; n + 1];
    for v in 0..n {
        off[v + 1] = off[v] + indeg[v].load(Relaxed);
        indeg[v].store(off[v], Relaxed); // now the write cursor of v
    }
    let mut adj = vec![(0u32, 0u32); g.adj.len()];
    let dst = SharedPtr(adj.as_mut_ptr());
    thread::scope(|sc| {
        for t in 0..threads {
            let (indeg, range) = (&indeg, ranges(t));
            sc.spawn(move || {
                let dst = dst;
                for u in range {
                    for &(v, w) in &g.adj[g.off[u] as usize..g.off[u + 1] as usize] {
                        let k = indeg[v as usize].fetch_add(1, Relaxed) as usize;
                        // SAFETY: fetch_add hands every in-edge its own slot.
                        unsafe { *dst.0.add(k) = (u as u32, w) };
                    }
                }
            });
        }
    });
    Graph { off, adj }
}

/// One direction of a search. `dist` holds distance + 1, with 0 for "not reached": it is
/// allocated zeroed, so only the pages a thread's searches actually touch are committed,
/// and u32 halves it. A distance that would not fit sets `wide`; the query is then re-run
/// by `shortest_wide`. Only paths over edges of weight near 2^32 get there.
struct Side {
    dist: Vec<u32>,
    touched: Vec<u32>,
    heap: RadixHeap,
}

impl Side {
    fn new(n: usize) -> Self {
        Side {
            dist: vec![0; n],
            touched: Vec::new(),
            heap: RadixHeap::new(),
        }
    }

    fn start(&mut self, x: u32) {
        self.dist[x as usize] = 1;
        self.touched.push(x);
        self.heap.push(0, x);
    }

    fn reset(&mut self) {
        for &v in &self.touched {
            self.dist[v as usize] = 0;
        }
        self.touched.clear();
        self.heap.clear();
    }

    /// Settles one node and relaxes its edges in `g`. Every relaxed edge that reaches a
    /// node the other side has labelled is a complete s-t path; `best` keeps the shortest.
    fn step(&mut self, g: &Graph, other: &[u32], best: &mut u64, wide: &mut bool) {
        let (d, u) = self.heap.pop().unwrap();
        let d = u64::from(d);
        if d + 1 > u64::from(self.dist[u as usize]) {
            return; // stale entry
        }
        for &(v, w) in &g.adj[g.off[u as usize] as usize..g.off[u as usize + 1] as usize] {
            let nd = d + u64::from(w);
            if nd >= u64::from(u32::MAX) {
                *wide = true;
                continue;
            }
            let o = other[v as usize];
            if o != 0 {
                *best = (*best).min(nd + u64::from(o) - 1);
            }
            let dv = &mut self.dist[v as usize];
            if *dv == 0 || nd + 1 < u64::from(*dv) {
                if *dv == 0 {
                    self.touched.push(v);
                }
                *dv = nd as u32 + 1;
                self.heap.push(nd as u32, v);
            }
        }
    }
}

/// Bidirectional Dijkstra: forward from `s` in `fwd`, backward from `t` in `bwd`, always
/// growing the side with fewer queued entries (growing by smaller key would let one side
/// flood the graph when the other starts with an edge of weight near 2^32). Once the two
/// next keys add up to at least the best path seen, no shorter path can exist.
/// Returns Err(()) if a distance left u32, else the distance or None if unreachable.
fn shortest(
    fwd: &Graph,
    bwd: &Graph,
    f: &mut Side,
    b: &mut Side,
    s: u32,
    t: u32,
) -> Result<Option<u64>, ()> {
    let (mut best, mut wide) = (u64::MAX, false);
    f.start(s);
    b.start(t);
    while let (Some(kf), Some(kb)) = (f.heap.min(), b.heap.min()) {
        if wide || u64::from(kf) + u64::from(kb) >= best {
            break;
        }
        if f.heap.len <= b.heap.len {
            f.step(fwd, &b.dist, &mut best, &mut wide);
        } else {
            b.step(bwd, &f.dist, &mut best, &mut wide);
        }
    }
    f.reset();
    b.reset();
    if wide {
        return Err(());
    }
    Ok((best != u64::MAX).then_some(best))
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
        for &(v, w) in &g.adj[g.off[u as usize] as usize..g.off[u as usize + 1] as usize] {
            let nd = d + u64::from(w);
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

    let (g, new_id) = relabel(
        build_graph(map(&format!("{data_dir}/graph.txt")), threads),
        threads,
    );
    let rev = reverse(&g, threads);
    let qtext = map(&format!("{data_dir}/queries.txt"));
    let mut queries = Vec::new();
    let mut i = 0;
    while i < qtext.len() {
        let s = new_id[number(qtext, &mut i) as usize];
        queries.push((s, new_id[number(qtext, &mut i) as usize]));
    }

    // Queries sorted by source and handed out in contiguous runs (two per thread): after
    // the renumbering, each thread then searches one part of the map, so it touches only
    // that part of its distance arrays and keeps it in cache. Answers: i64, -1 unreachable.
    let mut by_source: Vec<usize> = (0..queries.len()).collect();
    by_source.sort_unstable_by_key(|&q| queries[q].0);
    let runs: Vec<&[usize]> = by_source
        .chunks(queries.len().div_ceil(threads * 2).max(1))
        .collect();
    let answers: Vec<AtomicI64> = queries.iter().map(|_| AtomicI64::new(-1)).collect();
    let next = AtomicUsize::new(0);
    let n = g.off.len() - 1;
    let no_edges = |g: &Graph, x: usize| g.off[x] == g.off[x + 1];
    thread::scope(|sc| {
        for _ in 0..threads {
            sc.spawn(|| {
                let mut sides: Option<(Side, Side)> = None;
                while let Some(run) = runs.get(next.fetch_add(1, Relaxed)) {
                    for &q in *run {
                        let (s, t) = queries[q];
                        let answer = if s == t {
                            Some(0)
                        } else if no_edges(&g, s as usize) || no_edges(&rev, t as usize) {
                            None // no way out of s, or no way into t
                        } else {
                            let (f, b) = sides.get_or_insert_with(|| (Side::new(n), Side::new(n)));
                            shortest(&g, &rev, f, b, s, t)
                                .unwrap_or_else(|()| shortest_wide(&g, s, t))
                        };
                        answers[q].store(answer.map_or(-1, |d| d as i64), Relaxed);
                    }
                }
            });
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
