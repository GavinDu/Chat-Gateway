package api

import (
	"context"
	"encoding/json"
	"fmt"
	"net/http"
	"strings"
	"time"

	"go-proxy/internal/dashboard"
	"go-proxy/internal/hooks"
	"go-proxy/internal/memory"
	"go-proxy/internal/router"
	"go-proxy/internal/telemetry"
	"go-proxy/internal/types"

	"github.com/gin-gonic/gin"
	"github.com/google/uuid"
	"github.com/rs/zerolog/log"
)

// Handler holds shared state for API handlers.
type Handler struct {
	registry *router.Registry
	store    memory.Store
	hooks    *hooks.Dispatcher
}

// NewHandler creates a new Handler.  store may be a memory.NoopStore when
// FalkorDB is not configured.  dispatcher may be nil for a no-op hooks layer;
// all Fire* methods on Dispatcher guard against nil receivers.
func NewHandler(reg *router.Registry, store memory.Store, dispatcher *hooks.Dispatcher) *Handler {
	return &Handler{registry: reg, store: store, hooks: dispatcher}
}

// RegisterRoutes wires up all API routes onto a gin Engine.
func (h *Handler) RegisterRoutes(r *gin.Engine) {
	v1 := r.Group("/v1")
	v1.GET("/models", h.ListModels)
	v1.POST("/chat/completions", h.ChatCompletions)
	r.GET("/health", h.Health)
}

// Health is a lightweight liveness probe.
func (h *Handler) Health(c *gin.Context) {
	status := gin.H{"status": "ok", "time": time.Now().UTC()}
	providerStatus := make(map[string]string)
	for _, p := range h.registry.AllProviders() {
		if h.registry.IsDegraded(p.Name()) {
			providerStatus[p.Name()] = "degraded"
		} else {
			providerStatus[p.Name()] = "ok"
		}
	}
	status["providers"] = providerStatus
	c.JSON(http.StatusOK, status)
}

// ListModels returns the effective model catalog across all providers. The
// catalog combines configured aliases with the last successful page discovery,
// preserving the standard OpenAI model-list response shape.
func (h *Handler) ListModels(c *gin.Context) {
	var models []Model
	for _, info := range h.registry.GetProviderInfos() {
		created := info.ModelsLastRefreshedAt
		if created == 0 {
			created = 1700000000
		}
		for _, m := range info.Models {
			models = append(models, Model{
				ID:      m,
				Object:  "model",
				Created: created,
				OwnedBy: info.Name,
			})
		}
	}
	c.JSON(http.StatusOK, ModelListResponse{Object: "list", Data: models})
}

