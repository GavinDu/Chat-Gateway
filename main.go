package main

import (
	"context"
	"flag"
	"fmt"
	"net/http"
	"os"
	"os/signal"
	"path/filepath"
	"syscall"
	"time"

	"go-proxy/internal/api"
	"go-proxy/internal/browser"
	"go-proxy/internal/config"
	"go-proxy/internal/dashboard"
	"go-proxy/internal/hooks"
	"go-proxy/internal/memory"
	"go-proxy/internal/providers"
	"go-proxy/internal/router"

	"github.com/gin-gonic/gin"
	"github.com/rs/zerolog"
	"github.com/rs/zerolog/log"
)

func main() {
	configPath := flag.String("config", "config.yaml", "path to config file")
	loginProvider := flag.String("login", "", "open a headed browser for manual provider login")
	testAdapter := flag.String("test-adapter", "", "probe all CSS selectors for a provider and report which ones match")
	flag.Parse()

	zerolog.SetGlobalLevel(zerolog.InfoLevel)
	log.Logger = zerolog.New(os.Stderr).With().Timestamp().Logger()

	cfg, err := config.Load(*configPath)
	if err != nil {
		log.Fatal().Err(err).Msg("loading config")
	}

	if *loginProvider != "" {
		runLoginMode(*loginProvider, cfg)
		return
	}

	if *testAdapter != "" {
		runTestAdapter(*testAdapter, cfg)
		return
	}

	pool := browser.NewPool(cfg.Server.Headless, cfg.Server.ChromeProfileDir)
	for _, pc := range cfg.Providers {
		if pc.Headless != nil {
			headless := *pc.Headless
			pool.SetProviderHeadless(pc.Name, &headless)
		}
	}
	defer pool.Close()

	idleMinutes := cfg.Server.ConversationIdleMinutes
	if idleMinutes <= 0 {
		idleMinutes = 30
	}
	conversations := browser.NewConversationManager(time.Duration(idleMinutes) * time.Minute)
	provs := buildProviders(cfg, pool, conversations)

	for _, p := range provs {
		if browser.HasSession(p.Name()) {
			log.Info().Str("provider", p.Name()).Msg("session found — ready")
		} else {
			log.Warn().Str("provider", p.Name()).
				Str("action", fmt.Sprintf("./go-proxy --login %s", p.Name())).
				Msg("no session — re-login required")
		}
	}

	reg := router.NewRegistry(cfg, provs)
	ctx, cancel := context.WithCancel(context.Background())
	defer cancel()
	reg.StartHealthChecker(ctx, 5*time.Minute)
	reg.StartModelRefresher(ctx, filepath.Join(filepath.Dir(*configPath), "models-cache.json"), 24*time.Hour)

	hooksDispatcher := hooks.NewDispatcher(hooks.Config{
		ConversationStart: cfg.Hooks.ConversationStart,
		ConversationEnd:   cfg.Hooks.ConversationEnd,
		Secret:            cfg.Hooks.Secret,
	})
	if cfg.Hooks.ConversationStart != "" || cfg.Hooks.ConversationEnd != "" {
		log.Info().
			Str("conversation_start", cfg.Hooks.ConversationStart).
			Str("conversation_end", cfg.Hooks.ConversationEnd).
			Msg("hooks: lifecycle webhooks enabled")
	}

	conversations.OnEvict = func(conversationID, providerName string) {
		hooksDispatcher.FireConversationEnd(conversationID, providerName, "idle_timeout")
	}
	conversations.StartCleanup(ctx)
	log.Info().Int("idle_minutes", idleMinutes).Msg("conversation manager started")

	bc := dashboard.NewBroadcaster(500)
	sessionBc := dashboard.NewSessionBroadcaster()

	var watchedProviders []string
	for _, pc := range cfg.Providers {
		if pc.Enabled {
			watchedProviders = append(watchedProviders, pc.Name)
		}
	}
	go browser.StartSessionWatcher(ctx, watchedProviders, 2*time.Second, func(providerName string, mtime int64) {
		sessionBc.Emit(dashboard.SessionEvent{Provider: providerName, SessionSavedAt: mtime})
		go func() {
			if err := reg.RefreshProvider(ctx, providerName); err != nil {
				log.Warn().Err(err).Str("provider", providerName).Msg("model refresh after session change failed")
			}
		}()
	})

	pool.OnSessionChange = func(providerName string, mtime int64) {
		sessionBc.Emit(dashboard.SessionEvent{Provider: providerName, SessionSavedAt: mtime})
		go func() {
			if err := reg.RefreshProvider(ctx, providerName); err != nil {
				log.Warn().Err(err).Str("provider", providerName).Msg("model refresh after browser session reload failed")
			}
		}()
	}

	gin.SetMode(gin.ReleaseMode)
	engine := gin.New()
	engine.Use(gin.Recovery())
	engine.Use(requestLogger(bc))

	var memStore memory.Store = &memory.NoopStore{}
	if fcfg := cfg.Memory.FalkorDB; fcfg != nil {
		port := fcfg.Port
		if port == 0 {
			port = 6379
		}
		graph := fcfg.Graph
		if graph == "" {
			graph = "go_proxy_memory"
		}
		store, storeErr := memory.NewFalkorStore(memory.FalkorDBConfig{
			Host:     fcfg.Host,
			Port:     port,
			Password: fcfg.Password,
			Graph:    graph,
		})
		if storeErr != nil {
			log.Warn().Err(storeErr).Msg("memory: FalkorDB unavailable — falling back to no-op store (conversations will not be persisted)")
		} else {
			memStore = store
			defer store.Close()
			log.Info().
				Str("host", fcfg.Host).
				Int("port", port).
				Str("graph", graph).
				Msg("memory: FalkorDB connected")
		}
	}

	h := api.NewHandler(reg, memStore, hooksDispatcher)
	h.RegisterRoutes(engine)

	dh := dashboard.NewHandler(reg, cfg, *configPath, bc, sessionBc)
	defer dh.Close()
	dh.RegisterRoutes(engine)
	if cfg.Server.DashboardSecret == "" {
		log.Warn().Msg("dashboard_secret not set — mutation endpoints are unprotected; set server.dashboard_secret in config.yaml to restrict access")
	}

	port := cfg.Server.Port
	if p := os.Getenv("PORT"); p != "" {
		fmt.Sscanf(p, "%d", &port)
	}

	srv := &http.Server{
		Addr:    fmt.Sprintf(":%d", port),
		Handler: engine,
	}

	stop := make(chan os.Signal, 1)
	signal.Notify(stop, syscall.SIGINT, syscall.SIGTERM)

	go func() {
		log.Info().Int("port", port).Msg("go-proxy listening")
		if err := srv.ListenAndServe(); err != nil && err != http.ErrServerClosed {
			log.Fatal().Err(err).Msg("server error")
		}
	}()

	<-stop
	log.Info().Msg("shutting down...")
	cancel()

	shutCtx, shutCancel := context.WithTimeout(context.Background(), 10*time.Second)
	defer shutCancel()
	_ = srv.Shutdown(shutCtx)

	pool.Close()
	log.Info().Msg("shutdown complete")
}

