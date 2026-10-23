package providers

import (
	"context"
	"fmt"
	"strings"
	"time"

	"go-proxy/internal/browser"

	"github.com/go-rod/rod"
	"github.com/go-rod/rod/lib/input"
	"github.com/go-rod/rod/lib/proto"
	"github.com/rs/zerolog/log"
)

// openDiscoveryPage uses a short-lived authenticated browser so scheduled
// catalog work never takes the main provider browser lock ahead of an
// interactive chat request.
func openDiscoveryPage(providerName, providerURL string) (*browser.Pool, *rod.Page, error) {
	pool := browser.NewPool(true, "")
	entry, err := pool.Get(providerName)
	if err != nil {
		pool.Close()
		return nil, nil, fmt.Errorf("%s: launch discovery browser: %w", providerName, err)
	}
	page, err := browser.NewPage(entry.Browser, providerURL)
	if err != nil {
		pool.Close()
		return nil, nil, fmt.Errorf("%s: open discovery page: %w", providerName, err)
	}
	_ = page.WaitLoad()
	if err := CheckSessionExpiry(providerName, page); err != nil {
		_ = page.Close()
		pool.Close()
		return nil, nil, err
	}
	return pool, page, nil
}

// DiscoverMenuModels opens a provider's model/bot menu and returns the visible
// option labels. Selector pairs are tried in order because provider UIs change
// their generated class names and accessibility markup periodically.
func DiscoverMenuModels(ctx context.Context, page *rod.Page, providerName string, selectorPairs [][2]string) ([]string, error) {
	deadline := time.Now().Add(20 * time.Second)
	for time.Now().Before(deadline) {
		if err := ctx.Err(); err != nil {
			return nil, err
		}

		for _, pair := range selectorPairs {
			trigger, err := page.Timeout(time.Second).Element(pair[0])
			if err != nil {
				continue
			}
			if err := trigger.Click(proto.InputMouseButtonLeft, 1); err != nil {
				continue
			}
			time.Sleep(350 * time.Millisecond)

			options, err := page.Elements(pair[1])
			if err != nil {
				_ = page.Keyboard.Press(input.Escape)
				continue
			}

			models := make([]string, 0, len(options))
			for _, option := range options {
				if err := ctx.Err(); err != nil {
					_ = page.Keyboard.Press(input.Escape)
					return nil, err
				}
				text, err := option.Text()
				if err != nil {
					continue
				}
				if model := normalizeModelOption(text); model != "" {
					models = appendUniqueModel(models, model)
				}
			}
			_ = page.Keyboard.Press(input.Escape)

			if len(models) > 0 {
				log.Info().
					Str("provider", providerName).
					Int("count", len(models)).
					Msg("model options discovered from provider page")
				return models, nil
			}
		}
		time.Sleep(300 * time.Millisecond)
	}

	return nil, fmt.Errorf("%s: no visible model or bot options found", providerName)
}

// DiscoverTextModels reads already-rendered model cards, as used by provider
// home pages that show a catalog instead of a dropdown.
func DiscoverTextModels(ctx context.Context, page *rod.Page, providerName string, selectors ...string) ([]string, error) {
	deadline := time.Now().Add(12 * time.Second)
	for time.Now().Before(deadline) {
		if err := ctx.Err(); err != nil {
			return nil, err
		}
		for _, selector := range selectors {
			options, err := page.Elements(selector)
			if err != nil {
				continue
			}
			models := make([]string, 0, len(options))
			for _, option := range options {
				text, err := option.Text()
				if err != nil {
					continue
				}
				if model := normalizeModelOption(text); model != "" {
					models = appendUniqueModel(models, model)
				}
			}
			if len(models) > 0 {
				log.Info().
					Str("provider", providerName).
					Int("count", len(models)).
					Msg("model cards discovered from provider page")
				return models, nil
			}
		}
		time.Sleep(300 * time.Millisecond)
	}
	return nil, fmt.Errorf("%s: no visible model cards found", providerName)
}

// TryClickVisibleModel selects a model card whose visible label matches the
// requested alias. Clicking the label itself bubbles to the card's link/button
// handler on provider pages with nested clickable model cards.
func TryClickVisibleModel(page *rod.Page, modelAlias string, selectors ...string) bool {
	target := strings.ToLower(strings.TrimSpace(modelAlias))
	if target == "" {
		return false
	}

	for _, selector := range selectors {
		options, err := page.Elements(selector)
		if err != nil {
			continue
		}
		for _, exact := range []bool{true, false} {
			for _, option := range options {
				text, err := option.Text()
				if err != nil {
					continue
				}
				label := normalizeModelOption(text)
				normalized := strings.ToLower(label)
				matches := normalized == target
				if !exact {
					matches = strings.Contains(normalized, target) || strings.Contains(target, normalized)
				}
				if !matches {
					continue
				}
				if err := option.Click(proto.InputMouseButtonLeft, 1); err == nil {
					time.Sleep(500 * time.Millisecond)
					log.Info().Str("model", modelAlias).Str("selected", label).Msg("model card selected in UI")
					return true
				}
			}
		}
	}
	return false
}

// DiscoverFixedModel checks page text for a provider whose current web UI has
// one fixed model rather than a selectable menu. The returned name is still
// sourced from authenticated page content, so a page redirect or sign-in wall
// cannot be reported as a successful discovery.
func DiscoverFixedModel(ctx context.Context, page *rod.Page, providerName, modelName string, markers ...string) ([]string, error) {
	if err := ctx.Err(); err != nil {
		return nil, err
	}
	result, err := page.Eval(`() => (document.body && (document.body.innerText || document.body.textContent || '')).toLowerCase()`)
	if err != nil || result.Value.Nil() {
		return nil, fmt.Errorf("%s: read page text: %w", providerName, err)
	}
	text := result.Value.Str()
	for _, marker := range markers {
		if !strings.Contains(text, strings.ToLower(marker)) {
			return nil, fmt.Errorf("%s: fixed model marker %q not found", providerName, marker)
		}
	}
	return []string{modelName}, nil
}

func normalizeModelOption(raw string) string {
	lines := strings.FieldsFunc(raw, func(r rune) bool {
		return r == '\n' || r == '\r'
	})
	for _, line := range lines {
		line = strings.Join(strings.Fields(strings.TrimSpace(line)), " ")
		if line == "" || isNonModelLabel(line) || len([]rune(line)) > 120 {
			continue
		}
		return line
	}
	return ""
}

func isNonModelLabel(value string) bool {
	switch strings.ToLower(strings.TrimSpace(value)) {
	case "selected", "current", "select", "close", "cancel", "settings",
		"new chat", "new conversation", "search", "more", "home", "explore",
		"recent", "history":
		return true
	default:
		return false
	}
}

func appendUniqueModel(models []string, model string) []string {
	for _, existing := range models {
		if strings.EqualFold(existing, model) {
			return models
		}
	}
	return append(models, model)
}
