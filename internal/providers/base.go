package providers

import (
	"context"
	"encoding/json"
	"fmt"
	"strconv"
	"strings"
	"time"

	"go-proxy/internal/telemetry"
	"go-proxy/internal/types"

	"github.com/go-rod/rod"
	"github.com/go-rod/rod/lib/input"
	"github.com/go-rod/rod/lib/proto"
	"github.com/rs/zerolog/log"
)

// loginKeywords are URL fragments that indicate the user has been redirected to
// a login / sign-in page, meaning the session has expired.
var loginKeywords = []string{"login", "signin", "sign-in", "auth", "accounts.google.com", "auth0"}

// SelectorGroup groups related CSS selectors for a specific UI role.
// Selectors are tried in order; the first match wins.
type SelectorGroup struct {
	Role      string
	Selectors []string
}

// IsLoginPage returns true if the current page URL indicates the session has
// expired and the provider is requiring re-authentication.
func IsLoginPage(page *rod.Page) bool {
	info, err := page.Info()
	if err != nil {
		return false
	}
	u := strings.ToLower(info.URL)
	for _, kw := range loginKeywords {
		if strings.Contains(u, kw) {
			return true
		}
	}
	return false
}

// CheckSessionExpiry returns an error if the current page indicates the session
// has expired. It logs a clear re-login hint and returns a sentinel error so
// the caller can mark the provider degraded.
func CheckSessionExpiry(providerName string, page *rod.Page) error {
	if IsLoginPage(page) {
		log.Error().
			Str("provider", providerName).
			Str("action", fmt.Sprintf("run: ./go-proxy --login %s", providerName)).
			Msg("session expired — re-login required")
		return fmt.Errorf("%s: session expired, re-login required", providerName)
	}
	return nil
}

// FindElement tries each selector in order and returns the first element found.
// It logs a warning for every selector that fails so the exact failing name is
// visible in logs for easy updating. timeout applies to the overall search.
func FindElement(page *rod.Page, timeout time.Duration, providerName, role string, selectors []string) (*rod.Element, error) {
	deadline := time.Now().Add(timeout)
	for _, sel := range selectors {
		remaining := time.Until(deadline)
		if remaining <= 0 {
			remaining = 500 * time.Millisecond
		}
		el, err := page.Timeout(remaining).Element(sel)
		if err != nil {
			log.Warn().
				Str("provider", providerName).
				Str("role", role).
				Str("selector", sel).
				Msg("selector not found — trying next fallback")
			continue
		}
		log.Info().
			Str("provider", providerName).
			Str("role", role).
			Str("selector", sel).
			Msg("selector matched")
		return el, nil
	}
	return nil, fmt.Errorf("%s: no selector matched for %q — tried: %s", providerName, role, strings.Join(selectors, ", "))
}

// ProbeSelectors checks each SelectorGroup against the page and logs whether a
// match was found. It is used by --test-adapter mode and returns the count of
// groups where at least one selector matched.
func ProbeSelectors(page *rod.Page, providerName string, groups []SelectorGroup) int {
	matched := 0
	for _, g := range groups {
		found := false
		for _, sel := range g.Selectors {
			el, err := page.Timeout(5 * time.Second).Element(sel)
			if err == nil && el != nil {
				log.Info().
					Str("provider", providerName).
					Str("role", g.Role).
					Str("selector", sel).
					Msg("test-adapter: selector OK")
				found = true
				break
			}
			log.Warn().
				Str("provider", providerName).
				Str("role", g.Role).
				Str("selector", sel).
				Msg("test-adapter: selector MISS")
		}
		if found {
			matched++
		} else {
			log.Error().
				Str("provider", providerName).
				Str("role", g.Role).
				Strs("selectors", g.Selectors).
				Msg("test-adapter: ALL selectors failed for role")
		}
	}
	return matched
}

// AdapterTestSpec carries the selector groups needed for RunAdapterTest.
type AdapterTestSpec struct {
	InputSelectors    []string
	SendSelectors     []string
	ResponseSelectors []string
}

