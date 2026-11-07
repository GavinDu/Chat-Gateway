# Release and pairing testing checklist

Run local E2E from a clean, merged `main` checkout. Print `pwd` and the main commit
before every test stage. Use disposable containers, keys and volumes; never print
administrator secrets, install tokens or live provider credentials in test logs.

- [ ] Run the helper contract tests: `python3 -B -m unittest discover -s scripts -p test_docker_run.py`.
- [ ] Confirm `pairing-code` prints exactly eight digits, preserving leading zeros, for compact and spaced JSON responses.
- [ ] Confirm missing/nonstring/invalid-length codes and failed HTTP rotation exit nonzero without printing a code.
- [ ] Run `bash scripts/docker-main-e2e.sh` to build and exercise the actual release image.
- [ ] Call the actual `bash scripts/docker-run.sh pairing-code` command against the disposable running release container twice, using its key volume and published loopback port. Require exactly eight digits each time. Do not substitute a direct endpoint request for this command.
- [ ] Confirm unauthenticated rotation is rejected, the first code is invalidated by the second, the current code pairs successfully, and replay is rejected.
- [ ] Confirm health/request IDs, offline readiness, and graceful shutdown (exit 0).
- [ ] From the local MCP sandbox, print its workspace and run HTTP acceptance against the same main release: models, malformed JSON, Host/Origin guards and anonymous browser-control rejection.
- [ ] Verify Rust, native browser/SSH, x86 Docker and bounded ARM64 release checks in GitHub Actions.
- [ ] Remove disposable containers, key volumes and scratch files; preserve saved gateway state.

The Docker runtime smoke automatically invokes the real pairing-code helper and
checks rotation/invalidation/single-use on both x86 and ARM64 CI, and through the
main-only local release E2E runner. Parser fixtures alone cannot establish that
Docker key-volume access and HTTP authentication work together.

## Local extension approval and automatic refresh

- [ ] Connect with the local relay URL; approve once in the local confirmation tab, without entering a pairing code.
- [ ] Reject approval page fetches from scripts/frames, wrong Origin, wrong CSRF capability, expired requests, and token-exchange replay. Polling before approval must not return a token.
- [ ] Close and reopen the popup during approval; confirm it completes and reconnects after browser/gateway restarts.
- [ ] Show provider statuses inside the extension and refresh them while open.
- [ ] With all eight providers enabled, keep status responses responsive during browser probes; unavailable live observations must fall back to saved-session and cached availability without claiming verified login.
- [ ] Connect an already signed-in provider and sync its existing session; with no session, open the official provider site and sync cookie changes after sign-in.
- [ ] Keep permission denial, logout/expired session, disabled provider, and unavailable gateway visible without claiming cookies prove a verified login.

- [ ] Start through the real Docker helper using an owner-only (`0600`) host config; confirm the nonroot gateway reads its private config volume and the host file remains `0600`.
