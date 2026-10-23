// Package hooks delivers lifecycle event notifications to user-configured
// webhook URLs.  All deliveries are fire-and-forget (async) so they never
// add latency to the request path.
//
// Supported events
//
//	conversation.start — a new browser tab was opened for a conversation
//	conversation.end   — a conversation tab was closed (idle timeout or delete)
//
// Configuration (config.yaml)
//
//	hooks:
//	  conversation_start: http://localhost:3000/hooks/start
//	  conversation_end:   http://localhost:3000/hooks/end
//	  secret: "optional-shared-secret"   # sent as X-Hook-Secret header
package hooks

import (
	"bytes"
	"context"
	"encoding/json"
	"net/http"
	"time"

	"github.com/rs/zerolog/log"
)

// Config holds the webhook URLs and optional shared secret.
// Fields are intentionally simple strings so they can be hot-reloaded from
// config.yaml without restarting the server.
type Config struct {
	// ConversationStart is the URL to POST when a new conversation tab is opened.
	ConversationStart string `yaml:"conversation_start,omitempty"`
	// ConversationEnd is the URL to POST when a conversation tab is closed.
	ConversationEnd string `yaml:"conversation_end,omitempty"`
	// Secret is an optional shared secret sent as the X-Hook-Secret request
	// header so the receiving end can verify the origin of webhook calls.
	Secret string `yaml:"secret,omitempty"`
}

// Event is the JSON body POSTed to a webhook URL.
type Event struct {
	// Event is the event name: "conversation.start" or "conversation.end".
	Event string `json:"event"`
	// ConversationID is the proxy-assigned UUID for the conversation.
	ConversationID string `json:"conversation_id"`
	// Provider is the name of the LLM provider that owns the tab
	// (e.g. "qwen" or "metaai").
	Provider string `json:"provider"`
	// Model is the requested model alias (populated for start events only).
	Model string `json:"model,omitempty"`
	// Reason explains why the conversation ended (populated for end events).
	// Current values: "idle_timeout", "deleted".
	Reason string `json:"reason,omitempty"`
	// Timestamp is the Unix epoch second when the event was generated.
	Timestamp int64 `json:"timestamp"`
}

// Dispatcher delivers hook events to configured URLs.
// The zero value is safe to use (all Fire* calls become no-ops when no URL
// is configured).
type Dispatcher struct {
	cfg    Config
	client *http.Client
}

// NewDispatcher creates a Dispatcher for cfg.  Pass an empty Config (or a
// Config with no URLs set) to use a no-op dispatcher.
func NewDispatcher(cfg Config) *Dispatcher {
	return &Dispatcher{
		cfg:    cfg,
		client: &http.Client{Timeout: 5 * time.Second},
	}
}

// FireConversationStart delivers a conversation.start event asynchronously.
// It is a no-op when ConversationStart URL is not configured.
func (d *Dispatcher) FireConversationStart(conversationID, provider, model string) {
	if d == nil || d.cfg.ConversationStart == "" {
		return
	}
	go d.fire(d.cfg.ConversationStart, Event{
		Event:          "conversation.start",
		ConversationID: conversationID,
		Provider:       provider,
		Model:          model,
		Timestamp:      time.Now().Unix(),
	})
}

// FireConversationEnd delivers a conversation.end event asynchronously.
// reason should be "idle_timeout" or "deleted".
// It is a no-op when ConversationEnd URL is not configured.
func (d *Dispatcher) FireConversationEnd(conversationID, provider, reason string) {
	if d == nil || d.cfg.ConversationEnd == "" {
		return
	}
	go d.fire(d.cfg.ConversationEnd, Event{
		Event:          "conversation.end",
		ConversationID: conversationID,
		Provider:       provider,
		Reason:         reason,
		Timestamp:      time.Now().Unix(),
	})
}

// fire serialises evt as JSON and POSTs it to url.  Runs in a goroutine;
// failures are logged at WARN level and silently discarded.
func (d *Dispatcher) fire(url string, evt Event) {
	body, err := json.Marshal(evt)
	if err != nil {
		return
	}

	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Second)
	defer cancel()

	req, err := http.NewRequestWithContext(ctx, http.MethodPost, url, bytes.NewReader(body))
	if err != nil {
		log.Warn().Err(err).Str("event", evt.Event).Str("url", url).Msg("hooks: build request failed")
		return
	}
	req.Header.Set("Content-Type", "application/json")
	if d.cfg.Secret != "" {
		req.Header.Set("X-Hook-Secret", d.cfg.Secret)
	}

	resp, err := d.client.Do(req)
	if err != nil {
		log.Warn().Err(err).Str("event", evt.Event).Str("url", url).Msg("hooks: delivery failed")
		return
	}
	defer resp.Body.Close()

	log.Debug().
		Str("event", evt.Event).
		Str("conversation_id", evt.ConversationID).
		Str("provider", evt.Provider).
		Str("url", url).
		Int("status", resp.StatusCode).
		Msg("hooks: delivered")
}
