package providers

import (
	"context"
	"fmt"
	"strings"
	"sync"
	"time"

	"go-proxy/internal/browser"
	"go-proxy/internal/telemetry"
	"go-proxy/internal/types"

	"github.com/go-rod/rod"
	"github.com/google/uuid"
	"github.com/rs/zerolog/log"
)

const qwenURL = "https://chat.qwenlm.ai/"

// qwenModelSelectors contains [triggerSelector, optionSelector] pairs tried
// in order to select a Qwen model variant.
var qwenModelSelectors = [][2]string{
	{"[role='button'][aria-label*='select model' i], button[aria-label*='model' i], .model-selector button, [data-model-selector]", "[role='listbox'] [role='option'], [role='menuitem'], [role='option'], [data-model-option], .ant-select-item"},
	{".ant-select-selector", ".ant-select-item, [role='option']"},
}

// qwenInputSelectors lists candidate CSS selectors for the prompt input,
// tried in order so a UI change only requires adding a new entry at the top.
var qwenInputSelectors = []string{
	`textarea`,
	`textarea[placeholder*="Message"]`,
	`div[contenteditable="true"][aria-label*="message"]`,
	`div[contenteditable="true"]`,
}

// qwenSendSelectors lists candidate CSS selectors for the send button.
var qwenSendSelectors = []string{
	`button[aria-label*="Send"]`,
	`button[type="submit"]`,
	`button[data-testid="send-button"]`,
}

// qwenResponseSelectors lists candidate CSS selectors for the assistant
// response container used by the mutation observer.
var qwenResponseSelectors = []string{
	`.qwen-chat-message-assistant .response-message-content.phase-answer`,
	`.qwen-chat-message-assistant .custom-qwen-markdown`,
	`.qwen-chat-message-assistant .qwen-markdown`,
	`.assistant-message:last-of-type`,
	`.response:last-of-type`,
	`[data-testid="assistant-message"]:last-of-type`,
	`.chat-message.assistant:last-of-type`,
}

// QwenSelectorGroups exposes all selector groups for --test-adapter mode.
var QwenSelectorGroups = []SelectorGroup{
	{Role: "input", Selectors: qwenInputSelectors},
	{Role: "send", Selectors: qwenSendSelectors},
	{Role: "response", Selectors: qwenResponseSelectors},
}

type QwenProvider struct {
	pool          *browser.Pool
	conversations *browser.ConversationManager
	models        []string
	warmPages     chan *rod.Page
	warmMu        sync.Mutex
	warming       bool
}

func NewQwen(pool *browser.Pool, conversations *browser.ConversationManager, models []string, sessionPoolSize int) *QwenProvider {
	if sessionPoolSize <= 0 {
		sessionPoolSize = 2
	}
	provider := &QwenProvider{
		pool:          pool,
		conversations: conversations,
		models:        models,
		warmPages:     make(chan *rod.Page, sessionPoolSize),
	}
	provider.ensureWarmPages()
	return provider
}

func (p *QwenProvider) Name() string     { return "qwen" }
func (p *QwenProvider) Models() []string { return p.models }

// DiscoverModels reads Qwen's authenticated model picker without sending a
// prompt or changing the current conversation.
func (p *QwenProvider) DiscoverModels(ctx context.Context) ([]string, error) {
	if !browser.HasSession(p.Name()) {
		return nil, fmt.Errorf("qwen: no saved session")
	}
	discoveryPool, page, err := openDiscoveryPage(p.Name(), qwenURL)
	if err != nil {
		return nil, err
	}
	defer discoveryPool.Close()
	defer page.Close()
	return DiscoverMenuModels(ctx, page, p.Name(), qwenModelSelectors)
}

func (p *QwenProvider) IsHealthy(ctx context.Context) bool {
	entry, err := p.pool.Get(p.Name())
	if err != nil {
		return false
	}
	entry.Lock()
	defer entry.Unlock()

	page, err := browser.NewPage(entry.Browser, qwenURL)
	if err != nil {
		return false
	}
	defer page.Close()
	_ = page.WaitLoad()

	info, err := page.Info()
	if err != nil {
		return false
	}
	if !strings.Contains(info.URL, "qwen.ai") && !strings.Contains(info.URL, "qwenlm.ai") && !strings.Contains(info.URL, "tongyi.aliyun.com") {
		log.Warn().Str("provider", "qwen").Msg("session expired (IsHealthy)")
		return false
	}
	return true
}

// ensureWarmPages keeps authenticated Qwen tabs ready inside the existing
// provider browser. New conversations avoid both Chromium startup and page load.
func (p *QwenProvider) ensureWarmPages() {
	if !browser.HasSession(p.Name()) || len(p.warmPages) >= cap(p.warmPages) {
		return
	}
	p.warmMu.Lock()
	if p.warming {
		p.warmMu.Unlock()
		return
	}
	p.warming = true
	p.warmMu.Unlock()

	go func() {
		defer func() {
			p.warmMu.Lock()
			p.warming = false
			p.warmMu.Unlock()
		}()
		entry, err := p.pool.Get(p.Name())
		if err != nil {
			log.Warn().Err(err).Msg("qwen: could not warm session pool")
			return
		}
		for len(p.warmPages) < cap(p.warmPages) {
			page, err := browser.NewPage(entry.Browser, qwenURL)
			if err != nil {
				log.Warn().Err(err).Msg("qwen: could not create warm session")
				return
			}
			_ = page.Timeout(20 * time.Second).WaitLoad()
			if err := CheckSessionExpiry(p.Name(), page); err != nil {
				_ = page.Close()
				return
			}
			select {
			case p.warmPages <- page:
				log.Info().
					Int("ready", len(p.warmPages)).
					Int("capacity", cap(p.warmPages)).
					Msg("qwen: session pool warmed")
			default:
				_ = page.Close()
				return
			}
		}
	}()
}

