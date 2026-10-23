package dashboard

import (
	_ "embed"
	"encoding/json"
	"errors"
	"fmt"
	"net/http"
	"os"
	"sync"

	"go-proxy/internal/browser"
	"go-proxy/internal/config"
	"go-proxy/internal/router"

	"github.com/gin-gonic/gin"
	"gopkg.in/yaml.v3"
)

//go:embed index.html
var indexHTML []byte

// Handler serves the web dashboard and its companion JSON/SSE API.
type Handler struct {
	registry         *router.Registry
	cfg              *config.Config
	cfgPath          string
	broadcaster      *Broadcaster
	sessionBc        *SessionBroadcaster
	loginMgr         *LoginManager
	priorityEdited   map[string]bool // providers whose priority was changed since startup
	priorityEditedMu sync.RWMutex
}

// NewHandler creates a Handler.
func NewHandler(reg *router.Registry, cfg *config.Config, cfgPath string, bc *Broadcaster, sessionBc *SessionBroadcaster) *Handler {
	return &Handler{
		registry:       reg,
		cfg:            cfg,
		cfgPath:        cfgPath,
		broadcaster:    bc,
		sessionBc:      sessionBc,
		loginMgr:       NewLoginManager(),
		priorityEdited: make(map[string]bool),
	}
}

// Close releases any headed browsers still owned by dashboard login jobs.
func (h *Handler) Close() {
	h.loginMgr.Close()
}

// RegisterRoutes mounts the dashboard under /dashboard on the given engine.
func (h *Handler) RegisterRoutes(r *gin.Engine) {
	// Redirect bare root to the dashboard.
	r.GET("/", func(c *gin.Context) {
		c.Redirect(http.StatusFound, "/dashboard/")
	})

	d := r.Group("/dashboard")
	d.GET("/", h.ServeHTML)

	// Read-only endpoints — no auth required.
	d.GET("/api/providers", h.ListProviders)
	d.GET("/api/logs/stream", h.StreamLogs)
	d.GET("/api/logs/recent", h.RecentLogs)

	// Mutation endpoints — protected by dashboard secret when configured.
	mut := d.Group("/api", h.requireSecret())
	mut.PUT("/config/providers/:name", h.UpdateProviderConfig)
	mut.POST("/providers/:name/login", h.StartLogin)
	mut.GET("/providers/:name/login/status", h.LoginStatus)
	mut.GET("/providers/:name/login/frame", h.LoginFrame)
	mut.POST("/providers/:name/login/input", h.LoginInput)
	mut.POST("/providers/:name/login/save", h.SaveLogin)
	mut.POST("/providers/:name/login/cancel", h.CancelLogin)
}

// requireSecret returns a middleware that enforces "Authorization: Bearer <secret>"
// when server.dashboard_secret is non-empty. If the secret is not set, all requests
// pass through with a startup-time warning logged by the caller.
func (h *Handler) requireSecret() gin.HandlerFunc {
	secret := h.cfg.Server.DashboardSecret
	return func(c *gin.Context) {
		if secret == "" {
			c.Next()
			return
		}
		auth := c.GetHeader("Authorization")
		const prefix = "Bearer "
		if len(auth) <= len(prefix) || auth[:len(prefix)] != prefix || auth[len(prefix):] != secret {
			c.AbortWithStatusJSON(http.StatusUnauthorized, gin.H{
				"error": "unauthorized: set Authorization: Bearer <dashboard_secret> header",
			})
			return
		}
		c.Next()
	}
}

// ServeHTML serves the embedded single-page dashboard.
func (h *Handler) ServeHTML(c *gin.Context) {
	c.Header("Cache-Control", "no-store")
	c.Data(http.StatusOK, "text/html; charset=utf-8", indexHTML)
}

