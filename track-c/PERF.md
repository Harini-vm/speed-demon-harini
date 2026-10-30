# Track C: performance log

Every step from the reference to the final version, each with the live-board run that measured it. The board identifies a run by its commit. Local numbers come from a 2-core dev machine (full seed 1, warm cache, min of 7 runs) and only show direction; the board numbers are the ones that count.

| Step | Change | Board run (commit) | T (s) | M (MB) | Lines | K | Points |
|---|---|---|---|---|---|---|---|
| 0 | The reference, unchanged (scoring-box calibration) | | 14.44 | 367 | 108 | 1 | 0 |
| 1 | Rust: structural (AVX2 bitmask) CSV parsing, array join, shared lock-free name sets | _fill in_ | | | | | |

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
