# Track A: Ingest — Team Flyhigh

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/a-ingest.md).

## Approach

Rust, standard library only, in `src/main.rs`.

1. **No copy of the input.** `requests.log` is memory-mapped read-only. The harness pre-warms it into the page cache, so the program reads it in place instead of copying 239 MB into its own memory.
2. **Parallel hand-written parsing.** The file is cut into one chunk per hardware thread, each ending on a line boundary. Each thread walks its bytes once:
   - the timestamp is fixed width, so the hour comes from fixed offsets and a small calendar formula (no date library, no full timestamp parse);
   - the method is skipped;
   - the endpoint is hashed from its first and last eight bytes and its length, and interned in a small open-addressing table (1,842 distinct endpoints);
   - only the first digit of the status matters (5xx);
   - the latency is parsed eight digits at a time.

   Every line has exactly four spaces and an LF, so each thread builds a bitmask of spaces and LFs, 64 bytes at a time with AVX2, and takes the five field ends with trailing-zero counts. The latency's digits are converted by SWAR. The common path has almost no unpredictable branches. Neighbouring lines usually share their hour, so the calendar maths runs only when it changes. Each thread also counts lines per (hour, endpoint) cell as it parses, and keeps 6 bytes per line: the latency (`u32`) and the endpoint id with the 5xx flag (`u16`). The hour is not stored per line: neighbouring lines share it, so each thread keeps only a list of the lines where it changes.
3. **Group ids that are already in output order.** Each thread sorts its own endpoint names, and main merges the sorted lists, which are nearly identical, in linear passes. A group is `hour × endpoints + rank`, so walking groups in index order is exactly the spec's output order: no sort of 127k output keys.
4. **Counting sort instead of growing lists.** One prefix sum in output order turns every thread's cell counts into its own write cursors, and the threads scatter latencies in parallel into one flat `u32` array.
5. **Percentiles without sorting.** For each group, `select_nth_unstable` finds rank p99, then p95 within the part below it, then p50 below that. That is linear time instead of n log n.
6. **Parallel output.** Groups are split into ranges of equal latency count, one per thread. Each range is formatted with a hand-written integer printer, and the buffers are written in order.

Correctness was checked against the reference's SHA-256 on seed 1 at all three scales, tiny seeds 2–12, sample seeds 2–7, full seed 42, the spec's worked example, an empty file, and with 1, 3, 7, 12 and 13 threads.

## Dependencies

None. `mmap` is declared directly against the C library the Rust standard library already links.

## With another 24 hours

- Lower peak memory: the 8-byte per-line record could shrink to 6 bytes (latency plus endpoint id, with hour changes run-length encoded).
- Profile-guided optimisation with the allowlisted `speed-demon-rust` image, training on generator output at build time.
