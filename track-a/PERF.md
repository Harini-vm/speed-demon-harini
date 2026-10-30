# Track A: performance log

Every step from the reference to the final version, each with the live-board run that measured it. The board identifies a run by its commit, so the commit column is the run ID. Local numbers come from a 2-core dev machine (warm cache, min of 11 runs) and only show direction; the board numbers are the ones that count.

| Step | Change | Board run (commit) | T (s) | M (MB) | Lines | K | Points |
|---|---|---|---|---|---|---|---|
| 0 | The reference, unchanged (scoring-box calibration) | | 32.05 | 228 | 56 | 1 | 0 |
| 1 | Rust rewrite: mmap, parallel hand parsing, counting-sort scatter, select_nth percentiles, parallel formatting | `7d3b2cd` | 0.074 | 61 | 300 | 21.06 | 66.175 |
| 2 | Branch-free parsing; counts kept per thread during the parse | `155b431` | 0.047 | 55 | 353 | 26.43 | 71.109 |
| 3 | The serial steps between phases made parallel | _fill in_ | | | | | |
| 4 | Smaller source: the same logic in fewer lines | _fill in_ | | | | | |

## Step 1

The Python reference spends most of its time in `datetime.strptime`. Here the hour comes from fixed byte offsets, every thread parses its own slice of the mapped file, and percentiles use selection instead of a full sort.

Board breakdown: T 433x faster than the reference contributes 20.8 to K, M 3.7x smaller contributes 1.30, and 300 lines (5.4x the reference) costs 0.78. The mapped input was **not** charged to M, which confirmed the mmap design.

Local phase timings showed where the time went: parsing was about 77% of the run (150 of 195 ms). A parallel byte sum over the same file takes 27 ms and touching every page only 4 ms, so the parse loop itself was the problem, not page faults.

## Step 2

Parsing, in order of what was tried:

1. **Hour cache.** Neighbouring lines almost always share `YYYY-MM-DDTHH`, so two 8-byte compares replace the calendar maths on most lines.
2. **Word-at-a-time endpoint scan and hash.** The FNV byte loop became a SWAR space search plus a hash of the first and last eight bytes and the length. Local: 195 ms to about 150 ms.
3. **Fewer unpredictable branches.** The method's length came from a 7-way match, the latency from a digit loop of 1 to 10 iterations, and the endpoint search from a variable-length loop. Each misprediction costs about 15 to 20 cycles, several per line. Now the method's space is found in one word, the endpoint end with one 32-byte AVX2 compare in the common case, and the latency's last 8 digits are converted in a few multiplies (SWAR) after finding the LF in at most two words.
   - Tried and measured: skipping the endpoint byte comparison entirely made no measurable difference, so the hash table's memcmp was never the cost.

Merge and memory:

4. **Counts during the parse.** Each thread counts lines and 5xx per (hour, local endpoint) cell while parsing, in a zero-allocated table whose untouched pages are never committed. This removes a full pass over the records and the per-thread count arrays sized to every group (about 13 MB at 12 threads). The cell counts become each thread's write cursors after one prefix sum in output order, and the scatter step no longer remaps endpoint ids per line.

Local result, full seed 1: 179 ms to 118 ms (min of 11). Anonymous memory forced to 12 threads: 45 MB to 39 MB peak. The board's M also counts the 10 MB output file in the page cache, so expect about 55 MB there. The board measured exactly 55 MB, and T fell from 74 ms to 47 ms (1.57x, close to the 1.5x seen locally).

Correctness after this step: identical SHA-256 to the reference on tiny seeds 1 to 12, sample seeds 1 to 7, full seeds 1 and 42, the spec's worked example and an empty input, and on samples run with 1, 3, 7, 12 and 13 threads (chunk boundaries move with the thread count).

## Step 3

The box runs 12 threads, the dev machine 2, so a step that runs on one thread costs relatively more on the box than it does here. I built a copy forced to 12 threads and timed each phase. Parallel phases look inflated on 2 cores, but single-threaded ones show their real cost, and it grows with the thread count:

| Serial step | Cost at 12 threads | What it did |
|---|---|---|
| Endpoint ranks | ~5 ms | sorted about 22,000 names (1,842 per thread x 12), then std `HashMap` lookups |
| Prefix sum | ~5.5 ms | 132,000 groups x 12 threads, one cell at a time |
| Output write | ~2.5 ms | 10 MB copied by one thread |

A model of the box run (parse about 20 ms, scatter and percentiles about 8 ms, plus these 13 ms) matched the measured 46 ms.

- **Ranks.** Each parse thread sorts its own names at the end of its parse, and main does a linear dedup-merge of the sorted lists: 1.7 ms. Each thread maps its local ids to ranks by binary search, in parallel.
- **Prefix sum by hour.** Each hour's total line count first (a sequential read), then a running sum over 72 hours, then every hour in parallel sets its groups' starts and turns each thread's cell counts into cursors. Cells belong to exactly one hour, so the workers never touch the same cell.
  - Tried and fixed: the first version wrote every cell, including empty ones. Writing a zero cell in a page nobody had touched commits that page, and that alone took the step to 34 ms. Skipping empty cells, as the serial version did, brought it back.
- **Output.** Each formatting thread's buffer goes straight to its offset in the file with `write_all_at`, all at once.

Correctness: identical SHA-256 to the reference on tiny seeds 1 to 12, sample seeds 1 to 7, full seeds 1 and 42, the worked example, an empty input, and with 1, 3, 7, 12 and 13 threads.

## Step 4: source size

Source size is 15% of the exponent, and this file was 398 code lines against the reference's few dozen. Same algorithms, same phases:

The parallel sections shared one pattern, written out in full five times: spawn a thread per chunk, join, collect. A 6-line `par_map(items, f)` helper replaces each with one line. Endpoint ranks now use a stable sort over the concatenated per-thread sorted lists, which merges sorted runs in linear passes, instead of a hand-written merge loop. The 8-byte SWAR tier of `find_space` only ever handled the last bytes before a chunk boundary, which the plain byte search covers.

398 to 344 code lines. Run time unchanged in alternating runs against the previous build. Correctness: identical SHA-256 to the reference on the full seed battery, the worked example, and with 1, 3 and 13 threads.
