package browser

import (
	"context"
	"sync"
	"time"

	"github.com/go-rod/rod"
	"github.com/rs/zerolog/log"
)

// ConversationEntry holds an open browser page that belongs to a specific
// proxy-assigned conversation ID. The page remains open between turns so the
// provider's web UI retains its full conversation history.
type ConversationEntry struct {
	Page         *rod.Page
	ProviderName string
	lastUsed     time.Time
	inFlight     int // number of active requests using this entry
}

// ConversationManager maps proxy-issued conversation IDs to open browser pages.
// It runs a background goroutine that closes pages that have been idle for
// longer than idleTimeout. Eviction is skipped for entries with active
// in-flight requests so a long-running stream is never interrupted.
type ConversationManager struct {
	mu          sync.Mutex
	entries     map[string]*ConversationEntry
	idleTimeout time.Duration
	// OnEvict is called (in a goroutine) whenever a conversation tab is closed
	// due to idle timeout.  The arguments are the conversation ID and the
	// provider name that owned the tab.  Safe to leave nil.
	OnEvict func(conversationID, providerName string)
}

// NewConversationManager creates a manager with the given idle timeout.
// Call StartCleanup to begin background eviction.
func NewConversationManager(idleTimeout time.Duration) *ConversationManager {
	return &ConversationManager{
		entries:     make(map[string]*ConversationEntry),
		idleTimeout: idleTimeout,
	}
}

// Get returns the conversation entry for id (if it exists) and updates its
// last-used timestamp. Returns (nil, false) when id is unknown.
func (m *ConversationManager) Get(id string) (*ConversationEntry, bool) {
	m.mu.Lock()
	defer m.mu.Unlock()
	e, ok := m.entries[id]
	if ok {
		e.lastUsed = time.Now()
	}
	return e, ok
}

// Acquire increments the in-flight counter for id so that the eviction loop
// will not close the associated tab while the request is active. It is a
// no-op if id is not currently tracked (e.g. a new conversation whose page
// has not been Store'd yet). Callers must always pair Acquire with Release.
func (m *ConversationManager) Acquire(id string) {
	m.mu.Lock()
	defer m.mu.Unlock()
	if e, ok := m.entries[id]; ok {
		e.inFlight++
	}
}

// Release decrements the in-flight counter for id. Safe to call even if the
// entry was already evicted.
func (m *ConversationManager) Release(id string) {
	m.mu.Lock()
	defer m.mu.Unlock()
	if e, ok := m.entries[id]; ok && e.inFlight > 0 {
		e.inFlight--
	}
}

// Store registers an open page under the given conversation ID. Any previously
// stored page for the same ID is closed first, unless it is the same pointer
// (continuation turn). The in-flight counter of the existing entry is preserved
// when the same page pointer is stored again.
func (m *ConversationManager) Store(id string, page *rod.Page, providerName string) {
	m.mu.Lock()
	old, hadOld := m.entries[id]
	newEntry := &ConversationEntry{
		Page:         page,
		ProviderName: providerName,
		lastUsed:     time.Now(),
	}
	// Carry forward the in-flight count so callers that Acquire'd before Store
	// returns continue to protect the entry from eviction.
	if hadOld && old.Page == page {
		newEntry.inFlight = old.inFlight
	}
	m.entries[id] = newEntry
	m.mu.Unlock()

	// Only close the previous page if it is a *different* page object.
	if hadOld && old.Page != nil && old.Page != page {
		_ = old.Page.Close()
	}
}

// Delete removes and closes the page for a conversation ID (no-op if unknown).
func (m *ConversationManager) Delete(id string) {
	m.mu.Lock()
	e, ok := m.entries[id]
	if ok {
		delete(m.entries, id)
	}
	m.mu.Unlock()

	if ok && e.Page != nil {
		_ = e.Page.Close()
	}
}

// StartCleanup starts a background goroutine that evicts idle conversations
// every minute until ctx is cancelled.
func (m *ConversationManager) StartCleanup(ctx context.Context) {
	go func() {
		ticker := time.NewTicker(1 * time.Minute)
		defer ticker.Stop()
		for {
			select {
			case <-ctx.Done():
				return
			case <-ticker.C:
				m.evictIdle()
			}
		}
	}()
}

type evictedEntry struct {
	id    string
	entry *ConversationEntry
}

func (m *ConversationManager) evictIdle() {
	m.mu.Lock()
	var toClose []evictedEntry
	for id, e := range m.entries {
		if e.inFlight > 0 {
			// Active request in progress — refresh lastUsed and skip eviction
			// so the tab is never closed mid-stream.
			e.lastUsed = time.Now()
			continue
		}
		if time.Since(e.lastUsed) > m.idleTimeout {
			toClose = append(toClose, evictedEntry{id: id, entry: e})
			delete(m.entries, id)
			log.Info().
				Str("conversation_id", id).
				Str("provider", e.ProviderName).
				Dur("idle_timeout", m.idleTimeout).
				Msg("conversation evicted (idle timeout)")
		}
	}
	cb := m.OnEvict
	m.mu.Unlock()

	for _, ev := range toClose {
		if ev.entry.Page != nil {
			_ = ev.entry.Page.Close()
		}
		if cb != nil {
			go cb(ev.id, ev.entry.ProviderName)
		}
	}
}
