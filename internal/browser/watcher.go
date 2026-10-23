package browser

import (
	"context"
	"time"
)

// StartSessionWatcher polls the session files for all given provider names and
// calls onChange whenever a file appears or its mtime advances. It is
// intentionally independent of the browser Pool so it works even when a
// provider's browser has never been launched (e.g. the user runs the CLI login
// command before the proxy has served its first request for that provider).
//
// interval controls how often files are checked (2-3 s is recommended for a
// responsive dashboard). The watcher stops when ctx is cancelled.
func StartSessionWatcher(ctx context.Context, providers []string, interval time.Duration, onChange func(providerName string, mtime int64)) {
	// Seed the baseline with the current mtimes so that pre-existing session
	// files do not trigger a change event on the very first poll tick.
	last := make(map[string]int64, len(providers))
	for _, name := range providers {
		last[name] = SessionMtime(name)
	}

	ticker := time.NewTicker(interval)
	defer ticker.Stop()

	for {
		select {
		case <-ctx.Done():
			return
		case <-ticker.C:
			for _, name := range providers {
				mtime := SessionMtime(name)
				if mtime != 0 && mtime != last[name] {
					last[name] = mtime
					onChange(name, mtime)
				}
			}
		}
	}
}
