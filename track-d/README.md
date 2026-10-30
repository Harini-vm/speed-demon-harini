# Track D: Traverse — Team FlyHigh

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/d-traverse.md).

## Approach

Rust, standard library only, in `src/main.rs`. The work splits into building a cache-friendly graph and then answering 2,000 queries with bidirectional Dijkstra on all threads.

1. **Parse into buckets.** `graph.txt` is memory-mapped and cut into one chunk per thread. Each thread parses its edge lines straight into buckets by tail node (65,536 node ids per bucket), dropping self-loops: they never shorten a path.
2. **CSR without atomics.** Each bucket is turned into its slice of a compressed sparse row graph by one thread: count out-degrees (the counts fit in L2), prefix sum, place `(head, weight)` pairs. Buckets own disjoint ranges of the arrays, so no atomics are needed.
3. **Renumber for locality.** The input ids are a random relabelling, so following an edge is almost always a cache miss. Each thread grows compact breadth-first patches of at most 64 nodes from roots in its own slice of the ids, claiming nodes with an atomic flag. Each patch starts where the last one stopped, and the graph is rebuilt in that order. Nodes that are close on the map are now close in memory.
4. **Reverse graph** for the backward search: in-degrees, prefix sum, parallel scatter.
5. **Queries.** `s == t` answers 0. A source with no out-edges or a target with no in-edges answers -1 without a search. Every other query runs a bidirectional Dijkstra:
   - forward from `s` and backward from `t`, always growing the side with fewer queued entries. Growing by the smaller key would let the backward side flood the graph when `s` starts with an edge of weight near 2^32;
   - it stops when the two next keys add up to at least the best path seen;
   - the priority queue is a radix heap (Dijkstra's keys never decrease), which beat a binary heap by about a third;
   - distances are u32, stored as distance + 1 in 1 KB pages from a per-thread pool: a 256-node block gets a page when the search first reaches it, and every page goes back to the pool when the search ends, so memory follows the largest single search rather than the whole graph. A query whose distances would leave u32 (the paths over near-2^32 edges) is re-run by a small 64-bit Dijkstra.
6. **Work split.** Queries are sorted by source and handed out four at a time. All threads then work in one moving part of the renumbered map, which the shared cache holds, and no thread is left finishing a long run alone.
7. **Memory.** Every allocation of 128 KiB or more goes to its own mmap (`mallopt`), so the parse buckets and the first copy of the graph go back to the OS when freed instead of staying in glibc's per-thread arenas.

Correctness was checked against the reference's SHA-256 on seed 1 at all three scales, tiny seeds 1–20, sample seeds 1–6, a full-size fresh seed, the spec's worked example, and with 1, 3, 12 and 13 threads.

## Dependencies

None. `mmap` and `mallopt` are declared directly against the C library the Rust standard library already links.

## With another 24 hours

- A smaller source: the 64-bit fallback could reuse the main search with a generic distance type.
- Store `(head, weight)` as one array per field, so the backward and forward graphs share fewer cache lines per relaxation.
- Profile-guided optimisation with the allowlisted `speed-demon-rust` image.
