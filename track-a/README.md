# Track A: Ingest — Team Flyhigh

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/a-ingest.md).

## Approach

Rust, standard library only, in `src/main.rs`.

1. **No copy of the input.** `requests.log` is memory-mapped read-only. The harness pre-warms it into the page cache, so the program reads it in place instead of copying 239 MB into its own memory.
2. **Parallel hand-written parsing.** The file is cut into one chunk per hardware thread, each ending on a line boundary. Each thread walks its bytes once:
   - the timestamp is fixed width, so the hour comes from fixed offsets and a small calendar formula (no date library, no full timestamp parse);
   - the method is skipped;
   - the endpoint is hashed while it is scanned and interned in a small open-addressing table (1,842 distinct endpoints);
   - only the first digit of the status matters (5xx);
   - the latency is parsed digit by digit.

   Each line becomes one packed `u64`: endpoint id, hour, error bit, latency.
3. **Group ids that are already in output order.** All endpoint names are merged and sorted bytewise once. A group is `hour × endpoints + rank`, so walking groups in index order is exactly the spec's output order: no sort of 127k output keys.
4. **Counting sort instead of growing lists.** Each thread counts its lines per group, a prefix sum gives every thread its own slice of every group, and the threads scatter latencies in parallel into one flat `u32` array.
5. **Percentiles without sorting.** For each group, `select_nth_unstable` finds rank p99, then p95 within the part below it, then p50 below that. That is linear time instead of n log n.
6. **Parallel output.** Groups are split into ranges of equal latency count, one per thread. Each range is formatted with a hand-written integer printer, and the buffers are written in order.

Correctness was checked against the reference's SHA-256 on seed 1 at all three scales, on tiny seeds 2–12, sample seeds 2–7, full seed 42, the spec's worked example and an empty file.

## Dependencies

None. `mmap` is declared directly against the C library the Rust standard library already links.

## With another 24 hours

- Lower peak memory: store a 4-byte group id per line instead of an 8-byte record, and reuse the count arrays as cursors.
- Profile-guided optimisation with the allowlisted `speed-demon-rust` image, training on generator output at build time.
- SIMD (AVX2) scanning for the space and newline delimiters.
