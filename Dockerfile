FROM rust:1.89 as builder
WORKDIR /app
COPY Cargo.toml Cargo.lock ./
COPY src ./src
RUN apt-get update && apt-get install -y build-essential cmake clang libclang-dev llvm-dev
RUN cargo build --release --features render,stealth
FROM gcr.io/distroless/cc:nonroot
COPY --from=builder /app/target/release/chat-gateway-rs /usr/local/bin/chat-gateway-rs
COPY config.example.yaml /config.example.yaml
EXPOSE 8080
# Mount your config at /config.yaml and pass --config /config.yaml
ENTRYPOINT ["chat-gateway-rs"]