// AdapterTestResult reports the outcome of each phase of the end-to-end test.
// Which exact CSS selector matched each role is logged at debug level by
// FindElement, so it is not duplicated here.
type AdapterTestResult struct {
	SessionOK     bool
	InputFound    bool
	SendFound     bool // true when a button was clicked; false means Enter was used
	ResponseToken bool // at least one token received from the response observer
	Errors        []string
}

// RunAdapterTest exercises the full input→send→response path against a live
// provider page. It types a short dummy prompt, submits it, and waits up to
// responseTimeout for the mutation observer to yield at least one token. Each
// phase is independently reported so the caller knows exactly what failed.
//
// Call this from --test-adapter mode after the page has loaded.
func RunAdapterTest(ctx context.Context, page *rod.Page, providerName string, spec AdapterTestSpec, responseTimeout time.Duration) AdapterTestResult {
	res := AdapterTestResult{}

	// Phase 0 — session check.
	if IsLoginPage(page) {
		res.Errors = append(res.Errors, "session expired: re-login required before running --test-adapter")
		log.Error().
			Str("provider", providerName).
			Str("action", fmt.Sprintf("./go-proxy --login %s", providerName)).
			Msg("test-adapter: not logged in — cannot send dummy message")
		return res
	}
	res.SessionOK = true
	log.Info().Str("provider", providerName).Msg("test-adapter: phase 0 — session OK")

	// Phase 1 — locate input element.
	inputEl, err := FindElement(page, 20*time.Second, providerName, "input", spec.InputSelectors)
	if err != nil {
		res.Errors = append(res.Errors, fmt.Sprintf("input: %v", err))
		log.Error().Err(err).Str("provider", providerName).Msg("test-adapter: phase 1 FAILED — input element not found")
		return res
	}
	res.InputFound = true
	log.Info().Str("provider", providerName).Msg("test-adapter: phase 1 — input found")

	// Phase 2 — type a clearly labelled dummy prompt.
	dummyPrompt := "go-proxy selector test — please reply with the word OK"
	if err := inputEl.Click(proto.InputMouseButtonLeft, 1); err != nil {
		res.Errors = append(res.Errors, fmt.Sprintf("input click: %v", err))
		log.Error().Err(err).Str("provider", providerName).Msg("test-adapter: phase 2 FAILED — cannot click input")
		return res
	}
	if err := typeIntoInput(inputEl, dummyPrompt); err != nil {
		res.Errors = append(res.Errors, fmt.Sprintf("input type: %v", err))
		log.Error().Err(err).Str("provider", providerName).Msg("test-adapter: phase 2 FAILED — cannot type into input")
		return res
	}
	log.Info().Str("provider", providerName).Msg("test-adapter: phase 2 — dummy prompt typed")

	time.Sleep(400 * time.Millisecond)

	// Phase 3 — submit (send button or Enter).
	sendBtn, err := FindElement(page, 5*time.Second, providerName, "send", spec.SendSelectors)
	if err != nil {
		log.Warn().Str("provider", providerName).Msg("test-adapter: phase 3 — send button not found, pressing Enter")
		if err2 := pressEnter(inputEl); err2 != nil {
			res.Errors = append(res.Errors, fmt.Sprintf("send enter: %v", err2))
			log.Error().Err(err2).Str("provider", providerName).Msg("test-adapter: phase 3 FAILED — cannot submit prompt")
			return res
		}
	} else {
		res.SendFound = true
		if err := sendBtn.Click(proto.InputMouseButtonLeft, 1); err != nil {
			res.Errors = append(res.Errors, fmt.Sprintf("send click: %v", err))
			log.Error().Err(err).Str("provider", providerName).Msg("test-adapter: phase 3 FAILED — cannot click send button")
			return res
		}
	}
	log.Info().Str("provider", providerName).Msg("test-adapter: phase 3 — prompt submitted")

	// Phase 4 — attach mutation observer and wait for at least one response token.
	time.Sleep(1500 * time.Millisecond)
	if err := injectMutationObserver(page, providerName, spec.ResponseSelectors...); err != nil {
		res.Errors = append(res.Errors, fmt.Sprintf("response observer: %v", err))
		log.Error().Err(err).Str("provider", providerName).Msg("test-adapter: phase 4 FAILED — response selector not found")
		return res
	}

	deadline := time.Now().Add(responseTimeout)
	for time.Now().Before(deadline) {
		token, _ := pollMutationQueue(page)
		if token != "" {
			res.ResponseToken = true
			log.Info().
				Str("provider", providerName).
				Str("token_preview", truncate(token, 80)).
				Msg("test-adapter: phase 4 — response token received")
			break
		}
		time.Sleep(300 * time.Millisecond)
	}

	if !res.ResponseToken {
		res.Errors = append(res.Errors, fmt.Sprintf("response: no token within %s", responseTimeout))
		log.Error().
			Str("provider", providerName).
			Dur("timeout", responseTimeout).
			Msg("test-adapter: phase 4 FAILED — no response token received")
	}

	return res
}

