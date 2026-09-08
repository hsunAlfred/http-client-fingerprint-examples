FROM rust:1.98.0-slim-bookworm AS toolchain
RUN apt-get update \
    && apt-get install -y --no-install-recommends build-essential cmake perl pkg-config libclang-dev git ca-certificates \
    && rm -rf /var/lib/apt/lists/*
RUN rustup component add rustfmt clippy
WORKDIR /work

FROM toolchain AS build
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN --mount=type=cache,target=/usr/local/cargo/registry \
    --mount=type=cache,target=/work/target \
    cargo build --locked --release -j 2 \
    && cp target/release/fingerprint-client-rs /usr/local/bin/fingerprint-client-rs

FROM debian:bookworm-slim AS runtime
RUN apt-get update \
    && apt-get install -y --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/*
COPY --from=build /usr/local/bin/fingerprint-client-rs /usr/local/bin/fingerprint-client-rs
USER 10001:10001
WORKDIR /tmp
ENTRYPOINT ["fingerprint-client-rs"]
CMD ["--help"]
