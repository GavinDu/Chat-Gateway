// Package types holds shared domain types used across multiple packages.
// It must not import any other internal package to avoid import cycles.
package types

// Message is an OpenAI-style chat message.
type Message struct {
	Role    string `json:"role"`
	Content string `json:"content"`
}
