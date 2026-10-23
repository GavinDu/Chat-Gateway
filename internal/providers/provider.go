package providers

import (
	"context"

	"go-proxy/internal/types"
)

// Provider is the interface every chat-service adapter must implement.
type Provider interface {
	// Name returns the canonical provider identifier (e.g. "qwen" or "metaai").
	Name() string

	// Models returns the list of model aliases this provider handles.
	Models() []string

	// SendMessage sends a conversation to the provider and returns a channel
	// that emits response text tokens as they arrive, the resolved conversation
	// ID, and any error. The channel is closed when the response is complete or
	// an error occurs.
	//
	// model is the requested model alias (e.g. "qwen-plus").
	// If empty, the provider's default model is used.
	//
	// conversationID is an optional proxy-issued opaque token representing an
	// existing conversation to continue. If empty a new conversation is started
	// and a fresh ID is generated. The resolved ID is returned as the second
	// value so the caller can pass it back on subsequent turns via the
	// X-Conversation-Id request header.
	//
	// The caller must drain the channel fully.
	SendMessage(ctx context.Context, model string, conversationID string, messages []types.Message) (<-chan string, string, error)

	// IsHealthy performs a lightweight check (e.g. verifying the user is still
	// authenticated on the provider's site). Returns false if the session has
	// expired or the provider site is unreachable.
	IsHealthy(ctx context.Context) bool
}

// ModelDiscoverer is an optional capability implemented by providers that can
// read the model or bot choices exposed by their authenticated web UI. It is
// intentionally separate from Provider so small test providers and future
// adapters do not need browser discovery support to participate in routing.
type ModelDiscoverer interface {
	DiscoverModels(ctx context.Context) ([]string, error)
}
