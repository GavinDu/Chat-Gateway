## Validation before merge

- [ ] Unit, format, lint, audit, and all required GitHub Actions checks passed on this PR head: `<commit>`.
- [ ] Local MCP sandbox E2E passed against the candidate from `<commit>`.
- MCP sandbox workspace: `/workspace/chat-gateway`
- E2E report or summary: `<link or concise results>`

## Complete after merge

- Merge commit: `<commit>`
- [ ] Fast-forwarded the local project checkout to that commit.
- [ ] All applicable steps in `docs/TESTING_CHECKLIST.md` passed against merged `main`.
- [ ] Built and installed the merged release; `/health` passed.
- Local E2E report: `<link or concise results>`