// truncate shortens s to at most n runes.
func truncate(s string, n int) string {
	runes := []rune(s)
	if len(runes) <= n {
		return s
	}
	return string(runes[:n]) + "…"
}

// TrySelectModel attempts to select the requested model variant in a provider's
// UI. It tries each entry in selectorPairs in order; each pair is
// [triggerSelector, optionSelector].
//
// modelAlias is matched case-insensitively against the visible text of each
// option element.
func TrySelectModel(page *rod.Page, modelAlias string, selectorPairs [][2]string) bool {
	if modelAlias == "" {
		return false
	}
	target := strings.ToLower(modelAlias)

	for _, pair := range selectorPairs {
		triggerSel, optionSel := pair[0], pair[1]

		trigger, err := page.Timeout(3 * time.Second).Element(triggerSel)
		if err != nil {
			log.Debug().
				Str("trigger_selector", triggerSel).
				Msg("model-switcher trigger not found — trying next pair")
			continue
		}
		if err := trigger.Click(proto.InputMouseButtonLeft, 1); err != nil {
			continue
		}
		time.Sleep(400 * time.Millisecond)

		opts, err := page.Elements(optionSel)
		if err != nil || len(opts) == 0 {
			log.Debug().
				Str("option_selector", optionSel).
				Msg("model-switcher options not found — trying next pair")
			_ = page.Keyboard.Press(input.Escape)
			continue
		}

		var matched bool
		for _, opt := range opts {
			txt, err := opt.Text()
			if err != nil {
				continue
			}
			if strings.Contains(strings.ToLower(txt), target) {
				if err := opt.Click(proto.InputMouseButtonLeft, 1); err == nil {
					matched = true
					time.Sleep(300 * time.Millisecond)
					log.Debug().
						Str("model", modelAlias).
						Str("selected", txt).
						Msg("model selected in UI")
				}
				break
			}
		}
		if matched {
			return true
		}
		_ = page.Keyboard.Press(input.Escape)
	}
	return false
}

func selectRequestedModel(
	page *rod.Page,
	providerName string,
	model string,
	defaultAliases []string,
	attempt func(*rod.Page, string) bool,
) bool {
	if strings.TrimSpace(model) == "" {
		return true
	}
	for _, defaultAlias := range defaultAliases {
		if strings.EqualFold(strings.TrimSpace(model), defaultAlias) {
			return true
		}
	}
	if attempt(page, model) {
		return true
	}
	log.Error().
		Str("provider", providerName).
		Str("model", model).
		Msg("requested model could not be selected")
	return false
}

// typeIntoInput clears the given element and types the text.
func typeIntoInput(el *rod.Element, text string) error {
	if err := el.SelectAllText(); err != nil {
		return err
	}
	return el.Input(text)
}

// pressEnter sends the Enter key via the element's parent page keyboard.
func pressEnter(el *rod.Element) error {
	return el.Page().Keyboard.Press(input.Enter)
}

