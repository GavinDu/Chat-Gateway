package router

import (
	"context"
	"encoding/json"
	"errors"
	"fmt"
	"os"
	"path/filepath"
	"strings"
	"sync"
	"time"

	"go-proxy/internal/providers"

	"github.com/rs/zerolog/log"
)

type modelCatalogState struct {
	DiscoveredModels []string  `json:"models,omitempty"`
	LastRefreshedAt  time.Time `json:"last_refreshed_at,omitempty"`
	LastAttemptAt    time.Time `json:"last_attempt_at,omitempty"`
	LastError        string    `json:"last_error,omitempty"`
	Source           string    `json:"-"`
	Refreshing       bool      `json:"-"`
}

type modelCacheFile struct {
	Version   int                          `json:"version"`
	Providers map[string]modelCatalogState `json:"providers"`
}

var modelRefreshWriteMu sync.Mutex

// LoadModelCache restores the last successful discovery results. A missing
// cache is normal on first startup; malformed cache data is reported so it can
// be repaired instead of silently hiding a persistence problem.
func (r *Registry) LoadModelCache(path string) error {
	r.mu.Lock()
	r.modelCachePath = path
	r.mu.Unlock()

	data, err := os.ReadFile(path)
	if errors.Is(err, os.ErrNotExist) {
		return nil
	}
	if err != nil {
		return fmt.Errorf("read model cache: %w", err)
	}

	var cache modelCacheFile
	if err := json.Unmarshal(data, &cache); err != nil {
		return fmt.Errorf("parse model cache: %w", err)
	}

	r.mu.Lock()
	defer r.mu.Unlock()
	for providerName, state := range cache.Providers {
		if _, ok := r.modelCatalog[providerName]; !ok {
			continue
		}
		state.DiscoveredModels = cleanModelNames(state.DiscoveredModels)
		if len(state.DiscoveredModels) == 0 {
			continue
		}
		provider := r.providerByNameLocked(providerName)
		if provider == nil {
			continue
		}
		state.Source = "cache"
		r.modelCatalog[providerName] = state
		r.discoveredModels[providerName] = append([]string(nil), state.DiscoveredModels...)
		r.rebuildProviderModelsLocked()
	}
	return nil
}

// StartModelRefresher loads persisted results and starts a background refresh
// loop. Refreshes never run on the request path, and each provider is attempted
// at most once per interval unless RefreshProvider is explicitly requested.
func (r *Registry) StartModelRefresher(ctx context.Context, path string, interval time.Duration) {
	if interval <= 0 {
		interval = 24 * time.Hour
	}
	r.mu.Lock()
	r.modelCachePath = path
	r.modelRefreshInterval = interval
	r.mu.Unlock()

	if err := r.LoadModelCache(path); err != nil {
		log.Warn().Err(err).Str("path", path).Msg("model cache unavailable — using configured aliases until refresh")
	}

	go func() {
		r.refreshDueModels(ctx, interval)
		checkEvery := interval
		if checkEvery > time.Hour {
			checkEvery = time.Hour
		}
		ticker := time.NewTicker(checkEvery)
		defer ticker.Stop()
		for {
			select {
			case <-ctx.Done():
				return
			case <-ticker.C:
				r.refreshDueModels(ctx, interval)
			}
		}
	}()
	log.Info().Dur("refresh_interval", interval).Str("cache", path).Msg("provider model refresher started")
}

// RefreshProvider refreshes one provider synchronously. Callers that do not
// need the result should invoke it from a goroutine, such as a session watcher.
func (r *Registry) RefreshProvider(ctx context.Context, providerName string) error {
	r.mu.RLock()
	provider := r.providerByNameLocked(providerName)
	r.mu.RUnlock()
	if provider == nil {
		return fmt.Errorf("provider %q is not registered", providerName)
	}
	return r.refreshProvider(ctx, provider, true)
}

