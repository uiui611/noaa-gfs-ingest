FROM rust:1.98.0-bookworm AS builder

RUN apt-get update \
    && apt-get install --yes --no-install-recommends libblosc-dev libeccodes-dev \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --locked --release --bin noaa-gfs-ingest

FROM debian:bookworm-slim

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates libblosc1 libeccodes0 \
    && rm -rf /var/lib/apt/lists/*
WORKDIR /app
COPY --from=builder /build/target/release/noaa-gfs-ingest /usr/local/bin/noaa-gfs-ingest
COPY gfs-fields.csv ./

USER 65532:65532
ENTRYPOINT ["/usr/local/bin/noaa-gfs-ingest"]
