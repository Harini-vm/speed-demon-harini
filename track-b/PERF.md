# Track B: performance log

Every step from the reference to the final version, each with the live-board run that measured it. The board identifies a run by its commit. Local numbers come from a 2-core dev machine (full seed 1, warm cache, min of 7 runs) and only show direction; the board numbers are the ones that count.

| Step | Change | Board run (commit) | T (s) | M (MB) | Lines | K | Points |
|---|---|---|---|---|---|---|---|
| 0 | The reference, unchanged (scoring-box calibration) | | 56.35 | 2200 | 91 | 1 | 0 |
| 1 | Rust: query-term-only index, Aho-Corasick phrases, SIMD tokenizer, batch bitmaps | _fill in_ | | | | | |

## Step 1, built in measured stages (local, 2 threads)

| Stage | Time | Notes |
|---|---|---|
| Byte-loop tokenizer, FNV hash, sorted-list merges per batch | 2.64 s | correct first time: sample and full hashes match the spec |
| Elimination tests | 2.41 s | byte scan ~0.6 s, term lookup and pairs ~0.9 s, automaton ~0.44 s, evaluation ~0.47 s |
| AVX2 token masks; terms hashed and compared as u64 words | 2.11 s | |
| Automaton: dense start-state transitions, packed transition and output tables | 1.94 s | |
| Batch size 1,024 / 4,096 / 16,384 | 2.07 / 1.94 / 1.90 s | evaluation is real list work, not per-batch overhead |
| Running set as a bitmap instead of sorted-list merges | 1.90 s | smaller than expected; see below |
| Bitmaps for terms in at least 64 of a batch's documents; repeated BOOL lines evaluated once | 1.73 s | |
| Counts and id sums by popcount, a word at a time | 1.57 s | |

### Why the bitmap alone did little

Counting what the queries actually touch explained it. Over seed 1, the BOOL queries reference 418 million postings, and the top 1,000 queries carry 190 million of them: `was OR to OR and OR of OR were OR therein` touches 748,000 postings, and `in AND the AND matter AND of` (four times in the file) 636,000. Bit operations made each posting cheaper, but there were still hundreds of millions of them. Precomputed bitmaps for common terms made the count of operations independent of how common the words are.

### Memory

About 114 MB at 12 threads, plus the mapped input, which the harness does not charge. Most of it is per-thread partial answers (50,000 queries x 56 bytes) and the (term, doc) pairs of the current batch. The memory factor reaches its cap at 2,200 / 50 = 44 MB, so this is the next target.
