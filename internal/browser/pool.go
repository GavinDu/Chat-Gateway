package browser

import (
	"encoding/json"
	"fmt"
	"os"
	"path/filepath"
	"sync"
	"time"

	"github.com/go-rod/rod"
	"github.com/go-rod/rod/lib/launcher"
	"github.com/go-rod/rod/lib/proto"
	"github.com/rs/zerolog/log"
)

// BrowserEntry holds a browser instance for a single provider.
type BrowserEntry struct {
	Browser          *rod.Browser
	providerName     string
	providerURL      string
	headless         bool
	mu               sync.Mutex
	lastSessionMtime int64
}

// Lock / Unlock serialise per-provider browser access (one request at a time).
func (e *BrowserEntry) Lock()   { e.mu.Lock() }
func (e *BrowserEntry) Unlock() { e.mu.Unlock() }

// Pool manages one browser instance per provider.
type Pool struct {
	globalHeadless bool
	// profileDir is an optional base directory for persistent Chrome user-data
	// directories.  When non-empty, each provider gets its own subdirectory so
	// Google account logins and site sessions survive proxy restarts.
	profileDir string
	mu         sync.Mutex
	browsers   map[string]*BrowserEntry
	// headlessOverride per provider name; nil means use globalHeadless.
	headlessOverride map[string]*bool
	// OnSessionChange is called (from a background goroutine) whenever a
	// provider's session file is detected as new or updated on disk.
	// providerName is the provider key; mtime is the Unix-second mtime of the file.
	OnSessionChange func(providerName string, mtime int64)
}

// NewPool creates a new browser pool.  profileDir may be empty to use
// temporary (ephemeral) Chrome profiles; when non-empty, each provider gets
// a persistent subdirectory inside profileDir.
func NewPool(globalHeadless bool, profileDir string) *Pool {
	return &Pool{
		globalHeadless:   globalHeadless,
		profileDir:       profileDir,
		browsers:         make(map[string]*BrowserEntry),
		headlessOverride: make(map[string]*bool),
	}
}

// SetProviderHeadless stores a per-provider headless override.
func (p *Pool) SetProviderHeadless(name string, headless *bool) {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.headlessOverride[name] = headless
}

func (p *Pool) effectiveHeadless(name string) bool {
	if v, ok := p.headlessOverride[name]; ok && v != nil {
		return *v
	}
	return p.globalHeadless
}

// Get returns (creating if necessary) a browser entry for the given provider.
func (p *Pool) Get(providerName string) (*BrowserEntry, error) {
	p.mu.Lock()
	defer p.mu.Unlock()

	if entry, ok := p.browsers[providerName]; ok {
		return entry, nil
	}

	entry, err := p.launch(providerName)
	if err != nil {
		return nil, err
	}
	p.browsers[providerName] = entry
	return entry, nil
}

// HasBrowser reports whether this provider's Chromium process is already
// present in the pool. It is used only for low-cost performance telemetry.
func (p *Pool) HasBrowser(providerName string) bool {
	p.mu.Lock()
	defer p.mu.Unlock()
	_, ok := p.browsers[providerName]
	return ok
}

// ProviderHomeURLs maps provider names to their home URL (used for localStorage restore
// and as the target URL for login sessions).
var ProviderHomeURLs = map[string]string{
	"qwen":    "https://chat.qwenlm.ai/",
	"metaai":  "https://www.meta.ai/",
	"claude":  "https://claude.ai/new",
	"chatgpt": "https://chatgpt.com/",
	"gemini":  "https://gemini.google.com/app",
}

