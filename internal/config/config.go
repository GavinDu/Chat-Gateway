package config

import (
	"fmt"
	"os"

	"gopkg.in/yaml.v3"
)

type ServerConfig struct {
	Port     int  `yaml:"port"`
	Headless bool `yaml:"headless"`
	// DashboardSecret protects dashboard mutation endpoints (config edits, login).
	// Set to any non-empty string to require "Authorization: Bearer <secret>" on
	// PUT /dashboard/api/config/* and POST /dashboard/api/providers/*/login*.
	// If empty, mutation endpoints are unprotected (acceptable on private networks).
	DashboardSecret string `yaml:"dashboard_secret,omitempty"`
	// ConversationIdleMinutes is how long an open conversation tab is kept alive
	// without a new request before it is closed. Defaults to 30 minutes.
	ConversationIdleMinutes int `yaml:"conversation_idle_minutes,omitempty"`
	// ChromeProfileDir is an optional base directory for persistent per-provider
	// Chromium user-data directories.  When set, each provider gets its own
	// subdirectory (<ChromeProfileDir>/<providerName>/) so Google account logins,
	// cookies, and site preferences survive proxy restarts.
	// Example: "chrome-profiles" → chrome-profiles/qwen/, chrome-profiles/metaai/, …
	// Leave empty to use a temporary profile (default behaviour).
	ChromeProfileDir string `yaml:"chrome_profile_dir,omitempty"`
}

// HooksConfig holds optional webhook URLs fired on conversation lifecycle events.
type HooksConfig struct {
	// ConversationStart is POSTed when a new conversation tab is opened.
	ConversationStart string `yaml:"conversation_start,omitempty"`
	// ConversationEnd is POSTed when a conversation tab is closed.
	ConversationEnd string `yaml:"conversation_end,omitempty"`
	// Secret is sent as the X-Hook-Secret header so receivers can verify origin.
	Secret string `yaml:"secret,omitempty"`
}

type ProviderConfig struct {
	Name            string   `yaml:"name"`
	Enabled         bool     `yaml:"enabled"`
	Priority        int      `yaml:"priority"`
	Models          []string `yaml:"models"`
	TimeoutSeconds  int      `yaml:"timeout_seconds"`
	SessionPoolSize int      `yaml:"session_pool_size,omitempty"`
	// Headless overrides the global server.headless for this provider only.
	// Omit (null) to inherit the global setting.
	Headless *bool `yaml:"headless,omitempty"`
}

// FalkorDBConfig holds connection parameters for the FalkorDB memory store.
type FalkorDBConfig struct {
	Host     string `yaml:"host"`
	Port     int    `yaml:"port"`
	Password string `yaml:"password,omitempty"`
	Graph    string `yaml:"graph"`
}

// MemoryConfig enables the conversation memory layer.
// When falkordb is non-nil, the proxy persists every conversation turn to
// FalkorDB so history survives provider switches, tab evictions, and restarts.
type MemoryConfig struct {
	FalkorDB *FalkorDBConfig `yaml:"falkordb,omitempty"`
}

type Config struct {
	Server    ServerConfig     `yaml:"server"`
	Providers []ProviderConfig `yaml:"providers"`
	Memory    MemoryConfig     `yaml:"memory,omitempty"`
	Hooks     HooksConfig      `yaml:"hooks,omitempty"`
}

func DefaultConfig() *Config {
	return &Config{
		Server: ServerConfig{
			Port:     8080,
			Headless: true,
		},
		Providers: []ProviderConfig{
			{Name: "qwen", Enabled: true, Priority: 1, TimeoutSeconds: 120, SessionPoolSize: 2,
				Models: []string{"qwen", "qwen-turbo", "qwen-plus", "qwen-max", "qwen2.5"}},
			{Name: "metaai", Enabled: true, Priority: 2, TimeoutSeconds: 120,
				Models: []string{"meta-ai", "llama", "llama-3"}},
			{Name: "claude", Enabled: true, Priority: 3, TimeoutSeconds: 120,
				Models: []string{"claude", "claude-sonnet", "claude-opus", "claude-haiku"}},
			{Name: "chatgpt", Enabled: true, Priority: 4, TimeoutSeconds: 120,
				Models: []string{"gpt", "chatgpt", "gpt-5", "gpt-4o"}},
			{Name: "gemini", Enabled: true, Priority: 5, TimeoutSeconds: 120,
				Models: []string{"gemini", "gemini-pro", "gemini-flash"}},
		},
	}
}

func Load(path string) (*Config, error) {
	cfg := DefaultConfig()
	data, err := os.ReadFile(path)
	if err != nil {
		if os.IsNotExist(err) {
			return cfg, nil
		}
		return nil, fmt.Errorf("reading config: %w", err)
	}
	if err := yaml.Unmarshal(data, cfg); err != nil {
		return nil, fmt.Errorf("parsing config: %w", err)
	}
	return cfg, nil
}

// ProviderByName returns a provider config by name.
func (c *Config) ProviderByName(name string) *ProviderConfig {
	for i := range c.Providers {
		if c.Providers[i].Name == name {
			return &c.Providers[i]
		}
	}
	return nil
}

// EnabledProvidersSorted returns enabled providers sorted by priority (ascending).
func (c *Config) EnabledProvidersSorted() []ProviderConfig {
	var enabled []ProviderConfig
	for _, p := range c.Providers {
		if p.Enabled {
			enabled = append(enabled, p)
		}
	}
	for i := 1; i < len(enabled); i++ {
		for j := i; j > 0 && enabled[j].Priority < enabled[j-1].Priority; j-- {
			enabled[j], enabled[j-1] = enabled[j-1], enabled[j]
		}
	}
	return enabled
}

// EffectiveHeadless returns the headless setting for a provider, falling back
// to the global server.headless if the provider doesn't override it.
func (c *Config) EffectiveHeadless(pc *ProviderConfig) bool {
	if pc.Headless != nil {
		return *pc.Headless
	}
	return c.Server.Headless
}
