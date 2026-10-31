# Track D: Traverse — Team FlyHigh

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/d-traverse.md).

## Approach

Rust, standard library only, in `src/main.rs`. The work splits into building a cache-friendly graph and then answering 2,000 queries with bidirectional Dijkstra on all threads. On the scoring box (a 24 MB L3 cache) the graph does not fit in cache, so most of the design is about how many cache lines each step touches.

1. **Parse into buckets.** `graph.txt` is memory-mapped and cut into one chunk per thread. For each 64-byte block, AVX2 compares give a bitmask of its spaces and LFs, and every number ends at the next set bit; its last eight digits are converted at once (SWAR). Each edge goes into a bucket by its tail (65,536 node ids per bucket) as 8 bytes: the tail's low 16 bits, a 16-bit weight and the head. Self-loops are dropped: they never shorten a path. The ~50 weights of 65,534 or more are stored as 65,534 (`BIG`) and kept exactly in a short sorted side list.
2. **CSR without atomics.** Each bucket becomes its slice of a compressed sparse row graph, by one thread: degree counts that fit in L2, a prefix sum, and 6-byte edges (`u32` head, `u16` weight). Buckets own disjoint ranges of the arrays, so no atomics are needed.
3. **Renumber for locality, without a copy.** The input ids are a random relabelling, so following an edge is almost always a cache miss. Each thread grows compact breadth-first patches of at most 64 nodes from roots in its own slice of the ids, claiming nodes with an atomic flag, and each patch starts where the last one stopped. While it does, it appends every list it visits to its own buffer, so the new graph's edges already exist in the new order. The old graph is freed right after; each buffer is copied to its place with its heads renumbered. Nodes that are close on the map are now close in memory.
4. **No reverse graph.** The backward search needs in-edges. Road segments are nearly all two-way, so in-edge y -> x is usually the reverse of an out-edge x -> y, and finding y -> x means scanning y's short list, which after the renumbering is near x's. Each edge becomes 8 bytes (head, weight, reverse weight or "absent"), so both search directions read the same cache line. The 144,000 in-edges that are not reverses (one-way streets) sit in a side list grouped by blocks of 64 heads, with one bit per head saying whether to look.
5. **Queries.** `s == t` answers 0. A source with no out-edges or a target with no in-edges answers -1 without a search. Every other query runs a bidirectional Dijkstra:
   - forward from `s` and backward from `t`, always growing the side with fewer queued entries. Growing by the smaller key would let the backward side flood the graph when `s` starts with an edge of weight near 2^32;
   - it stops when the two next keys add up to at least the best path seen;
   - the queue is a radix heap (keys never decrease) with a 64-bit mask of its non-empty buckets;
   - after each pop the search starts loading the edges of the node it will settle next, which is usually already known (the last entry of the lowest bucket);
   - distances are u32, stored as distance + 1 in 1 KB pages from a per-thread pool, so memory follows the largest single search rather than the whole graph. A query whose distances would leave u32 (paths over edges near 2^32) is re-run by a small 64-bit Dijkstra.
6. **Work split.** Queries are sorted by source and handed out four at a time, so all threads work in one moving part of the renumbered map.
7. **Memory.** Every allocation of 128 KiB or more gets its own mmap (`mallopt`), so freed buffers go back to the OS. Transparent huge pages are turned off (`prctl`) while the parse buckets and the renumbering buffers fill, since a 2 MB page there would commit their unused room, and back on for the graph arrays, whose random reads they speed up. The program exits right after writing its answers.

## How it was measured

The box has a 24 MB L3 cache; the development machine has 260 MB, so local timings cannot judge memory-layout changes. Box phase times come from builds that exit with one phase's milliseconds as their exit code, which the board reports for a failed run. Variants are compared over several alternating board runs, since single runs of the same code vary by about 10%. PERF.md has every step, including the ones the box rejected.

Correctness: identical SHA-256 to the reference on tiny seeds 1 to 20, sample seeds 1 to 6, full seeds 1 and 9, the spec's worked example, a hand-made graph with a 300-edge hub and the extreme weights, and with 1, 3, 12 and 13 threads in a build without AVX2.

## Dependencies

None. `mmap`, `mallopt` and `prctl` are declared directly against the C library the Rust standard library already links.

## With another 24 hours

- Contraction-style shortcuts for the long queries: the 125 queries of the top Dijkstra-rank octave do about half of all the search work.
- A smaller source: the 64-bit fallback could reuse the main search with a generic distance type.
- Profile-guided optimisation with the allowlisted `speed-demon-rust` image.