// providerStatusJSON is the JSON shape returned by /api/providers.
type providerStatusJSON struct {
	Name                  string   `json:"name"`
	Status                string   `json:"status"`
	Priority              int      `json:"priority"`
	Models                []string `json:"models"`
	ConfiguredModels      []string `json:"configured_models"`
	DiscoveredModels      []string `json:"discovered_models"`
	ModelSource           string   `json:"model_source"`
	ModelsLastRefreshedAt int64    `json:"models_refreshed_at"`
	ModelsLastAttemptAt   int64    `json:"models_attempted_at"`
	ModelsLastError       string   `json:"models_error,omitempty"`
	ModelsStale           bool     `json:"models_stale"`
	ModelsRefreshing      bool     `json:"models_refreshing"`
	SessionSavedAt        int64    `json:"session_saved_at"`
	// FallbackCount is how many times this provider served a request that
	// started on a different (higher-priority) provider.  Resets on restart.
	FallbackCount int `json:"fallback_count"`
	// RequiresRestart is true when the priority was edited via the dashboard;
	// priority routing order only takes effect after a server restart.
	RequiresRestart bool `json:"requires_restart"`
}

// ListProviders returns status info for every registered provider.
func (h *Handler) ListProviders(c *gin.Context) {
	infos := h.registry.GetProviderInfos()
	fbCounts := h.broadcaster.FallbackCounts()
	h.priorityEditedMu.RLock()
	out := make([]providerStatusJSON, 0, len(infos))
	for _, info := range infos {
		status := "ready"
		if !browser.HasSession(info.Name) {
			status = "no_session"
		}
		if info.Degraded {
			status = "degraded"
		}
		out = append(out, providerStatusJSON{
			Name:                  info.Name,
			Status:                status,
			Priority:              info.Priority,
			Models:                info.Models,
			ConfiguredModels:      info.ConfiguredModels,
			DiscoveredModels:      info.DiscoveredModels,
			ModelSource:           info.ModelSource,
			ModelsLastRefreshedAt: info.ModelsLastRefreshedAt,
			ModelsLastAttemptAt:   info.ModelsLastAttemptAt,
			ModelsLastError:       info.ModelsLastError,
			ModelsStale:           info.ModelsStale,
			ModelsRefreshing:      info.ModelsRefreshing,
			SessionSavedAt:        browser.SessionMtime(info.Name),
			FallbackCount:         fbCounts[info.Name],
			RequiresRestart:       h.priorityEdited[info.Name],
		})
	}
	h.priorityEditedMu.RUnlock()
	c.JSON(http.StatusOK, out)
}

// updateProviderConfigRequest is the body for PUT /api/config/providers/:name.
type updateProviderConfigRequest struct {
	Priority int      `json:"priority"`
	Models   []string `json:"models"`
}

// UpdateProviderConfig updates a single provider's priority and model list,
// writes the change to the config YAML file on disk, and applies the model
// alias changes to the in-memory registry immediately (priority requires restart).
func (h *Handler) UpdateProviderConfig(c *gin.Context) {
	name := c.Param("name")

	var req updateProviderConfigRequest
	if err := c.ShouldBindJSON(&req); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": "invalid body: " + err.Error()})
		return
	}
	if len(req.Models) == 0 {
		c.JSON(http.StatusBadRequest, gin.H{"error": "models must not be empty"})
		return
	}

	pc := h.cfg.ProviderByName(name)
	if pc == nil {
		c.JSON(http.StatusNotFound, gin.H{"error": fmt.Sprintf("provider %q not found", name)})
		return
	}

	priorityChanged := pc.Priority != req.Priority
	pc.Priority = req.Priority
	pc.Models = req.Models

	// Persist to YAML file.
	data, err := yaml.Marshal(h.cfg)
	if err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": "marshal config: " + err.Error()})
		return
	}
	if err := os.WriteFile(h.cfgPath, data, 0644); err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": "write config: " + err.Error()})
		return
	}

	// Apply model alias changes to the live registry immediately.
	h.registry.UpdateModels(name, req.Models)
	// Update stored priority so /api/providers reflects the new value right away.
	h.registry.UpdatePriority(name, req.Priority)

	// Track which providers have pending priority changes (routing order change
	// requires restart since priorityList is built at startup).
	if priorityChanged {
		h.priorityEditedMu.Lock()
		h.priorityEdited[name] = true
		h.priorityEditedMu.Unlock()
	}

	c.JSON(http.StatusOK, gin.H{
		"ok":               true,
		"priority_changed": priorityChanged,
		"restart_required": priorityChanged,
		"message": func() string {
			if priorityChanged {
				return "Config saved. Priority routing order takes effect after a server restart."
			}
			return "Config saved. Model aliases updated immediately."
		}(),
	})
}

