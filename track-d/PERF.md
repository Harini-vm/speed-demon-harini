# Track D: performance log

Every step from the reference to the final version, each with the live-board run that measured it. The board identifies a run by its commit. Local numbers come from a 2-core dev machine (full seed 1, warm cache) and only show direction; the board numbers are the ones that count.

| Step | Change | Board run (commit) | T (s) | M (MB) | Lines | K | Points |
|---|---|---|---|---|---|---|---|
| 0 | The reference, unchanged (scoring-box calibration) | | 72.69 | 893 | 43 | 1 | 0 |
| 1 | Rust: bucketed CSR, BFS renumbering, bidirectional Dijkstra with a radix heap, u32 distances | `155b431` | 0.252 | 203 | 435 | 16.12 | 60.373 |
| 2 | SWAR number parsing, u32 heap keys, parallel renumbering bookkeeping | _fill in_ | | | | | |

## Step 1, built in measured stages (local, 2 threads)

| Stage | Total time | Notes |
|---|---|---|
| Parse, CSR with atomic degrees, one Dijkstra per query (binary heap) | 4.5 s | correct first time: sample and full hashes match the spec |
| Renumber nodes in BFS order | 2.3 s | queries 2x faster: the input ids are random, so every edge was a cache miss |
| CSR built per bucket (no atomics), BFS in parallel | 1.85 s | build 690 ms to 310 ms |
| Radix heap instead of `BinaryHeap` | 1.25 s | |
| Bidirectional Dijkstra, sides balanced by key | 1.25 s | no gain: see below |
| Bidirectional, sides balanced by queue size | 0.80 s | |
| u32 distances in zero-allocated arrays, 64-bit fallback | 0.80 s | memory 433 MB to 265 MB at 12 threads |
| Queries sorted by source, contiguous runs per thread | 0.80 s | memory to 232 MB |
| `mallopt` mmap threshold, 65,536-node buckets | 0.82 s | memory to about 130 MB live |

### The failed experiment

Bidirectional Dijkstra balanced by the smaller next key gave nothing. The cause was the queries from a node with no in-edges: its two out-edges weigh about 2^32, so the forward side's next key jumps to 4.29 billion after one step. The backward side, always "smaller", then grew until its keys reached 4.29 billion: the whole main body. Balancing by queue size fixed it.

### Memory

At 12 threads the first version used about 433 MB, nearly all of it 24 distance arrays of 14 MB (two per thread, u64). Three changes took it down:

1. u32 distances, stored as distance + 1 in zero-allocated arrays, so untouched pages are never committed;
2. each thread searching one region of the renumbered map, so it touches fewer pages;
3. returning freed memory to the OS. Per-phase logging showed that 72 MB of parse buckets stayed resident after being freed: glibc kept them in per-thread arenas. Forcing large allocations onto mmap returns them.

Correctness after these changes: identical SHA-256 to the reference on tiny seeds 1 to 20, sample seeds 1 to 6, full seeds 1 and 9, the spec's worked example, and with 1, 3, 12 and 13 threads.

Board, step 1: T 288x faster contributes 16.98 to K, M 4.4x smaller contributes 1.35, and 435 lines (10x the reference) costs 0.71. M was 203 MB against about 130 MB of live data measured locally, so the box's `memory.peak` sees more than the live set: a target for the next step, along with the source size.

## Step 2

- **Parsing.** Every edge line is `u v w`, and each number was read one digit at a time with a branch per digit. Now the separator is found a word at a time (a SWAR "byte below `0`" test, since spaces and LFs are the only bytes below `0`). The last eight digits are converted at once from the word ending at the separator, as in Track A. Local parse phase: about 115 ms to about 75 ms.
- **Heap entries.** Every key in the main search is below 2^32; a larger distance already sends the query to the 64-bit fallback. So the radix heap holds (u32 key, u32 node): 8 bytes instead of 16, and 33 buckets instead of 65.
- **Serial loops.** Filling `new_id[old]` and the renumbered degrees were single-threaded loops over 1.74 million nodes with scattered writes. On 12 threads such loops become a large share of a 250 ms run. Each thread now handles its own range of the new order, and only a sequential prefix sum remains.

Counting the search work (an instrumented build): 7.6 million nodes settled and 21 million edges relaxed over the 2,000 queries, against about 25 million settled for one-directional searches. The searches are already tight in node count; their cost is memory latency per node.

Correctness: identical SHA-256 to the reference on tiny seeds 1 to 20, sample seeds 1 to 6, full seeds 1 and 9, the worked example, and with 1, 3 and 13 threads.
