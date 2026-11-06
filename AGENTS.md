# Repository workflow requirements

Follow [`docs/DEVELOPMENT_WORKFLOW.md`](docs/DEVELOPMENT_WORKFLOW.md) for every code change. Work on a feature branch and open a pull request; never push feature changes directly to `main`. Require all CI checks and local MCP sandbox E2E to pass before merging. After the merge, update the local project from `origin/main`, run the complete main E2E checklist, then build and install the merged version. Do not install a feature-branch build as the latest release.

Before reporting completion, include the feature PR and commit, passing CI checks, MCP sandbox E2E commit/results, merge commit, final main E2E results, and local build/install result. If any gate fails, fix it and rerun that gate before proceeding.
