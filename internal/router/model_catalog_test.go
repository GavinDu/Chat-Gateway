package router

import (
	"context"
	"errors"
	"os"
	"path/filepath"
	"sync"
	"testing"
	"time"

	"go-proxy/internal/config"
	"go-proxy/internal/providers"
)

type discoveringTestProvider struct {
	*testProvider
	discovered []string
	err        error
	mu         sync.Mutex
	calls      int
	started    chan struct{}
	release    chan struct{}
}

func (p *discoveringTestProvider) DiscoverModels(context.Context) ([]string, error) {
	p.mu.Lock()
	p.calls++
	p.mu.Unlock()
	if p.started != nil {
		select {
		case p.started <- struct{}{}:
		default:
		}
	}
	if p.release != nil {
		<-p.release
	}
	return p.discovered, p.err
}

func (p *discoveringTestProvider) callCount() int {
	p.mu.Lock()
	defer p.mu.Unlock()
	return p.calls
}

func TestRefreshProviderCombinesConfiguredAndDiscoveredModels(t *testing.T) {
	cfg := configForTestProvider("qwen", []string{"qwen", "qwen-plus"})
	provider := &discoveringTestProvider{
		testProvider: &testProvider{name: "qwen"},
		discovered:   []string{"Qwen Latest", "qwen-plus", " Qwen Coder "},
	}
	registry := NewRegistry(cfg, []providers.Provider{provider})
	cachePath := filepath.Join(t.TempDir(), "models-cache.json")
	if err := registry.LoadModelCache(cachePath); err != nil {
		t.Fatalf("LoadModelCache() error = %v", err)
	}

	if err := registry.RefreshProvider(context.Background(), "qwen"); err != nil {
		t.Fatalf("RefreshProvider() error = %v", err)
	}

	info := registry.GetProviderInfos()[0]
	want := []string{"qwen", "qwen-plus", "Qwen Latest", "Qwen Coder"}
	if !equalStrings(info.Models, want) {
		t.Fatalf("effective models = %#v, want %#v", info.Models, want)
	}
	if info.ModelSource != "discovered" || info.ModelsLastRefreshedAt == 0 || info.ModelsStale {
		t.Fatalf("unexpected catalog status: %#v", info)
	}
	if got := registry.ResolveAll("Qwen Latest"); len(got) != 1 || got[0].Name() != "qwen" {
		t.Fatalf("discovered model did not route to qwen: %#v", got)
	}

	restored := NewRegistry(cfg, []providers.Provider{&testProvider{name: "qwen"}})
	if err := restored.LoadModelCache(cachePath); err != nil {
		t.Fatalf("restored LoadModelCache() error = %v", err)
	}
	restoredInfo := restored.GetProviderInfos()[0]
	if restoredInfo.ModelSource != "cache" || !equalStrings(restoredInfo.Models, want) {
		t.Fatalf("restored catalog = %#v", restoredInfo)
	}
}

func TestRefreshProviderFailureRetainsConfiguredAliases(t *testing.T) {
	cfg := configForTestProvider("qwen", []string{"qwen", "qwen-plus"})
	provider := &discoveringTestProvider{
		testProvider: &testProvider{name: "qwen"},
		err:          errors.New("model menu unavailable"),
	}
	registry := NewRegistry(cfg, []providers.Provider{provider})

	if err := registry.RefreshProvider(context.Background(), "qwen"); err == nil {
		t.Fatal("RefreshProvider() error = nil, want discovery error")
	}
	info := registry.GetProviderInfos()[0]
	if !equalStrings(info.Models, []string{"qwen", "qwen-plus"}) {
		t.Fatalf("models after failure = %#v", info.Models)
	}
	if info.ModelSource != "configured" || info.ModelsLastError == "" || !info.ModelsStale {
		t.Fatalf("unexpected failure status: %#v", info)
	}
}

