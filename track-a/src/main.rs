//! Speed Demon 2026, Track A (Ingest). README.md explains the approach.
//!
//! Pipeline:
//! 1. mmap /data/requests.log (already in the page cache, so no copy and no anon memory).
//! 2. Split it into one chunk per thread at line boundaries. Each thread parses its lines
//!    by hand, interns endpoints in a small local hash table, counts lines per (hour,
//!    local endpoint) cell, and keeps per line its latency (u32) and its endpoint id with
//!    the 5xx flag (u16). Hour changes are kept apart: they are rare.
//! 3. Merge the endpoint names, sort them bytewise and give each a global rank. A group is
//!    then `hour * n_endpoints + rank`, so group order is exactly the output order.
//! 4. Counting sort: a prefix sum over groups turns every thread's cell counts into write
//!    cursors, and the threads scatter latencies so each group's latencies are contiguous.
//! 5. Per group, nearest-rank percentiles with select_nth_unstable (no full sort), and
//!    format the lines in parallel. Write the buffers out in order.

use std::fs::File;
use std::os::fd::AsRawFd;
use std::os::unix::fs::FileExt;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::thread;

/// A thread's cell for (hour, local endpoint id) is `hour * EP_CAP + id`. The spec fixes the
/// catalogue at 1,842 endpoints and the span at 72 hours, both well inside these caps.
const EP_CAP: usize = 2048;
const HOUR_CAP: usize = 256;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
}
const PROT_READ: i32 = 1;
const MAP_PRIVATE: i32 = 2;

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's algorithm).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// The inverse of `days_from_civil`.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

#[inline(always)]
fn digits(b: &[u8]) -> i64 {
    b.iter().fold(0, |acc, &c| acc * 10 + i64::from(c - b'0'))
}

/// One thread's parse result. Endpoint ids are local to the thread.
struct Parsed<'a> {
    names: Vec<&'a [u8]>,
    /// The same names, sorted bytewise (done here, in parallel, for the merge in main).
    sorted: Vec<&'a [u8]>,
    /// Per line: the latency, and the endpoint id | 5xx << 15.
    lats: Vec<u32>,
    ids: Vec<u16>,
    /// (line index, hour) wherever the hour changes from the line before.
    hours: Vec<(u32, u32)>,
    /// Per cell: lines. Allocated zeroed, so untouched pages cost nothing.
    cells: Vec<u32>,
    max_hour: usize,
}

#[inline(always)]
fn word(buf: &[u8], i: usize) -> u64 {
    u64::from_le_bytes(buf[i..i + 8].try_into().unwrap())
}

const ONES: u64 = 0x0101_0101_0101_0101;

/// Spaces and LFs of 64 bytes from `at` as a bitmask (bit k = byte at + k). Past the end
/// of `buf` the bytes read as zero.
#[inline(always)]
fn separators(buf: &[u8], at: usize) -> u64 {
    if at + 64 > buf.len() {
        let mut pad = [0u8; 64];
        pad[..buf.len() - at].copy_from_slice(&buf[at..]);
        return separators(&pad, 0);
    }
    #[cfg(target_feature = "avx2")]
    {
        use std::arch::x86_64::*;
        // SAFETY: AVX2 is enabled at compile time and the 64 bytes are inside `buf`.
        unsafe {
            let half = |k: usize| {
                let v = _mm256_loadu_si256(buf.as_ptr().add(at + k).cast());
                let sp = _mm256_cmpeq_epi8(v, _mm256_set1_epi8(b' ' as i8));
                let lf = _mm256_cmpeq_epi8(v, _mm256_set1_epi8(b'\n' as i8));
                u64::from(_mm256_movemask_epi8(_mm256_or_si256(sp, lf)) as u32)
            };
            half(0) | half(32) << 32
        }
    }
    #[cfg(not(target_feature = "avx2"))]
    (0..64).fold(0, |m, k| {
        m | u64::from(matches!(buf[at + k], b' ' | b'\n')) << k
    })
}

/// Walks the spaces and LFs of a buffer in order, 64 bytes of bitmask at a time, so
/// finding a field's end costs a trailing-zero count instead of a byte loop.
struct Seps<'a> {
    buf: &'a [u8],
    at: usize,
    mask: u64,
}

impl Seps<'_> {
    #[inline(always)]
    fn next(&mut self) -> usize {
        while self.mask == 0 {
            self.at += 64;
            self.mask = separators(self.buf, self.at);
        }
        let i = self.at + self.mask.trailing_zeros() as usize;
        self.mask &= self.mask - 1;
        i
    }
}

