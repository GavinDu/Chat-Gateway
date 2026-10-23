# Chapter 10 — Historical CEF Evaluation (Rejected)

This is the sole retained CEF document. It records a past investigation and does not
describe a shipped backend, executable, source tree, build target, or migration plan.

## Evaluation

CEF could embed Chromium in a C++ companion process and would require a separate
automation RPC surface, packaging, platform runtime management, and substantial
maintenance. That complexity does not justify replacing the working Go + Rod/CDP path
for the current three-provider scope.

## Decision

**Rejected.** CEF is not part of the product, build, distribution, or backlog. The CEF
executable/source proof of concept has been removed. Browser automation remains Go + Rod
over CDP. Playwright, rather than CEF, is the only deferred automation migration under
consideration (see Chapter 9).