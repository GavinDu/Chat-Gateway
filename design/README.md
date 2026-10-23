# Go Proxy design notes

These notes describe the implemented Go + Rod/CDP proxy and its supported production
providers: **Qwen and Meta AI**.

| Document | Coverage |
|---|---|
| [01 Architecture](./01-architecture.md) | Components and request flow |
| [02 Configuration](./02-configuration.md) | YAML, profiles, and manual sessions |
| [03 API](./03-api.md) | OpenAI-compatible endpoints |
| [04 Browser](./04-browser.md) | Chromium, Rod/CDP, and session capture |
| [05 Providers](./05-providers.md) | Qwen and Meta AI adapters |
| [06 Router](./06-router.md) | Model resolution, fallback, and health |
| [07 Dashboard](./07-dashboard.md) | Status, settings, and manual login |
| [08 Diagnostics](./08-diagnostics.md) | Manual login and adapter checks |
| [09 Backlog](./09-backlog.md) | Deferred work, including Playwright |
| [10 CEF evaluation](./10-cef-evaluation.md) | Historical evaluation; CEF rejected |
| [11 Beta test plan](./11-beta-test-plan.md) | Repeatable beta cases and current execution record |
| [12 Performance](./12-performance.md) | Current request flow, bottlenecks, benchmarks, and improvements |

The automation stack does not include automatic authentication, mailbox access,
magic-link handling, CAPTCHA solving/relaying, inactive adapters, or CEF runtime/source
work.