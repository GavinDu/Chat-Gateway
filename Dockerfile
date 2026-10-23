FROM rust:1.82 as builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y ca-certificates curl && rm -rf /var/lib/apt/lists/*
# Install obscura binary
RUN curl -LO https://github.com/h4ckf0r0day/obscura/releases/latest/download/obscura-x86_64-linux.tar.gz && tar xzf obscura-x86_64-linux.tar.gz && mv obscura /usr/local/bin/ && chmod +x /usr/local/bin/obscura
COPY --from=builder /app/target/release/chat-gateway-rs /usr/local/bin/
COPY --from=builder /app/config.example.yaml /app/config.example.yaml
WORKDIR /app
EXPOSE 8080
CMD ["chat-gateway-rs", "--config", "config.example.yaml"]