// waitForElement polls for selector to appear in the DOM, up to timeout.
// Returns true if found, false if the timeout expired.
func waitForElement(page *rod.Page, selector string, timeout time.Duration) bool {
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		res, err := page.Eval(fmt.Sprintf(`() => !!document.querySelector(%q)`, selector))
		if err == nil && !res.Value.Nil() && res.Value.Bool() {
			return true
		}
		time.Sleep(300 * time.Millisecond)
	}
	return false
}

// waitForResponseStart is an alias for waitForElement, kept for call-site clarity.
func waitForResponseStart(page *rod.Page, selector string, timeout time.Duration) {
	waitForElement(page, selector, timeout)
}

// waitForElementGone polls until selector disappears from the DOM, up to timeout.
// Returns true when gone, false on timeout.
func waitForElementGone(page *rod.Page, selector string, timeout time.Duration) bool {
	deadline := time.Now().Add(timeout)
	for time.Now().Before(deadline) {
		res, err := page.Eval(fmt.Sprintf(`() => !!document.querySelector(%q)`, selector))
		if err != nil || res.Value.Nil() || !res.Value.Bool() {
			return true // gone (or eval error means page navigated away)
		}
		time.Sleep(300 * time.Millisecond)
	}
	return false
}

// streamByPolling polls the page every 200 ms and emits text deltas onto the
// returned channel.  Unlike the mutation-observer approach it does not require
// attaching to a specific DOM element in advance — it just queries the page
// repeatedly.  The channel is closed when text has been stable for stableFor or
// ctx is done.
//
// selectors is an ordered list of CSS selectors; the first one that matches and
// has non-empty text wins for that poll tick.
//
// textTransform, if non-nil, is applied to every raw text before comparison or
// emission.  Use it to strip provider-specific aria labels or prefixes.
func streamByPolling(ctx context.Context, page *rod.Page, stableFor time.Duration, textTransform func(string) string, selectors ...string) <-chan string {
	ch := make(chan string, 64)

	// Build a JS snippet that returns the innerText of the first matching
	// selector that has non-empty content.
	selsJSON, _ := json.Marshal(selectors)
	queryJS := fmt.Sprintf(`() => {
                var sels = %s;
                for (var i = 0; i < sels.length; i++) {
                        var el = document.querySelector(sels[i]);
                        if (el) {
                                var t = (el.innerText || el.textContent || '').trim();
                                if (t) return t;
                        }
                }
                return '';
        }`, string(selsJSON))

	applyTransform := func(s string) string {
		if textTransform != nil {
			return textTransform(s)
		}
		return s
	}

	go func() {
		defer close(ch)
		var last string
		var stableSince time.Time

		ticker := time.NewTicker(200 * time.Millisecond)
		defer ticker.Stop()

		for {
			select {
			case <-ctx.Done():
				return
			case <-ticker.C:
				res, err := page.Eval(queryJS)
				if err != nil || res.Value.Nil() {
					if !stableSince.IsZero() && time.Since(stableSince) > stableFor {
						return
					}
					continue
				}
				raw := res.Value.Str()
				text := applyTransform(raw)
				if text == "" {
					if !stableSince.IsZero() && time.Since(stableSince) > stableFor {
						return
					}
					continue
				}

				if text != last {
					var delta string
					if strings.HasPrefix(text, last) {
						delta = text[len(last):]
					} else {
						// Non-prefix change: the selector switched mid-stream
						// or the container was replaced.  Emit only text that
						// was not already sent.
						if strings.Contains(text, last) {
							idx := strings.LastIndex(text, last)
							delta = text[idx+len(last):]
						} else {
							delta = text
						}
					}
					if delta != "" {
						ch <- delta
					}
					last = text
					stableSince = time.Now()
				} else if !stableSince.IsZero() && time.Since(stableSince) > stableFor {
					return
				}
			}
		}
	}()

	return ch
}

