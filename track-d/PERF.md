# Track D: performance log

Every step from the reference to the final version, each with the live-board run that measured it. The board identifies a run by its commit. Local numbers come from a 2-core dev machine (full seed 1, warm cache) and only show direction; the board numbers are the ones that count.

| Step | Change | Board run (commit) | T (s) | M (MB) | Lines | K | Points |
|---|---|---|---|---|---|---|---|
| 0 | The reference, unchanged (scoring-box calibration) | | 72.69 | 893 | 43 | 1 | 0 |
| 1 | Rust: bucketed CSR, BFS renumbering, bidirectional Dijkstra with a radix heap, u32 distances | `155b431` | 0.252 | 203 | 435 | 16.12 | 60.373 |
| 2 | SWAR number parsing, u32 heap keys, parallel renumbering bookkeeping | `5e6ffc1` | 0.236 | 199 | 482 | 16.50 | 60.873 |
| 3 | Smaller source: the same logic in fewer lines | `e5a377e` | 0.230 | 202 | 436 | 16.90 | 61.394 |
| 4 | Distances in a small page pool; parse buckets that free back to the OS | `1d33d23` | 0.217 | 114 | 469 | 19.28 | 64.257 |
| 5 | Patch-shaped renumbering; queries handed out four at a time; parse buckets freed one by one | `acaedaf` | 0.196 | 111 | 488 | 20.30 | 65.375 |
| 6 | Experiment: transparent huge pages off for the process | `d430b7c` | 0.223 | 112 | 490 | 18.81 | 63.918 |
| 7 | Reverted: huge pages back on (the code of step 5) | `81d8989` | 0.196 | 111 | 488 | 20.25 | 65.329 |
| 8 | 6-byte edges: 16-bit weights, with the few larger ones in a side list | `094ec9f` | 0.196 | 96 | 551 | 20.60 | 65.629 |
| 9 | Renumbering copies by old id, so the node order is freed before both graphs exist | _fill in_ | | | | | |

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

Board, step 2: T 252 to 236 ms, but the source grew from 435 to 482 lines, which took back a third of the gain (the S factor fell from 0.707 to 0.697). The next step trims it.

## Step 3: source size

Source size is 15% of the exponent, and this file was 473 code lines against the reference's few dozen. Same algorithms, same phases:

`par_map` for the seven parallel sections, and a `SharedPtr::set` method so each disjoint write is one line. Parsing reads `u v w` with one array expression, and the bidirectional search takes its two sides as one pair.

473 to 427 code lines. Run time unchanged in alternating runs against the previous build. Correctness: identical SHA-256 to the reference on the full seed battery, the worked example, and with 1, 3 and 13 threads.

Board, step 3: 482 to 436 lines, and T 236 to 230 ms.

## Step 4: memory

The board's M was 202 MB against about 101 MB of graphs (forward and reverse, 47 MB each, plus the renumbering map). Snapshots between phases never showed more than 131 MB, so I sampled the process's anonymous memory every millisecond instead, with a copy forced to 12 threads. The peak was 192 MB, and it came from two places.

1. **Malloc arenas.** The parse buckets were built with `vec![Vec::with_capacity(c); nb]`, which clones the vector, and a clone does not keep the capacity. So all buckets but one started empty and doubled their way up, and every step below the 128 KB mmap threshold left a freed chunk in that thread's malloc arena. Freeing the buckets returned the large chunks to the OS but not those: about 35 MB stayed resident for the rest of the run. Now every bucket gets its capacity up front, so at full scale each one is mmap-backed from the start.
2. **Distance arrays.** Each thread had two arrays of 1.74 million entries (7 MB each) for the two sides of the search. Pages were committed only when touched, but they stayed committed, so over a run each thread's arrays filled up with the union of every search's pages: about 65 MB across 24 arrays. Now distances live in 1 KB pages from a per-thread pool. A small table maps each 256-node block to its page, and blocks with no page point at an all-zero page, so a read needs no branch. A search takes pages from the pool as it reaches new blocks and hands them all back when it ends. Its touched entries are cleared anyway, so every page is zero for the next search. Each side now needs only the pages of its largest search, about 0.5 MB.

The renumbering map is also dropped once the queries are translated, before the reverse graph is built.

Peak anonymous memory at 12 threads: 192 MB to 105 MB. It was also faster locally: 786 to 702 ms (2 threads, min of 9, alternating runs). The page table fits in L1 and the pool in L2, where the old arrays sent most reads to memory.

