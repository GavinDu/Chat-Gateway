package telemetry

import (
	"context"
	"sync"
	"time"
)

type contextKey struct{}

// AttemptTrace records a small fixed set of monotonic timestamps for one
// provider attempt. It deliberately avoids per-token events and payload data.
type AttemptTrace struct {
	mu                  sync.Mutex
	start               time.Time
	marks               map[string]time.Time
	flags               Flags
	responseUpdateCount int64
}

type Flags struct {
	ReusedBrowser bool `json:"reused_browser"`
	ReusedPage    bool `json:"reused_page"`
	NewPage       bool `json:"new_page"`
	WarmPage      bool `json:"warm_page"`
}

type Snapshot struct {
	BrowserAcquireMs      int64 `json:"browser_acquire_ms"`
	ProviderQueueMs       int64 `json:"provider_queue_ms"`
	PageReadyMs           int64 `json:"page_ready_ms"`
	InputMs               int64 `json:"input_ms"`
	RequestSendMs         int64 `json:"request_send_ms"`
	TTFTMs                int64 `json:"ttft_ms"`
	GenerationMs          int64 `json:"generation_ms"`
	ProviderFirstUpdateMs int64 `json:"provider_first_update_ms"`
	ActiveGenerationMs    int64 `json:"active_generation_ms"`
	ResponseSettleMs      int64 `json:"response_settle_ms"`
	ResponseDeliveryMs    int64 `json:"response_delivery_ms"`
	ResponseUpdateCount   int64 `json:"response_update_count"`
	AttemptTotalMs        int64 `json:"attempt_total_ms"`
	Flags                 Flags `json:"flags"`
}

func WithAttempt(ctx context.Context) (context.Context, *AttemptTrace) {
	t := &AttemptTrace{start: time.Now(), marks: make(map[string]time.Time, 8)}
	return context.WithValue(ctx, contextKey{}, t), t
}

func Mark(ctx context.Context, name string) {
	if t, ok := ctx.Value(contextKey{}).(*AttemptTrace); ok {
		t.mu.Lock()
		if _, exists := t.marks[name]; !exists {
			t.marks[name] = time.Now()
		}
		t.mu.Unlock()
	}
}

func SetFlags(ctx context.Context, update func(*Flags)) {
	if t, ok := ctx.Value(contextKey{}).(*AttemptTrace); ok {
		t.mu.Lock()
		update(&t.flags)
		t.mu.Unlock()
	}
}

// ObserveResponseUpdate records aggregate provider response activity without
// retaining response content or emitting per-update logs.
func ObserveResponseUpdate(ctx context.Context) {
	if t, ok := ctx.Value(contextKey{}).(*AttemptTrace); ok {
		t.mu.Lock()
		now := time.Now()
		if _, exists := t.marks["response_first_update"]; !exists {
			t.marks["response_first_update"] = now
		}
		t.marks["response_last_update"] = now
		t.responseUpdateCount++
		t.mu.Unlock()
	}
}

func (t *AttemptTrace) Snapshot() Snapshot {
	t.mu.Lock()
	defer t.mu.Unlock()
	now := time.Now()
	return Snapshot{
		BrowserAcquireMs:      elapsed(t.start, t.marks["browser_acquired"]),
		ProviderQueueMs:       elapsed(t.marks["browser_acquired"], t.marks["lock_acquired"]),
		PageReadyMs:           elapsed(t.marks["lock_acquired"], t.marks["page_ready"]),
		InputMs:               elapsed(t.marks["page_ready"], t.marks["prompt_injected"]),
		RequestSendMs:         elapsed(t.marks["prompt_injected"], t.marks["request_sent"]),
		TTFTMs:                elapsed(t.marks["request_sent"], t.marks["first_token"]),
		GenerationMs:          elapsed(t.marks["first_token"], t.marks["complete"]),
		ProviderFirstUpdateMs: elapsed(t.marks["request_sent"], t.marks["response_first_update"]),
		ActiveGenerationMs:    elapsed(t.marks["response_first_update"], t.marks["response_last_update"]),
		ResponseSettleMs:      elapsed(t.marks["response_last_update"], t.marks["response_stable"]),
		ResponseDeliveryMs:    elapsed(t.marks["response_stable"], t.marks["complete"]),
		ResponseUpdateCount:   t.responseUpdateCount,
		AttemptTotalMs:        now.Sub(t.start).Milliseconds(),
		Flags:                 t.flags,
	}
}

func elapsed(start, end time.Time) int64 {
	if start.IsZero() || end.IsZero() || end.Before(start) {
		return 0
	}
	return end.Sub(start).Milliseconds()
}
