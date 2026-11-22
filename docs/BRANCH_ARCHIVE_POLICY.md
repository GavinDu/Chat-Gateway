# Branch cleanup: loss-preserving archival
The 2026-11-21 branch reconciliation distinguishes **merged** from **archived**.
Archived branches are **not** declared integrated into `main`. A verified Git tag
`archive/branches-20261121/<original-branch>` points at the exact deleted branch
HEAD, preserving every commit and tree for later review or selective cherry-pick. Archive tags are Git refs and remain mutable by users with sufficient permissions; repository policy should protect them from force-updates and deletion.

## Protection
- Exclude `main`, protected refs, all open PR heads and bases, and active `consolidate/` branches.
- Archive only tips dated before 2026-11-14 UTC that are at least 500 commits behind `main`.
- Create the tag and read back its immutable target before deleting a branch.
- Re-read branch HEAD immediately before deletion and re-check open PR dependencies.
- If the tag has a conflicting SHA, the branch changed, or any check is uncertain, retain the branch.
- Deletion is a ref cleanup, **not** feature integration; use the audit report for remaining unique diffs.
- GitHub does not provide an atomic compare-and-delete for Git refs. Avoid concurrent branch writers
  during cleanup; recovered tags remain even if the final delete fails.

## Recovery
Browse GitHub Tags and inspect `archive/branches-20261121/<original-branch>`.
To recover an old working branch locally:

```sh
git fetch origin 'refs/tags/archive/branches-20261121/*:refs/tags/archive/branches-20261121/*'
git switch -c recovered-branch 'refs/tags/archive/branches-20261121/feat/example'
```

Review the diff against the current `main`, port safe changes to a **new**
small feature PR, require current-head CI and local MCP sandbox E2E per
`docs/DEVELOPMENT_WORKFLOW.md`, and merge only when both gates pass.

## Report
The workflow emits a per-branch matrix in its GitHub Actions job summary
and a machine-readable `BRANCH_ARCHIVE_JSON` line in the job log.
