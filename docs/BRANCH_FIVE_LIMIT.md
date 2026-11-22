# Consolidation to five total branches

**User goal:** `main` plus four working branch heads. Archiving a branch is NOT
merging its code, resolving its GitHub issue, or passing local MCP sandbox E2E.

## Retained workstreams

| Active branch | Related PR | Focus |
|---|---:|---|
| `consolidate/replay-cookie-fidelity-20261121` | 1573 | Replay/cookie fidelity |
| `fix/chatgpt-composer-investigation` | 944 | ChatGPT composer and provider execution |
| `fix/native-provider-flow-20261118` | 1417 | Meta AI/native browser runtime |
| `fix/fence-diagnosis-evidence-file-identity-1525` | 1526 | Credential diagnosis security |

## All other branch heads

One-shot `.github/workflows/reconcile-to-five-branches.yml` creates and verifies
`archive/max5-20261121/<original-branch-name>` at the exact original SHA for
every remaining branch, then rechecks the head SHA. For unfinished open PRs,
the workflow records the archive tag and source commit in a PR comment,
administratively closes the PR (NOT merged), and verifies closure before removing
its branch ref. The underlying GitHub issue remains open.

The run exports JSON/CSV describing every branch, ahead/behind against current
main, changed paths, PR provenance, original SHA, archive tag, and result.

### Resume work without creating long-lived branch sprawl

1. Inspect the archived PR and the corresponding tag; do not assume historical
   changes still fit `main`.
2. Rebase or selectively cherry-pick only compatible atomic changes into one
   of the four workstreams; avoid blindly merging unrelated old histories.
3. Run source checks, all current-head GitHub Actions gates, and owner-local
   MCP sandbox E2E as described in `docs/DEVELOPMENT_WORKFLOW.md`.
4. Merge to `main` only on verified acceptance and clean up the merged branch.
   Temporary PR branches may temporarily exceed five; restore the limit promptly.

### Safety

SHA-verified archive tags preserve commits until changed/deleted by a writer
authorized to modify refs; use GitHub rulesets to protect archival tag patterns.
GitHub does not provide an atomic compare-and-delete for branch refs; avoid
concurrent branch writes during the one-shot operation.