func (r *Registry) refreshDueModels(ctx context.Context, interval time.Duration) {
	r.mu.RLock()
	providersCopy := append([]providers.Provider(nil), r.providers...)
	r.mu.RUnlock()

	for _, provider := range providersCopy {
		if ctx.Err() != nil {
			return
		}
		r.mu.RLock()
		state := r.modelCatalog[provider.Name()]
		r.mu.RUnlock()
		if !state.LastAttemptAt.IsZero() && time.Since(state.LastAttemptAt) < interval {
			continue
		}
		if err := r.refreshProvider(ctx, provider, false); err != nil {
			log.Warn().Err(err).Str("provider", provider.Name()).Msg("scheduled model discovery failed")
		}
	}
}

func (r *Registry) refreshProvider(ctx context.Context, provider providers.Provider, force bool) error {
	name := provider.Name()
	r.mu.Lock()
	state := r.modelCatalog[name]
	if state.Refreshing {
		r.mu.Unlock()
		return nil
	}
	if !force && !state.LastAttemptAt.IsZero() && time.Since(state.LastAttemptAt) < r.modelRefreshInterval {
		r.mu.Unlock()
		return nil
	}
	state.Refreshing = true
	state.LastAttemptAt = time.Now().UTC()
	r.modelCatalog[name] = state
	r.mu.Unlock()

	defer func() {
		r.mu.Lock()
		current := r.modelCatalog[name]
		current.Refreshing = false
		r.modelCatalog[name] = current
		r.mu.Unlock()
	}()

	discoverer, ok := provider.(providers.ModelDiscoverer)
	if !ok {
		return r.recordDiscoveryFailure(name, fmt.Errorf("provider does not support model discovery"))
	}

	discoverCtx, cancel := context.WithTimeout(ctx, 45*time.Second)
	defer cancel()
	models, err := discoverer.DiscoverModels(discoverCtx)
	if err != nil {
		return r.recordDiscoveryFailure(name, err)
	}
	models = cleanModelNames(models)
	if len(models) == 0 {
		return r.recordDiscoveryFailure(name, fmt.Errorf("provider returned no model options"))
	}

	r.mu.Lock()
	state = r.modelCatalog[name]
	state.DiscoveredModels = append([]string(nil), models...)
	state.LastRefreshedAt = time.Now().UTC()
	state.LastError = ""
	state.Source = "discovered"
	r.discoveredModels[name] = append([]string(nil), models...)
	r.rebuildProviderModelsLocked()
	cachePath := r.modelCachePath
	r.modelCatalog[name] = state
	r.mu.Unlock()

	if err := r.persistModelCache(cachePath); err != nil {
		log.Warn().Err(err).Str("provider", name).Msg("could not persist discovered models")
	}
	log.Info().Str("provider", name).Int("count", len(models)).Msg("provider model catalog refreshed")
	return nil
}

func (r *Registry) recordDiscoveryFailure(name string, discoveryErr error) error {
	r.mu.Lock()
	state := r.modelCatalog[name]
	state.LastError = discoveryErr.Error()
	if state.Source == "" {
		state.Source = "configured"
	}
	cachePath := r.modelCachePath
	r.modelCatalog[name] = state
	r.mu.Unlock()

	if err := r.persistModelCache(cachePath); err != nil {
		log.Warn().Err(err).Str("provider", name).Msg("could not persist model discovery error")
	}
	return discoveryErr
}

