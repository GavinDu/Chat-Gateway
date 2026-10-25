# Docker deployment (Linux)

The image runs as the unprivileged `nonroot` user. It needs a real
configuration file and writable persistent storage for SQLite conversation
memory and provider sessions.

Build from the repository root:

```sh
docker build -t chat-gateway-rs .
mkdir -p data sessions
sudo chown 65532:65532 data sessions
chmod 700 data sessions
```

The gateway intentionally binds to `127.0.0.1` and validates loopback Host and
Origin authorities. On Linux, use host networking so the process retains that
loopback-only boundary; do not publish a container port to a LAN or public
interface. Host networking is not available in every Docker Desktop setup.

```sh
docker run --rm --network host \
  --mount type=bind,src="$PWD/config.yaml",dst=/config.yaml,readonly \
  --mount type=bind,src="$PWD/data",dst=/app/data \
  --mount type=bind,src="$PWD/sessions",dst=/app/sessions \
  chat-gateway-rs --config /config.yaml
```

Then use `http://127.0.0.1:8080` from the host (or the configured `PORT`).
The image does not expose a remotely reachable listener; remote access requires
a separate authenticated deployment design. Keep `config.yaml` and session
files private. The container image is build-checked in CI, but live provider
login/send behavior still depends on the configured provider and session.
