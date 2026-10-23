package memory

import (
	"context"

	"go-proxy/internal/types"
)

// NoopStore is a no-op Store used when FalkorDB is not configured.
// All operations succeed silently and return empty data.
type NoopStore struct{}

func (n *NoopStore) GetHistory(_ context.Context, _ string) ([]types.Message, error) {
	return nil, nil
}

func (n *NoopStore) SaveTurn(_ context.Context, _, _, _ string, _ []types.Message) error {
	return nil
}

func (n *NoopStore) Close() error { return nil }
