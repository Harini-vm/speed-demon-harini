//! Speed Demon 2026, Track C (Join). README.md explains the approach.
//!
//! 1. Load regions.csv into a direct table: region_id -> country rank, where ranks follow
//!    the output order (country codes sorted, `_UNMATCHED` last).
//! 2. mmap records.csv and split it into one chunk per thread at record boundaries.
//! 3. Each thread parses its records by hand (spec section 2.1) and keeps, per country,
//!    the running sums and a bitset of the names it saw, by thread-local name id.
//! 4. Merge: give every distinct name a global id (sharded across threads), OR the
//!    per-thread bitsets into one per country, popcount, and write the sorted lines.

use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::*};
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

const BOM: &[u8] = b"\xEF\xBB\xBF";

/// Bit masks of `"`, `,` and LF in the 64 bytes at `buf[i..]` (bit k is byte i + k).
/// Bytes past the end of `buf` count as none of them.
#[inline(always)]
fn masks(buf: &[u8], i: usize) -> (u64, u64, u64) {
    if i + 64 > buf.len() {
        return masks_tail(buf, i);
    }
    let block = &buf[i..i + 64];
    #[cfg(target_feature = "avx2")]
    {
        use std::arch::x86_64::*;
        // SAFETY: AVX2 is enabled at compile time and `block` holds 64 bytes.
        unsafe {
            let (lo, hi) = (
                _mm256_loadu_si256(block.as_ptr().cast()),
                _mm256_loadu_si256(block.as_ptr().add(32).cast()),
            );
            let mask = |c: u8| {
                let c = _mm256_set1_epi8(c as i8);
                let l = _mm256_movemask_epi8(_mm256_cmpeq_epi8(lo, c)) as u32;
                let h = _mm256_movemask_epi8(_mm256_cmpeq_epi8(hi, c)) as u32;
                u64::from(l) | u64::from(h) << 32
            };
            (mask(b'"'), mask(b','), mask(b'\n'))
        }
    }
    #[cfg(not(target_feature = "avx2"))]
    {
        let mask = |c: u8| {
            block
                .iter()
                .enumerate()
                .fold(0u64, |m, (k, &x)| m | u64::from(x == c) << k)
        };
        (mask(b'"'), mask(b','), mask(b'\n'))
    }
}

/// `masks` for the last, partial block: padded with zeros, which match nothing.
#[cold]
fn masks_tail(buf: &[u8], i: usize) -> (u64, u64, u64) {
    let mut tmp = [0u8; 64];
    tmp[..buf.len() - i].copy_from_slice(&buf[i..]);
    masks(&tmp, 0)
}

/// Walks the field separators of whole records: the commas and LFs that are outside
/// quotes. Inside-quote bytes are a prefix XOR of the quote mask (`""` toggles twice, so
/// escapes need no special case). Each separator comes with the number of quotes before
/// it, so a field's quote count is a subtraction.
struct Separators<'a> {
    buf: &'a [u8],
    base: usize, // start of the current 64-byte block
    seps: u64,   // separators of the current block not yet returned
    quotes: u64, // quote mask of the current block
    before: u64, // quotes before the current block
    inside: u64, // all ones if the previous block ended inside quotes
}

impl<'a> Separators<'a> {
    fn new(buf: &'a [u8]) -> Self {
        let mut s = Separators {
            buf,
            base: 0,
            seps: 0,
            quotes: 0,
            before: 0,
            inside: 0,
        };
        s.load();
        s
    }

    fn load(&mut self) {
        let (q, c, l) = masks(self.buf, self.base);
        let mut x = q;
        for shift in [1, 2, 4, 8, 16, 32] {
            x ^= x << shift;
        }
        x ^= self.inside;
        self.inside = 0u64.wrapping_sub(x >> 63);
        self.seps = (c | l) & !x;
        self.quotes = q;
    }

    /// The next separator's position (`buf.len()` at the end) and the quotes before it.
    #[inline(always)]
    fn next(&mut self) -> (usize, u64) {
        while self.seps == 0 {
            if self.base + 64 >= self.buf.len() {
                return (
                    self.buf.len(),
                    self.before + u64::from(self.quotes.count_ones()),
                );
            }
            self.before += u64::from(self.quotes.count_ones());
            self.base += 64;
            self.load();
        }
        let k = self.seps.trailing_zeros();
        self.seps &= self.seps - 1;
        let below = self.quotes & ((1u64 << k) - 1);
        (
            self.base + k as usize,
            self.before + u64::from(below.count_ones()),
        )
    }
}

