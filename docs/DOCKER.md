# Docker build and local runtime

Build the image from the repository root:

```sh
docker build --pull -t chat-gateway-rs:local .
```

The image runs as a non-root user. The conversation database and provider sessions
live under `/app/data` and `/app/sessions`; mount persistent host directories if
you need them to survive container replacement.

The gateway intentionally binds to `127.0.0.1` because its login relay can
control browser sessions. A conventional `-p 8080:8080` port mapping does not
make a loopback-bound process reachable inside the container, and the application
does not support binding to a public interface. Do not work around this by
publishing the container to an untrusted network.

On Linux hosts, use host networking to keep the listener on the host loopback
interface. Prepare writable directories for the image's non-root UID (65532):

```sh
mkdir -p data sessions
sudo chown -R 65532:65532 data sessions

docker run --rm --network host \
  -v "$PWD/config.yaml:/app/config.yaml:ro" \
  -v "$PWD/data:/app/data" \
  -v "$PWD/sessions:/app/sessions" \
  chat-gateway-rs:local
```

Docker Desktop host networking requires a Docker Desktop version and setting that
support it. If host networking is unavailable, do not expose the service through
a reverse proxy, tunnel, or port publication; run it directly on the host until
an authenticated remote-access design exists.

The example config enables the listed providers. Review provider policy and
disable any provider you do not intend to use before starting the gateway.
