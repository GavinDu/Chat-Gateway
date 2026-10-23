package router

import (
	"context"
	"fmt"
	"strings"
	"sync"
	"time"

	"go-proxy/internal/config"
	"go-proxy/internal/providers"

	"github.com/rs/zerolog/log"
)

// ProviderInfo is a snapshot of a single provider's registry state.
type ProviderInfo struct {
	Name                  string
	Priority              int
	Models                []string
	ConfiguredModels      []string
	DiscoveredModels      []string
	ModelSource           string
	ModelsLastRefreshedAt int64
	ModelsLastAttemptAt   int64
	ModelsLastError       string
	ModelsStale           bool
	ModelsRefreshing      bool
	Degraded              bool
}

type modelRoute struct {
	Provider      providers.Provider
	ProviderModel string
}

// Registry maps model aliases to providers and tracks health state.
type Registry struct {
	mu                   sync.RWMutex
	providers            []providers.Provider
	modelMap             map[string]modelRoute        // lowercase public model ID -> provider route
	degraded             map[string]bool              // providerName -> degraded
	priorityList         []providers.Provider         // ordered by priority (ascending)
	timeouts             map[string]time.Duration     // providerName -> configured timeout
	priorities           map[string]int               // providerName -> configured priority number
	models               map[string][]string          // providerName -> effective model aliases
	configuredModels     map[string][]string          // providerName -> config aliases
	discoveredModels     map[string][]string          // providerName -> last successful page discovery
	modelCatalog         map[string]modelCatalogState // providerName -> discovery metadata
	modelCachePath       string
	modelRefreshInterval time.Duration
}

// NewRegistry builds the registry from config and a list of instantiated providers.
func NewRegistry(cfg *config.Config, provs []providers.Provider) *Registry {
	r := &Registry{
		modelMap:             make(map[string]modelRoute),
		degraded:             make(map[string]bool),
		timeouts:             make(map[string]time.Duration),
		priorities:           make(map[string]int),
		models:               make(map[string][]string),
		configuredModels:     make(map[string][]string),
		discoveredModels:     make(map[string][]string),
		modelCatalog:         make(map[string]modelCatalogState),
		modelRefreshInterval: 24 * time.Hour,
	}

	sorted := cfg.EnabledProvidersSorted()
	for _, pc := range sorted {
		for _, p := range provs {
			if p.Name() == pc.Name {
				r.priorityList = append(r.priorityList, p)
				// Store the configured timeout; default 120s if unset.
				secs := pc.TimeoutSeconds
				if secs <= 0 {
					secs = 120
				}
				r.timeouts[pc.Name] = time.Duration(secs) * time.Second
				r.priorities[pc.Name] = pc.Priority
				r.configuredModels[pc.Name] = append([]string(nil), pc.Models...)
				r.models[pc.Name] = append([]string(nil), pc.Models...)
				r.modelCatalog[pc.Name] = modelCatalogState{Source: "configured"}
				break
			}
		}
	}
	r.providers = provs
	r.rebuildProviderModelsLocked()
	return r
}

// TimeoutFor returns the per-provider configured request timeout.
func (r *Registry) TimeoutFor(providerName string) time.Duration {
	r.mu.RLock()
	defer r.mu.RUnlock()
	if d, ok := r.timeouts[providerName]; ok {
		return d
	}
	return 120 * time.Second
}

// ResolveAll returns the ordered list of providers to try for the given model.
// If model is "" or "auto", the full priority list is returned.
// If a specific configured model is named, its provider is first, followed by
// the remaining priority-ordered providers as fallbacks. Unknown model names
// return nil so the API can reject them without opening provider browsers.
func (r *Registry) ResolveAll(model string) []providers.Provider {
	r.mu.RLock()
	defer r.mu.RUnlock()

	model = strings.ToLower(strings.TrimSpace(model))

	if model == "" || model == "auto" {
		return r.copyPriorityList()
	}

	route, ok := r.resolveModelRouteLocked(model)
	if ok {
		return r.providerFirst(route.Provider)
	}

	// An explicit unknown model must not silently fall back to an unrelated
	// provider or spend request time opening browsers.
	return nil
}

// ProviderModel returns the on-page model label for the selected provider. A
// collision-qualified public ID is translated back to its raw provider label.
// Fallback providers receive their default alias rather than another
// provider's qualified model ID.
func (r *Registry) ProviderModel(publicModelID, providerName string) string {
	r.mu.RLock()
	defer r.mu.RUnlock()

	modelID := strings.ToLower(strings.TrimSpace(publicModelID))
	if modelID == "" || modelID == "auto" {
		return providerName
	}
	route, ok := r.resolveModelRouteLocked(modelID)
	if !ok {
		return publicModelID
	}
	if route.Provider.Name() == providerName {
		return route.ProviderModel
	}
	return providerName
}

func (r *Registry) resolveModelRouteLocked(modelID string) (modelRoute, bool) {
	if route, ok := r.modelMap[modelID]; ok {
		return route, true
	}

	var match modelRoute
	found := false
	for alias, route := range r.modelMap {
		if !strings.HasPrefix(alias, modelID) && !strings.HasPrefix(modelID, alias) {
			continue
		}
		if found && (match.Provider.Name() != route.Provider.Name() || !strings.EqualFold(match.ProviderModel, route.ProviderModel)) {
			return modelRoute{}, false
		}
		match = route
		found = true
	}
	return match, found
}

