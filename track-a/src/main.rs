//! Speed Demon 2026, Track A (Ingest). README.md explains the approach.
//!
//! Pipeline:
//! 1. mmap /data/requests.log (already in the page cache, so no copy and no anon memory).
//! 2. Split it into one chunk per thread at line boundaries; each thread parses its lines
//!    by hand into packed u64 records and interns endpoints in a small local hash table.
//! 3. Merge the endpoint names, sort them bytewise and give each a global rank. A group is
//!    then `hour * n_endpoints + rank`, so group order is exactly the output order.
//! 4. Counting sort: count per group per thread, prefix-sum, scatter latencies so each
//!    group's latencies are contiguous.
//! 5. Per group, nearest-rank percentiles with select_nth_unstable (no full sort), and
//!    format the lines in parallel. Write the buffers out in order.

use std::collections::HashMap;
use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::thread;

/// Record layout: [endpoint local id: 16][hour: 15][is 5xx: 1][latency: 32].
const LAT_MASK: u64 = 0xFFFF_FFFF;
const ERR_BIT: u64 = 1 << 32;
const HOUR_SHIFT: u32 = 33;
const HOUR_MASK: u64 = 0x7FFF;
const EP_SHIFT: u32 = 48;

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
    recs: Vec<u64>,
    max_hour: u64,
}