459 code lines (from 427). Correctness: identical SHA-256 to the reference on tiny seeds 1 to 20, sample seeds 1 to 6, full seeds 1 and 9, the worked example, and with 1, 3 and 13 threads.

Board, step 4: M 202 to 114 MB and T 230 to 217 ms, 61.394 to 64.257 points.

## Step 5: locality and scheduling

The searches are limited by memory latency, not instructions. A callgrind profile counted many instructions in the radix heap, but swapping it for a binary heap of packed keys changed nothing measurable (query phase 328 vs 333 ms), and neither did a bitmask of non-empty buckets. So both experiments were dropped.

- **Patch-shaped renumbering.** A plain breadth-first order lays the map out as long rings, so a search's ball touches a thin slice of many rings, and each slice is a few nodes of a different cache line. Each region is now a breadth-first ball of at most 64 nodes, about an 8 x 8 patch, and the next region starts at the edge of the last one. Query phase (2 threads, min of 7 runs): 360 to 333 ms, for about 10 ms more renumbering. Region sizes 16, 32, 64, 128 and 256 were all within noise of each other; 64 was the best. The box's 24 MB L3 holds much less of the graph than this machine's, so the effect there should be larger.
- **Scheduling.** I simulated the old split (24 runs in source order, 12 workers) with per-query costs from an instrumented build: the busiest thread got 1.20x the average. Runs of 4 queries bring that to about 1.06x. Distances now live in per-thread page pools, so short runs no longer cost memory.
- **Parse buckets** are regrouped by bucket and each bucket's lists are freed right after the CSR build uses them: build phase peak 99 to 65 MB at 12 threads. The overall peak (about 101 MB) is now renumbering, with the old and new graphs both alive, and the queries.

479 code lines. Correctness: identical SHA-256 to the reference on tiny seeds 1 to 20, sample seeds 1 to 6, full seeds 1 and 9, the worked example, and with 1, 3 and 13 threads.

## Experiment: transparent huge pages

On Track A, turning transparent huge pages off for the process (`prctl(PR_SET_THP_DISABLE)`) took the board's M from 50 to 38 MB with time unchanged. The box backs memory with 2 MB pages, and each one commits all its memory on its first write. This track measured closer to its local number already, so this step finds out whether the same line saves memory here too, and what it costs in time: huge pages also mean fewer TLB misses on large, randomly accessed arrays.

Board, huge pages off: the memory did not move here, and the time got worse. A re-run of Track B's unchanged code in the same push was 7% slower than its earlier run, so part of that is the box. The rest is what huge pages buy on large, randomly read arrays (fewer TLB misses). Turning them off helped only Track A, whose arrays are partly touched. They are back on for this track.

## Step 8: 6-byte edges

Each graph held its edges as (head u32, weight u32), 8 bytes. Every peak is set by graphs: about 47 MB each for the old and new graph during the renumbering, and for the forward and reverse graph during the queries. Weights here are travel times, and on full seed 1 all but 50 of the 5 million fit in 16 bits. An edge is now a packed (u32, u16), 6 bytes. A weight of 65,535 or more is stored as 65,535, and the search looks it up in the graph's sorted `big` list of (tail, head, weight), which the renumbering and the reverse graph remap like the edges. Parallel edges that share a pair also share the lookup's answer, the smallest of their large weights; that is the weight of a real edge, so shortest paths are unchanged.

Allocation matters for a type that is not a plain number: `vec![edge(0, 0); n]` writes every element in turn (about 10 ms per graph on one thread), so the edge arrays are now allocated zeroed and committed as the threads fill them.

Local cgroup at 12 threads: 104 to 88 MB. Phase times within noise of before. Correctness: identical SHA-256 to the reference on tiny seeds 1 to 20, sample seeds 1 to 6, full seeds 1 and 9 (they include the 2^32 - 1 and over-2^32 answers), the worked example, with 1, 3 and 13 threads, and with a build without AVX2.

Board, step 8: M 106 to 91.5 MiB at the same time. The source grew by 63 lines, so the score rose only 0.25.

## Step 9

During the renumbering's copy, the old graph, the new graph, `new_id` and the BFS `order` all existed together, and that is the run's peak. `order` is only needed to build `new_id`. The degrees and the copy now run over ranges of old ids, each node writing its list at its new offset, so `order` is freed first. Local cgroup at 12 threads: 88 to 85 MB, time within noise.