// injectMutationObserver injects JavaScript that watches for text mutations on
// a given CSS selector and accumulates them in a JS-visible queue.
// It tries multiple selectors in order and uses the first one it finds.
// providerName is used for logging so a failing selector is easy to identify.
func injectMutationObserver(page *rod.Page, providerName string, selectors ...string) error {
	for _, selector := range selectors {
		script := fmt.Sprintf(`
                        window.__proxyMutationQueue = [];
                        window.__proxyLastText = '';
                        (function() {
                                const target = document.querySelector(%q);
                                if (!target) return false;
                                const obs = new MutationObserver(() => {
                                        const current = target.innerText || target.textContent || '';
                                        if (current !== window.__proxyLastText) {
                                                window.__proxyMutationQueue.push(current);
                                                window.__proxyLastText = current;
                                        }
                                });
                                obs.observe(target, {childList: true, subtree: true, characterData: true});
                                return true;
                        })();
                `, selector)
		result, err := page.Eval(script)
		if err == nil && !result.Value.Nil() && result.Value.Bool() {
			log.Debug().
				Str("provider", providerName).
				Str("selector", selector).
				Msg("mutation observer attached")
			return nil
		}
		log.Warn().
			Str("provider", providerName).
			Str("selector", selector).
			Msg("mutation observer target not found — trying next selector")
	}
	return fmt.Errorf("%s: no mutation observer target found — tried: %s", providerName, strings.Join(selectors, ", "))
}

// injectLatestResponseObserver watches the document for the newest element that
// matches any response selector. It is installed before submission so fast
// responses are not missed when their container is created after the click.
func injectLatestResponseObserver(page *rod.Page, providerName string, selectors ...string) error {
	selectorsJSON, err := json.Marshal(selectors)
	if err != nil {
		return fmt.Errorf("%s: encode response selectors: %w", providerName, err)
	}
	script := fmt.Sprintf(`() => {
		if (window.__proxyResponseObserver) {
			window.__proxyResponseObserver.disconnect();
		}
                window.__proxyMutationQueue = [];
		return (function(selectors) {
                        function latestResponse() {
                                for (const selector of selectors) {
                                        const matches = document.querySelectorAll(selector);
                                        if (matches.length) return matches[matches.length - 1];
                                }
                                return null;
                        }
                        let lastElement = latestResponse();
                        let lastText = lastElement
                                ? (lastElement.innerText || lastElement.textContent || '')
                                : '';
                        const root = document.body || document.documentElement;
                        if (!root) return false;
                        const obs = new MutationObserver(() => {
                                const currentElement = latestResponse();
                                if (!currentElement) return;
                                const currentText = currentElement.innerText || currentElement.textContent || '';
                                if (currentText && (currentElement !== lastElement || currentText !== lastText)) {
                                        window.__proxyMutationQueue.push(currentText);
                                }
                                lastElement = currentElement;
                                lastText = currentText;
                        });
                        obs.observe(root, {childList: true, subtree: true, characterData: true});
                        window.__proxyResponseObserver = obs;
                        return true;
                })(%s);
	}`, string(selectorsJSON))
	result, err := page.Eval(script)
	if err != nil {
		return fmt.Errorf("%s: install latest response observer: %w", providerName, err)
	}
	if result.Value.Nil() || !result.Value.Bool() {
		return fmt.Errorf("%s: response observer root not found", providerName)
	}
	return nil
}

// pollMutationQueue drains the JS mutation queue and returns the latest text, or "".
func pollMutationQueue(page *rod.Page) (string, error) {
	result, err := page.Eval(`
                (function() {
                        const q = window.__proxyMutationQueue || [];
                        window.__proxyMutationQueue = [];
                        return q.length > 0 ? q[q.length - 1] : null;
                })()
        `)
	if err != nil {
		return "", err
	}
	if result.Value.Nil() {
		return "", nil
	}
	return result.Value.Str(), nil
}

type responseSnapshot struct {
	count int
	text  string
}

