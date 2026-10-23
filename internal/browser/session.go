package browser

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
)

// CookieEntry mirrors a browser cookie for serialization.
type CookieEntry struct {
	Name     string  `json:"name"`
	Value    string  `json:"value"`
	Domain   string  `json:"domain"`
	Path     string  `json:"path"`
	Expires  float64 `json:"expires"`
	HTTPOnly bool    `json:"httpOnly"`
	Secure   bool    `json:"secure"`
}

// SessionData holds serialized browser state for a provider.
type SessionData struct {
	Cookies []CookieEntry `json:"cookies"`
	// LocalStorage maps origin URL → key → value.
	LocalStorage map[string]map[string]string `json:"local_storage,omitempty"`
}

const SessionDir = "sessions"

// SessionPath returns the file path for a provider's session.
func SessionPath(providerName string) string {
	return filepath.Join(SessionDir, providerName+".json")
}

// SaveSession writes session data to disk.
func SaveSession(providerName string, data *SessionData) error {
	if err := os.MkdirAll(SessionDir, 0755); err != nil {
		return fmt.Errorf("creating sessions dir: %w", err)
	}
	b, err := json.MarshalIndent(data, "", "  ")
	if err != nil {
		return fmt.Errorf("marshalling session: %w", err)
	}
	return os.WriteFile(SessionPath(providerName), b, 0600)
}

// LoadSession reads session data from disk. Returns nil if not found.
func LoadSession(providerName string) (*SessionData, error) {
	data, err := os.ReadFile(SessionPath(providerName))
	if err != nil {
		if os.IsNotExist(err) {
			return nil, nil
		}
		return nil, fmt.Errorf("reading session: %w", err)
	}
	var s SessionData
	if err := json.Unmarshal(data, &s); err != nil {
		return nil, fmt.Errorf("parsing session: %w", err)
	}
	return &s, nil
}

// HasSession returns true if a saved session file exists for the provider.
func HasSession(providerName string) bool {
	_, err := os.Stat(SessionPath(providerName))
	return err == nil
}

// SessionMtime returns the last-modified time of the session file (Unix seconds),
// or 0 if the file does not exist.
func SessionMtime(providerName string) int64 {
	info, err := os.Stat(SessionPath(providerName))
	if err != nil {
		return 0
	}
	return info.ModTime().Unix()
}