func (p *Pool) launch(providerName string) (*BrowserEntry, error) {
	headless := p.effectiveHeadless(providerName)
	log.Info().Str("provider", providerName).Bool("headless", headless).Msg("launching browser")

	l := launcher.New().
		Headless(headless).
		Set("no-sandbox").
		Set("disable-dev-shm-usage").
		Set("disable-gpu").
		Set("disable-setuid-sandbox").
		// Stealth: remove the primary bot-detection signals.
		Set("disable-blink-features", "AutomationControlled").
		Set("window-size", "1920,1080").
		Set("lang", "en-US,en").
		Set("no-first-run").
		Set("no-default-browser-check").
		Set("disable-infobars")

	// Persistent Chrome profile: gives each provider its own user-data-dir so
	// Google account logins, cookies, and site preferences survive restarts.
	if p.profileDir != "" {
		providerProfile := filepath.Join(p.profileDir, providerName)
		if err := os.MkdirAll(providerProfile, 0755); err != nil {
			log.Warn().Err(err).Str("profile_dir", providerProfile).Msg("could not create profile dir — using temp profile")
		} else {
			l = l.UserDataDir(providerProfile)
			log.Info().Str("provider", providerName).Str("profile_dir", providerProfile).Msg("using persistent Chrome profile")
		}
	}

	l = applySystemChromium(l)

	u, err := l.Launch()
	if err != nil {
		return nil, fmt.Errorf("launching browser for %s: %w", providerName, err)
	}

	b := rod.New().ControlURL(u).MustConnect()

	entry := &BrowserEntry{
		Browser:      b,
		providerName: providerName,
		providerURL:  ProviderHomeURLs[providerName],
		headless:     headless,
	}

	// Restore saved session if available.
	session, err := LoadSession(providerName)
	if err != nil {
		log.Warn().Err(err).Str("provider", providerName).Msg("could not load session file")
	} else if session != nil {
		mtime := SessionMtime(providerName)
		if err := applySession(b, session, entry.providerURL); err != nil {
			log.Warn().Err(err).Str("provider", providerName).Msg("could not restore session")
		} else {
			entry.lastSessionMtime = mtime
			log.Info().Str("provider", providerName).Int("cookies", len(session.Cookies)).Msg("session restored")
		}
	} else {
		log.Warn().Str("provider", providerName).
			Msgf("no saved session — run: ./go-proxy --login %s", providerName)
	}

	// Start session file watcher.
	go p.watchSession(entry)

	return entry, nil
}

// watchSession polls the session file for changes and re-applies it to the browser.
func (p *Pool) watchSession(entry *BrowserEntry) {
	ticker := time.NewTicker(15 * time.Second)
	defer ticker.Stop()

	for range ticker.C {
		p.mu.Lock()
		_, still := p.browsers[entry.providerName]
		p.mu.Unlock()
		if !still {
			return
		}

		mtime := SessionMtime(entry.providerName)
		if mtime == 0 || mtime == entry.lastSessionMtime {
			continue
		}

		session, err := LoadSession(entry.providerName)
		if err != nil || session == nil {
			continue
		}

		entry.Lock()
		if err := applySession(entry.Browser, session, entry.providerURL); err != nil {
			log.Warn().Err(err).Str("provider", entry.providerName).Msg("session reload failed")
		} else {
			entry.lastSessionMtime = mtime
			log.Info().Str("provider", entry.providerName).Msg("session reloaded from disk")
			if cb := p.OnSessionChange; cb != nil {
				go cb(entry.providerName, mtime)
			}
		}
		entry.Unlock()
	}
}

