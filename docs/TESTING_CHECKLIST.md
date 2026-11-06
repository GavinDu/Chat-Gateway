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
