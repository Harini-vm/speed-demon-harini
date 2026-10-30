# Track B: performance log

Every step from the reference to the final version, each with the live-board run that measured it. The board identifies a run by its commit. Local numbers come from a 2-core dev machine (full seed 1, warm cache, min of 7 runs) and only show direction; the board numbers are the ones that count.

| Step | Change | Board run (commit) | T (s) | M (MB) | Lines | K | Points |
|---|---|---|---|---|---|---|---|
| 0 | The reference, unchanged (scoring-box calibration) | | 56.35 | 2200 | 91 | 1 | 0 |
| 1 | Rust: query-term-only index, Aho-Corasick phrases, SIMD tokenizer, batch bitmaps | `e8571e5` | 0.469 | 132 | 613 | 14.46 | 58.008 |
| 2 | Compact per-thread answers, packed pairs, filtered term lookup, CSR automaton | _fill in_ | | | | | |
| 3 | Smaller source: the same logic in fewer lines | _fill in_ | | | | | |

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

Board, step 1: T 120x faster contributes 10.96 to K, M 16.7x smaller contributes 1.76, and 613 lines (6.7x the reference) costs 0.75. Local 1.57 s on 2 cores became 0.47 s on 12 threads: the automaton and the term lookup scale with cores, but each thread still reads the whole query list per batch.

## Step 2

Memory first, because the memory factor stops growing at 44 MB:

- Each thread kept 56 bytes for every one of the 50,000 query lines, including phrase lines and repeats that never used them. Now only distinct BOOL queries and distinct phrases get a slot, holding count and id sum (16 bytes). The first ten ids go to a per-thread list of (slot, id), appended only while a slot has fewer than ten hits in that thread's range. Because threads cover documents in id order and each list is in id order, the merge needs no sort.
- A (term, local doc) pair is one u32 instead of eight bytes.

At 12 threads: 114 MB to 79 MB, time unchanged within noise.

Then time, with elimination tests on the scan (2 threads, wall): scanning about 330 ms, term lookup about 400, automaton about 300, postings and evaluation about 360.

- A 256 KB filter with one bit per hash value now sits in front of the term table. About a fifth of corpus tokens are not query terms, and a clear bit rejects them without probing a cold slot.
- Each term's key, length, BOOL index and phrase flag share one record, so a hit reads one cache line instead of three or four.
- The automaton's transitions moved from a 2 MB hash table to per-state sorted edge lists (0.75 MB): most states have one to three edges.

Local: about 1.65 s to 1.56 s. Steps of this size are below what the 2-core machine resolves, so the board decides.

## Step 3: tried, measured, reverted

- **A cache for automaton transitions.** Counting showed that of 36.3 million tokens, 25 million are phrase terms, giving 14.6 million transitions from non-start states and 13.6 million failure-link hops. A per-thread 64K-entry cache of (state, term) -> next state seemed promising, since word pairs repeat. It was slower (1.54 s to 1.63 s in alternating runs): the combinations are too varied for the cache to pay for its hashing. Reverted.

## Step 3: source size

Source size is 15% of the exponent, and this file was 621 code lines against the reference's few dozen. Same algorithms, same phases:

`par_map` for the worker threads. BOOL evaluation has one path: each operand becomes a 64-word bitmap (a precomputed one for a dense term, a scratch one built from the list otherwise), then one word-wise OR, AND or NOT. `class_mask` has one fallback that covers both the file's last block and builds without AVX2, and the 256-entry token table it used became `is_ascii_alphanumeric()`, which is exactly spec 2.2's token byte.

621 to 571 code lines. Run time unchanged in alternating runs against the previous build. Correctness: identical SHA-256 to the reference on the full seed battery, the worked example, and with 1, 3 and 13 threads.
