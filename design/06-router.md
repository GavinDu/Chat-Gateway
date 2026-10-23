# Chapter 6 — Router & Health

---

## 6.1 Registry `[DONE]`

**File:** `internal/router/router.go`

The `Registry` is the central coordination layer. It is built once at startup and shared between the API handler and the dashboard handler via a pointer.

```go
type Registry struct {
    providers    []providers.Provider      // all registered providers
    modelMap     map[string]providers.Provider  // lowercase alias -> provider
    degraded     map[string]bool           // providerName -> degraded flag
    priorityList []providers.Provider      // ordered by priority (ascending)
    timeouts     map[string]time.Duration  // providerName -> timeout
    priorities   map[string]int            // providerName -> priority number
    models       map[string][]string       // providerName -> alias list
    mu           sync.RWMutex
}
```

### 6.1.1 Model Resolution Algorithm `[DONE]`

```go
func (r *Registry) ResolveAll(model string) []providers.Provider
```

Given a model alias from the request, returns an ordered `[]Provider` to try:

| Input | Result |
|---|---|
| `""` or `"auto"` | Full `priorityList` |
| Exact alias match (case-insensitive) | Matching provider first, rest of priority list appended |
| Prefix match (alias starts with model OR model starts with alias) | Same as exact match |
| No match | No providers; the API rejects the model before opening a browser |

The caller (API handler) iterates this list, skipping degraded providers, until one succeeds.

### 6.1.2 Priority List `[DONE]`

`priorityList` is built once in `NewRegistry` by iterating `cfg.EnabledProvidersSorted()` (insertion sort by `priority` field, ascending). The list is immutable after construction.

**Implication:** changing a provider's `priority` via the dashboard writes the config file and updates the `priorities` map (for display), but the actual routing order doesn't change until the server is restarted.

### 6.1.3 `MarkDegraded` / `MarkHealthy` `[DONE]`

```go
func (r *Registry) MarkDegraded(name string)
func (r *Registry) MarkHealthy(name string)
func (r *Registry) IsDegraded(name string) bool
```

`MarkDegraded` is called by the API handler when:
- `provider.SendMessage` returns a non-nil error.
- `SendMessage` returns a channel that closes with zero tokens.

`MarkHealthy` is called by the background health checker (§6.2) when `IsHealthy` returns `true`.

Both methods hold the write lock. `IsDegraded` holds the read lock.

---

## 6.2 Health Checker `[DONE]`

### 6.2.1 Checker Loop `[DONE]`

```go
func (r *Registry) StartHealthChecker(ctx context.Context, interval time.Duration)
```

Runs a background goroutine that ticks every `interval` (default: 5 minutes). On each tick it calls `IsHealthy` on every provider in the priority list.

- If healthy: `MarkHealthy` (clears degraded flag, logs if the provider was previously degraded).
- If unhealthy: `MarkDegraded` (sets flag, logs).

The goroutine exits when `ctx` is cancelled (server shutdown).

### 6.2.2 `IsHealthy` per Provider `[DONE]`

Each provider's `IsHealthy` implementation:
1. Calls `pool.Get(name)` to get the browser entry.
2. Acquires `entry.Lock()`.
3. Opens a new stealth page at the provider's URL via `browser.NewPage`.
4. Calls a bounded page load wait.
5. Checks the URL for login keywords using `IsLoginPage`.
6. Closes the page.
7. Returns `true` if not on a login page.

**Cost:** Each `IsHealthy` call opens and closes a real browser tab while holding that
provider's browser lock. With the current two enabled providers and a five-minute
interval, this is two real tab navigations every five minutes. A check that overlaps a
request can add queueing delay, so Chapter 12 recommends staggered, cached, or
lightweight health probes.

---

## 6.3 Live Config Updates `[PARTIAL]`

### 6.3.1 Model Alias Hot-Swap `[DONE]`

```go
func (r *Registry) UpdateModels(providerName string, newModels []string)
```

When the dashboard saves a provider config, `UpdateModels` is called immediately:
1. Removes all existing `modelMap` entries belonging to the provider.
2. Inserts new entries for each alias in `newModels`.
3. Updates the `models` map.

New requests use the updated aliases within milliseconds. No restart required.

### 6.3.2 Priority Restart Requirement `[DONE]`

```go
func (r *Registry) UpdatePriority(providerName string, priority int)
```

Updates the `priorities` map for display purposes only. The `priorityList` (which determines actual routing order) is NOT updated.

The dashboard tracks which providers have had priority edits since startup in `priorityEdited` and shows a `requires_restart` badge on those provider cards.

## 6.4 Performance implications

Routing itself is in-memory and inexpensive. Most latency occurs after selection:
waiting for the provider-wide browser lock, browser or page startup, selector fallback,
provider generation, and DOM response completion. A failed first provider adds its
entire attempt latency before fallback begins. Attempt history and request duration are
therefore essential when diagnosing slow responses.

See [Chapter 12](./12-performance.md) for the end-to-end flow and prioritized
improvements.