// applySession restores cookies and localStorage.
//
// Network.setCookies must be called on a page-level target (not the browser
// object) on Chrome 112+.  Calling it on *rod.Browser returns -32601 because
// the browser session has no targetId.  We open a temporary page at
// providerURL so the CDP call has a valid target, which also lets us restore
// localStorage in the same navigation.
func applySession(b *rod.Browser, session *SessionData, providerURL string) error {
	if len(session.Cookies) == 0 && len(session.LocalStorage) == 0 {
		return nil
	}

	// Build cookie params.
	params := make([]*proto.NetworkCookieParam, 0, len(session.Cookies))
	for _, c := range session.Cookies {
		cp := &proto.NetworkCookieParam{
			Name:     c.Name,
			Value:    c.Value,
			Domain:   c.Domain,
			Path:     c.Path,
			HTTPOnly: c.HTTPOnly,
			Secure:   c.Secure,
		}
		if c.Expires > 0 {
			cp.Expires = proto.TimeSinceEpoch(c.Expires)
		}
		params = append(params, cp)
	}

	// No URL: fall back to browser-level call (older Chrome, best-effort).
	if providerURL == "" {
		if len(params) == 0 {
			return nil
		}
		setCookies := proto.NetworkSetCookies{Cookies: params}
		if err := setCookies.Call(b); err != nil {
			return fmt.Errorf("set cookies (browser-level): %w", err)
		}
		return nil
	}

	// Open a real page so Network.setCookies has a valid page-level target.
	// This populates the browser cookie store for the domain and is
	// inherited by all subsequent pages opened in the same browser.
	page, err := NewPage(b, providerURL)
	if err != nil {
		return fmt.Errorf("opening page for session restore: %w", err)
	}
	defer page.Close()
	_ = page.Timeout(20 * time.Second).WaitLoad()

	if len(params) > 0 {
		setCookies := proto.NetworkSetCookies{Cookies: params}
		if err := setCookies.Call(page); err != nil {
			return fmt.Errorf("set cookies: %w", err)
		}
	}

	// Restore localStorage on the same page.
	for _, kvMap := range session.LocalStorage {
		if len(kvMap) == 0 {
			continue
		}
		jsonBytes, err := json.Marshal(kvMap)
		if err != nil {
			continue
		}
		script := fmt.Sprintf(`
                        (function(data) {
                                try {
                                        Object.entries(data).forEach(function(kv) {
                                                localStorage.setItem(kv[0], kv[1]);
                                        });
                                } catch(e) {}
                        })(%s)
                `, string(jsonBytes))
		_, _ = page.Eval(script)
	}

	return nil
}

// CaptureSessionData reads all browser cookies and localStorage from a page
// without writing them. Callers can revalidate ownership before committing.
func CaptureSessionData(page *rod.Page) (*SessionData, error) {
	// --- Cookies ---
	all, err := proto.NetworkGetAllCookies{}.Call(page)
	if err != nil {
		return nil, fmt.Errorf("reading cookies: %w", err)
	}

	var entries []CookieEntry
	for _, c := range all.Cookies {
		entries = append(entries, CookieEntry{
			Name:     c.Name,
			Value:    c.Value,
			Domain:   c.Domain,
			Path:     c.Path,
			Expires:  float64(c.Expires),
			HTTPOnly: c.HTTPOnly,
			Secure:   c.Secure,
		})
	}

	// --- LocalStorage ---
	lsResult, err := page.Eval(`
                (function() {
                        try {
                                var out = {};
                                for (var i = 0; i < localStorage.length; i++) {
                                        var k = localStorage.key(i);
                                        out[k] = localStorage.getItem(k);
                                }
                                return JSON.stringify(out);
                        } catch(e) { return "{}"; }
                })()
        `)

	var lsMap map[string]string
	if err == nil && !lsResult.Value.Nil() {
		_ = json.Unmarshal([]byte(lsResult.Value.Str()), &lsMap)
	}

	// Store under the current page URL as origin key.
	info, _ := page.Info()
	origin := "default"
	if info != nil {
		origin = info.URL
	}

	lsByOrigin := map[string]map[string]string{}
	if len(lsMap) > 0 {
		lsByOrigin[origin] = lsMap
	}

	return &SessionData{
		Cookies:      entries,
		LocalStorage: lsByOrigin,
	}, nil
}

// CaptureSession reads browser state and saves it for CLI login callers.
func CaptureSession(providerName string, page *rod.Page) error {
	data, err := CaptureSessionData(page)
	if err != nil {
		return err
	}
	return SaveSession(providerName, data)
}

// Close shuts down all browsers in the pool.
func (p *Pool) Close() {
	p.mu.Lock()
	defer p.mu.Unlock()
	for name, entry := range p.browsers {
		log.Info().Str("provider", name).Msg("closing browser")
		_ = entry.Browser.Close()
		delete(p.browsers, name)
	}
}