func latestResponseSnapshot(page *rod.Page, selectors ...string) (responseSnapshot, error) {
	selectorsJSON, err := json.Marshal(selectors)
	if err != nil {
		return responseSnapshot{}, err
	}
	result, err := page.Timeout(2 * time.Second).Eval(fmt.Sprintf(`() => {
		const selectors = %s;
		for (const selector of selectors) {
			const matches = document.querySelectorAll(selector);
			if (!matches.length) continue;
			const element = matches[matches.length - 1];
			const text = element.innerText || element.textContent || '';
			return String(matches.length) + '\n' + text;
		}
		return '0\n';
	}`, string(selectorsJSON)))
	if err != nil {
		return responseSnapshot{}, err
	}
	raw := result.Value.Str()
	countText, text, _ := strings.Cut(raw, "\n")
	count, err := strconv.Atoi(countText)
	if err != nil {
		return responseSnapshot{}, fmt.Errorf("parse response element count %q: %w", countText, err)
	}
	return responseSnapshot{count: count, text: text}, nil
}

// streamFromResponsePolling follows the newest matching response across page
// navigations. This is used by SPAs that replace the document after a prompt is
// submitted, which destroys an in-page MutationObserver.
func streamFromResponsePolling(
	ctx context.Context,
	page *rod.Page,
	baseline responseSnapshot,
	stableFor time.Duration,
	selectors ...string,
) <-chan string {
	ch := make(chan string, 64)
	go func() {
		defer close(ch)
		ticker := time.NewTicker(200 * time.Millisecond)
		defer ticker.Stop()

		var last string
		var stableSince time.Time
		started := false
		for {
			select {
			case <-ctx.Done():
				return
			case <-ticker.C:
				current, err := latestResponseSnapshot(page, selectors...)
				if err != nil || current.text == "" {
					continue
				}
				if !started {
					if current.count == baseline.count && current.text == baseline.text {
						continue
					}
					started = true
				}
				if current.text != last {
					delta := current.text
					if strings.HasPrefix(current.text, last) {
						delta = current.text[len(last):]
					}
					if delta != "" {
						telemetry.ObserveResponseUpdate(ctx)
						ch <- delta
					}
					last = current.text
					stableSince = time.Now()
					continue
				}
				if !stableSince.IsZero() && time.Since(stableSince) > stableFor {
					telemetry.Mark(ctx, "response_stable")
					return
				}
			}
		}
	}()
	return ch
}

// streamFromMutations watches the mutation queue and emits text deltas onto the
// returned channel. The channel is closed when text has been stable for stableFor
// or ctx is done.
func streamFromMutations(ctx context.Context, page *rod.Page, stableFor time.Duration) <-chan string {
	ch := make(chan string, 64)

	go func() {
		defer close(ch)
		var last string
		var stableSince time.Time

		ticker := time.NewTicker(200 * time.Millisecond)
		defer ticker.Stop()

		for {
			select {
			case <-ctx.Done():
				return
			case <-ticker.C:
				text, err := pollMutationQueue(page)
				if err != nil || text == "" {
					if !stableSince.IsZero() && time.Since(stableSince) > stableFor {
						return
					}
					continue
				}

				if text != last {
					var delta string
					if strings.HasPrefix(text, last) {
						delta = text[len(last):]
					} else {
						delta = text
					}
					if delta != "" {
						ch <- delta
					}
					last = text
					stableSince = time.Now()
				} else if !stableSince.IsZero() && time.Since(stableSince) > stableFor {
					return
				}
			}
		}
	}()

	return ch
}

// buildPrompt converts a messages slice into a single text prompt for web UIs.
func buildPrompt(messages []types.Message) string {
	if len(messages) == 0 {
		return ""
	}
	if len(messages) == 1 && messages[0].Role == "user" {
		return messages[0].Content
	}
	var sb strings.Builder
	for _, m := range messages {
		switch m.Role {
		case "system":
			sb.WriteString("[System]: ")
		case "user":
			sb.WriteString("Human: ")
		case "assistant":
			sb.WriteString("Assistant: ")
		}
		sb.WriteString(m.Content)
		sb.WriteString("\n\n")
	}
	sb.WriteString("Assistant:")
	return sb.String()
}

// lastUserMessage returns the content of the last user-role message.
func lastUserMessage(messages []types.Message) string {
	for i := len(messages) - 1; i >= 0; i-- {
		if messages[i].Role == "user" {
			return messages[i].Content
		}
	}
	if len(messages) > 0 {
		return messages[len(messages)-1].Content
	}
	return ""
}
