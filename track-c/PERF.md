# Track C: performance log

Every step from the reference to the final version, each with the live-board run that measured it. The board identifies a run by its commit. Local numbers come from a 2-core dev machine (full seed 1, warm cache, min of 7 runs) and only show direction; the board numbers are the ones that count.

| Step | Change | Board run (commit) | T (s) | M (MB) | Lines | K | Points |
|---|---|---|---|---|---|---|---|
| 0 | The reference, unchanged (scoring-box calibration) | | 14.44 | 367 | 108 | 1 | 0 |
| 1 | Rust: structural (AVX2 bitmask) CSV parsing, array join, shared lock-free name sets | `e8571e5` | 0.092 | 41 | 416 | 15.88 | 60.036 |
| 2 | Country marks moved out of the shared table into private per-thread lists; regions read with plain iterators | `5e6ffc1` | 0.082 | 41 | 411 | 16.83 | 61.302 |
| 3 | Smaller source: the same logic in fewer lines | _fill in_ | | | | | |
| 4 | Name ids folded into per-country bitmaps after each of four parse rounds | _fill in_ | | | | | |

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

387 code lines (from 385). Correctness: identical SHA-256 to the reference on tiny seeds 1 to 25, sample seeds 1 to 6, full seeds 1 and 13, the worked example, a file without a final terminator, a header-only file, and with 1, 3 and 13 threads.
