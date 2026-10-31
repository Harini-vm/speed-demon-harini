# Track C: performance log

Every step from the reference to the final version, each with the live-board run that measured it. The board identifies a run by its commit. Local numbers come from a 2-core dev machine (full seed 1, warm cache, min of 7 runs) and only show direction; the board numbers are the ones that count.

| Step | Change | Board run (commit) | T (s) | M (MB) | Lines | K | Points |
|---|---|---|---|---|---|---|---|
| 0 | The reference, unchanged (scoring-box calibration) | | 14.44 | 367 | 108 | 1 | 0 |
| 1 | Rust: structural (AVX2 bitmask) CSV parsing, array join, shared lock-free name sets | `e8571e5` | 0.092 | 41 | 416 | 15.88 | 60.036 |
| 2 | Country marks moved out of the shared table into private per-thread lists; regions read with plain iterators | `5e6ffc1` | 0.082 | 41 | 411 | 16.83 | 61.302 |
| 3 | Smaller source: the same logic in fewer lines | _fill in_ | | | | | |
| 4 | Name ids folded into per-country bitmaps after each of four parse rounds; one-line name slots | `9157dea` | 0.064 | 47 | 416 | 18.57 | 63.442 |
| 5 | regions.csv parsed in parallel in one pass; name slots prefetched four records ahead | `7c515ca` | 0.067 | 47 | 444 | 17.90 | 62.638 |
| 6 | Fixed 128 KiB mmap threshold; name bitmaps grown to the ids in use | `16b8be5` | 0.060 | 25 | 448 | 21.53 | 66.652 |
| 7 | Experiment: transparent huge pages off for the process | _fill in_ | | | | | |
| 8 | Name table committed in one call before the parse | _fill in_ | | | | | |

## Step 1, built in measured stages (local, 2 threads)

| Stage | Time | Notes |
|---|---|---|
| Hand-written field-by-field parser, per-thread name interning and bitsets | 630 ms | correct first time on tiny, sample and full |
| Interner keys copied into a compact arena, hash tags in the slots, u8 region table | 355 ms | elimination tests: interning was 40% of the time, because each key comparison read the 200 MB input at random |
| Structural parser (quote/comma/LF bitmasks, prefix-XOR quote state) | 389 ms | slower at first: see below |
| Zero-fill of the 64-byte pad buffer moved to a cold path | 389 ms | |
| Interner record in one place (id, length, bytes); direct 26³ table for region codes | 356 ms | regions load 34 ms to 4 ms |
| One name table shared by all threads, country masks stored with each name | 323 ms | |

### What the elimination tests showed

The structural parser looked like a regression, but removing pieces one at a time told a different story. Without the name bookkeeping the whole run took 112 ms, and with it 356 ms: parsing had become cheap, and the cost was cache misses in the per-thread name dictionaries. Each thread had about 6 to 7 MB of dictionary, arena and bitsets. At 12 threads on the scoring box, that is about 80 MB of randomly accessed structures against a 24 MB L3 (this dev machine has 260 MB of L3, so it understates the effect). The shared table brings that down to one copy of about 7 MB.

Single eliminations did not add up (interning alone about 55 ms, bitsets alone about 34 ms, together about 245 ms). The structures were competing for the same cache, so removing either one helped the other.

### Memory

Peak RSS is about 210 MB locally, of which about 204 MB is the mapped input, which the box's harness does not charge. The name storage is sized for the worst case (every record a new name) but allocated zeroed, so only the pages actually used are committed.

Board, step 1: T 157x faster contributes 12.53 to K, M 9x smaller contributes 1.55, and 416 lines (3.9x the reference) costs 0.82. Local 323 ms on 2 cores became 92 ms on 12 threads, only 3.5x for 6x the cores: the next step looks at the shared name table, whose country masks every thread writes.

## Step 2

Parsing alone is about 110 ms locally, about 25 ms of the box's 92, so the box spent most of its time on names. Every thread wrote country bits into the shared name records, and each write invalidates that cache line in every other core's cache, so the dictionary's lines bounced between cores. Two local cores cannot show that; twelve can.

