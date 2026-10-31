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
   - the latency is read later, in step 4.

   Every line has exactly four spaces and an LF, so each thread builds a bitmask of spaces and LFs, 64 bytes at a time with AVX2, and takes the five field ends with trailing-zero counts. The common path has almost no unpredictable branches. Neighbouring lines usually share their hour, so the calendar maths runs only when it changes. Each thread also counts lines per (hour, endpoint) cell as it parses, and keeps 2 bytes per line: the endpoint id with the 5xx flag. The hour is not stored per line: neighbouring lines share it, so each thread keeps only a list of the lines where it changes.
3. **Group ids that are already in output order.** Each thread sorts its own endpoint names, and main merges the sorted lists, which are nearly identical, in linear passes. A group is `hour × endpoints + rank`, so walking groups in index order is exactly the spec's output order: no sort of 127k output keys.
4. **Counting sort instead of growing lists.** One prefix sum in output order turns every thread's cell counts into its own write cursors. Each thread then walks its text a second time with the same separator bitmask, converts only the latency (eight digits at a time, SWAR) and writes it straight to its group in one flat `u32` array, so the 3.1 million latencies are stored once. 5xx counts go to shared atomic counters per group.
5. **Percentiles without sorting.** For each group, `select_nth_unstable` finds rank p99, then p95 within the part below it, then p50 below that. That is linear time instead of n log n.
6. **Parallel output.** Groups are split into ranges of equal latency count, one per thread. Each range is formatted with a hand-written integer printer, and the buffers are written in order.

Correctness was checked against the reference's SHA-256 on seed 1 at all three scales, tiny seeds 2–12, sample seeds 2–7, full seed 42, the spec's worked example, an empty file, and with 1, 3, 7, 12 and 13 threads.

## Memory

The board's memory figure is the run's cgroup peak, page cache included. Two settings matter there: every allocation of 128 KiB or more gets its own mmap (`mallopt`), so freed blocks go back to the OS, and transparent huge pages are off for this process (`prctl`), because a 2 MB page commits all its memory on first write. PERF.md has the measurements.

## Dependencies

None. `mmap`, `mallopt` and `prctl` are declared directly against the C library the Rust standard library already links.

## With another 24 hours

- A persistent thread pool: the phases still start a fresh set of threads each (tried: faster, but 75 more lines).
- Profile-guided optimisation with the allowlisted `speed-demon-rust` image, training on generator output at build time.
