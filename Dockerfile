FROM rust:1.89 AS builder
WORKDIR /app

# Cargo.toml patches these local vendored crates and declares a benchmark target.
COPY Cargo.toml Cargo.lock ./
COPY src ./src
COPY benches ./benches
COPY vendor ./vendor
COPY config.example.yaml ./config.example.yaml

RUN apt-get update && apt-get install -y --no-install-recommends \
    build-essential cmake clang libclang-dev llvm-dev \
    && rm -rf /var/lib/apt/lists/*

RUN cargo build --locked --release --features render,stealth

# Prepare writable runtime paths for the non-root distroless user.
RUN mkdir -p /app/data /app/sessions \
    && chown -R 65532:65532 /app/data /app/sessions

FROM gcr.io/distroless/cc:nonroot
WORKDIR /app

COPY --from=builder --chown=65532:65532 /app/target/release/chat-gateway-rs /usr/local/bin/chat-gateway-rs
COPY --from=builder --chown=65532:65532 /app/config.example.yaml /app/config.yaml
COPY --from=builder --chown=65532:65532 /app/data /app/data
COPY --from=builder --chown=65532:65532 /app/sessions /app/sessions

# This application intentionally binds to loopback. Use host networking on
# supported Docker hosts; do not publish this port to an untrusted network.
EXPOSE 8080
VOLUME ["/app/data", "/app/sessions"]
ENTRYPOINT ["chat-gateway-rs"]
