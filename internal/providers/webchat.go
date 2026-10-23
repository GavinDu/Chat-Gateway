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

type webChatSpec struct {
	name              string
	url               string
	hosts             []string
	defaultModels     []string
	modelSelectors    [][2]string
	inputSelectors    []string
	sendSelectors     []string
	responseSelectors []string
}

type WebChatProvider struct {
	pool          *browser.Pool
	conversations *browser.ConversationManager
	models        []string
	spec          webChatSpec
	warmPages     chan *rod.Page
	warmMu        sync.Mutex
	warming       bool
}

var claudeSpec = webChatSpec{
	name: "claude", url: "https://claude.ai/new", hosts: []string{"claude.ai"},
	defaultModels: []string{"claude"},
	modelSelectors: [][2]string{
		{"button[aria-haspopup='menu'][data-testid*='model' i], button[aria-label*='model' i], [data-testid='model-selector-dropdown']", "[role='menuitem'], [role='option'], [data-testid*='model-option' i]"},
	},
	inputSelectors: []string{
		`div.ProseMirror[contenteditable="true"]`,
		`div[contenteditable="true"][data-placeholder*="Claude" i]`,
		`div[contenteditable="true"]`,
	},
	sendSelectors: []string{
		`button[aria-label*="Send" i]`,
		`button[data-testid="send-button"]`,
		`button[type="submit"]`,
	},
	responseSelectors: []string{
		`[data-is-streaming="true"]`,
		`[data-testid="assistant-message"]:last-of-type`,
		`.font-claude-response:last-of-type`,
		`div[data-test-render-count]:last-of-type`,
	},
}

var chatGPTSpec = webChatSpec{
	name: "chatgpt", url: "https://chatgpt.com/", hosts: []string{"chatgpt.com", "chat.openai.com"},
	defaultModels: []string{"gpt", "chatgpt"},
	modelSelectors: [][2]string{
		{"button[data-testid='model-switcher-dropdown-button'], button[aria-label*='model' i], [data-testid='model-switcher-dropdown-button']", "[role='menuitem'], [role='option'], [data-testid*='model' i]"},
	},
	inputSelectors: []string{
		`#prompt-textarea`,
		`textarea[data-testid="prompt-textarea"]`,
		`div[contenteditable="true"][data-placeholder]`,
		`div[contenteditable="true"]`,
	},
	sendSelectors: []string{
		`button[data-testid="send-button"]`,
		`button[aria-label*="Send" i]`,
		`button[type="submit"]`,
	},
	responseSelectors: []string{
		`[data-message-author-role="assistant"]:last-of-type .markdown`,
		`[data-message-author-role="assistant"]:last-of-type`,
		`article[data-testid*="conversation-turn"]:last-of-type .markdown`,
	},
}

var geminiSpec = webChatSpec{
	name: "gemini", url: "https://gemini.google.com/app", hosts: []string{"gemini.google.com"},
	defaultModels: []string{"gemini"},
	modelSelectors: [][2]string{
		{"button[aria-label*='model' i], [data-test-id*='model' i], .bard-mode-switcher", "[role='menuitem'], [role='option'], mat-option, [data-test-id*='model-option' i]"},
	},
	inputSelectors: []string{
		`rich-textarea .ql-editor[contenteditable="true"]`,
		`div.ql-editor[contenteditable="true"]`,
		`textarea[aria-label*="prompt" i]`,
		`div[contenteditable="true"]`,
	},
	sendSelectors: []string{
		`button[aria-label*="Send message" i]`,
		`button[aria-label*="Send" i]`,
		`button.send-button`,
		`button[type="submit"]`,
	},
	responseSelectors: []string{
		`model-response:last-of-type .markdown-main-panel`,
		`model-response:last-of-type`,
		`response-container:last-of-type message-content`,
		`.model-response-text:last-of-type`,
	},
}

var ClaudeSelectorGroups = selectorGroups(claudeSpec)
var ChatGPTSelectorGroups = selectorGroups(chatGPTSpec)
var GeminiSelectorGroups = selectorGroups(geminiSpec)