func (r *Registry) persistModelCache(path string) error {
	if path == "" {
		return nil
	}

	// Serialize the complete snapshot-through-replace boundary. Taking the
	// snapshot before this lock would allow a slower writer to overwrite a
	// newer provider result with an older whole-cache view.
	modelRefreshWriteMu.Lock()
	defer modelRefreshWriteMu.Unlock()

	r.mu.RLock()
	cache := modelCacheFile{Version: 1, Providers: make(map[string]modelCatalogState, len(r.modelCatalog))}
	for name, state := range r.modelCatalog {
		state.Source = ""
		state.Refreshing = false
		cache.Providers[name] = state
	}
	r.mu.RUnlock()

	data, err := json.MarshalIndent(cache, "", "  ")
	if err != nil {
		return fmt.Errorf("marshal model cache: %w", err)
	}

	if err := os.MkdirAll(filepath.Dir(path), 0755); err != nil {
		return fmt.Errorf("create model cache directory: %w", err)
	}
	tmp, err := os.CreateTemp(filepath.Dir(path), ".models-cache-*.tmp")
	if err != nil {
		return fmt.Errorf("create model cache temp file: %w", err)
	}
	tmpName := tmp.Name()
	defer os.Remove(tmpName)
	if err := tmp.Chmod(0600); err != nil {
		_ = tmp.Close()
		return fmt.Errorf("set model cache permissions: %w", err)
	}
	if _, err := tmp.Write(data); err != nil {
		_ = tmp.Close()
		return fmt.Errorf("write model cache: %w", err)
	}
	if err := tmp.Close(); err != nil {
		return fmt.Errorf("close model cache: %w", err)
	}
	if err := os.Rename(tmpName, path); err != nil {
		return fmt.Errorf("replace model cache: %w", err)
	}
	return nil
}

func (r *Registry) providerByNameLocked(name string) providers.Provider {
	for _, provider := range r.providers {
		if provider.Name() == name {
			return provider
		}
	}
	return nil
}

func (r *Registry) rebuildProviderModelsLocked() {
	rawByProvider := make(map[string][]string, len(r.priorityList))
	ownersByModel := make(map[string]int)
	configuredOwnerByModel := make(map[string]string)
	for _, provider := range r.priorityList {
		name := provider.Name()
		rawModels := mergeModels(r.configuredModels[name], r.discoveredModels[name])
		rawByProvider[name] = rawModels
		for _, model := range r.configuredModels[name] {
			key := strings.ToLower(model)
			if _, exists := configuredOwnerByModel[key]; !exists {
				configuredOwnerByModel[key] = name
			}
		}
		for _, model := range rawModels {
			ownersByModel[strings.ToLower(model)]++
		}
	}

	r.modelMap = make(map[string]modelRoute)
	for _, provider := range r.priorityList {
		name := provider.Name()
		publicModels := make([]string, 0, len(rawByProvider[name]))
		for _, providerModel := range rawByProvider[name] {
			publicID := providerModel
			key := strings.ToLower(providerModel)
			configuredOwner := configuredOwnerByModel[key]
			if ownersByModel[key] > 1 && configuredOwner != name {
				publicID = name + ":" + providerModel
			}
			publicModels = append(publicModels, publicID)
			r.modelMap[strings.ToLower(publicID)] = modelRoute{
				Provider:      provider,
				ProviderModel: providerModel,
			}
		}
		r.models[name] = publicModels
	}
}

func mergeModels(groups ...[]string) []string {
	var result []string
	for _, group := range groups {
		for _, model := range cleanModelNames(group) {
			found := false
			for _, existing := range result {
				if strings.EqualFold(existing, model) {
					found = true
					break
				}
			}
			if !found {
				result = append(result, model)
			}
		}
	}
	return result
}

func cleanModelNames(models []string) []string {
	result := make([]string, 0, len(models))
	for _, model := range models {
		model = strings.Join(strings.Fields(strings.TrimSpace(model)), " ")
		if model == "" {
			continue
		}
		duplicate := false
		for _, existing := range result {
			if strings.EqualFold(existing, model) {
				duplicate = true
				break
			}
		}
		if !duplicate {
			result = append(result, model)
		}
	}
	return result
}

func unixTime(t time.Time) int64 {
	if t.IsZero() {
		return 0
	}
	return t.Unix()
}

func catalogStale(state modelCatalogState, interval time.Duration, now time.Time) bool {
	return state.LastRefreshedAt.IsZero() || now.Sub(state.LastRefreshedAt) >= interval
}
