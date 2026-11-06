FROM rust:1.99.1 AS builder
WORKDIR /app
# Keep system libraries and Rust dependencies independent of source edits.
RUN apt-get update && apt-get install -y --no-install-recommends build-essential cmake clang libclang-dev llvm-dev \
    && rm -rf /var/lib/apt/lists/*
# btls-sys compiles BoringSSL's very large FIPS C++ module. Docker's available
# memory may be far below its reported CPU count, so the default Cargo and
# CMake parallelism can trigger an OOM-killed cc1plus (exit code 101).
# Serialize the native build by default; larger builders may override Cargo
# parallelism with --build-arg CARGO_BUILD_JOBS=2 after ensuring enough RAM.
ARG CARGO_BUILD_JOBS=1
ENV CARGO_BUILD_JOBS=${CARGO_BUILD_JOBS} \
    CMAKE_BUILD_PARALLEL_LEVEL=1
COPY Cargo.toml Cargo.lock ./
COPY vendor ./vendor
# Compile the *same pinned dependency graph* with a dummy binary. BuildKit
# can reuse this layer when only application sources change. Keep the bench
# target present because it is explicitly declared in Cargo.toml.
RUN mkdir -p src benches \
    && printf 'fn main() {}\n' > src/main.rs \
    && printf 'fn main() {}\n' > benches/memory_sqlite.rs
RUN cargo build --locked --release --features render,stealth
# The real executable pulls in the large V8/BoringSSL archives. GNU ld can be
# OOM-killed at the final ARM64 link even with Cargo/CMake serialized. Install
# LLD after the dependency layer so existing compiled dependencies stay reusable.
RUN apt-get update && apt-get install -y --no-install-recommends lld \
    && rm -rf /var/lib/apt/lists/*
# Build the actual application using the already compiled dependency artifacts.
COPY src ./src
COPY benches ./benches
# Apply linker flags only to the application target, not every dependency.
# Bound LLD to one thread to keep the release link within the small-builder gate.
RUN touch src/main.rs && cargo rustc --locked --release --features render,stealth -- \
    -C linker=clang -C link-arg=-fuse-ld=lld -C link-arg=-Wl,--threads=1
RUN mkdir -p /app/data /app/sessions && chown -R 65532:65532 /app/data /app/sessions

FROM gcr.io/distroless/cc:nonroot
WORKDIR /app
# This native-only image supports the extension relay, not embedded Chromium.
# See browser-login/README.md for the separately supported Replit-native runtime.
# Browser admission also checks for worker assets; overriding this cannot enable
# an incomplete runtime.
ENV CHAT_GATEWAY_EMBEDDED_BROWSER=disabled
COPY --from=builder --chown=nonroot:nonroot /app/target/release/chat-gateway-rs /usr/local/bin/chat-gateway-rs
COPY --from=builder --chown=nonroot:nonroot /app/data /app/data
COPY --from=builder --chown=nonroot:nonroot /app/sessions /app/sessions
COPY --chown=nonroot:nonroot config.example.yaml /config.example.yaml
VOLUME ["/app/data", "/app/sessions"]
EXPOSE 8080
# Mount a real config at /config.yaml and pass --config /config.yaml.
# On Linux, use --network host to preserve the gateway's loopback-only bind.
ENTRYPOINT ["/usr/local/bin/chat-gateway-rs"]