// ChatCompletions handles POST /v1/chat/completions with full provider failover.
//
// Memory layer (FalkorDB):
//   - If X-Conversation-Id is present, the stored message history is loaded and
//     merged with req.Messages before calling the provider.  This means a client
//     can send only the latest user message and the proxy reconstructs the full
//     context from the graph.
//   - On success, the new turn (messages + assistant response) is saved to the
//     graph in a background goroutine so the response is not delayed.
//
// Provider failover:
//   - Providers are tried in priority order.  When a provider is degraded or
//     returns an empty response, the next one is tried transparently.  The
//     response headers X-Provider and X-Conversation-Id reflect the provider
//     that ultimately served the request.
func (h *Handler) ChatCompletions(c *gin.Context) {
	requestStart := time.Now()
	traceID := uuid.NewString()
	c.Set("trace_id", traceID)
	var req ChatCompletionRequest
	if err := c.ShouldBindJSON(&req); err != nil {
		c.JSON(http.StatusBadRequest, ErrorResponse{Error: ErrorDetail{
			Message: "invalid request body: " + err.Error(),
			Type:    "invalid_request_error",
			Code:    "bad_request",
		}})
		return
	}
	c.Set("parse_ms", time.Since(requestStart).Milliseconds())
	if len(req.Messages) == 0 {
		c.JSON(http.StatusBadRequest, ErrorResponse{Error: ErrorDetail{
			Message: "messages array must not be empty",
			Type:    "invalid_request_error",
			Code:    "bad_request",
		}})
		return
	}

	c.Set("model_requested", req.Model)

	conversationID := c.GetHeader("X-Conversation-Id")
	baseCtx := c.Request.Context()

	// --- Memory layer: load stored history and build effective messages ------
	var hist []types.Message
	memoryReadStart := time.Now()
	if conversationID != "" {
		if h2, err := h.store.GetHistory(baseCtx, conversationID); err == nil {
			hist = h2
		} else {
			log.Warn().Err(err).Str("conversation_id", conversationID).Msg("memory: get history failed, using request messages")
		}
	}
	c.Set("memory_read_ms", time.Since(memoryReadStart).Milliseconds())
	contextBuildStart := time.Now()
	effectiveMessages := buildEffectiveMessages(hist, req.Messages)
	c.Set("context_build_ms", time.Since(contextBuildStart).Milliseconds())
	// -------------------------------------------------------------------------

	routingStart := time.Now()
	candidates := h.registry.ResolveAll(req.Model)
	c.Set("routing_ms", time.Since(routingStart).Milliseconds())
	if len(candidates) == 0 {
		model := strings.ToLower(strings.TrimSpace(req.Model))
		if model != "" && model != "auto" {
			c.JSON(http.StatusBadRequest, ErrorResponse{Error: ErrorDetail{
				Message: fmt.Sprintf("model %q is not configured", req.Model),
				Type:    "invalid_request_error",
				Code:    "model_not_found",
			}})
			return
		}
		c.JSON(http.StatusServiceUnavailable, ErrorResponse{Error: ErrorDetail{
			Message: "no providers configured",
			Type:    "server_error",
			Code:    "no_provider",
		}})
		return
	}

	completionID := "chatcmpl-" + uuid.New().String()
	created := time.Now().Unix()

	var attempted []string
	var skippedAny bool
	var lastErr error
	var resolvedConvID string

	// attemptDetails records per-provider outcome for the dashboard retry view.
	var attemptDetails []dashboard.AttemptDetail

	for _, provider := range candidates {
		if h.registry.IsDegraded(provider.Name()) {
			log.Debug().Str("provider", provider.Name()).Msg("skipping degraded provider")
			skippedAny = true
			attemptDetails = append(attemptDetails, dashboard.AttemptDetail{
				Provider: provider.Name(),
				Skipped:  true,
			})
			continue
		}

		timeout := h.registry.TimeoutFor(provider.Name())
		reqCtx, cancel := context.WithTimeout(baseCtx, timeout)
		reqCtx, attemptTrace := telemetry.WithAttempt(reqCtx)

		log.Info().
			Str("provider", provider.Name()).
			Str("model", req.Model).
			Bool("stream", req.Stream).
			Dur("timeout", timeout).
			Msg("trying provider")

		attempted = append(attempted, provider.Name())
		attemptStart := time.Now()

		var tokenCh <-chan string
		var sendErr error
		providerModel := h.registry.ProviderModel(req.Model, provider.Name())
		tokenCh, resolvedConvID, sendErr = provider.SendMessage(reqCtx, providerModel, conversationID, effectiveMessages)
		if sendErr != nil {
			cancel()
			dur := time.Since(attemptStart).Milliseconds()
			h.registry.MarkDegraded(provider.Name())
			lastErr = fmt.Errorf("provider %s: %w", provider.Name(), sendErr)
			log.Warn().Err(sendErr).Str("provider", provider.Name()).Msg("provider failed, trying next")
			attemptDetails = append(attemptDetails, dashboard.AttemptDetail{
				Provider:    provider.Name(),
				DurationMs:  dur,
				Error:       sendErr.Error(),
				Performance: attemptTrace.Snapshot(),
			})
			continue
		}

		resolvedModel := req.Model
		if resolvedModel == "" || resolvedModel == "auto" {
			resolvedModel = provider.Name()
		}

		var succeeded bool
		var responseContent string
		if req.Stream {
			succeeded, responseContent = h.streamResponse(reqCtx, c, tokenCh, completionID, created, resolvedModel, provider.Name(), resolvedConvID)
		} else {
			succeeded, responseContent = h.collectResponse(reqCtx, c, tokenCh, completionID, created, resolvedModel, provider.Name(), resolvedConvID)
		}
		cancel()
		dur := time.Since(attemptStart).Milliseconds()

		if succeeded {
			attemptDetails = append(attemptDetails, dashboard.AttemptDetail{
				Provider:    provider.Name(),
				DurationMs:  dur,
				Succeeded:   true,
				Performance: attemptTrace.Snapshot(),
			})

			// Fire conversation.start hook when a new tab was opened (new
			// conversation or tab eviction + re-open).  A new tab is indicated
			// by the resolved ID differing from what the client sent.
			isNewConversation := resolvedConvID != conversationID
			if isNewConversation {
				h.hooks.FireConversationStart(resolvedConvID, provider.Name(), resolvedModel)
			}

			// Persist the turn to FalkorDB in a background goroutine.
			if responseContent != "" {
				pName := provider.Name()
				go h.saveConversationTurn(resolvedConvID, conversationID, hist, effectiveMessages, responseContent, pName, resolvedModel)
			}

			fallback := skippedAny || len(attempted) > 1
			c.Set("fallback_triggered", fallback)
			c.Set("attempted_providers", attempted)
			c.Set("attempt_details", attemptDetails)
			return
		}

		attemptDetails = append(attemptDetails, dashboard.AttemptDetail{
			Provider:    provider.Name(),
			DurationMs:  dur,
			Error:       "empty response",
			Performance: attemptTrace.Snapshot(),
		})
		h.registry.MarkDegraded(provider.Name())
		log.Warn().Str("provider", provider.Name()).Msg("provider returned empty response, trying next")
	}

	c.Set("fallback_triggered", skippedAny || len(attempted) > 1)
	c.Set("attempted_providers", attempted)
	c.Set("attempt_details", attemptDetails)

	msg := "all providers are unavailable"
	if lastErr != nil {
		msg = lastErr.Error()
	}
	if !c.Writer.Written() {
		c.JSON(http.StatusServiceUnavailable, ErrorResponse{Error: ErrorDetail{
			Message: msg,
			Type:    "server_error",
			Code:    "no_provider_available",
		}})
	}
}

