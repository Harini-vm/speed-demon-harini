# Track B: Index — Team FlyHigh

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/b-index.md).

## Approach

Rust, standard library only, in `src/main.rs`. The queries are readable from the start, so nothing is indexed that no query asks for, and the corpus is read exactly once.

1. **Query vocabulary first.** Every distinct term of the 50,000 queries (about 76,000) goes into one shared, read-only table. Tokens are compared as four u64 words (terms are at most 32 bytes), so a lookup never calls `memcmp`. Repeated BOOL lines are evaluated once.
2. **Phrases as one automaton.** The 18,947 distinct phrases are compiled into an Aho-Corasick automaton over term ids. Reading a document's tokens through it reports exactly the phrases that occur, so there is no candidate re-check. That matters for phrases such as `"the the the"`, whose candidate documents would be a third of the corpus. Transitions from the start state, the most common case, are a dense array.
3. **Tokenizing with bitmasks.** For each 64-byte block, AVX2 compares give a mask of token bytes ([A-Za-z0-9] per spec 2.2: every byte of 0x80 or above is a separator). Token runs are read off the mask. Inside a token, OR-ing 0x20 lowercases letters and leaves digits alone, so a token's lowercased form is at most four masked 8-byte loads.
4. **Batches of 4,096 documents per thread.** Each thread owns a contiguous range of documents. Per batch it collects (BOOL term, local doc) pairs, counting-sorts them into a small index of u16 lists, and evaluates every BOOL query on that batch. The index is reused batch after batch, so it stays a few MB whatever the corpus size.
5. **BOOL evaluation on bitmaps.** The running set is a 64-word bitmap of the batch, so OR, NOT and AND are branch-free bit operations. Terms in at least 64 of a batch's documents also get a bitmap, built once per batch and shared by every query that uses them, so an operation with a common word is 64 word operations. Counts and id sums come from popcounts a word at a time: the sum of a word's set positions is six masked popcounts.
6. **Merging.** Thread ranges are in id order, so per query the counts and sums add, and the first ten ids come from the earliest ranges.

## How it was checked

Identical SHA-256 to the Python reference on:
- seed 1 at all three scales;
- tiny seeds 1–25, sample seeds 1–6 and full seed 5;
- the spec's worked example;
- 1, 3, 12, 13 and 24 threads, since thread ranges change where the first-ten merge splits.

## Dependencies

None. `mmap` is declared directly against the C library the Rust standard library already links.

## With another 24 hours

- Smaller per-thread answer storage, and (term, doc) pairs packed into one u32, to bring memory under 44 MB, where the memory factor reaches its cap.
- Skip the automaton for documents that hold no phrase's rarest term.
- Profile-guided optimisation with the allowlisted `speed-demon-rust` image.