/// The value of a name or note field `buf[start..end]` that holds `quotes` quote bytes:
/// (raw bytes, value length). Unquoted text cannot hold a quote, so a field with quotes is
/// a quoted one: its raw bytes keep their `""` escapes, which is fine as a key because
/// escaping is canonical, and every `""` counts one byte. An empty quoted and an empty
/// unquoted field both give the empty value.
#[inline(always)]
fn text(buf: &[u8], start: usize, end: usize, quotes: u64) -> (&[u8], u64) {
    if quotes == 0 {
        (&buf[start..end], (end - start) as u64)
    } else {
        (
            &buf[start + 1..end - 1],
            (end - start - 2) as u64 - (quotes - 2) / 2,
        )
    }
}

/// An unsigned decimal made of all of `b`.
#[inline(always)]
fn number(b: &[u8]) -> u64 {
    b.iter().fold(0, |v, &c| v * 10 + u64::from(c - b'0'))
}

/// Word `i` of a byte string, little-endian, zero-padded past its end.
#[inline(always)]
fn word(s: &[u8], i: usize) -> u64 {
    let rest = &s[i * 8..];
    if rest.len() >= 8 {
        u64::from_le_bytes(rest[..8].try_into().unwrap())
    } else {
        let mut t = [0u8; 8];
        t[..rest.len()].copy_from_slice(rest);
        u64::from_le_bytes(t)
    }
}

fn hash(s: &[u8]) -> u64 {
    const K: u64 = 0x9E37_79B9_7F4A_7C15;
    (0..s.len().div_ceil(8))
        .fold(s.len() as u64, |h, i| {
            (h ^ word(s, i)).wrapping_mul(K).rotate_left(29)
        })
        .wrapping_mul(K)
}

/// The distinct names, shared by all threads, each with a dense id.
///
/// A name is stored once in `words` as [id][len][name, zero-padded] and found through an
/// open-addressing table whose slots hold the top 32 bits of the name's hash and the
/// record's word offset. It is one copy of about 100,000 names, small enough for L3, and
/// after warm-up nothing writes to it, so its cache lines stay shared by all cores. New
/// names are added without locks: the record is written at a bump-allocated offset, then
/// published with a compare-and-swap on the empty slot (a loser's record stays unused).
struct Names<'a> {
    slots: &'a [AtomicU64],
    words: &'a [AtomicU64],
    used: AtomicUsize,
    ids: AtomicUsize,
}

/// Views zeroed u64 storage as atomics. Zeroed allocations are lazily committed, so a
/// generous capacity costs only the pages that are written.
fn atomics(v: &mut [u64]) -> &[AtomicU64] {
    // SAFETY: AtomicU64 has the same size and alignment as u64, and `v` is borrowed
    // mutably for the lifetime of the result, so nothing else accesses it.
    unsafe { &*(v as *mut [u64] as *const [AtomicU64]) }
}

impl Names<'_> {
    /// The id of `name`, adding it if it is new.
    fn id(&self, name: &[u8]) -> u32 {
        let h = hash(name);
        let n = name.len().div_ceil(8);
        let (mask, tag) = (self.slots.len() - 1, h & !0xFFFF_FFFF);
        let mut s = h as usize & mask;
        loop {
            let mut e = self.slots[s].load(Acquire);
            if e == 0 {
                let w = self.used.fetch_add(2 + n, Relaxed);
                assert!(w + 2 + n <= self.words.len(), "name storage full");
                self.words[w].store(self.ids.fetch_add(1, Relaxed) as u64, Relaxed);
                self.words[w + 1].store(name.len() as u64, Relaxed);
                for i in 0..n {
                    self.words[w + 2 + i].store(word(name, i), Relaxed);
                }
                match self.slots[s].compare_exchange(0, tag | (w as u64 + 1), Release, Acquire) {
                    Ok(_) => return self.words[w].load(Relaxed) as u32,
                    Err(winner) => e = winner, // another thread filled this slot first
                }
            }
            let w = (e & 0xFFFF_FFFF) as usize - 1;
            if e & !0xFFFF_FFFF == tag
                && self.words[w + 1].load(Relaxed) == name.len() as u64
                && (0..n).all(|i| self.words[w + 2 + i].load(Relaxed) == word(name, i))
            {
                return self.words[w].load(Relaxed) as u32;
            }
            s = (s + 1) & mask;
            assert!(s != h as usize & mask, "name table full");
        }
    }
}