// providerFirst returns the priority list with the given provider moved to front.
func (r *Registry) providerFirst(preferred providers.Provider) []providers.Provider {
	result := make([]providers.Provider, 0, len(r.priorityList))
	result = append(result, preferred)
	for _, p := range r.priorityList {
		if p.Name() != preferred.Name() {
			result = append(result, p)
		}
	}
	return result
}

func (r *Registry) copyPriorityList() []providers.Provider {
	out := make([]providers.Provider, len(r.priorityList))
	copy(out, r.priorityList)
	return out
}

// IsDegraded returns true if the provider is currently marked degraded.
func (r *Registry) IsDegraded(name string) bool {
	r.mu.RLock()
	defer r.mu.RUnlock()
	return r.degraded[name]
}

// MarkDegraded marks a provider as unavailable.
func (r *Registry) MarkDegraded(name string) {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.degraded[name] = true
	log.Warn().Str("provider", name).Msg("provider marked degraded")
}

// MarkHealthy clears the degraded flag.
func (r *Registry) MarkHealthy(name string) {
	r.mu.Lock()
	defer r.mu.Unlock()
	if r.degraded[name] {
		delete(r.degraded, name)
		log.Info().Str("provider", name).Msg("provider restored to healthy")
	}
}

// AllProviders returns all registered providers.
func (r *Registry) AllProviders() []providers.Provider {
	return r.providers
}

// GetProviderInfos returns a snapshot of every provider's config and health state.
func (r *Registry) GetProviderInfos() []ProviderInfo {
	r.mu.RLock()
	defer r.mu.RUnlock()
	out := make([]ProviderInfo, 0, len(r.providers))
	for _, p := range r.providers {
		name := p.Name()
		catalog := r.modelCatalog[name]
		out = append(out, ProviderInfo{
			Name:                  name,
			Priority:              r.priorities[name],
			Models:                append([]string(nil), r.models[name]...),
			ConfiguredModels:      append([]string(nil), r.configuredModels[name]...),
			DiscoveredModels:      append([]string(nil), r.discoveredModels[name]...),
			ModelSource:           catalog.Source,
			ModelsLastRefreshedAt: unixTime(catalog.LastRefreshedAt),
			ModelsLastAttemptAt:   unixTime(catalog.LastAttemptAt),
			ModelsLastError:       catalog.LastError,
			ModelsStale:           catalogStale(catalog, r.modelRefreshInterval, time.Now()),
			ModelsRefreshing:      catalog.Refreshing,
			Degraded:              r.degraded[name],
		})
	}
	return out
}

// UpdatePriority stores the new priority number for a provider so that
// GetProviderInfos reflects the saved value immediately. The actual routing
// order (priorityList) is built at startup and requires a restart to change.
func (r *Registry) UpdatePriority(providerName string, priority int) {
	r.mu.Lock()
	defer r.mu.Unlock()
	r.priorities[providerName] = priority
}

// UpdateModels replaces the in-memory model alias mapping for a provider.
// This takes effect immediately for new requests; priority changes still
// require a server restart (the priorityList is built at startup).
func (r *Registry) UpdateModels(providerName string, newModels []string) {
	r.mu.Lock()
	defer r.mu.Unlock()

	// Find the provider instance.
	var target providers.Provider
	for _, p := range r.providers {
		if p.Name() == providerName {
			target = p
			break
		}
	}
	if target == nil {
		return
	}

	r.configuredModels[providerName] = cleanModelNames(newModels)
	r.rebuildProviderModelsLocked()
}

// StartHealthChecker runs a background loop that periodically calls IsHealthy
// on each provider, updating degraded state accordingly.
func (r *Registry) StartHealthChecker(ctx context.Context, interval time.Duration) {
	go func() {
		ticker := time.NewTicker(interval)
		defer ticker.Stop()
		for {
			select {
			case <-ctx.Done():
				return
			case <-ticker.C:
				r.mu.RLock()
				list := r.copyPriorityList()
				r.mu.RUnlock()

				for _, p := range list {
					healthy := p.IsHealthy(ctx)
					if healthy {
						r.MarkHealthy(p.Name())
					} else {
						r.MarkDegraded(p.Name())
					}
				}
			}
		}
	}()
	log.Info().Dur("interval", interval).Msg("provider health checker started")
}

// FirstAvailable returns the first non-degraded provider from the list, or an error.
func FirstAvailable(candidates []providers.Provider, isDegraded func(string) bool) (providers.Provider, error) {
	for _, p := range candidates {
		if !isDegraded(p.Name()) {
			return p, nil
		}
	}
	// All degraded — return the first anyway as a last resort.
	if len(candidates) > 0 {
		return candidates[0], fmt.Errorf("all providers degraded, using %s as last resort", candidates[0].Name())
	}
	return nil, fmt.Errorf("no providers configured")
}
