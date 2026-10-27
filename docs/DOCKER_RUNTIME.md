# Docker runtime and loopback access

The image intentionally runs as a non-root user and the gateway intentionally binds
to `127.0.0.1`. These are security properties, not a general-purpose remote
service configuration. Do not publish the container port on an untrusted network.

## Build

From the repository root:

```sh
docker build --pull -t chat-gateway-rs:local .
docker run --rm --entrypoint /usr/local/bin/chat-gateway-rs chat-gateway-rs:local --help
```

The second command validates that the binary can start in the final distroless image.
The CI workflow runs both commands on pull requests.

## Run on Linux with host loopback

The gateway's loopback-only listener is inside the container's network namespace
unless host networking is enabled. On Linux, host networking allows a process
bound to `127.0.0.1:8080` to be reached through the host's loopback interface.
It does not make the service reachable from other machines unless the host itself
has been separately configured to expose it.

Create a private configuration and data directories first:

```sh
cp config.example.yaml config.yaml
mkdir -p data sessions
chmod 700 data sessions
```

Review `docs/PROVIDERS_POLICY.md` and explicitly enable only providers whose
applicable terms permit the intended automation. All providers are disabled in
the example configuration. Configure API keys if you enable authentication.

```sh
docker run --rm --name chat-gateway-rs \
  --network host \
  --mount type=bind,src="$(pwd)/config.yaml",dst=/config.yaml,readonly \
  --mount type=bind,src="$(pwd)/data",dst=/app/data \
  --mount type=bind,src="$(pwd)/sessions",dst=/app/sessions \
  chat-gateway-rs:local --config /config.yaml
```

Use host networking only on Linux. On Docker Desktop (macOS/Windows), host
networking and loopback semantics differ by Docker Desktop version; do not assume
this command provides the same host-loopback behavior. If you need to run there,
use a deliberate local-only proxy or a separately reviewed networking design that
preserves the Host/Origin protections. Do not simply publish `8080:8080`: a
loopback bind inside the container is not equivalent to authenticated remote access.

## Persistence and permissions

- `/app/data` stores SQLite conversation memory; back it up using SQLite's backup
  API or stop the gateway before copying the database.
- `/app/sessions` may contain reusable provider authentication material. Keep it
  private and never include it in logs, images, or source control.
- The image uses a non-root runtime user. Ensure mounted directories are writable
  by that user (UID/GID 65532) before starting the container.
- Keep configuration, database backups, and session files out of public volumes.

For API behavior, security boundaries, and the offline verification commands, see
the main README and `MIGRATION.md`.