func (p *QwenProvider) acquireSessionPage(entry *browser.BrowserEntry) (*rod.Page, bool, error) {
	select {
	case page := <-p.warmPages:
		log.Info().Int("remaining", len(p.warmPages)).Msg("qwen: reusing warm session page")
		p.ensureWarmPages()
		return page, true, nil
	default:
		// The browser entry itself is still reused; only a new tab is needed.
		page, err := browser.NewPage(entry.Browser, qwenURL)
		return page, false, err
	}
}

func (p *QwenProvider) SendMessage(ctx context.Context, model string, conversationID string, messages []types.Message) (<-chan string, string, error) {
	reusedBrowser := p.pool.HasBrowser(p.Name())
	entry, err := p.pool.Get(p.Name())
	if err != nil {
		return nil, "", fmt.Errorf("qwen: get browser: %w", err)
	}
	telemetry.Mark(ctx, "browser_acquired")
	telemetry.SetFlags(ctx, func(flags *telemetry.Flags) { flags.ReusedBrowser = reusedBrowser })
	entry.Lock()
	telemetry.Mark(ctx, "lock_acquired")

	var existingPage *rod.Page
	resolvedID := ""
	if conversationID != "" {
		if conv, ok := p.conversations.Get(conversationID); ok && conv.ProviderName == p.Name() {
			existingPage = conv.Page
			resolvedID = conversationID
			log.Info().Str("conversation_id", resolvedID).Msg("qwen: reusing existing conversation tab")
		}
	}
	if resolvedID == "" {
		resolvedID = uuid.New().String()
	}
	telemetry.SetFlags(ctx, func(flags *telemetry.Flags) {
		flags.ReusedPage = existingPage != nil
		flags.NewPage = existingPage == nil
	})

	ch := make(chan string, 64)

	go func() {
		defer entry.Unlock()
		defer close(ch)

		if existingPage != nil {
			p.conversations.Acquire(resolvedID)
			defer p.conversations.Release(resolvedID)
		}

		var page *rod.Page

		if existingPage != nil {
			page = existingPage
		} else {
			var openErr error
			var warm bool
			page, warm, openErr = p.acquireSessionPage(entry)
			telemetry.SetFlags(ctx, func(flags *telemetry.Flags) { flags.WarmPage = warm })
			if openErr != nil {
				log.Error().Err(openErr).Msg("qwen: open page")
				return
			}
			_ = page.Timeout(20 * time.Second).WaitLoad()

			if err := CheckSessionExpiry("qwen", page); err != nil {
				log.Error().Err(err).Msg("qwen: session expired")
				_ = page.Close()
				return
			}

			if !selectRequestedModel(page, p.Name(), model, []string{"qwen"}, func(page *rod.Page, model string) bool {
				return TrySelectModel(page, model, qwenModelSelectors)
			}) {
				_ = page.Close()
				return
			}
		}
		telemetry.Mark(ctx, "page_ready")

		inputEl, err := FindElement(page, 20*time.Second, "qwen", "input", qwenInputSelectors)
		if err != nil {
			log.Error().Err(err).Msg("qwen: input not found")
			if existingPage == nil {
				_ = page.Close()
			} else {
				p.conversations.Delete(resolvedID)
			}
			return
		}

		text := lastUserMessage(messages)
		if existingPage == nil {
			text = buildPrompt(messages)
		}

		_, err = inputEl.Timeout(5*time.Second).Eval(`function(text) {
			const element = this;
			element.focus();
			const setter = Object.getOwnPropertyDescriptor(
				window.HTMLTextAreaElement.prototype,
				'value'
			).set;
			setter.call(element, text);
			element.dispatchEvent(new InputEvent('input', {
				bubbles: true,
				inputType: 'insertText',
				data: text
			}));
			element.dispatchEvent(new Event('change', {bubbles: true}));
			return element.value;
		}`, text)
		if err != nil {
			log.Error().Err(err).Msg("qwen: set input")
			if existingPage == nil {
				_ = page.Close()
			} else {
				p.conversations.Delete(resolvedID)
			}
			return
		}
		log.Info().Msg("qwen: prompt entered")
		telemetry.Mark(ctx, "prompt_injected")

		time.Sleep(300 * time.Millisecond)

		baseline, _ := latestResponseSnapshot(page, qwenResponseSelectors...)
		log.Info().Msg("qwen: response baseline captured")

		sendBtn, err := FindElement(page, 5*time.Second, "qwen", "send", qwenSendSelectors)
		if err != nil {
			log.Warn().Msg("qwen: send button not found, pressing Enter")
			_ = pressEnter(inputEl)
		} else {
			if _, err := sendBtn.Timeout(5 * time.Second).Eval(`function() {
				this.click();
				return true;
			}`); err != nil {
				log.Error().Err(err).Msg("qwen: click send")
				if existingPage == nil {
					_ = page.Close()
				} else {
					p.conversations.Delete(resolvedID)
				}
				return
			}
		}
		telemetry.Mark(ctx, "request_sent")

		tokens := streamFromResponsePolling(ctx, page, baseline, 4*time.Second, qwenResponseSelectors...)
		for token := range tokens {
			select {
			case ch <- token:
			case <-ctx.Done():
				p.conversations.Store(resolvedID, page, p.Name())
				return
			}
		}

		p.conversations.Store(resolvedID, page, p.Name())
	}()

	return ch, resolvedID, nil
}
