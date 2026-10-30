# Track A: performance log

Every step from the reference to the final version, each with the live-board run ID that measured it. Local numbers are from a 2-core dev machine and are only for direction; the board numbers are the ones that count.

| Step | Change | Run ID | T (s) | M (MB) | Points |
|---|---|---|---|---|---|
| 0 | The reference, unchanged (scoring box calibration) | | 32.05 | 228 | 0 |
| 1 | Rust rewrite: mmap, parallel hand parsing, counting-sort scatter, select_nth percentiles, parallel formatting | _fill in_ | | | |

## Step 1 notes

- Local, 2 cores, full seed 1, warm cache: about 0.19–0.25 s, output hash identical to the reference (`495ca096…`).
- The Python reference spends most of its time in `datetime.strptime`. Here the hour comes straight from fixed byte offsets, so timestamp cost is a few integer operations per line.
- Local peak RSS is 268 MB, but about 239 MB of it is the mapped input file, which already sits in the page cache. Whether the scoring box's `memory.peak` charges those pages to the run is the open question this board run answers.