Now the shared table is only written when a name is first seen, and after warm-up it is read-only, so its lines stay shared by all cores. Each record carries a dense id, and each thread appends the id to a private per-country list: a sequential write nobody else touches. At the end one worker per country counts distinct ids, keeping in `last[id]` the last country that counted that name, so the array never needs clearing between countries.

Local: 323 ms to 315 ms, which is expected on two cores; the effect is on the box. The regions file is now read with plain iterators instead of the AVX2 search, which only that small file used: 416 to 402 lines.

Board, step 2: T 92 to 82 ms with 12 threads, the effect the 2-core machine could not show, and 416 to 411 lines.

## Step 3: source size

Source size is 15% of the exponent, and this file was 402 code lines against the reference's few dozen. Same algorithms, same phases:

Region codes need no sort: a code's base-26 key orders like the code, so walking the 26^3 table in key order hands out ranks in output order. `par_map` for the parse threads and the distinct-count workers.

402 to 385 code lines. Run time unchanged in alternating runs against the previous build. Correctness: identical SHA-256 to the reference on the full seed battery, the worked example, and with 1, 3 and 13 threads.

## Step 4: memory

I sampled anonymous memory every millisecond with a copy forced to 12 threads: the peak was 29 MB, and it grew steadily through the parse. The per-country id lists were most of it. Every record appends a 4-byte name id, so 3.3 million ids, about 13 MB, plus up to the same again in unused capacity from doubling.

The file is now parsed in four rounds, each one chunk per thread. After each round, one worker per country sets the bits of that round's ids in the country's bitmap of 2^19 bits, and the lists are cleared for the next round, keeping their capacity. The bitmaps are zero-allocated, and ids are dense, so only about 12.5 KB per country is ever touched. The distinct count is a popcount, which also replaces the old pass with a `last[id]` stamp array per worker.

Peak anonymous memory at 12 threads by number of rounds: 29 MB with one pass, then 22 (2 rounds), 18 (4) and 15 (8). Every round costs two thread launches and a wait for the slowest chunk. On this 2-core machine with 12 forced threads, 8 rounds measured about 7% slower, 4 about 4%. With 2 threads, run time was unchanged. I chose 4.

Correctness: identical SHA-256 to the reference on tiny seeds 1 to 25, sample seeds 1 to 6, full seeds 1 and 13, the worked example, a file without a final terminator, a header-only file, and with 1, 3 and 13 threads.

### Name lookups

Elimination test (2 threads, min of 7): the run took 323 ms, 161 ms with the lookup replaced by the hash alone, and 139 ms with neither. So lookups were about half the run, and hashing only a small part of that. Each lookup read two random places: the slot, then the name's record in the arena. The bytes were also gathered with a variable-length copy for the last word, in the hash and again in every comparison.

- **32-byte slots.** A slot now holds the hash tag, the length, the id and, for names up to 24 bytes, the name itself, so a lookup reads one cache line. Longer names (about one record in 100,000 at full scale) keep their bytes in the arena. A thread claims an empty slot with a compare-and-swap to a busy marker, fills it, then publishes it; a reader that meets the marker waits for it. Ids are now handed out only to the thread that won the slot, so they have no gaps. With 2^18 slots, about 100,000 names fill under two fifths of the table. The table is 8 MB, about what the old slots and arena used together.
- **Eight-byte loads.** A name is always followed in its record by at least `,,1,0.00`, so loading a whole word at its last bytes stays inside the mapped file. The word is masked instead of copied.

Local, 2 threads: 328 ms to 216 ms (min of 9, alternating runs); with 12 forced threads, 306 ms to 224 ms. Peak anonymous memory at 12 threads: 19 MB.

407 code lines (from 385). Correctness: identical SHA-256 to the reference on tiny seeds 1 to 25, sample seeds 1 to 6, full seeds 1 and 13, the worked example, a file without a final terminator, a header-only file, a hand-made file with names longer than 24 bytes, exactly 23 and 24 bytes and with escaped quotes, and with 1, 3 and 13 threads.

