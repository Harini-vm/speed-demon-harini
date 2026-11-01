# Build: Rust with no crates, offline, tuned for the scoring box's CPU (target-cpu=native
# makes the board build this track on the box itself, which has AVX2 and no AVX-512).
# Profile-guided: an instrumented build runs once on generator output (pgo/: sample scale,
# seed 2, not the board's seed 1), and the release build is compiled with that profile.
# The allowlisted image adds llvm-tools to rust:1-trixie, so llvm-profdata matches rustc.
FROM halloumee/speed-demon-rust:1.99.1-trixie-llvm-tools@sha256:13e4ee7135d8eac6c56476c7d1aee201f36479f4745fd5d0e6b4529f4e13251a AS build
WORKDIR /src
COPY . .
ENV RUSTFLAGS="-C target-cpu=native"
RUN RUSTFLAGS="$RUSTFLAGS -C profile-generate=/tmp/p" cargo build --release --offline --target-dir /tmp/t && mkdir /tmp/o && /tmp/t/release/solution pgo /tmp/o && llvm-profdata merge -o /tmp/p.profdata /tmp/p && RUSTFLAGS="$RUSTFLAGS -C profile-use=/tmp/p.profdata" cargo build --release --offline

# Run: reads /data/records.csv and regions.csv, writes /out/aggregates.txt, exits 0.
FROM debian:13-slim@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad
COPY --from=build /src/target/release/solution /app/solution
ENTRYPOINT ["/app/solution"]
