package dashboard

import (
	"encoding/json"
	"sync"
	"time"

	"go-proxy/internal/telemetry"
)

// AttemptDetail captures the outcome of a single provider attempt within a
// request that used the fallback chain.
type AttemptDetail struct {
	Provider    string             `json:"provider"`
	Skipped     bool               `json:"skipped"`         // true = degraded at call time, not tried
	DurationMs  int64              `json:"duration_ms"`     // 0 for skipped attempts
	Error       string             `json:"error,omitempty"` // non-empty when the attempt failed
	Succeeded   bool               `json:"succeeded"`
	Performance telemetry.Snapshot `json:"performance"`
}

// LogEvent captures the details of a single proxied request.
type LogEvent struct {
	Time               time.Time       `json:"time"`
	Provider           string          `json:"provider"`
	ModelRequested     string          `json:"model_requested"`
	ResponseTimeMs     int64           `json:"response_time_ms"`
	Success            bool            `json:"success"`
	StatusCode         int             `json:"status_code"`
	Fallback           bool            `json:"fallback"`
	AttemptedProviders []string        `json:"attempted_providers,omitempty"`
	Attempts           []AttemptDetail `json:"attempts,omitempty"`
	TraceID            string          `json:"trace_id,omitempty"`
	ParseMs            int64           `json:"parse_ms"`
	MemoryReadMs       int64           `json:"memory_read_ms"`
	ContextBuildMs     int64           `json:"context_build_ms"`
	RoutingMs          int64           `json:"routing_ms"`
}

// Broadcaster holds a capped ring-buffer of recent LogEvents and fans them out
// to any number of SSE subscribers.  It also maintains per-provider fallback
// counters that reset on server restart.
type Broadcaster struct {
	mu             sync.Mutex
	recent         []LogEvent
	maxRecent      int
	subscribers    []chan LogEvent
	fallbackCounts map[string]int // provider → number of times it served as a fallback
}

// NewBroadcaster creates a Broadcaster that keeps the last maxRecent events.
func NewBroadcaster(maxRecent int) *Broadcaster {
	return &Broadcaster{
		maxRecent:      maxRecent,
		fallbackCounts: make(map[string]int),
	}
}

// Emit records a log event and pushes it to all active subscribers.
// If the event is a fallback, the serving provider's fallback counter is incremented.
func (b *Broadcaster) Emit(e LogEvent) {
	b.mu.Lock()
	defer b.mu.Unlock()

	b.recent = append(b.recent, e)
	if len(b.recent) > b.maxRecent {
		b.recent = b.recent[len(b.recent)-b.maxRecent:]
	}

	if e.Fallback && e.Provider != "" {
		b.fallbackCounts[e.Provider]++
	}

	for _, ch := range b.subscribers {
		select {
		case ch <- e:
		default:
		}
	}
}

// FallbackCounts returns a snapshot of per-provider fallback counts.
func (b *Broadcaster) FallbackCounts() map[string]int {
	b.mu.Lock()
	defer b.mu.Unlock()
	out := make(map[string]int, len(b.fallbackCounts))
	for k, v := range b.fallbackCounts {
		out[k] = v
	}
	return out
}

// Subscribe returns a new channel that receives future LogEvents.
func (b *Broadcaster) Subscribe() chan LogEvent {
	ch := make(chan LogEvent, 64)
	b.mu.Lock()
	b.subscribers = append(b.subscribers, ch)
	b.mu.Unlock()
	return ch
}

// Unsubscribe removes a subscriber channel and closes it.
func (b *Broadcaster) Unsubscribe(ch chan LogEvent) {
	b.mu.Lock()
	defer b.mu.Unlock()
	for i, s := range b.subscribers {
		if s == ch {
			b.subscribers = append(b.subscribers[:i], b.subscribers[i+1:]...)
			close(ch)
			return
		}
	}
}

// Recent returns a copy of the current ring buffer contents.
func (b *Broadcaster) Recent() []LogEvent {
	b.mu.Lock()
	defer b.mu.Unlock()
	out := make([]LogEvent, len(b.recent))
	copy(out, b.recent)
	return out
}

// MarshalEvent serialises a LogEvent to JSON for SSE data payloads.
func (b *Broadcaster) MarshalEvent(e LogEvent) string {
	data, _ := json.Marshal(e)
	return string(data)
}

// SessionEvent notifies the dashboard that a provider's session file appeared or changed.
type SessionEvent struct {
	Provider       string `json:"provider"`
	SessionSavedAt int64  `json:"session_saved_at"`
}

// SessionBroadcaster fans out SessionEvents to SSE subscribers.
type SessionBroadcaster struct {
	mu          sync.Mutex
	subscribers []chan SessionEvent
}

// NewSessionBroadcaster creates a SessionBroadcaster.
func NewSessionBroadcaster() *SessionBroadcaster {
	return &SessionBroadcaster{}
}

// Emit pushes a SessionEvent to all active subscribers.
func (sb *SessionBroadcaster) Emit(e SessionEvent) {
	sb.mu.Lock()
	defer sb.mu.Unlock()
	for _, ch := range sb.subscribers {
		select {
		case ch <- e:
		default:
		}
	}
}

// Subscribe returns a channel that receives future SessionEvents.
func (sb *SessionBroadcaster) Subscribe() chan SessionEvent {
	ch := make(chan SessionEvent, 16)
	sb.mu.Lock()
	sb.subscribers = append(sb.subscribers, ch)
	sb.mu.Unlock()
	return ch
}

// Unsubscribe removes a subscriber channel and closes it.
func (sb *SessionBroadcaster) Unsubscribe(ch chan SessionEvent) {
	sb.mu.Lock()
	defer sb.mu.Unlock()
	for i, s := range sb.subscribers {
		if s == ch {
			sb.subscribers = append(sb.subscribers[:i], sb.subscribers[i+1:]...)
			close(ch)
			return
		}
	}
}