Board, step 4: T 78 to 64 ms. M went from 41 to 47 MB on the board, while this machine's 12-thread peak fell from 29 to 19 MB. A local memory cgroup (the same measure as the box's `memory.peak`) gives 30 and 20 MB, and I could not reproduce the difference here: transparent huge pages set to `always`, and more thread starts, both left it unchanged. The other tracks measure within 3 to 7 MB of the board this way.

## Step 5

- **regions.csv in parallel.** The table was built in four serial passes over the 2 MB file (codes, ranks, the last id, then the fill), about 6 ms on one thread. That is about a tenth of a 64 ms run, and none of it scaled. Now the threads parse their part of the lines into (region id, code key) pairs in one pass. Main then sets the ranks and fills the table from those pairs with plain loops.
- **Prefetching name slots.** Lookups were about 80 of 214 ms (2 threads) beyond hashing, mostly waiting for a random slot in the 8 MB table. Each record's slot is now prefetched as soon as its name is hashed, and the lookup itself runs four records later. Prefetching one record ahead gave about 5%, four records ahead a little more (2, 4 and 8 were within noise of each other).
- **A crash fixed.** `split_records` read `buf[c - 1]` with `c = 0` when the text was shorter than the number of pieces. Since step 4 the records are split into 4 × threads pieces, so a records file of one short record crashed. The cut now starts at 1 at least. Tested with that file and with 1, 3, 13 and 52 threads.

Local, 2 threads: about 205 to 200 ms (min of 9, alternating). Most of the gain is the serial regions step, which this machine's two threads hide and the box's twelve do not.

435 code lines. Correctness: identical SHA-256 to the reference on tiny seeds 1 to 25, sample seeds 1 to 6, full seeds 1 and 13, the worked example, a file without a final terminator, a header-only file, a one-record file, the long-name file, with 1, 3, 13 and 52 threads, and with a build without AVX2.

## Step 6: memory

Board, step 5: T 64 to 67 ms, within one run's noise, and M again 47 MB against 20 MB in a local memory cgroup. The top teams run this track in 9 to 14 MB.

- **`mallopt(M_MMAP_THRESHOLD, 128 KiB)`, as in Track D.** It stops glibc from moving large blocks into arenas that keep their pages after a free.
- **Name bitmaps sized to use.** They were one zero-allocated block of 2^18 bits per country (7.9 MB of address space, about 3 MB touched). Each country's row is now a vector, grown at each fold to the ids handed out so far: about 12.6 KB per country at full scale. No fixed bound on the number of distinct names is assumed, so a dataset with more names than the generator's pool still runs.

Local: 21 MB peak at 12 threads, time unchanged. This step tests what the box charges that this machine does not. Correctness: identical SHA-256 on tiny seeds 1 to 25, sample seeds 1 to 6, full seeds 1 and 13, the worked example, the edge-case files (no final terminator, header only, one record, long names), and with 1, 3 and 13 threads.

## Experiment: transparent huge pages

On Track A, turning transparent huge pages off for the process (`prctl(PR_SET_THP_DISABLE)`) took the board's M from 50 to 38 MB with time unchanged. The box backs memory with 2 MB pages, and each one commits all its memory on its first write. This track measured closer to its local number already, so this step finds out whether the same line saves memory here too, and what it costs in time: huge pages also mean fewer TLB misses on large, randomly accessed arrays.

## Step 8: page faults

Phase timings showed the first parse round taking about 70 to 90 ms against about 36 to 50 ms for each later round (2 threads). The first round inserts the 100,000 names and touches every page of the 8 MB name table for the first time. Writing one byte per page of the table before the parse took the first round down by 15 to 20 ms, while the touching itself cost about 3 ms: two threads first-touching the same table fault it in page by page and contend in the kernel. The table is now committed in one call (`madvise(MADV_POPULATE_WRITE)`, about 2 ms, best effort on older kernels).

Local, 2 threads, three alternating rounds of 9: about 8 to 10% faster. The same call on Track A's latency array and Track D's adjacency arrays gave no gain there (both are written in order, so their faults do not contend), so it is used only here.

Correctness: identical SHA-256 to the reference on tiny seeds 1 to 25, sample seeds 1 to 6, full seeds 1 and 13, the worked example, the edge-case files, and with 1, 3 and 13 threads.