// buildEffectiveMessages merges stored history with the incoming request
// messages.  When history exists, the proxy reconstructs full context by
// combining the stored turns with the last user message from the request —
// so a client only needs to send the new user message, not the full history.
// When history is empty the request messages are used unchanged.
func buildEffectiveMessages(hist []types.Message, reqMessages []types.Message) []types.Message {
	if len(hist) == 0 {
		return reqMessages
	}

	// Find the last user message in the request (the new turn input).
	var lastUser *types.Message
	for i := len(reqMessages) - 1; i >= 0; i-- {
		if reqMessages[i].Role == "user" {
			cp := reqMessages[i]
			lastUser = &cp
			break
		}
	}
	if lastUser == nil {
		// No user message — unusual, fall back to the request as-is.
		return reqMessages
	}

	// Prepend stored history before the new user message.
	effective := make([]types.Message, len(hist)+1)
	copy(effective, hist)
	effective[len(hist)] = *lastUser
	return effective
}

// saveConversationTurn determines which messages are new (delta) and persists
// them to the memory store.  Called in a background goroutine so the HTTP
// response is never delayed by graph writes.
func (h *Handler) saveConversationTurn(
	resolvedConvID, originalConvID string,
	hist, effectiveMessages []types.Message,
	responseContent, provider, model string,
) {
	assistantMsg := types.Message{Role: "assistant", Content: responseContent}

	var msgsToSave []types.Message

	// isNewUUID is true when the provider opened a new tab (either because
	// it's a first turn, the old tab was evicted, or a provider switch
	// happened and a new UUID was minted).  In that case we persist the
	// full reconstructed history so the new UUID has complete provenance.
	isNewUUID := resolvedConvID != originalConvID
	if len(hist) == 0 || isNewUUID {
		msgsToSave = make([]types.Message, len(effectiveMessages)+1)
		copy(msgsToSave, effectiveMessages)
		msgsToSave[len(effectiveMessages)] = assistantMsg
	} else {
		// Continuation on the same tab: append only the new user message
		// (last element of effectiveMessages) and the assistant response.
		newUserMsg := effectiveMessages[len(effectiveMessages)-1]
		msgsToSave = []types.Message{newUserMsg, assistantMsg}
	}

	if err := h.store.SaveTurn(context.Background(), resolvedConvID, provider, model, msgsToSave); err != nil {
		log.Warn().Err(err).
			Str("conversation_id", resolvedConvID).
			Msg("memory: save turn failed")
	}
}