func TestConcurrentRefreshPersistsEveryProvider(t *testing.T) {
	cfg := config.DefaultConfig()
	for i := range cfg.Providers {
		cfg.Providers[i].Enabled = cfg.Providers[i].Name == "qwen" || cfg.Providers[i].Name == "metaai"
	}
	qwen := &discoveringTestProvider{
		testProvider: &testProvider{name: "qwen"},
		discovered:   []string{"Qwen Current"},
		started:      make(chan struct{}, 1),
		release:      make(chan struct{}),
	}
	meta := &discoveringTestProvider{
		testProvider: &testProvider{name: "metaai"},
		discovered:   []string{"Meta Current"},
		started:      make(chan struct{}, 1),
		release:      make(chan struct{}),
	}
	registry := NewRegistry(cfg, []providers.Provider{qwen, meta})
	cachePath := filepath.Join(t.TempDir(), "models-cache.json")
	if err := registry.LoadModelCache(cachePath); err != nil {
		t.Fatal(err)
	}

	errs := make(chan error, 2)
	go func() { errs <- registry.RefreshProvider(context.Background(), "qwen") }()
	go func() { errs <- registry.RefreshProvider(context.Background(), "metaai") }()
	<-qwen.started
	<-meta.started
	close(qwen.release)
	close(meta.release)
	for range 2 {
		if err := <-errs; err != nil {
			t.Fatalf("concurrent RefreshProvider() error = %v", err)
		}
	}

	restored := NewRegistry(cfg, []providers.Provider{
		&testProvider{name: "qwen"},
		&testProvider{name: "metaai"},
	})
	if err := restored.LoadModelCache(cachePath); err != nil {
		t.Fatal(err)
	}
	infos := restored.GetProviderInfos()
	if len(infos) != 2 {
		t.Fatalf("restored provider count = %d, want 2", len(infos))
	}
	for _, info := range infos {
		if len(info.DiscoveredModels) != 1 {
			t.Fatalf("%s discovered models = %#v", info.Name, info.DiscoveredModels)
		}
	}
}

func TestDiscoveryDoesNotHoldRegistryRoutingLock(t *testing.T) {
	cfg := configForTestProvider("qwen", []string{"qwen"})
	provider := &discoveringTestProvider{
		testProvider: &testProvider{name: "qwen"},
		discovered:   []string{"Qwen Current"},
		started:      make(chan struct{}, 1),
		release:      make(chan struct{}),
	}
	registry := NewRegistry(cfg, []providers.Provider{provider})

	refreshDone := make(chan error, 1)
	go func() { refreshDone <- registry.RefreshProvider(context.Background(), "qwen") }()
	<-provider.started

	routeDone := make(chan []providers.Provider, 1)
	go func() { routeDone <- registry.ResolveAll("qwen") }()
	select {
	case got := <-routeDone:
		if len(got) != 1 || got[0].Name() != "qwen" {
			t.Fatalf("ResolveAll() = %#v", got)
		}
	case <-time.After(250 * time.Millisecond):
		t.Fatal("ResolveAll blocked behind model discovery")
	}

	close(provider.release)
	if err := <-refreshDone; err != nil {
		t.Fatal(err)
	}
}

func TestRefreshDueModelsHonorsDailyAttemptGate(t *testing.T) {
	cfg := configForTestProvider("qwen", []string{"qwen"})
	provider := &discoveringTestProvider{
		testProvider: &testProvider{name: "qwen"},
		discovered:   []string{"Qwen Current"},
	}
	registry := NewRegistry(cfg, []providers.Provider{provider})
	registry.modelCatalog["qwen"] = modelCatalogState{
		LastAttemptAt: time.Now().UTC(),
		Source:        "configured",
	}

	registry.refreshDueModels(context.Background(), 24*time.Hour)
	if calls := provider.callCount(); calls != 0 {
		t.Fatalf("fresh attempt count = %d, want 0", calls)
	}

	state := registry.modelCatalog["qwen"]
	state.LastAttemptAt = time.Now().Add(-25 * time.Hour)
	registry.modelCatalog["qwen"] = state
	registry.refreshDueModels(context.Background(), 24*time.Hour)
	if calls := provider.callCount(); calls != 1 {
		t.Fatalf("stale attempt count = %d, want 1", calls)
	}

	if err := registry.RefreshProvider(context.Background(), "qwen"); err != nil {
		t.Fatal(err)
	}
	if calls := provider.callCount(); calls != 2 {
		t.Fatalf("explicit refresh count = %d, want 2", calls)
	}
}

func TestLoadModelCacheRejectsMalformedAndIgnoresEmptyModels(t *testing.T) {
	cfg := configForTestProvider("qwen", []string{"qwen"})
	registry := NewRegistry(cfg, []providers.Provider{&testProvider{name: "qwen"}})
	cachePath := filepath.Join(t.TempDir(), "models-cache.json")

	if err := os.WriteFile(cachePath, []byte("{"), 0600); err != nil {
		t.Fatal(err)
	}
	if err := registry.LoadModelCache(cachePath); err == nil {
		t.Fatal("LoadModelCache(malformed) error = nil")
	}

	emptyCache := `{"version":1,"providers":{"qwen":{"models":["   "]}}}`
	if err := os.WriteFile(cachePath, []byte(emptyCache), 0600); err != nil {
		t.Fatal(err)
	}
	if err := registry.LoadModelCache(cachePath); err != nil {
		t.Fatal(err)
	}
	info := registry.GetProviderInfos()[0]
	if info.ModelSource != "configured" || len(info.DiscoveredModels) != 0 {
		t.Fatalf("empty cached models should be ignored: %#v", info)
	}
}