/// Parses whole lines. Hours are counted from `base_hour` (the span start, 2026-10-30T18Z).
fn parse(buf: &[u8], base_hour: i64) -> Parsed<'_> {
    const SLOTS: usize = 1 << 13; // > 4x the 1,842 endpoints, so probes stay short
    let mut table = vec![0u32; SLOTS]; // id + 1, 0 = empty
    let mut names: Vec<&[u8]> = Vec::with_capacity(2048);
    let mut recs = Vec::with_capacity(buf.len() / 70);
    let mut max_hour = 0;
    let mut p = 0;
    while p < buf.len() {
        // ts is fixed width: YYYY-MM-DDTHH:MM:SS.ffffffZ
        let t = &buf[p..p + 27];
        let day = days_from_civil(digits(&t[0..4]), digits(&t[5..7]), digits(&t[8..10]));
        let hour = (day * 24 + digits(&t[11..13]) - base_hour) as u64;
        assert!(hour <= HOUR_MASK, "timestamp outside the supported span");
        max_hour = max_hour.max(hour);
        // method: 3 to 7 bytes, then a space
        let mut e = p + 31;
        while buf[e] != b' ' {
            e += 1;
        }
        e += 1;
        // endpoint: scan to the space, hashing as we go (FNV-1a)
        let mut s = e;
        let mut h: u64 = 0xcbf2_9ce4_8422_2325;
        while buf[s] != b' ' {
            h = (h ^ u64::from(buf[s])).wrapping_mul(0x100_0000_01b3);
            s += 1;
        }
        let name = &buf[e..s];
        let mut slot = (h ^ (h >> 29)) as usize & (SLOTS - 1);
        let id = loop {
            match table[slot] {
                0 => {
                    names.push(name);
                    table[slot] = names.len() as u32;
                    break names.len() - 1;
                }
                v if names[v as usize - 1] == name => break v as usize - 1,
                _ => slot = (slot + 1) & (SLOTS - 1),
            }
        };
        // status: three digits; 5xx is an error. latency: digits up to LF.
        let err = if buf[s + 1] == b'5' { ERR_BIT } else { 0 };
        let mut q = s + 5;
        let mut lat: u64 = 0;
        while buf[q] != b'\n' {
            lat = lat * 10 + u64::from(buf[q] - b'0');
            q += 1;
        }
        recs.push((id as u64) << EP_SHIFT | hour << HOUR_SHIFT | err | lat);
        p = q + 1;
    }
    Parsed {
        names,
        recs,
        max_hour,
    }
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
struct SharedPtr(*mut u32);
unsafe impl Send for SharedPtr {}
unsafe impl Sync for SharedPtr {}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = args.get(1).map_or("/data", String::as_str);
    let out_dir = args.get(2).map_or("/out", String::as_str);

    let file = File::open(format!("{data_dir}/requests.log")).expect("open requests.log");
    let len = file.metadata().expect("stat").len() as usize;
    let mut out = File::create(format!("{out_dir}/summary.txt")).expect("create summary.txt");
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
    let parts: Vec<Parsed> = thread::scope(|sc| {
        let handles: Vec<_> = split_lines(buf, threads)
            .into_iter()
            .map(|chunk| sc.spawn(move || parse(chunk, base_hour)))
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // 2. Global endpoint ranks, in byte order (slice Ord puts a proper prefix first).
    let mut all: Vec<&[u8]> = parts.iter().flat_map(|p| p.names.iter().copied()).collect();
    all.sort_unstable();
    all.dedup();
    let rank: HashMap<&[u8], u32> = all
        .iter()
        .enumerate()
        .map(|(i, &n)| (n, i as u32))
        .collect();
    let n_eps = all.len();
    let n_hours = parts.iter().map(|p| p.max_hour).max().unwrap_or(0) as usize + 1;
    let n_groups = n_hours * n_eps;
    let group_of = |local: &[u32], r: u64| {
        ((r >> HOUR_SHIFT & HOUR_MASK) as usize) * n_eps + local[(r >> EP_SHIFT) as usize] as usize
    };

    // 3. Count per thread and group (latencies and errors).
    let locals: Vec<Vec<u32>> = parts
        .iter()
        .map(|p| p.names.iter().map(|n| rank[n]).collect())
        .collect();
    let mut counts: Vec<(Vec<u32>, Vec<u32>)> = thread::scope(|sc| {
        let handles: Vec<_> = parts
            .iter()
            .zip(&locals)
            .map(|(p, local)| {
                sc.spawn(move || {
                    let (mut cnt, mut err) = (vec![0u32; n_groups], vec![0u32; n_groups]);
                    for &r in &p.recs {
                        let g = group_of(local, r);
                        cnt[g] += 1;
                        err[g] += (r >> 32 & 1) as u32;
                    }
                    (cnt, err)
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });

    // Prefix sums: group start offsets, and each thread's write cursor within each group.
    let mut start = vec![0u32; n_groups + 1];
    let mut errors = vec![0u32; n_groups];
    let mut total = 0u32;
    for g in 0..n_groups {
        start[g] = total;
        for (cnt, err) in counts.iter_mut() {
            errors[g] += err[g];
            let c = cnt[g];
            cnt[g] = total;
            total += c;
        }
    }
    start[n_groups] = total;

    // 4. Scatter latencies so every group is contiguous.
    let mut lats = vec![0u32; total as usize];
    let dst = SharedPtr(lats.as_mut_ptr());
    thread::scope(|sc| {
        for ((p, local), (cursor, _)) in parts.iter().zip(&locals).zip(counts.iter_mut()) {
            sc.spawn(move || {
                let dst = dst;
                for &r in &p.recs {
                    let g = group_of(local, r);
                    // SAFETY: each (thread, group) owns its own disjoint index range.
                    unsafe { *dst.0.add(cursor[g] as usize) = (r & LAT_MASK) as u32 };
                    cursor[g] += 1;
                }
            });
        }
    });
    drop(counts);
    drop(parts);

    // 5. Percentiles and formatting, groups split into ranges of about equal latency count.
    let hour_text: Vec<String> = (0..n_hours as i64)
        .map(|h| {
            let abs = base_hour + h;
            let (y, m, d) = civil_from_days(abs.div_euclid(24));
            format!("{y:04}-{m:02}-{d:02}T{:02}:00:00Z ", abs.rem_euclid(24))
        })
        .collect();
    let mut bounds = vec![0usize];
    for t in 1..threads {
        let want = (u64::from(total) * t as u64 / threads as u64) as u32;
        let g = start.partition_point(|&s| s < want).min(n_groups);
        bounds.push(g.max(*bounds.last().unwrap()));
    }
    bounds.push(n_groups);
    let mut rest: &mut [u32] = &mut lats;
    let mut jobs = Vec::new();
    for w in bounds.windows(2) {
        let size = (start[w[1]] - start[w[0]]) as usize;
        let (mine, tail) = std::mem::take(&mut rest).split_at_mut(size);
        rest = tail;
        jobs.push((w[0], w[1], mine));
    }
    let outputs: Vec<Vec<u8>> = thread::scope(|sc| {
        let (start, errors, all, hour_text) = (&start, &errors, &all, &hour_text);
        let handles: Vec<_> = jobs
            .into_iter()
            .map(|(g0, g1, mine)| {
                sc.spawn(move || {
                    let mut o = Vec::with_capacity((g1 - g0) * 80);
                    let base = start[g0];
                    for g in g0..g1 {
                        let n = (start[g + 1] - start[g]) as usize;
                        if n == 0 {
                            continue;
                        }
                        let v =
                            &mut mine[(start[g] - base) as usize..(start[g + 1] - base) as usize];
                        // Nearest rank: 1-indexed ceil(p*n/100). Select the largest rank first,
                        // then search only below it for the smaller ones.
                        let r = |p: usize| (p * n + 99) / 100 - 1;
                        let (r50, r95, r99) = (r(50), r(95), r(99));
                        let p99 = *v.select_nth_unstable(r99).1;
                        let p95 = *v[..=r99].select_nth_unstable(r95).1;
                        let p50 = *v[..=r95].select_nth_unstable(r50).1;
                        let e = errors[g];
                        o.extend_from_slice(hour_text[g / all.len()].as_bytes());
                        o.extend_from_slice(all[g % all.len()]);
                        for x in [
                            n as u64,
                            u64::from(p50),
                            u64::from(p95),
                            u64::from(p99),
                            u64::from(e),
                        ] {
                            o.push(b' ');
                            push_u64(&mut o, x);
                        }
                        o.push(b' ');
                        push_u64(&mut o, u64::from(e) * 1_000_000 / n as u64);
                        o.push(b'\n');
                    }
                    o
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    for o in &outputs {
        out.write_all(o).expect("write summary.txt");
    }
}
