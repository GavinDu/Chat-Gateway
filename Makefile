build:
	cargo build --release

run:
	cargo run -- --config config.example.yaml

login-qwen:
	cargo run -- --login qwen

test-adapter:
	cargo run -- --test-adapter qwen

obscura-serve:
	obscura serve --port 9222 --stealth --render --allow-private-network

fmt:
	cargo fmt && cargo clippy