func TestCollidingDiscoveredModelsUseStableQualifiedRoutesAfterCacheReload(t *testing.T) {
	cfg := config.DefaultConfig()
	for i := range cfg.Providers {
		cfg.Providers[i].Enabled = cfg.Providers[i].Name == "qwen" || cfg.Providers[i].Name == "metaai"
	}
	cachePath := filepath.Join(t.TempDir(), "models-cache.json")
	registry := NewRegistry(cfg, []providers.Provider{
		&discoveringTestProvider{testProvider: &testProvider{name: "qwen"}, discovered: []string{"Shared Model"}},
		&discoveringTestProvider{testProvider: &testProvider{name: "metaai"}, discovered: []string{"Shared Model"}},
	})
	if err := registry.LoadModelCache(cachePath); err != nil {
		t.Fatal(err)
	}
	if err := registry.RefreshProvider(context.Background(), "qwen"); err != nil {
		t.Fatal(err)
	}
	if err := registry.RefreshProvider(context.Background(), "metaai"); err != nil {
		t.Fatal(err)
	}

	restored := NewRegistry(cfg, []providers.Provider{
		&testProvider{name: "qwen"},
		&testProvider{name: "metaai"},
	})
	if err := restored.LoadModelCache(cachePath); err != nil {
		t.Fatal(err)
	}

	wantOwner := map[string]string{
		"qwen:Shared Model":   "qwen",
		"metaai:Shared Model": "metaai",
	}
	seen := make(map[string]string)
	for _, info := range restored.GetProviderInfos() {
		for _, modelID := range info.Models {
			if owner, wanted := wantOwner[modelID]; wanted {
				seen[modelID] = info.Name
				if info.Name != owner {
					t.Fatalf("model %q owner = %q, want %q", modelID, info.Name, owner)
				}
			}
			if modelID == "Shared Model" {
				t.Fatal("ambiguous unqualified model ID was advertised")
			}
		}
	}
	if len(seen) != len(wantOwner) {
		t.Fatalf("qualified collision IDs = %#v, want %#v", seen, wantOwner)
	}

	for modelID, owner := range wantOwner {
		candidates := restored.ResolveAll(modelID)
		if len(candidates) == 0 || candidates[0].Name() != owner {
			t.Fatalf("ResolveAll(%q) first owner = %#v, want %q", modelID, candidates, owner)
		}
		if providerModel := restored.ProviderModel(modelID, owner); providerModel != "Shared Model" {
			t.Fatalf("ProviderModel(%q, %q) = %q", modelID, owner, providerModel)
		}
	}
	if candidates := restored.ResolveAll("Shared Model"); candidates != nil {
		t.Fatalf("ambiguous unqualified model resolved to %#v", candidates)
	}
}

func TestConfiguredAliasStaysUnqualifiedWhenDiscoveryCollides(t *testing.T) {
	cfg := config.DefaultConfig()
	for i := range cfg.Providers {
		cfg.Providers[i].Enabled = cfg.Providers[i].Name == "qwen" || cfg.Providers[i].Name == "metaai"
	}
	registry := NewRegistry(cfg, []providers.Provider{
		&testProvider{name: "qwen"},
		&discoveringTestProvider{testProvider: &testProvider{name: "metaai"}, discovered: []string{"qwen"}},
	})
	if err := registry.LoadModelCache(filepath.Join(t.TempDir(), "models-cache.json")); err != nil {
		t.Fatal(err)
	}
	if err := registry.RefreshProvider(context.Background(), "metaai"); err != nil {
		t.Fatal(err)
	}

	owners := make(map[string]string)
	for _, info := range registry.GetProviderInfos() {
		for _, modelID := range info.Models {
			owners[modelID] = info.Name
		}
	}
	if owners["qwen"] != "qwen" {
		t.Fatalf("configured qwen alias owner = %q", owners["qwen"])
	}
	if owners["metaai:qwen"] != "metaai" {
		t.Fatalf("qualified Meta AI collision owner = %q", owners["metaai:qwen"])
	}
	if candidates := registry.ResolveAll("qwen"); len(candidates) == 0 || candidates[0].Name() != "qwen" {
		t.Fatalf("configured alias route = %#v", candidates)
	}
}

func configForTestProvider(name string, models []string) *config.Config {
	cfg := config.DefaultConfig()
	for i := range cfg.Providers {
		cfg.Providers[i].Enabled = cfg.Providers[i].Name == name
		if cfg.Providers[i].Name == name {
			cfg.Providers[i].Models = models
		}
	}
	return cfg
}

func equalStrings(got, want []string) bool {
	if len(got) != len(want) {
		return false
	}
	for i := range got {
		if got[i] != want[i] {
			return false
		}
	}
	return true
}
