# Track A: performance log

Every step from the reference to the final version, each with the live-board run that measured it. The board identifies a run by its commit, so the commit column is the run ID. Local numbers come from a 2-core dev machine (warm cache, min of 11 runs) and only show direction; the board numbers are the ones that count.

| Step | Change | Board run (commit) | T (s) | M (MB) | Lines | K | Points |
|---|---|---|---|---|---|---|---|
| 0 | The reference, unchanged (scoring-box calibration) | | 32.05 | 228 | 56 | 1 | 0 |
| 1 | Rust rewrite: mmap, parallel hand parsing, counting-sort scatter, select_nth percentiles, parallel formatting | `7d3b2cd` | 0.074 | 61 | 300 | 21.06 | 66.175 |
| 2 | Branch-free parsing; counts kept per thread during the parse | `155b431` | 0.047 | 55 | 353 | 26.43 | 71.109 |

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