func runLoginMode(providerName string, cfg *config.Config) {
	pc := cfg.ProviderByName(providerName)
	if pc == nil {
		log.Fatal().Str("provider", providerName).Msg("provider not found in config")
	}

	log.Info().Str("provider", providerName).Msg("login mode: launching headed browser")
	log.Info().Msg("Log in to the provider in the browser window, then press Ctrl+C to save your session.")

	pool := browser.NewPool(false, cfg.Server.ChromeProfileDir)
	defer pool.Close()

	entry, err := pool.Get(providerName)
	if err != nil {
		log.Fatal().Err(err).Msg("could not launch browser")
	}

	url := providerURL(providerName)
	page, err := browser.NewPage(entry.Browser, url)
	if err != nil {
		log.Fatal().Err(err).Msg("could not open page")
	}

	stop := make(chan os.Signal, 1)
	signal.Notify(stop, syscall.SIGINT, syscall.SIGTERM)
	<-stop

	log.Info().Str("provider", providerName).Msg("saving session (cookies + localStorage)...")
	if err := browser.CaptureSession(providerName, page); err != nil {
		log.Error().Err(err).Msg("could not save session")
	} else {
		log.Info().Str("file", browser.SessionPath(providerName)).
			Msg("session saved — restart go-proxy to use it")
	}
}

func runTestAdapter(providerName string, cfg *config.Config) {
	pc := cfg.ProviderByName(providerName)
	if pc == nil {
		log.Fatal().Str("provider", providerName).Msg("provider not found in config — add it to config.yaml first")
	}

	groups, ok := providerSelectorGroups()[providerName]
	if !ok {
		log.Fatal().Str("provider", providerName).
			Msgf("no selector groups registered for %q", providerName)
	}

	log.Info().Str("provider", providerName).Msg("test-adapter: launching headed browser")

	pool := browser.NewPool(false, cfg.Server.ChromeProfileDir)
	defer pool.Close()

	entry, err := pool.Get(providerName)
	if err != nil {
		log.Fatal().Err(err).Msg("test-adapter: could not launch browser")
	}

	url := providerURL(providerName)
	page, err := browser.NewPage(entry.Browser, url)
	if err != nil {
		log.Fatal().Err(err).Msg("test-adapter: could not open page")
	}
	defer page.Close()

	log.Info().Str("url", url).Msg("test-adapter: waiting for page to load")
	_ = page.WaitLoad()
	time.Sleep(2 * time.Second)

	log.Info().Str("provider", providerName).Msg("test-adapter: running static selector pre-flight")
	_ = providers.ProbeSelectors(page, providerName, groups)

	log.Info().Str("provider", providerName).Msg("test-adapter: running end-to-end send/response test")
	spec := adapterTestSpec(groups)
	ctx := context.Background()
	result := providers.RunAdapterTest(ctx, page, providerName, spec, 45*time.Second)

	log.Info().
		Str("provider", providerName).
		Bool("session_ok", result.SessionOK).
		Bool("input_found", result.InputFound).
		Bool("send_found", result.SendFound).
		Bool("response_token", result.ResponseToken).
		Msg("test-adapter: summary")

	if len(result.Errors) > 0 {
		for _, e := range result.Errors {
			log.Error().Str("provider", providerName).Str("error", e).Msg("test-adapter: phase failure")
		}
		os.Exit(1)
	}

	log.Info().Str("provider", providerName).Msg("test-adapter: all phases passed — adapter is healthy")
}

