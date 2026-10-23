package router

import (
	"context"
	"testing"

	"go-proxy/internal/config"
	"go-proxy/internal/providers"
	"go-proxy/internal/types"
)

type testProvider struct {
	name string
}

func (p *testProvider) Name() string { return p.name }

func (p *testProvider) Models() []string {
	switch p.name {
	case "qwen":
		return []string{"qwen", "qwen-plus"}
	case "metaai":
		return []string{"meta-ai"}
	default:
		return nil
	}
}

func (p *testProvider) SendMessage(context.Context, string, string, []types.Message) (<-chan string, string, error) {
	return nil, "", nil
}

func (p *testProvider) IsHealthy(context.Context) bool { return true }

func TestResolveAllRejectsUnknownModel(t *testing.T) {
	cfg := config.DefaultConfig()
	providers := []providers.Provider{
		&testProvider{name: "qwen"},
		&testProvider{name: "metaai"},
	}
	registry := NewRegistry(cfg, providers)

	if got := registry.ResolveAll("not-a-real-model"); got != nil {
		t.Fatalf("ResolveAll(unknown) returned %d providers, want nil", len(got))
	}
}

func TestResolveAllKeepsConfiguredFallbackOrder(t *testing.T) {
	cfg := config.DefaultConfig()
	providers := []providers.Provider{
		&testProvider{name: "qwen"},
		&testProvider{name: "metaai"},
	}
	registry := NewRegistry(cfg, providers)

	got := registry.ResolveAll("qwen-plus")
	if len(got) != 2 {
		t.Fatalf("ResolveAll(configured) returned %d providers, want 2", len(got))
	}
	want := []string{"qwen", "metaai"}
	for i, provider := range got {
		if provider.Name() != want[i] {
			t.Fatalf("provider %d = %q, want %q", i, provider.Name(), want[i])
		}
	}
}