func selectorGroups(spec webChatSpec) []SelectorGroup {
	return []SelectorGroup{
		{Role: "input", Selectors: spec.inputSelectors},
		{Role: "send", Selectors: spec.sendSelectors},
		{Role: "response", Selectors: spec.responseSelectors},
	}
}

func NewClaude(pool *browser.Pool, conversations *browser.ConversationManager, models []string) *WebChatProvider {
	return newWebChatProvider(pool, conversations, models, claudeSpec)
}

func NewChatGPT(pool *browser.Pool, conversations *browser.ConversationManager, models []string) *WebChatProvider {
	return newWebChatProvider(pool, conversations, models, chatGPTSpec)
}

func NewGemini(pool *browser.Pool, conversations *browser.ConversationManager, models []string) *WebChatProvider {
	return newWebChatProvider(pool, conversations, models, geminiSpec)
}

func newWebChatProvider(pool *browser.Pool, conversations *browser.ConversationManager, models []string, spec webChatSpec) *WebChatProvider {
	provider := &WebChatProvider{
		pool:          pool,
		conversations: conversations,
		models:        models,
		spec:          spec,
		warmPages:     make(chan *rod.Page, 1),
	}
	provider.ensureWarmPage()
	return provider
}

func (p *WebChatProvider) Name() string     { return p.spec.name }
func (p *WebChatProvider) Models() []string { return p.models }

func (p *WebChatProvider) DiscoverModels(ctx context.Context) ([]string, error) {
	if !browser.HasSession(p.Name()) {
		return nil, fmt.Errorf("%s: no saved session", p.Name())
	}
	discoveryPool, page, err := openDiscoveryPage(p.Name(), p.spec.url)
	if err != nil {
		return nil, err
	}
	defer discoveryPool.Close()
	defer page.Close()
	return DiscoverMenuModels(ctx, page, p.Name(), p.spec.modelSelectors)
}

func (p *WebChatProvider) IsHealthy(ctx context.Context) bool {
	entry, err := p.pool.Get(p.Name())
	if err != nil {
		return false
	}
	entry.Lock()
	defer entry.Unlock()

	page, err := browser.NewPage(entry.Browser, p.spec.url)
	if err != nil {
		return false
	}
	defer page.Close()
	_ = page.Timeout(20 * time.Second).WaitLoad()
	info, err := page.Info()
	if err != nil || IsLoginPage(page) {
		return false
	}
	for _, host := range p.spec.hosts {
		if strings.Contains(strings.ToLower(info.URL), host) {
			return true
		}
	}
	log.Warn().Str("provider", p.Name()).Str("url", info.URL).Msg("session expired or provider redirected")
	return false
}

func (p *WebChatProvider) ensureWarmPage() {
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
			log.Warn().Err(err).Str("provider", p.Name()).Msg("could not warm browser")
			return
		}
		page, err := browser.NewPage(entry.Browser, p.spec.url)
		if err != nil {
			log.Warn().Err(err).Str("provider", p.Name()).Msg("could not warm page")
			return
		}
		_ = page.Timeout(20 * time.Second).WaitLoad()
		if err := CheckSessionExpiry(p.Name(), page); err != nil {
			_ = page.Close()
			return
		}
		select {
		case p.warmPages <- page:
			log.Info().Str("provider", p.Name()).Msg("warm page ready")
		default:
			_ = page.Close()
		}
	}()
}

func (p *WebChatProvider) acquirePage(entry *browser.BrowserEntry) (*rod.Page, bool, error) {
	select {
	case page := <-p.warmPages:
		p.ensureWarmPage()
		return page, true, nil
	default:
		page, err := browser.NewPage(entry.Browser, p.spec.url)
		return page, false, err
	}
}

