package api

import (
	"context"
	"encoding/json"
	"net/http"
	"net/http/httptest"
	"path/filepath"
	"strings"
	"testing"

	"go-proxy/internal/config"
	"go-proxy/internal/memory"
	"go-proxy/internal/providers"
	"go-proxy/internal/router"
	"go-proxy/internal/types"

	"github.com/gin-gonic/gin"
)

type catalogTestProvider struct {
	name       string
	discovered []string
	sentModel  string
	sendCount  int
}

func (p *catalogTestProvider) Name() string     { return p.name }
func (p *catalogTestProvider) Models() []string { return nil }
func (p *catalogTestProvider) IsHealthy(context.Context) bool {
	return true
}
func (p *catalogTestProvider) SendMessage(_ context.Context, model string, _ string, _ []types.Message) (<-chan string, string, error) {
	p.sentModel = model
	p.sendCount++
	ch := make(chan string, 1)
	ch <- "OK"
	close(ch)
	return ch, "test-conversation", nil
}
func (p *catalogTestProvider) DiscoverModels(context.Context) ([]string, error) {
	return p.discovered, nil
}

func TestListModelsAdvertisesCollisionSafeIDs(t *testing.T) {
	gin.SetMode(gin.TestMode)
	cfg := config.DefaultConfig()
	for i := range cfg.Providers {
		cfg.Providers[i].Enabled = cfg.Providers[i].Name == "qwen" || cfg.Providers[i].Name == "metaai"
	}
	qwen := &catalogTestProvider{name: "qwen", discovered: []string{"Shared Model"}}
	meta := &catalogTestProvider{name: "metaai", discovered: []string{"Shared Model"}}
	registry := router.NewRegistry(cfg, []providers.Provider{qwen, meta})
	if err := registry.LoadModelCache(filepath.Join(t.TempDir(), "models-cache.json")); err != nil {
		t.Fatal(err)
	}
	if err := registry.RefreshProvider(context.Background(), "qwen"); err != nil {
		t.Fatal(err)
	}
	if err := registry.RefreshProvider(context.Background(), "metaai"); err != nil {
		t.Fatal(err)
	}

	engine := gin.New()
	NewHandler(registry, nil, nil).RegisterRoutes(engine)
	request := httptest.NewRequest(http.MethodGet, "/v1/models", nil)
	response := httptest.NewRecorder()
	engine.ServeHTTP(response, request)
	if response.Code != http.StatusOK {
		t.Fatalf("GET /v1/models status = %d", response.Code)
	}

	var body ModelListResponse
	if err := json.Unmarshal(response.Body.Bytes(), &body); err != nil {
		t.Fatal(err)
	}
	owners := make(map[string]string)
	for _, model := range body.Data {
		key := strings.ToLower(model.ID)
		if _, exists := owners[key]; exists {
			t.Fatalf("duplicate model ID %q in /v1/models", model.ID)
		}
		owners[key] = model.OwnedBy
	}
	if owners["qwen:shared model"] != "qwen" {
		t.Fatalf("qwen collision owner = %q", owners["qwen:shared model"])
	}
	if owners["metaai:shared model"] != "metaai" {
		t.Fatalf("metaai collision owner = %q", owners["metaai:shared model"])
	}
	if _, ambiguous := owners["shared model"]; ambiguous {
		t.Fatal("/v1/models advertised ambiguous unqualified collision")
	}
}

func TestChatCompletionTranslatesQualifiedIDToProviderLabel(t *testing.T) {
	gin.SetMode(gin.TestMode)
	cfg := config.DefaultConfig()
	for i := range cfg.Providers {
		cfg.Providers[i].Enabled = cfg.Providers[i].Name == "qwen" || cfg.Providers[i].Name == "metaai"
	}
	qwen := &catalogTestProvider{name: "qwen", discovered: []string{"Shared Model"}}
	meta := &catalogTestProvider{name: "metaai", discovered: []string{"Shared Model"}}
	registry := router.NewRegistry(cfg, []providers.Provider{qwen, meta})
	if err := registry.LoadModelCache(filepath.Join(t.TempDir(), "models-cache.json")); err != nil {
		t.Fatal(err)
	}
	if err := registry.RefreshProvider(context.Background(), "qwen"); err != nil {
		t.Fatal(err)
	}
	if err := registry.RefreshProvider(context.Background(), "metaai"); err != nil {
		t.Fatal(err)
	}

	engine := gin.New()
	NewHandler(registry, &memory.NoopStore{}, nil).RegisterRoutes(engine)
	request := httptest.NewRequest(
		http.MethodPost,
		"/v1/chat/completions",
		strings.NewReader(`{"model":"metaai:Shared Model","messages":[{"role":"user","content":"hello"}]}`),
	)
	request.Header.Set("Content-Type", "application/json")
	response := httptest.NewRecorder()
	engine.ServeHTTP(response, request)

	if response.Code != http.StatusOK {
		t.Fatalf("POST /v1/chat/completions status = %d body = %s", response.Code, response.Body.String())
	}
	if meta.sendCount != 1 || meta.sentModel != "Shared Model" {
		t.Fatalf("Meta AI received count=%d model=%q", meta.sendCount, meta.sentModel)
	}
	if qwen.sendCount != 0 {
		t.Fatalf("Qwen unexpectedly received %d calls", qwen.sendCount)
	}
}