/// Running aggregates for one output line.
#[derive(Clone, Copy)]
struct Agg {
    count: u64,
    id_sum: u64,
    amount_sum: i64,
    min: i64,
    max: i64,
    name_bytes: u64,
    note_bytes: u64,
}

impl Agg {
    const EMPTY: Agg = Agg {
        count: 0,
        id_sum: 0,
        amount_sum: 0,
        min: i64::MAX,
        max: i64::MIN,
        name_bytes: 0,
        note_bytes: 0,
    };

    fn merge(&mut self, o: &Agg) {
        self.count += o.count;
        self.id_sum += o.id_sum;
        self.amount_sum += o.amount_sum;
        self.min = self.min.min(o.min);
        self.max = self.max.max(o.max);
        self.name_bytes += o.name_bytes;
        self.note_bytes += o.note_bytes;
    }
}

/// Parses whole records (`buf` starts at a record and ends after a terminator or at EOF)
/// into per-country aggregates and per-country lists of name ids.
fn parse(buf: &[u8], country_of: &[u8], names: &Names, groups: usize) -> (Vec<Agg>, Vec<Vec<u32>>) {
    let mut aggs = vec![Agg::EMPTY; groups];
    let mut name_ids = vec![Vec::new(); groups];
    let mut seps = Separators::new(buf);
    let mut start = 0;
    while start < buf.len() {
        let (e0, q0) = seps.next();
        let (e1, q1) = seps.next();
        let (e2, q2) = seps.next();
        let (e3, _) = seps.next();
        let (e4, _) = seps.next();
        // id, after the byte order mark a record may start with (it is not data)
        let id = number(&buf[start + if buf[start] == BOM[0] { 3 } else { 0 }..e0]);
        let (name, name_len) = text(buf, e0 + 1, e1, q1 - q0);
        let (_, note_len) = text(buf, e1 + 1, e2, q2 - q1);
        let region = number(&buf[e2 + 1..e3]) as usize;
        // amount: -?digits.dd, then the CR of a CRLF terminator if there is one
        let mut amount_text = &buf[e3 + 1..e4];
        if amount_text.last() == Some(&b'\r') {
            amount_text = &amount_text[..amount_text.len() - 1];
        }
        let negative = amount_text[0] == b'-';
        let digits = &amount_text[usize::from(negative)..];
        let cents = number(&digits[..digits.len() - 3]) as i64 * 100
            + number(&digits[digits.len() - 2..]) as i64;
        let amount = if negative { -cents } else { cents };
        start = e4 + 1;

        let g = country_of.get(region).map_or(groups - 1, |&c| c as usize);
        let a = &mut aggs[g];
        a.count += 1;
        a.id_sum += id;
        a.amount_sum += amount;
        a.min = a.min.min(amount);
        a.max = a.max.max(amount);
        a.name_bytes += name_len;
        a.note_bytes += note_len;
        name_ids[g].push(names.id(name));
    }
    (aggs, name_ids)
}

