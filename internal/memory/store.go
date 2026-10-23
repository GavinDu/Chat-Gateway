// Package memory provides a provider-agnostic conversation store backed by
// FalkorDB (a property graph database built on Redis).  It records every
// message exchanged through the proxy so that full conversation context can be
// reconstructed when switching providers, after browser-tab eviction, or
// following a server restart.
package memory

import (
	"context"

	"go-proxy/internal/types"
)

// Store is the interface for the conversation memory layer.
type Store interface {
	// GetHistory returns all messages for conversationID in chronological
	// order.  Returns nil, nil when the conversation is unknown.
	GetHistory(ctx context.Context, conversationID string) ([]types.Message, error)

	// SaveTurn appends messages to the conversation graph, assigning
	// sequential IDs starting after the current last message.  Creates the
	// Conversation node if it does not exist yet.
	// provider and model are stored on assistant message nodes for
	// provenance tracking.
	SaveTurn(ctx context.Context, conversationID, provider, model string, messages []types.Message) error

	// Close releases underlying connections.
	Close() error
}