/// The decimal in `buf[q..nl]` (1 to 10 digits, with a line of at least 8 bytes before).
#[inline(always)]
fn latency(buf: &[u8], q: usize, nl: usize) -> u64 {
    // The last (up to) 8 digits, as one word ending at the LF. Bytes before the number are
    // masked to zero digits; then pairs, quads and octets are combined (2561, 6553601, ...).
    let keep = (nl - q).min(8);
    let mask = u64::MAX << (8 * (8 - keep));
    let d = (word(buf, nl - 8) & mask) - (ONES * u64::from(b'0') & mask);
    let v = (d * 10 + (d >> 8)) & 0x00FF_00FF_00FF_00FF;
    let v = (v * 100 + (v >> 16)) & 0x0000_FFFF_0000_FFFF;
    let low = (v * 10_000 + (v >> 32)) & 0xFFFF_FFFF;
    let high = buf[q..nl - keep]
        .iter()
        .fold(0, |a, &c| a * 10 + u64::from(c - b'0'));
    high * 100_000_000 + low
}

/// Parses whole lines. Hours are counted from `base_hour` (the span start, 2026-10-30T18Z).
fn parse(buf: &[u8], base_hour: i64) -> Parsed<'_> {
    const BITS: u32 = 13;
    const SLOTS: usize = 1 << BITS; // > 4x the 1,842 endpoints, so probes stay short
    let mut table = vec![0u32; SLOTS]; // id + 1, 0 = empty
    let mut names: Vec<&[u8]> = Vec::with_capacity(2048);
    let mut lats = Vec::with_capacity(buf.len() / 70);
    let mut ids = Vec::with_capacity(buf.len() / 70);
    let mut hours = Vec::new();
    let mut cells = vec![0u32; HOUR_CAP * EP_CAP];
    let mut max_hour = 0;
    let (mut prev, mut hour) = ((u64::MAX, u64::MAX), 0);
    let mut seps = Seps {
        buf,
        at: 0,
        mask: separators(buf, 0),
    };
    let mut p = 0;
    while p < buf.len() {
        // ts is fixed width: YYYY-MM-DDTHH:MM:SS.ffffffZ. Neighbouring lines nearly always
        // share "YYYY-MM-DDTHH" (bytes 0..13), so the calendar maths runs only on a change.
        let key = (word(buf, p), word(buf, p + 5));
        if key != prev {
            let t = &buf[p..p + 13];
            let day = days_from_civil(digits(&t[0..4]), digits(&t[5..7]), digits(&t[8..10]));
            hour = (day * 24 + digits(&t[11..13]) - base_hour) as usize;
            assert!(hour < HOUR_CAP, "timestamp outside the supported span");
            max_hour = max_hour.max(hour);
            prev = key;
            if hours.last().map_or(true, |&(_, h)| h != hour as u32) {
                hours.push((ids.len() as u32, hour as u32));
            }
        }
        // A line has exactly four spaces and an LF: after the timestamp, the method, the
        // endpoint and the status. Hash the endpoint's first and last eight bytes with its
        // length; the line goes on for at least 7 bytes after it, so the loads stay in bounds.
        let [_, m, s, t, nl] = [0; 5].map(|_| seps.next());
        let e = m + 1;
        let name = &buf[e..s];
        let len = s - e;
        let first = if len < 8 {
            word(buf, e) & ((1 << (8 * len)) - 1)
        } else {
            word(buf, e)
        };
        let last = if len > 8 { word(buf, s - 8) } else { 0 };
        let h = (first ^ last.rotate_left(29) ^ len as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        let mut slot = (h >> (64 - BITS)) as usize;
        let id = loop {
            match table[slot] {
                0 => {
                    assert!(names.len() < EP_CAP, "too many distinct endpoints");
                    names.push(name);
                    table[slot] = names.len() as u32;
                    break names.len() - 1;
                }
                v if names[v as usize - 1] == name => break v as usize - 1,
                _ => slot = (slot + 1) & (SLOTS - 1),
            }
        };
        // status: three digits; 5xx is an error. latency: digits up to LF.
        cells[hour * EP_CAP + id] += 1;
        lats.push(latency(buf, t + 1, nl) as u32);
        ids.push(id as u16 | u16::from(buf[s + 1] == b'5') << 15);
        p = nl + 1;
    }
    let mut sorted = names.clone();
    sorted.sort_unstable();
    Parsed {
        names,
        sorted,
        lats,
        ids,
        hours,
        cells,
        max_hour,
    }
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

fn push_u64(out: &mut Vec<u8>, mut v: u64) {
    let mut tmp = [0u8; 20];
    let mut i = tmp.len();
    loop {
        i -= 1;
        tmp[i] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    out.extend_from_slice(&tmp[i..]);
}

/// A raw pointer that threads may share when they provably write disjoint indices.
#[derive(Clone, Copy)]
struct SharedPtr<T>(*mut T);
unsafe impl<T> Send for SharedPtr<T> {}
unsafe impl<T> Sync for SharedPtr<T> {}
impl<T> SharedPtr<T> {
    /// A pointer to element `i`. The caller guarantees `i` is in bounds and that no other
    /// thread uses the same element.
    fn at(self, i: usize) -> *mut T {
        // SAFETY: in bounds, per the caller's guarantee.
        unsafe { self.0.add(i) }
    }
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = args.get(1).map_or("/data", String::as_str);
    let out_dir = args.get(2).map_or("/out", String::as_str);
    let file = File::open(format!("{data_dir}/requests.log")).expect("open requests.log");
    let len = file.metadata().expect("stat").len() as usize;
    let out = File::create(format!("{out_dir}/summary.txt")).expect("create summary.txt");
    if len == 0 {
        return;
    }
    // SAFETY: a read-only private mapping of a file nobody modifies while we run.
    let buf: &[u8] = unsafe {
        let ptr = mmap(
            std::ptr::null_mut(),
            len,
            PROT_READ,
            MAP_PRIVATE,
            file.as_raw_fd(),
            0,
        );
        assert!(ptr as isize != -1, "mmap failed");
        std::slice::from_raw_parts(ptr, len)
    };
    let threads = thread::available_parallelism().map_or(1, |n| n.get());
    let base_hour = days_from_civil(2026, 10, 30) * 24 + 18;

    // 1. Parse in parallel.
    let mut parts = par_map(split_lines(buf, threads), |chunk| parse(chunk, base_hour));

    // 2. Global endpoint ranks, in byte order (slice Ord puts a proper prefix first). Each
    //    thread's names arrive sorted, and a stable sort merges sorted runs in linear passes.
    //    Each thread then maps ranks to its local ids (NONE where it never saw the name).
    let mut all: Vec<&[u8]> = Vec::new();
    for p in &parts {
        // Union of two sorted, duplicate-free lists; they are nearly identical.
        let (mut i, mut j, mut merged) = (0, 0, Vec::with_capacity(all.len() + 64));
        while i < all.len() || j < p.sorted.len() {
            let take = match (all.get(i), p.sorted.get(j)) {
                (Some(a), Some(b)) => a.cmp(b),
                (Some(_), None) => std::cmp::Ordering::Less,
                _ => std::cmp::Ordering::Greater,
            };
            merged.push(if take.is_gt() { p.sorted[j] } else { all[i] });
            i += usize::from(take.is_le());
            j += usize::from(take.is_ge());
        }
        all = merged;
    }
    let (n_eps, n_hours) = (
        all.len(),
        parts.iter().map(|p| p.max_hour).max().unwrap_or(0) + 1,
    );
    let n_groups = n_hours * n_eps;
    const NONE: usize = usize::MAX;
    let (local_of, rank_of): (Vec<_>, Vec<_>) = par_map(parts.iter().collect(), |p: &Parsed| {
        let mut local = vec![NONE; n_eps];
        let rank: Vec<usize> = p
            .names
            .iter()
            .map(|n| all.binary_search(n).unwrap())
            .collect();
        rank.iter().enumerate().for_each(|(id, &r)| local[r] = id);
        (local, rank)
    })
    .into_iter()
    .unzip();

    // 3. Prefix sum in output order, by hour: each hour's line total, a running sum over
    //    the hours, then all hours in parallel set their groups' starts and turn each
    //    thread's cell counts into that thread's write cursors.
    let mut hour_start = vec![0u64; n_hours + 1];
    for h in 0..n_hours {
        let cells = parts
            .iter()
            .flat_map(|p| &p.cells[h * EP_CAP..h * EP_CAP + p.names.len()]);
        hour_start[h + 1] = hour_start[h] + cells.map(|&c| u64::from(c)).sum::<u64>();
    }
    let total = hour_start[n_hours];
    let mut start = vec![0u32; n_groups + 1];
    start[n_groups] = total as u32;
    let cells: Vec<SharedPtr<u32>> = parts
        .iter_mut()
        .map(|p| SharedPtr(p.cells.as_mut_ptr()))
        .collect();
    let hours: Vec<_> = start[..n_groups].chunks_mut(n_eps).enumerate().collect();
    par_map(hours, |(h, starts)| {
        let mut total = hour_start[h];
        for r in 0..n_eps {
            starts[r] = total as u32;
            for (cell, local) in cells.iter().zip(&local_of).filter(|(_, l)| l[r] != NONE) {
                // SAFETY: cell (h, local id) is touched only by the worker for hour h.
                let c = unsafe { &mut *cell.at(h * EP_CAP + local[r]) };
                if *c != 0 {
                    // (A zero cell is skipped: writing it would commit an untouched page.)
                    (*c, total) = (total as u32, total + u64::from(*c));
                }
            }
        }
    });

    // 4. Scatter latencies so every group is contiguous, and count 5xx lines per group (few
    //    lines, so shared atomic counters cost little). Then free the per-line records.
    let mut lats = vec![0u32; total as usize];
    let errors: Vec<AtomicU32> = (0..n_groups).map(|_| AtomicU32::new(0)).collect();
    let dst = SharedPtr(lats.as_mut_ptr());
    let jobs: Vec<_> = parts.iter_mut().zip(&rank_of).collect();
    par_map(jobs, |(p, rank)| {
        let (mut hour, mut next) = (0, 0);
        for (k, (&lat, &r)) in p.lats.iter().zip(&p.ids).enumerate() {
            if next < p.hours.len() && p.hours[next].0 as usize == k {
                hour = p.hours[next].1 as usize;
                next += 1;
            }
            let id = usize::from(r & 0x7FFF);
            let cursor = &mut p.cells[hour * EP_CAP + id];
            // SAFETY: each (thread, group) owns its own disjoint index range.
            unsafe { *dst.at(*cursor as usize) = lat };
            *cursor += 1;
            if r >> 15 != 0 {
                errors[hour * n_eps + rank[id]].fetch_add(1, Relaxed);
            }
        }
    });
    drop(parts);

    // 5. Percentiles and formatting, groups split into ranges of about equal latency count.
    let hour_text: Vec<String> = (0..n_hours as i64)
        .map(|h| {
            let (y, m, d) = civil_from_days((base_hour + h).div_euclid(24));
            format!(
                "{y:04}-{m:02}-{d:02}T{:02}:00:00Z ",
                (base_hour + h).rem_euclid(24)
            )
        })
        .collect();
    let mut bounds = vec![0usize];
    for t in 1..threads {
        let g = start.partition_point(|&s| u64::from(s) < total * t as u64 / threads as u64);
        bounds.push(g.min(n_groups).max(*bounds.last().unwrap()));
    }
    bounds.push(n_groups);
    let mut rest: &mut [u32] = &mut lats;
    let mut jobs = Vec::new();
    for w in bounds.windows(2) {
        let (mine, tail) =
            std::mem::take(&mut rest).split_at_mut((start[w[1]] - start[w[0]]) as usize);
        rest = tail;
        jobs.push((w[0], w[1], mine));
    }
    let outputs = par_map(jobs, |(g0, g1, mine)| {
        let mut o = Vec::with_capacity((g1 - g0) * 80);
        for g in g0..g1 {
            let n = (start[g + 1] - start[g]) as usize;
            if n == 0 {
                continue;
            }
            let v = &mut mine[(start[g] - start[g0]) as usize..][..n];
            // Nearest rank: 1-indexed ceil(p*n/100). Select the largest rank first, then
            // search only below it for the smaller ones.
            let r = |p: usize| (p * n + 99) / 100 - 1;
            let p99 = *v.select_nth_unstable(r(99)).1;
            let p95 = *v[..=r(99)].select_nth_unstable(r(95)).1;
            let p50 = *v[..=r(95)].select_nth_unstable(r(50)).1;
            let e = u64::from(errors[g].load(Relaxed));
            o.extend_from_slice(hour_text[g / n_eps].as_bytes());
            o.extend_from_slice(all[g % n_eps]);
            for x in [
                n as u64,
                p50.into(),
                p95.into(),
                p99.into(),
                e,
                e * 1_000_000 / n as u64,
            ] {
                o.push(b' ');
                push_u64(&mut o, x);
            }
            o.push(b'\n');
        }
        o
    });
    drop(lats); // before the file's pages are charged
                // Every buffer goes straight to its place in the file, all at once.
    let offsets = outputs
        .iter()
        .scan(0, |at, o| Some(std::mem::replace(at, *at + o.len() as u64)));
    par_map(outputs.iter().zip(offsets).collect(), |(o, at)| {
        out.write_all_at(o, at).expect("write summary.txt")
    });
}