// streamResponse writes SSE chunks as tokens arrive and returns true on
// success plus the accumulated response content.
//
// Critically, it blocks on the first token before writing any HTTP headers.
// A silently-failing provider (channel closes immediately) returns false with
// no bytes written, allowing the caller to fall through to the next provider.
func (h *Handler) streamResponse(ctx context.Context, c *gin.Context, tokenCh <-chan string, id string, created int64, model, providerName, conversationID string) (bool, string) {
	firstToken, ok := <-tokenCh
	if !ok {
		return false, ""
	}
	telemetry.Mark(ctx, "first_token")

	c.Header("Content-Type", "text/event-stream")
	c.Header("Cache-Control", "no-cache")
	c.Header("Connection", "keep-alive")
	c.Header("X-Accel-Buffering", "no")
	c.Header("X-Provider", providerName)
	if conversationID != "" {
		c.Header("X-Conversation-Id", conversationID)
	}
	w := c.Writer

	var content strings.Builder

	sendChunk(w, ChatCompletionChunk{
		ID: id, Object: "chat.completion.chunk", Created: created, Model: model,
		Choices: []ChunkChoice{{Index: 0, Delta: Delta{Role: "assistant"}}},
	})

	content.WriteString(firstToken)
	sendChunk(w, ChatCompletionChunk{
		ID: id, Object: "chat.completion.chunk", Created: created, Model: model,
		Choices: []ChunkChoice{{Index: 0, Delta: Delta{Content: firstToken}}},
	})
	w.Flush()

	for token := range tokenCh {
		content.WriteString(token)
		sendChunk(w, ChatCompletionChunk{
			ID: id, Object: "chat.completion.chunk", Created: created, Model: model,
			Choices: []ChunkChoice{{Index: 0, Delta: Delta{Content: token}}},
		})
		w.Flush()
	}

	sendChunk(w, ChatCompletionChunk{
		ID: id, Object: "chat.completion.chunk", Created: created, Model: model,
		Choices: []ChunkChoice{{Index: 0, Delta: Delta{}, FinishReason: "stop"}},
	})
	fmt.Fprint(w, "data: [DONE]\n\n")
	w.Flush()
	telemetry.Mark(ctx, "complete")
	return true, content.String()
}

func sendChunk(w gin.ResponseWriter, chunk ChatCompletionChunk) {
	b, _ := json.Marshal(chunk)
	fmt.Fprintf(w, "data: %s\n\n", b)
}

// collectResponse buffers all tokens and returns a single JSON response plus
// the accumulated content.  Returns false if the provider produced nothing.
func (h *Handler) collectResponse(ctx context.Context, c *gin.Context, tokenCh <-chan string, id string, created int64, model, providerName, conversationID string) (bool, string) {
	var sb strings.Builder
	for token := range tokenCh {
		telemetry.Mark(ctx, "first_token")
		sb.WriteString(token)
	}
	content := sb.String()
	if content == "" {
		return false, ""
	}
	words := len(strings.Fields(content))
	c.Header("X-Provider", providerName)
	if conversationID != "" {
		c.Header("X-Conversation-Id", conversationID)
	}
	c.JSON(http.StatusOK, ChatCompletionResponse{
		ID: id, Object: "chat.completion", Created: created, Model: model,
		Choices: []Choice{{
			Index:        0,
			Message:      Message{Role: "assistant", Content: content},
			FinishReason: "stop",
		}},
		Usage: Usage{CompletionTokens: words, TotalTokens: words},
	})
	telemetry.Mark(ctx, "complete")
	return true, content
}