// StartLogin launches a headed browser for the named provider and returns immediately.
// The browser opens asynchronously; poll GET .../login/status for progress.
func (h *Handler) StartLogin(c *gin.Context) {
	name := c.Param("name")
	if h.cfg.ProviderByName(name) == nil {
		c.JSON(http.StatusNotFound, gin.H{"error": fmt.Sprintf("provider %q not found", name)})
		return
	}
	job, err := h.loginMgr.StartLogin(name, c.GetHeader("X-Login-Job-ID"))
	if err != nil {
		c.JSON(http.StatusInternalServerError, gin.H{"error": err.Error()})
		return
	}
	c.JSON(http.StatusAccepted, job)
}

// LoginStatus returns the current state of an active login job.
func (h *Handler) LoginStatus(c *gin.Context) {
	name := c.Param("name")
	job, err := h.loginMgr.StatusFor(name, c.Query("job_id"))
	if err != nil {
		writeBridgeError(c, err)
		return
	}
	c.JSON(http.StatusOK, job)
}

// LoginFrame returns one short-lived JPEG capture of the current login page.
// The response contains pixels only; no DOM, CDP endpoint, cookies, storage, or
// unsanitized redirect URL is exposed.
func (h *Handler) LoginFrame(c *gin.Context) {
	frame, err := h.loginMgr.CaptureFrame(c.Param("name"), c.Query("job_id"))
	if err != nil {
		writeBridgeError(c, err)
		return
	}
	c.Header("Cache-Control", "no-store, no-cache, must-revalidate, private")
	c.Header("Pragma", "no-cache")
	c.Header("X-Content-Type-Options", "nosniff")
	if frame.Location != "" {
		c.Header("X-Login-Origin", frame.Location)
	}
	c.Data(http.StatusOK, "image/jpeg", frame.JPEG)
}

// LoginInput relays a constrained pointer, wheel, keyboard, or text event to
// the current login page. Request bodies are never logged or persisted.
func (h *Handler) LoginInput(c *gin.Context) {
	c.Request.Body = http.MaxBytesReader(c.Writer, c.Request.Body, 16<<10)
	var event remoteInput
	if err := c.ShouldBindJSON(&event); err != nil {
		c.JSON(http.StatusBadRequest, gin.H{"error": "invalid remote browser input"})
		return
	}
	if err := h.loginMgr.RelayInput(c.Param("name"), event); err != nil {
		writeBridgeError(c, err)
		return
	}
	c.Status(http.StatusNoContent)
}

func writeBridgeError(c *gin.Context, err error) {
	switch {
	case errors.Is(err, errLoginNotFound):
		c.JSON(http.StatusGone, gin.H{"error": "login browser is no longer active"})
	case errors.Is(err, errStaleLoginJob):
		c.JSON(http.StatusConflict, gin.H{"error": "login browser was replaced; start again"})
	case errors.Is(err, errLoginNotReady):
		c.JSON(http.StatusTooEarly, gin.H{"error": "login browser is not ready"})
	case errors.Is(err, errBridgeRateLimit):
		c.Header("Retry-After", "1")
		c.JSON(http.StatusTooManyRequests, gin.H{"error": "remote browser request rate exceeded"})
	case errors.Is(err, errInvalidInput):
		c.JSON(http.StatusBadRequest, gin.H{"error": "invalid remote browser input"})
	default:
		c.JSON(http.StatusBadGateway, gin.H{"error": "remote browser operation failed"})
	}
}

