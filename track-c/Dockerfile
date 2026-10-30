# Build: Rust with no crates, offline, tuned for the scoring box's CPU (target-cpu=native
# makes the board build this track on the box itself, which has AVX2 and no AVX-512).
FROM rust:1-slim-trixie@sha256:1a939748a7cbab172de91a014e3804a97af1394a8c844faa67fce9401c4e40fb AS build
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
ENV RUSTFLAGS="-C target-cpu=native"
RUN cargo build --release --offline

# Run: reads /data/records.csv and regions.csv, writes /out/aggregates.txt, exits 0.
FROM debian:13-slim@sha256:62bd423aeee912639ca8ee681c2b762cb3e25137e0a92b20be455898a9f034ad
COPY --from=build /src/target/release/solution /app/solution
ENTRYPOINT ["/app/solution"]
