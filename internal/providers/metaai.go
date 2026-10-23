package providers

import (
	"context"
	"fmt"
	"strings"
	"time"

	"go-proxy/internal/browser"
	"go-proxy/internal/telemetry"
	"go-proxy/internal/types"

	"github.com/go-rod/rod"
	"github.com/google/uuid"
	"github.com/rs/zerolog/log"
)

const metaAIURL = "https://www.meta.ai/"

var metaAIModelSelectors = [][2]string{
	{"button[aria-label*='model' i], button[aria-label*='llama' i], [data-testid*='model' i], [data-model-selector]", "[role='menuitem'], [role='option'], [data-model-option]"},
	{"[role='button'][aria-haspopup='menu']", "[role='menuitem'], [role='option']"},
}

// metaAIInputSelectors lists candidate CSS selectors for the prompt input,
// tried in order so a UI change only requires adding a new entry at the top.
var metaAIInputSelectors = []string{
	`div[contenteditable="true"][data-lexical-editor]`,
	`div[contenteditable="true"][aria-label*="message"]`,
	`textarea[placeholder*="Ask"]`,
	`div[contenteditable="true"]`,
}

// metaAISendSelectors lists candidate CSS selectors for the send button.
var metaAISendSelectors = []string{
	`button[aria-label="Send"]`,
	`button[type="submit"]`,
	`button[aria-label="Submit"]`,
}

// metaAIResponseSelectors lists candidate CSS selectors for the assistant
// response container used by the mutation observer.
var metaAIResponseSelectors = []string{
	`[data-testid="assistant-message"] .markdown-content`,
	`[data-testid="assistant-message"]`,
	`.assistant-response:last-of-type`,
	`[data-testid="ai-response"]:last-of-type`,
	`.message-content.assistant:last-of-type`,
	`.ai-message:last-of-type`,
}

// MetaAISelectorGroups exposes all selector groups for --test-adapter mode.
var MetaAISelectorGroups = []SelectorGroup{
	{Role: "input", Selectors: metaAIInputSelectors},
	{Role: "send", Selectors: metaAISendSelectors},
	{Role: "response", Selectors: metaAIResponseSelectors},
}

type MetaAIProvider struct {
	pool          *browser.Pool
	conversations *browser.ConversationManager
	models        []string
}

func NewMetaAI(pool *browser.Pool, conversations *browser.ConversationManager, models []string) *MetaAIProvider {
	provider := &MetaAIProvider{pool: pool, conversations: conversations, models: models}
	if browser.HasSession(provider.Name()) {
		go func() {
			if _, err := pool.Get(provider.Name()); err != nil {
				log.Warn().Err(err).Msg("metaai: could not warm browser")
			}
		}()
	}
	return provider
}

func (p *MetaAIProvider) Name() string     { return "metaai" }
func (p *MetaAIProvider) Models() []string { return p.models }

// DiscoverModels reads Meta AI's current model control. Meta AI has also
// shipped fixed-model layouts, so the page-text check preserves discovery for
// those layouts without fabricating options from configuration.
func (p *MetaAIProvider) DiscoverModels(ctx context.Context) ([]string, error) {
	if !browser.HasSession(p.Name()) {
		return nil, fmt.Errorf("metaai: no saved session")
	}
	discoveryPool, page, err := openDiscoveryPage(p.Name(), metaAIURL)
	if err != nil {
		return nil, err
	}
	defer discoveryPool.Close()
	defer page.Close()

	if models, menuErr := DiscoverMenuModels(ctx, page, p.Name(), metaAIModelSelectors); menuErr == nil {
		return models, nil
	}
	return DiscoverFixedModel(ctx, page, p.Name(), "meta-ai", "meta ai")
}

func (p *MetaAIProvider) IsHealthy(ctx context.Context) bool {
	entry, err := p.pool.Get(p.Name())
	if err != nil {
		return false
	}
	entry.Lock()
	defer entry.Unlock()

	page, err := browser.NewPage(entry.Browser, metaAIURL)
	if err != nil {
		return false
	}
	defer page.Close()
	_ = page.Timeout(20 * time.Second).WaitLoad()

	info, err := page.Info()
	if err != nil {
		return false
	}
	if !strings.Contains(info.URL, "meta.ai") {
		log.Warn().Str("provider", "metaai").Msg("session expired (IsHealthy)")
		return false
	}
	return true
}

func (p *MetaAIProvider) SendMessage(ctx context.Context, model string, conversationID string, messages []types.Message) (<-chan string, string, error) {
	reusedBrowser := p.pool.HasBrowser(p.Name())
	entry, err := p.pool.Get(p.Name())
	if err != nil {
		return nil, "", fmt.Errorf("metaai: get browser: %w", err)
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
			log.Info().Str("conversation_id", resolvedID).Msg("metaai: reusing existing conversation tab")
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
			page, openErr = browser.NewPage(entry.Browser, metaAIURL)
			if openErr != nil {
				log.Error().Err(openErr).Msg("metaai: open page")
				return
			}
			_ = page.Timeout(20 * time.Second).WaitLoad()

			if err := CheckSessionExpiry("metaai", page); err != nil {
				log.Error().Err(err).Msg("metaai: session expired")
				_ = page.Close()
				return
			}

			if !selectRequestedModel(page, p.Name(), model, []string{"meta-ai", "llama"}, func(page *rod.Page, model string) bool {
				return TrySelectModel(page, model, metaAIModelSelectors)
			}) {
				_ = page.Close()
				return
			}
		}
		telemetry.Mark(ctx, "page_ready")

		inputEl, err := FindElement(page, 20*time.Second, "metaai", "input", metaAIInputSelectors)
		if err != nil {
			log.Error().Err(err).Msg("metaai: input not found")
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
			this.focus();
			if (this.tagName === 'TEXTAREA') {
				const setter = Object.getOwnPropertyDescriptor(
					window.HTMLTextAreaElement.prototype,
					'value'
				).set;
				setter.call(this, text);
			} else {
				this.innerText = text;
			}
			this.dispatchEvent(new InputEvent('input', {
				bubbles: true,
				inputType: 'insertText',
				data: text
			}));
			this.dispatchEvent(new Event('change', {bubbles: true}));
			return this.innerText || this.value;
		}`, text)
		if err != nil {
			log.Error().Err(err).Msg("metaai: set input")
			if existingPage == nil {
				_ = page.Close()
			} else {
				p.conversations.Delete(resolvedID)
			}
			return
		}
		telemetry.Mark(ctx, "prompt_injected")

		time.Sleep(500 * time.Millisecond)

		baseline, _ := latestResponseSnapshot(page, metaAIResponseSelectors...)

		sendBtn, err := FindElement(page, 5*time.Second, "metaai", "send", metaAISendSelectors)
		if err != nil {
			log.Warn().Msg("metaai: send button not found, pressing Enter")
			_ = pressEnter(inputEl)
		} else {
			if _, err := sendBtn.Timeout(5 * time.Second).Eval(`function() {
				this.click();
				return true;
			}`); err != nil {
				log.Error().Err(err).Msg("metaai: click send")
				if existingPage == nil {
					_ = page.Close()
				} else {
					p.conversations.Delete(resolvedID)
				}
				return
			}
		}
		telemetry.Mark(ctx, "request_sent")

		tokens := streamFromResponsePolling(ctx, page, baseline, 4*time.Second, metaAIResponseSelectors...)
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