// LoginStream opens an SSE connection that pushes LoginJob state changes for the
// given provider. The stream closes automatically when the job reaches a terminal
// state (saved or error) or when the client disconnects.
func (h *Handler) LoginStream(c *gin.Context) {
	name := c.Param("name")

	c.Header("Content-Type", "text/event-stream")
	c.Header("Cache-Control", "no-cache")
	c.Header("Connection", "keep-alive")
	c.Header("X-Accel-Buffering", "no")

	sub := h.loginMgr.Subscribe(name)
	defer h.loginMgr.Unsubscribe(name, sub)

	clientGone := c.Request.Context().Done()
	w := c.Writer

	// Flush a heartbeat comment so the browser sees the connection immediately.
	fmt.Fprintf(w, ": connected\n\n")
	w.Flush()

	for {
		select {
		case <-clientGone:
			return
		case <-sub.done:
			// Subscriber was unsubscribed externally (e.g. server shutdown).
			return
		case job := <-sub.ch:
			data, _ := json.Marshal(job)
			fmt.Fprintf(w, "data: %s\n\n", data)
			w.Flush()
			// Close the stream once the job has reached a terminal state so the
			// client knows no further events are coming.
			if job.Status == LoginStatusSaved || job.Status == LoginStatusError {
				return
			}
		}
	}
}

// SaveLogin captures the current browser session and writes it to disk.
func (h *Handler) SaveLogin(c *gin.Context) {
	name := c.Param("name")
	if err := h.loginMgr.SaveSession(name, c.Query("job_id")); err != nil {
		if errors.Is(err, errStaleLoginJob) || errors.Is(err, errLoginNotFound) {
			writeBridgeError(c, err)
			return
		}
		c.JSON(http.StatusBadRequest, gin.H{"error": err.Error()})
		return
	}
	// Immediately notify any open SSE connections so the card flips to Ready
	// without waiting for the next poll cycle.
	mtime := browser.SessionMtime(name)
	h.sessionBc.Emit(SessionEvent{Provider: name, SessionSavedAt: mtime})
	c.JSON(http.StatusOK, gin.H{"ok": true, "message": "Session saved successfully."})
}

// CancelLogin closes the browser and discards the login job.
func (h *Handler) CancelLogin(c *gin.Context) {
	name := c.Param("name")
	if err := h.loginMgr.CancelLogin(name, c.Query("job_id")); err != nil {
		writeBridgeError(c, err)
		return
	}
	c.JSON(http.StatusOK, gin.H{"ok": true})
}

// StreamLogs opens an SSE connection and streams LogEvents and SessionEvents as they arrive.
func (h *Handler) StreamLogs(c *gin.Context) {
	c.Header("Content-Type", "text/event-stream")
	c.Header("Cache-Control", "no-cache")
	c.Header("Connection", "keep-alive")
	c.Header("X-Accel-Buffering", "no")

	logCh := h.broadcaster.Subscribe()
	defer h.broadcaster.Unsubscribe(logCh)

	sessCh := h.sessionBc.Subscribe()
	defer h.sessionBc.Unsubscribe(sessCh)

	clientGone := c.Request.Context().Done()
	w := c.Writer

	// Flush a comment immediately so the browser knows the connection is open.
	fmt.Fprintf(w, ": connected\n\n")
	w.Flush()

	for {
		select {
		case <-clientGone:
			return
		case e, ok := <-logCh:
			if !ok {
				return
			}
			data, _ := json.Marshal(e)
			fmt.Fprintf(w, "data: %s\n\n", data)
			w.Flush()
		case se, ok := <-sessCh:
			if !ok {
				return
			}
			data, _ := json.Marshal(se)
			fmt.Fprintf(w, "event: session_change\ndata: %s\n\n", data)
			w.Flush()
		}
	}
}

// RecentLogs returns the last N log events as a JSON array.
func (h *Handler) RecentLogs(c *gin.Context) {
	c.JSON(http.StatusOK, h.broadcaster.Recent())
}
