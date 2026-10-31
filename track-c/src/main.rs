//! Speed Demon 2026, Track C (Join). README.md explains the approach.
//!
//! 1. Load regions.csv into a direct table: region_id -> country rank, where ranks follow
//!    the output order (country codes sorted, `_UNMATCHED` last).
//! 2. mmap records.csv and split it into one chunk per thread at record boundaries.
//! 3. Each thread parses its records by hand (spec section 2.1), keeps the running sums
//!    per country, and gets each name's dense id from one shared lock-free table.
//! 4. The file is parsed in rounds. After each, one worker per country sets the bits of
//!    that round's name ids in the country's bitmap, so the id lists stay small.
//! 5. Merge the sums, popcount the bitmaps, and write the sorted lines.

use std::fs::File;
use std::io::Write;
use std::os::fd::AsRawFd;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::*};
use std::sync::Mutex;
use std::thread;

extern "C" {
    fn mmap(addr: *mut u8, len: usize, prot: i32, flags: i32, fd: i32, off: i64) -> *mut u8;
    fn mallopt(param: i32, value: i32) -> i32;
    fn prctl(option: i32, ...) -> i32;
    fn madvise(addr: *mut u8, len: usize, advice: i32) -> i32;
}

/// Commits the pages under `v` in one call (MADV_POPULATE_WRITE = 23). Twelve threads
/// first-touching a table fault it in page by page and contend in the kernel while doing
/// so; one call up front is several times cheaper. Best effort: on a kernel without it,
/// the pages simply fault in as before.
fn populate<T>(v: &mut [T]) {
    let at = v.as_mut_ptr() as usize;
    let page = at & !4095; // madvise wants a page-aligned start
                           // SAFETY: the range covers only `v`'s own memory and the start of its page, all of it
                           // mapped; populating leaves the (zeroed) contents as they are.
    unsafe { madvise(page as *mut u8, at - page + std::mem::size_of_val(v), 23) };
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

/// Word `i` of a name, little-endian, zero-padded past its end. The load may read up to
/// seven bytes past the name, which are always in the mapped file: a name is followed in
/// its record by at least a quote or comma, a note, a region id and an amount (`,,1,0.00`).
#[inline(always)]
fn word(s: &[u8], i: usize) -> u64 {
    let left = s.len() - i * 8;
    // SAFETY: bytes s[i * 8..i * 8 + 8] are inside the mapping, as above.
    let w = unsafe { std::ptr::read_unaligned(s.as_ptr().add(i * 8).cast::<u64>()) };
    if left >= 8 {
        w
    } else {
        w & ((1 << (left * 8)) - 1)
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
/// An open-addressing table of 32-byte slots, so one lookup reads one cache line. Word 0
/// is 0 for an empty slot, BUSY while a thread fills it, else the top 24 bits of the
/// name's hash, the length (capped at 255) and id + 1. A name of up to 24 bytes sits in
/// words 1 to 3; a longer one in `long` as [length][name], with its offset in word 1.
/// After warm-up nothing writes to the table, so its cache lines stay shared by all
/// cores. A new name is added without a lock: a compare-and-swap claims the slot.
struct Names<'a> {
    slots: &'a [AtomicU64],
    long: &'a [AtomicU64],
    used: AtomicUsize,
    ids: AtomicUsize,
}

const BUSY: u64 = u64::MAX;
const INLINE: usize = 24;

/// Views zeroed u64 storage as atomics. Zeroed allocations are lazily committed, so a
/// generous capacity costs only the pages that are written.
fn atomics(v: &mut [u64]) -> &[AtomicU64] {
    // SAFETY: AtomicU64 has the same size and alignment as u64, and `v` is borrowed
    // mutably for the lifetime of the result, so nothing else accesses it.
    unsafe { &*(v as *mut [u64] as *const [AtomicU64]) }
}

impl Names<'_> {
    /// The id of `name`, adding it if it is new.
    /// Starts loading the slot where a name with hash `h` is looked up first.
    #[inline(always)]
    fn prefetch(&self, h: u64) {
        #[cfg(target_arch = "x86_64")]
        // SAFETY: a prefetch is only a hint, and the address is inside `slots`.
        unsafe {
            use std::arch::x86_64::*;
            let s = h as usize & (self.slots.len() / 4 - 1);
            _mm_prefetch(self.slots.as_ptr().add(s * 4).cast(), _MM_HINT_T0);
        }
    }

    /// The id of `name`, whose hash is `h`, adding the name if it is new.
    fn id(&self, name: &[u8], h: u64) -> u32 {
        let n = name.len().div_ceil(8);
        let key = (h & !0xFF_FFFF_FFFF) | (name.len().min(255) as u64) << 32;
        let mask = self.slots.len() / 4 - 1;
        let mut s = h as usize & mask;
        loop {
            let slot = &self.slots[s * 4..s * 4 + 4];
            let mut e = slot[0].load(Acquire);
            if e == 0 {
                match slot[0].compare_exchange(0, BUSY, Acquire, Acquire) {
                    Ok(_) => return self.fill(slot, name, key),
                    Err(now) => e = now, // another thread claimed this slot first
                }
            }
            while e == BUSY {
                std::hint::spin_loop();
                e = slot[0].load(Acquire);
            }
            let words = |w: &[AtomicU64]| (0..n).all(|i| w[i].load(Relaxed) == word(name, i));
            if e & !0xFFFF_FFFF == key
                && if name.len() <= INLINE {
                    words(&slot[1..])
                } else {
                    let w = slot[1].load(Relaxed) as usize;
                    self.long[w].load(Relaxed) == name.len() as u64 && words(&self.long[w + 1..])
                }
            {
                return e as u32 - 1;
            }
            s = (s + 1) & mask;
            assert!(s != h as usize & mask, "name table full");
        }
    }

    /// Stores `name` in the slot this thread claimed, then publishes it with its new id.
    fn fill(&self, slot: &[AtomicU64], name: &[u8], key: u64) -> u32 {
        let n = name.len().div_ceil(8);
        let dst = if name.len() <= INLINE {
            &slot[1..]
        } else {
            let w = self.used.fetch_add(1 + n, Relaxed);
            assert!(w + 1 + n <= self.long.len(), "name storage full");
            self.long[w].store(name.len() as u64, Relaxed);
            slot[1].store(w as u64, Relaxed);
            &self.long[w + 1..]
        };
        (0..n).for_each(|i| dst[i].store(word(name, i), Relaxed));
        let id = self.ids.fetch_add(1, Relaxed) as u32;
        slot[0].store(key | u64::from(id + 1), Release);
        id
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

/// One parse thread's state, carried from round to round: aggregates per country, and
/// the name ids of the current round per country.
type State = (Vec<Agg>, Vec<Vec<u32>>);

/// Parses whole records (`buf` starts at a record and ends after a terminator or at EOF)
/// into the aggregates and this round's name id lists.
fn parse(buf: &[u8], country_of: &[u8], names: &Names, (mut aggs, mut name_ids): State) -> State {
    name_ids.iter_mut().for_each(Vec::clear); // the last round's ids are in the bitmaps
    let groups = aggs.len();
    let mut seps = Separators::new(buf);
    let mut pending: [Option<(&[u8], u64, usize)>; AHEAD] = [None; AHEAD];
    let mut count = 0;
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
        // The lookup runs AHEAD records late: this record's slot is prefetched now, and by
        // the time its lookup runs the slot is already in the cache.
        let h = hash(name);
        names.prefetch(h);
        if let Some((name, h, g)) = pending[count % AHEAD].replace((name, h, g)) {
            name_ids[g].push(names.id(name, h));
        }
        count += 1;
    }
    for (name, h, g) in pending.into_iter().flatten() {
        name_ids[g].push(names.id(name, h));
    }
    (aggs, name_ids)
}

/// How many records a name lookup trails its slot's prefetch.
const AHEAD: usize = 4;

/// Splits the records into `n` pieces, each starting at a record. Per spec section 2.2 a
/// note's lines are lowercase words and names never hold an LF, so an LF followed by a
/// digit or a BOM is always a record terminator, never data inside quotes.
fn split_records(buf: &[u8], n: usize) -> Vec<&[u8]> {
    let mut cuts = vec![0];
    for i in 1..n {
        // At least 1, so buf[c - 1] exists when buf is shorter than n bytes.
        let mut c = (buf.len() * i / n)
            .max(*cuts.last().unwrap())
            .max(1)
            .min(buf.len());
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

/// Parse rounds: more rounds keep the id lists smaller, at one more pause per round.
const ROUNDS: usize = 4;

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let data_dir = args.get(1).map_or("/data", String::as_str);
    let out_dir = args.get(2).map_or("/out", String::as_str);
    let threads = thread::available_parallelism().map_or(1, |n| n.get());
    // Serve every allocation of 128 KiB or more with its own mmap (M_MMAP_THRESHOLD = -3).
    // Left to itself, glibc raises that threshold each time such a block is freed, after
    // which large blocks come from per-thread arenas that keep their pages when freed.
    // SAFETY: mallopt only tunes the allocator; it is called before any thread starts.
    unsafe { mallopt(-3, 128 * 1024) };
    // No transparent huge pages for this process (PR_SET_THP_DISABLE = 41): a 2 MB page
    // commits all of its memory on the first write, which costs most where memory is
    // only partly touched.
    // SAFETY: prctl with this option only sets a flag on the calling process.
    unsafe { prctl(41, 1u64, 0u64, 0u64, 0u64) };

    // regions.csv: "region_id,country" lines after a header, ids ascending, codes [A-Z]{3}.
    // A code's base-26 key sorts like the code itself, so walking the 26^3 table in key
    // order hands out ranks in output order with no sort.
    let regions = map(&format!("{data_dir}/regions.csv"));
    let body = &regions[regions
        .iter()
        .position(|&c| c == b'\n')
        .map_or(regions.len(), |i| i + 1)..];
    // Parse the lines in parallel into (region id, code key) pairs.
    let pairs: Vec<(usize, usize)> = par_map(split_records(body, threads), |chunk| {
        chunk
            .split(|&c| c == b'\n')
            .filter(|l| l.len() > 4)
            .map(|l| {
                let key = l[l.len() - 3..]
                    .iter()
                    .fold(0, |k, &c| k * 26 + usize::from(c - b'A'));
                (number(&l[..l.len() - 4]) as usize, key)
            })
            .collect::<Vec<_>>()
    })
    .concat();
    let mut rank = vec![u8::MAX; 26 * 26 * 26];
    pairs.iter().for_each(|&(_, k)| rank[k] = 0);
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
    let mut country_of = vec![(groups - 1) as u8; pairs.last().map_or(0, |p| p.0 + 1)];
    pairs.iter().for_each(|&(id, k)| country_of[id] = rank[k]);
    drop(pairs);

    // records.csv: skip an optional BOM and the header line, then parse in parallel.
    let records = map(&format!("{data_dir}/records.csv"));
    let header_end = records
        .iter()
        .position(|&c| c == b'\n')
        .map_or(records.len(), |i| i + 1);
    let body = &records[header_end..];
    // Shared name storage: 2^18 slots (about 100,000 names fill under two fifths of them),
    // and room for every record to bring a long name, of which only used pages are touched.
    let mut slot_mem = vec![0u64; 4 << 18];
    populate(&mut slot_mem); // every page of it is written while parsing
    let mut long_mem = vec![0u64; body.len() / 2 + 1024];
    // Per-country name bitmaps, each grown to the ids handed out so far when it is folded:
    // no row is sized for ids that never come.
    let mut seen: Vec<Vec<u64>> = vec![Vec::new(); groups];
    let names = Names {
        slots: atomics(&mut slot_mem),
        long: atomics(&mut long_mem),
        used: AtomicUsize::new(0),
        ids: AtomicUsize::new(0),
    };
    // The file is parsed in ROUNDS rounds of one chunk per thread. After each round, one
    // worker per country moves that round's ids into the country's bitmap. The id lists
    // only ever hold one round's worth, a quarter of the file's 3.3 million ids.
    let (country_of, names) = (&country_of, &names);
    let mut states: Vec<State> = (0..threads)
        .map(|_| (vec![Agg::EMPTY; groups], vec![Vec::new(); groups]))
        .collect();
    for round in split_records(body, threads * ROUNDS).chunks(threads) {
        let jobs: Vec<_> = round.iter().zip(states).collect();
        states = par_map(jobs, |(chunk, st)| parse(chunk, country_of, names, st));
        let (rows, words) = (
            Mutex::new(seen.iter_mut().enumerate()),
            names.ids.load(Relaxed) / 64 + 1,
        );
        par_map(vec![(); threads], |()| loop {
            let Some((g, row)) = rows.lock().unwrap().next() else {
                break;
            };
            row.resize(words, 0);
            for &id in states.iter().flat_map(|st| &st.1[g]) {
                row[id as usize / 64] |= 1 << (id % 64);
            }
        });
    }
    let distinct: Vec<u32> = seen
        .iter()
        .map(|row| row.iter().map(|w| w.count_ones()).sum())
        .collect();
    let mut text = String::new();
    for g in 0..groups {
        let mut a = Agg::EMPTY;
        states.iter().for_each(|st| a.merge(&st.0[g]));
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
