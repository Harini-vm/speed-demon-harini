# Track C: Join — Team FlyHigh

The task, the input files and the exact output format are in the [spec](https://github.com/hackathon-raptors/speed-demon-2026/blob/main/specs/c-join.md).

## Approach

Rust, standard library only, in `src/main.rs`. Correctness first: the parser follows section 2.1 of the spec byte for byte, and it was checked against the reference on far more data than the board sees.

1. **The join is an array.** `regions.csv` is plain. Its 3-letter codes get numbers from a direct 26³ table, then ranks in sorted order, with `_UNMATCHED` last, so a rank is already the output position. `country_of[region_id]` is a u8 rank; ids past the table are unmatched.
2. **Safe parallel split.** By section 2.2, a note's lines are lowercase words and a name never holds an LF, so an LF followed by a digit or a BOM byte is always a record terminator, never data inside quotes. Each thread starts its chunk at such a point.
3. **Structural parsing, as in simdjson and simdcsv.** For each 64-byte block, AVX2 compares give bitmasks of `"`, `,` and LF. A six-step prefix XOR of the quote mask marks the bytes inside quotes. `""` escapes toggle twice, so they need no special case. The separators are the commas and LFs outside quotes, and a record is exactly the five fields between consecutive separators. A running popcount of quotes gives each field's quote count, so a field's value length is `raw length - 2 - (quotes - 2) / 2` without re-scanning it.
   - The BOM rule: three bytes `EF BB BF` at the start of a record are skipped; anywhere else, inside quotes, they are data and are counted.
   - A CRLF terminator leaves a CR at the end of `amount`, which is dropped. A final record without a terminator ends at end of file.
4. **Distinct names, shared by all threads.** A quoted name's raw bytes still hold their `""` escapes, but escaping is canonical, so the raw bytes identify the value and no unescaped copy is made. Each distinct name is stored once in a table shared by every thread, next to a 256-bit mask of the countries it occurs in. One lookup confirms the name and brings its mask into cache. New names are added without locks: the record is written at a bump-allocated offset, then published with a compare-and-swap. The per-country distinct count is a walk over the masks at the end.
5. **Everything else is per thread**: count, id sum, amount sum, min and max in integer cents, and name and note byte counts. These are merged at the end and printed with integer formatting.

## How it was checked

Identical SHA-256 to the Python reference on:
- tiny seeds 1–25, sample seeds 1–6, and full seeds 1 and 13;
- the spec's worked example, taken from `tests/test_c_join.py`;
- a file whose last record has no terminator, and a file with only the header;
- 1, 3, 12, 13 and 24 threads, since the chunk boundaries move with the thread count.

## Dependencies

None. `mmap` is declared directly against the C library the Rust standard library already links.

## With another 24 hours

- Profile-guided optimisation with the allowlisted `speed-demon-rust` image.
- Separate the country masks from the name bytes, so threads writing masks do not invalidate the lines that other threads read to compare names.
