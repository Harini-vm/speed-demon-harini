# Team FlyHigh: Speed Demon 2026

Our entry for all four tracks of [Speed Demon 2026](https://speeddemon.dev). Each track is a single Rust program with no dependencies. It reads its inputs from `/data`, writes one file to `/out`, and must match the Python reference's output byte for byte.

| Track | Task | Folder | Approach | Optimisation log |
|---|---|---|---|---|
| A: Ingest | Per-hour, per-endpoint request counts, latency percentiles and error rates from 3.1 million log lines | [`track-a`](track-a) | [README](track-a/README.md) | [PERF](track-a/PERF.md) |
| B: Index | 50,000 boolean and phrase queries over a text corpus | [`track-b`](track-b) | [README](track-b/README.md) | [PERF](track-b/PERF.md) |
| C: Join | A hostile 200 MB CSV joined against a region table, aggregated per country | [`track-c`](track-c) | [README](track-c/README.md) | [PERF](track-c/PERF.md) |
| D: Traverse | 2,000 shortest-path queries on a 1.7 million node road graph | [`track-d`](track-d) | [README](track-d/README.md) | [PERF](track-d/PERF.md) |

The specs are in the organisers' repository: [hackathon-raptors/speed-demon-2026](https://github.com/hackathon-raptors/speed-demon-2026/tree/main/specs).

## Results on the live board

Best measured run per track, on the board's box (12 threads, full dataset, seed 1), as of October 31, 2026:

| Track | Time | Memory | Source lines | Points |
|---|---|---|---|---|
| A | 46 ms | 27.2 MiB | 409 | 73.66 |
| B | 470 ms | 77.8 MiB | 582 | 60.24 |
| C | 54 ms | 19.6 MiB | 462 | 68.54 |
| D | 196 ms | 91.5 MiB | 551 | 65.63 |

Each PERF.md lists every step with the board run that measured it, including the steps that were measured worse and reverted.

## What the four programs share

- **No copies of the input.** Every input file is memory-mapped read-only and parsed in place.
- **One chunk per hardware thread.** Each file is cut at record boundaries that are safe to find without parsing from the start. Threads work on private state and merge at the end, so the hot loops have no locks.
- **Bitmask parsing.** In Tracks A, B and C, AVX2 compares turn each 64-byte block into bitmasks of the bytes that matter (separators, quotes, token bytes, line ends). Field boundaries come from trailing-zero counts, so the common path has few unpredictable branches. Track D's lines are short numbers, so it finds separators a word at a time with SWAR instead. Numbers are converted eight digits at a time (SWAR). The Dockerfiles build with `-C target-cpu=native` for the scoring box, and the code keeps a portable path for builds without AVX2.
- **Work that is already in output order.** Group ids are chosen so that walking them in index order is the output order, which removes the final sort.
- **Memory that goes back to the OS.** The board measures the cgroup's memory peak, so every allocation of 128 KiB or more gets its own mmap (`mallopt`) and is returned when freed, instead of staying in glibc's per-thread arenas. Track A also turns transparent huge pages off for the process, which measured smaller on the box; on the other tracks it was slower.
- **No dependencies.** `mmap`, `mallopt` and `prctl` are declared directly against the C library that the Rust standard library already links. Every `.speed-demon.toml` declares `dependency_count = 0`.

## Correctness

No change is kept unless its output has the reference's SHA-256 on every test below:

- seed 1 at the `tiny`, `sample` and `full` scales;
- 20 or more further `tiny` seeds, several `sample` seeds and at least one other `full` seed, from the public generators;
- the spec's worked example;
- 1, 3, 12 and 13 threads (and more for some tracks), because the chunk boundaries move with the thread count;
- each track's own edge cases, listed in its README, such as an empty input or a final record without a terminator.

## Build and run

Each folder builds offline from its Dockerfile, exactly as the board builds it. Without Docker:

```
cd track-a
cargo build --release --offline
./target/release/solution /path/to/data /path/to/out
```

The program's two optional arguments replace `/data` and `/out`. To build and run a track the way the board does, with Docker, and check its output:

```
python3 check.py a --data data/a --expected <sha256>
```

Test data comes from the public generators. Seed 1 at `full` scale is the board's dataset:

```
git clone https://github.com/hackathon-raptors/speed-demon-2026
python3.13 speed-demon-2026/generators/a_ingest.py --seed 1 --scale full --out data/a
python3.13 speed-demon-2026/reference/a_ingest.py --data data/a --out ref/a
```

## Licence

MIT, see [LICENSE](LICENSE).