func (p *WebChatProvider) SendMessage(ctx context.Context, model, conversationID string, messages []types.Message) (<-chan string, string, error) {
	reusedBrowser := p.pool.HasBrowser(p.Name())
	entry, err := p.pool.Get(p.Name())
	if err != nil {
		return nil, "", fmt.Errorf("%s: get browser: %w", p.Name(), err)
	}
	telemetry.Mark(ctx, "browser_acquired")
	telemetry.SetFlags(ctx, func(flags *telemetry.Flags) { flags.ReusedBrowser = reusedBrowser })
	entry.Lock()
	telemetry.Mark(ctx, "lock_acquired")

	var page *rod.Page
	resolvedID := ""
	if conversationID != "" {
		if conv, ok := p.conversations.Get(conversationID); ok && conv.ProviderName == p.Name() {
			page = conv.Page
			resolvedID = conversationID
		}
	}
	if resolvedID == "" {
		resolvedID = uuid.New().String()
	}
	telemetry.SetFlags(ctx, func(flags *telemetry.Flags) {
		flags.ReusedPage = page != nil
		flags.NewPage = page == nil
	})
	ch := make(chan string, 64)

	go func() {
		defer entry.Unlock()
		defer close(ch)
		existing := page != nil
		if existing {
			p.conversations.Acquire(resolvedID)
			defer p.conversations.Release(resolvedID)
		} else {
			var warm bool
			page, warm, err = p.acquirePage(entry)
			telemetry.SetFlags(ctx, func(flags *telemetry.Flags) { flags.WarmPage = warm })
			if err != nil {
				log.Error().Err(err).Str("provider", p.Name()).Msg("open page")
				return
			}
			_ = page.Timeout(20 * time.Second).WaitLoad()
			if err := CheckSessionExpiry(p.Name(), page); err != nil {
				_ = page.Close()
				return
			}
			if !selectRequestedModel(page, p.Name(), model, p.spec.defaultModels, func(page *rod.Page, model string) bool {
				return TrySelectModel(page, model, p.spec.modelSelectors)
			}) {
				_ = page.Close()
				return
			}
		}
		telemetry.Mark(ctx, "page_ready")

		inputEl, findErr := FindElement(page, 20*time.Second, p.Name(), "input", p.spec.inputSelectors)
		if findErr != nil {
			p.discardPage(resolvedID, page, existing)
			return
		}
		text := lastUserMessage(messages)
		if !existing {
			text = buildPrompt(messages)
		}
		if _, err := inputEl.Timeout(5*time.Second).Eval(`function(text) {
			this.focus();
			if (this.tagName === 'TEXTAREA') {
				const setter = Object.getOwnPropertyDescriptor(window.HTMLTextAreaElement.prototype, 'value').set;
				setter.call(this, text);
			} else {
				this.replaceChildren(document.createTextNode(text));
			}
			this.dispatchEvent(new InputEvent('input', {bubbles:true, inputType:'insertText', data:text}));
			this.dispatchEvent(new Event('change', {bubbles:true}));
			return this.innerText || this.value;
		}`, text); err != nil {
			p.discardPage(resolvedID, page, existing)
			return
		}
		telemetry.Mark(ctx, "prompt_injected")

		time.Sleep(400 * time.Millisecond)
		baseline, _ := latestResponseSnapshot(page, p.spec.responseSelectors...)
		if sendBtn, err := FindElement(page, 5*time.Second, p.Name(), "send", p.spec.sendSelectors); err != nil {
			_ = pressEnter(inputEl)
		} else if _, err := sendBtn.Timeout(5 * time.Second).Eval(`function(){ this.click(); return true; }`); err != nil {
			p.discardPage(resolvedID, page, existing)
			return
		}
		telemetry.Mark(ctx, "request_sent")

		for token := range streamFromResponsePolling(ctx, page, baseline, 4*time.Second, p.spec.responseSelectors...) {
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

func (p *WebChatProvider) discardPage(conversationID string, page *rod.Page, existing bool) {
	if existing {
		p.conversations.Delete(conversationID)
	} else if page != nil {
		_ = page.Close()
	}
}
