FROM rust:1.88-bookworm AS builder

WORKDIR /app

COPY Cargo.toml Cargo.lock ./
COPY crates ./crates
COPY services ./services

RUN cargo build --locked --release --package tuxd \
    && strip target/release/tuxd

FROM debian:bookworm-slim AS runtime

RUN apt-get update \
    && apt-get install --yes --no-install-recommends ca-certificates \
    && rm -rf /var/lib/apt/lists/* \
    && groupadd --system tux \
    && useradd --system --gid tux --no-create-home --home-dir /nonexistent tux \
    && mkdir --parents /data \
    && chown tux:tux /data

COPY --from=builder /app/target/release/tuxd /usr/local/bin/tuxd

ENV TUXD_SERVE=1 \
    TUXD_BIND=0.0.0.0 \
    TUXD_PORT=8080 \
    TUXD_DB=/data/tux.db \
    RUST_LOG=info

VOLUME ["/data"]
EXPOSE 8080
STOPSIGNAL SIGTERM

USER tux:tux
ENTRYPOINT ["/usr/local/bin/tuxd"]
