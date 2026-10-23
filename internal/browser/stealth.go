package browser

import (
	"github.com/go-rod/rod"
	"github.com/go-rod/rod/lib/proto"
)

// StealthScript is injected into every new document via
// PageAddScriptToEvaluateOnNewDocument so it runs before any page JS and
// removes the most common headless-browser fingerprints that Cloudflare and
// other bot-detection systems check.
const StealthScript = `(function () {
        // 1. navigator.webdriver — the primary signal; undefined = not a bot
        Object.defineProperty(navigator, 'webdriver', {
                get: () => undefined,
                configurable: true,
        });

        // 2. Notification permissions — headless returns 'denied'; real browsers
        //    return 'default'. This mismatch is a known CF signal.
        try {
                var _origQuery = window.navigator.permissions.query.bind(
                        window.navigator.permissions
                );
                window.navigator.permissions.query = function (params) {
                        if (params && params.name === 'notifications') {
                                return Promise.resolve({ state: Notification.permission, onchange: null });
                        }
                        return _origQuery(params);
                };
        } catch (_) {}

        // 3. Language list — headless often has an empty list
        Object.defineProperty(navigator, 'languages', {
                get: () => ['en-US', 'en'],
                configurable: true,
        });
})();`

// NewPage creates a new browser page with stealth JS injected before any
// content loads, then navigates to url. Call page.WaitLoad() after if needed.
// Use this everywhere instead of browser.Page(proto.TargetCreateTarget{URL:…})
// so that bot-detection fingerprints are suppressed on every page.
func NewPage(b *rod.Browser, url string) (*rod.Page, error) {
	// Open a blank page so we can register the stealth script BEFORE the
	// first real navigation. PageAddScriptToEvaluateOnNewDocument persists
	// for all future navigations within this target/tab.
	page, err := b.Page(proto.TargetCreateTarget{URL: "about:blank"})
	if err != nil {
		return nil, err
	}

	// Inject stealth script — runs before any document JS on every navigation.
	_, _ = proto.PageAddScriptToEvaluateOnNewDocument{
		Source: StealthScript,
	}.Call(page)

	// Now navigate to the real URL with stealth active.
	if url != "" && url != "about:blank" {
		if err := page.Navigate(url); err != nil {
			_ = page.Close()
			return nil, err
		}
	}

	return page, nil
}
