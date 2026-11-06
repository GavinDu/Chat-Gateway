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


## Builder prerequisite and release-link failures

The helper requires Docker Buildx and loads the resulting image into Docker.
Docker Desktop includes Buildx. With Homebrew/Colima use
`brew install docker-buildx` and follow Homebrew's CLI plugin registration
instructions (`brew info docker-buildx`); Linux users can install
`docker-buildx-plugin` from Docker's package repository.

An error ending in `ld terminated with signal 9 [Killed]` is an OOM-killed
release linker, not a cosmic-text warning. Use the current main Dockerfile's
single-threaded LLD link and inspect the Docker VM's available RAM. Cargo's
single build job alone does not reduce a single linker's peak memory. The
ARM64 CI build is limited to 2 GiB with no swap; the VM also needs room for
Docker and any other running services. See `docs/CI_PERFORMANCE.md` for the
exact regression gate and its limits.


For release Docker E2E, run this from a clean merged main checkout:

```sh
bash scripts/docker-main-e2e.sh
```

This builds through the real Docker helper **before** testing the resulting
image's entrypoint and isolated HTTP/pairing/shutdown behavior. It prints the
workspace and main commit at each stage, rejects feature/dirty checkouts,
propagates failures and uses a separate E2E image tag by default. Optionally
set `CHAT_GATEWAY_E2E_COMMIT` to pin the expected main SHA. The native
`scripts/main-e2e.sh` suite remains a separate browser/SSH runtime gate.

The isolated runtime smoke keeps its disposable pairing key in a temporary
directory under the user home, which Colima shares with its VM. macOS
`/var/folders` temporary paths are not shared by default. The test removes
the key and directory on exit.
