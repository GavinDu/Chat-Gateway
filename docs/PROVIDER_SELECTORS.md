# Provider selector overrides

Each `providers[]` entry can optionally specify `selectors` lists for `input`, `send`, `response`, and `picker`. Omitted roles retain the built-in defaults. A specified role replaces only that role's complete fallback list. Changes take effect when the gateway is restarted after editing its mounted YAML configuration; no source rebuild is necessary.

Example:

```yaml
providers:
  - name: chatgpt
    enabled: true
    selectors:
      input:
        - '#prompt-textarea'
        - 'textarea#mobile-composer-prompt[name="prompt"]'
      send:
        - 'button[data-testid="send-button"]'
        - 'form:has(textarea#mobile-composer-prompt[name="prompt"]) button[aria-label="Send message"]'
```

Selectors are CSS; `text:TAG=label` is supported only for picker roles. Keep send selectors scoped to the active composer form, rather than using generic submit-button or SVG matches. Do not configure selectors with account-specific tokens. The validator rejects empty lists, control characters, overly long lists, and some unsafe generic send patterns. An override cannot resolve a missing JavaScript bundle, an invalid authentication session, or a framework that ignores synthetic input events.

Before deploying a selector update, test it with disposable public/empty-state fixtures, run relevant CI and local sandbox checks, then verify an actual authenticated provider reply separately. Never infer login from a visible composer. Preserve encrypted sessions and vault state.