/// Splits the records into `n` pieces, each starting at a record. Per spec section 2.2 a
/// note's lines are lowercase words and names never hold an LF, so an LF followed by a
/// digit or a BOM is always a record terminator, never data inside quotes.
fn split_records(buf: &[u8], n: usize) -> Vec<&[u8]> {
    let mut cuts = vec![0];
    for i in 1..n {
        let mut c = (buf.len() * i / n).max(*cuts.last().unwrap());
        while c < buf.len()
            && !(buf[c - 1] == b'\n' && (buf[c].is_ascii_digit() || buf[c] == BOM[0]))
        {
            c += 1;
        }
        cuts.push(c);
    }
    cuts.push(buf.len());
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

/// Cents as `-?\d+\.\d\d`: -525 is "-5.25", -5 is "-0.05", 0 is "0.00".
fn money(c: i64) -> String {
    let sign = if c < 0 { "-" } else { "" };
    format!(
        "{sign}{}.{:02}",
        c.unsigned_abs() / 100,
        c.unsigned_abs() % 100
    )
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = args.get(1).map_or("/data", String::as_str);
    let out_dir = args.get(2).map_or("/out", String::as_str);
    let threads = thread::available_parallelism().map_or(1, |n| n.get());

    // regions.csv: "region_id,country" lines after a header, ids ascending, codes [A-Z]{3}.
    // A code's base-26 key sorts like the code itself, so walking the 26^3 table in key
    // order hands out ranks in output order with no sort.
    let regions = map(&format!("{data_dir}/regions.csv"));
    let lines = || {
        regions
            .split(|&c| c == b'\n')
            .skip(1)
            .filter(|l| l.len() > 4)
    };
    let key = |l: &[u8]| {
        l[l.len() - 3..]
            .iter()
            .fold(0, |k, &c| k * 26 + usize::from(c - b'A'))
    };
    let mut rank = vec![u8::MAX; 26 * 26 * 26];
    lines().for_each(|l| rank[key(l)] = 0);
    let mut codes: Vec<String> = Vec::new();
    for (k, r) in rank.iter_mut().enumerate().filter(|(_, r)| **r == 0) {
        assert!(codes.len() < 255, "more countries than a u8 rank holds");
        *r = codes.len() as u8;
        codes.push(
            [k / 676, k / 26 % 26, k % 26]
                .iter()
                .map(|&c| (b'A' + c as u8) as char)
                .collect(),
        );
    }
    let groups = codes.len() + 1; // the last group is _UNMATCHED
    let id = |l: &[u8]| number(&l[..l.len() - 4]) as usize;
    let mut country_of = vec![(groups - 1) as u8; lines().last().map_or(0, |l| id(l) + 1)];
    lines().for_each(|l| country_of[id(l)] = rank[key(l)]);

    // records.csv: skip an optional BOM and the header line, then parse in parallel.
    let records = map(&format!("{data_dir}/records.csv"));
    let header_end = records
        .iter()
        .position(|&c| c == b'\n')
        .map_or(records.len(), |i| i + 1);
    let body = &records[header_end..];
    // Shared name storage: 2^19 slots (about 100,000 names fill under a fifth of them) and
    // room for every record to bring a new name, of which only the used pages are touched.
    let mut slot_mem = vec![0u64; 1 << 19];
    let mut word_mem = vec![0u64; body.len() / 2 + 1024];
    let names = Names {
        slots: atomics(&mut slot_mem),
        words: atomics(&mut word_mem),
        used: AtomicUsize::new(0),
        ids: AtomicUsize::new(0),
    };
    let (country_of, names) = (&country_of, &names);
    let parts = par_map(split_records(body, threads), |chunk| {
        parse(chunk, country_of, names, groups)
    });

    // Distinct names per country, one country at a time per worker. `last[id]` is the last
    // country that counted name `id`, so it never needs clearing between countries.
    let (next, n_ids) = (AtomicUsize::new(0), names.ids.load(Relaxed));
    let mut distinct = vec![0usize; groups];
    let found = par_map(vec![(); threads], |()| {
        let (mut last, mut out) = (vec![u16::MAX; n_ids], Vec::new());
        loop {
            let g = next.fetch_add(1, Relaxed);
            if g >= groups {
                return out;
            }
            let ids = parts.iter().flat_map(|p| &p.1[g]).map(|&id| id as usize);
            out.push((
                g,
                ids.filter(|&id| std::mem::replace(&mut last[id], g as u16) != g as u16)
                    .count(),
            ));
        }
    });
    found
        .into_iter()
        .flatten()
        .for_each(|(g, n)| distinct[g] = n);
    let mut text = String::new();
    for g in 0..groups {
        let mut a = Agg::EMPTY;
        parts.iter().for_each(|p| a.merge(&p.0[g]));
        if a.count == 0 {
            continue;
        }
        let key = codes.get(g).map_or("_UNMATCHED", String::as_str);
        text += &format!(
            "{key} {} {} {} {} {} {} {} {}\n",
            a.count,
            a.id_sum,
            money(a.amount_sum),
            money(a.min),
            money(a.max),
            distinct[g],
            a.name_bytes,
            a.note_bytes
        );
    }
    File::create(format!("{out_dir}/aggregates.txt"))
        .and_then(|mut f| f.write_all(text.as_bytes()))
        .expect("write aggregates.txt");
}
