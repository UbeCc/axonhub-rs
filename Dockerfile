# syntax=docker/dockerfile:1
FROM rust:1.90-bookworm AS builder
WORKDIR /src
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN cargo build --release --locked --bin axonhub-rs

FROM debian:bookworm-slim
RUN apt-get update \
  && apt-get install -y --no-install-recommends ca-certificates \
  && rm -rf /var/lib/apt/lists/*
RUN useradd --system --uid 10001 --home /nonexistent --shell /usr/sbin/nologin axonhub
COPY --from=builder /src/target/release/axonhub-rs /usr/local/bin/axonhub-rs
COPY config.example.yml /etc/axonhub-rs/config.example.yml
USER axonhub
EXPOSE 8090
ENTRYPOINT ["/usr/local/bin/axonhub-rs", "--config", "/etc/axonhub-rs/config.yml"]
