# Chat Gateway development and release workflow

This is the required path from a code change to a local installation. The order matters: validate the feature before merging it, then validate the exact merged commit before installing it locally.

## 1. Make changes on a feature branch

- Start from the latest `origin/main`; create a named feature or fix branch.
- Make the code and unit-test changes on that branch. Commit and push the branch, then open a pull request targeting `main`.
- Keep the local project checkout on the feature branch while developing. Do not edit synced ChatGPT project `sources/` files.

## 2. Pass feature-branch checks

- Run the relevant unit tests, formatting, lint, dependency and security checks locally.
- Wait for every required GitHub Actions check on the current PR head commit to pass. A check on an earlier commit does not count after the branch changes.
- Fix failures on the same feature branch, push the fix, and wait for the checks on the new head to pass.

## 3. Run feature E2E in the local MCP sandbox

- Only after feature-branch CI is green, build a candidate image from that exact feature commit. Give it a commit-specific tag and disposable state; do not replace the installed `chat-gateway-rs:local` image or use saved provider sessions.
- Expose the isolated candidate to the local MCP sandbox over a private test network. Use the sandbox at `/workspace/chat-gateway` (host path `/Users/fangdu/projects/mcp-sandbox/chat-gateway`) as the E2E client. Do not try to run Docker inside the sandbox; its Docker daemon is on the host.
- Print the sandbox workspace and feature commit before each test invocation. Run the applicable API, authentication, browser/relay, pairing, and shutdown acceptance for the change. Keep temporary keys, pairing codes, and metadata out of logs; remove the candidate container, temporary volumes, and metadata after the run.
- Record the tested feature commit and results on the PR. If the sandbox E2E fails, fix the feature branch and repeat CI and E2E for its new head.

### Native Linux browser/SSH E2E capability gate (MCP sandbox)

Before selecting the native Linux browser/SSH suite, the sandbox operator must
record whether the execution host actually meets the prerequisites. Run this
read-only capability preflight **inside the sandbox execution host**, not on the
Mac Docker host (which is a different machine):

```sh
pwd
uname -s
command -v ssh || true
command -v node || true
command -v python3 || true
command -v cargo || true
test -n "${CHAT_GATEWAY_CHROMIUM_PATH:-}" && test -x "$CHAT_GATEWAY_CHROMIUM_PATH" && echo chromium_path_present || true
```

After installing the repository's locked Node dependencies on a **disposable**
Linux test environment, verify the Playwright Chromium executable and required
runtime libraries/fonts are actually available; an installed Node package or
an empty browser cache is not proof that Chromium can start. Use the native
fixture runners from [browser-login/README.md](../browser-login/README.md) to
validate the browser sandbox and temporary SSH transport, with no provider
secrets or production volumes. Do not use `--no-sandbox`, privileged Docker
containers, or weaken the MCP host to make the test pass.

If `ssh`, Chromium, the browser sandbox, or the native build prerequisites
are absent, record **Not run — missing <prerequisite>** for the corresponding
native-browser or SSH acceptance stage. HTTP-only tests can run separately
where supported but must never be called browser/SSH E2E. Do not close live
acceptance issues or install a release based on skipped tests. The Docker
host's CLI or a successful GitHub Actions fixture run cannot substitute for
an MCP sandbox capability check.

## 4. Merge only after both feature gates pass

Merge the PR only when all required CI checks on its current head and the MCP sandbox E2E both pass and are recorded. Do not merge around failing or pending checks. A direct push to `main` bypasses this workflow; make feature changes through reviewed PRs.

## 5. Update the local project and test merged `main`

- Fetch and fast-forward the local project checkout to `origin/main`. Confirm it is on `main`, clean, and at the expected merge commit.
- Run `bash scripts/docker-main-e2e.sh` with `CHAT_GATEWAY_E2E_COMMIT` set to that merge commit. It builds the actual release Docker image before checking the entrypoint and HTTP, pairing, and graceful-shutdown behavior.
- Follow every applicable item in [`TESTING_CHECKLIST.md`](TESTING_CHECKLIST.md), including the MCP sandbox HTTP acceptance against the merged release. Print the workspace and commit before every test. If a main E2E item fails, fix the issue in a new feature PR; do not install the failing build.

## 6. Build and install the validated merged version

Only after all applicable merged-main E2E checklist items pass:

1. Build the current merged code with `bash scripts/docker-run.sh build`.
2. Install/start it with `bash scripts/docker-run.sh restart` (or use the documented deployment procedure for the target environment).
3. Confirm the running image and `/health` status. Preserve the existing Docker data, sessions, and vault-key volumes.

## PR checklist

Every PR must state the tested head commit and provide:

- [ ] Unit, formatting, lint, audit, and all required CI results for the current feature commit.
- [ ] MCP sandbox E2E workspace, tested commit, scenarios, and results.
- [ ] Merge commit and completed merged-main E2E checklist results.
- [ ] Final local build/install and health-check result, or a clear statement that installation is intentionally pending.