func adapterTestSpec(groups []providers.SelectorGroup) providers.AdapterTestSpec {
	spec := providers.AdapterTestSpec{}
	for _, g := range groups {
		switch g.Role {
		case "input":
			spec.InputSelectors = g.Selectors
		case "send":
			spec.SendSelectors = g.Selectors
		case "response":
			spec.ResponseSelectors = g.Selectors
		}
	}
	return spec
}

func providerURL(providerName string) string {
	if u, ok := browser.ProviderHomeURLs[providerName]; ok {
		return u
	}
	return "about:blank"
}

func providerSelectorGroups() map[string][]providers.SelectorGroup {
	return map[string][]providers.SelectorGroup{
		"metaai":  providers.MetaAISelectorGroups,
		"qwen":    providers.QwenSelectorGroups,
		"claude":  providers.ClaudeSelectorGroups,
		"chatgpt": providers.ChatGPTSelectorGroups,
		"gemini":  providers.GeminiSelectorGroups,
	}
}

func buildProviders(cfg *config.Config, pool *browser.Pool, conversations *browser.ConversationManager) []providers.Provider {
	var provs []providers.Provider
	for _, pc := range cfg.EnabledProvidersSorted() {
		models := pc.Models
		switch pc.Name {
		case "claude":
			provs = append(provs, providers.NewClaude(pool, conversations, models))
		case "chatgpt":
			provs = append(provs, providers.NewChatGPT(pool, conversations, models))
		case "gemini":
			provs = append(provs, providers.NewGemini(pool, conversations, models))
		case "metaai":
			provs = append(provs, providers.NewMetaAI(pool, conversations, models))
		case "qwen":
			provs = append(provs, providers.NewQwen(pool, conversations, models, pc.SessionPoolSize))
		}
	}
	return provs
}

func requestLogger(bc *dashboard.Broadcaster) gin.HandlerFunc {
	return func(c *gin.Context) {
		start := time.Now()
		c.Next()
		elapsed := time.Since(start)
		status := c.Writer.Status()

		log.Info().
			Str("method", c.Request.Method).
			Str("path", c.Request.URL.Path).
			Int("status", status).
			Dur("duration", elapsed).
			Msg("request")

		path := c.Request.URL.Path
		if path == "/v1/chat/completions" {
			provider := c.Writer.Header().Get("X-Provider")
			fallback, _ := c.Get("fallback_triggered")
			fallbackBool, _ := fallback.(bool)
			attempted, _ := c.Get("attempted_providers")
			attemptedSlice, _ := attempted.([]string)
			details, _ := c.Get("attempt_details")
			detailsSlice, _ := details.([]dashboard.AttemptDetail)
			traceID := c.GetString("trace_id")
			parseMs, _ := c.Get("parse_ms")
			memoryReadMs, _ := c.Get("memory_read_ms")
			contextBuildMs, _ := c.Get("context_build_ms")
			routingMs, _ := c.Get("routing_ms")
			bc.Emit(dashboard.LogEvent{
				Time:               start,
				Provider:           provider,
				ModelRequested:     c.GetString("model_requested"),
				ResponseTimeMs:     elapsed.Milliseconds(),
				Success:            status >= 200 && status < 300,
				StatusCode:         status,
				Fallback:           fallbackBool,
				AttemptedProviders: attemptedSlice,
				Attempts:           detailsSlice,
				TraceID:            traceID,
				ParseMs:            int64Value(parseMs),
				MemoryReadMs:       int64Value(memoryReadMs),
				ContextBuildMs:     int64Value(contextBuildMs),
				RoutingMs:          int64Value(routingMs),
			})
			log.Info().
				Str("trace_id", traceID).
				Str("provider", provider).
				Int64("request_total_ms", elapsed.Milliseconds()).
				Int64("parse_ms", int64Value(parseMs)).
				Int64("memory_read_ms", int64Value(memoryReadMs)).
				Int64("context_build_ms", int64Value(contextBuildMs)).
				Int64("routing_ms", int64Value(routingMs)).
				Interface("attempts", detailsSlice).
				Msg("performance trace")
		}
	}
}

func int64Value(value any) int64 {
	if n, ok := value.(int64); ok {
		return n
	}
	return 0
}
