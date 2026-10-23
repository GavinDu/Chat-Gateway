package dashboard

import (
	"bytes"
	"net/http"
	"net/http/httptest"
	"strings"
	"sync"
	"testing"
	"time"

	"go-proxy/internal/browser"
	"go-proxy/internal/config"

	"github.com/gin-gonic/gin"
	"github.com/go-rod/rod/lib/proto"
)

type fakeRemoteLoginPage struct {
	mu sync.Mutex

	frame        []byte
	location     string
	captures     int
	sessionSaves int
	clicks       []remoteInput
	pointers     []remoteInput
	scrolls      []remoteInput
	keys         []remoteInput
	texts        []string
	captureStart chan struct{}
	captureDone  chan struct{}
	captureOnce  sync.Once
}

func (p *fakeRemoteLoginPage) CaptureFrame() ([]byte, string, error) {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.captures++
	return append([]byte(nil), p.frame...), p.location, nil
}

func (p *fakeRemoteLoginPage) Click(x, y float64, button proto.InputMouseButton, clickCount int) error {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.clicks = append(p.clicks, remoteInput{
		X: x, Y: y, Button: string(button), ClickCount: clickCount,
	})
	return nil
}

func (p *fakeRemoteLoginPage) PointerMove(x, y float64) error {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.pointers = append(p.pointers, remoteInput{Type: "pointer_move", X: x, Y: y})
	return nil
}

func (p *fakeRemoteLoginPage) PointerDown(button proto.InputMouseButton, clickCount int) error {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.pointers = append(p.pointers, remoteInput{Type: "pointer_down", Button: string(button), ClickCount: clickCount})
	return nil
}

func (p *fakeRemoteLoginPage) PointerUp(button proto.InputMouseButton, clickCount int) error {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.pointers = append(p.pointers, remoteInput{Type: "pointer_up", Button: string(button), ClickCount: clickCount})
	return nil
}

func (p *fakeRemoteLoginPage) Scroll(x, y, deltaX, deltaY float64) error {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.scrolls = append(p.scrolls, remoteInput{
		X: x, Y: y, DeltaX: deltaX, DeltaY: deltaY,
	})
	return nil
}

func (p *fakeRemoteLoginPage) Key(key, code string, modifiers int) error {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.keys = append(p.keys, remoteInput{Key: key, Code: code, Modifiers: modifiers})
	return nil
}

func (p *fakeRemoteLoginPage) InsertText(text string) error {
	p.mu.Lock()
	defer p.mu.Unlock()
	p.texts = append(p.texts, text)
	return nil
}

func (p *fakeRemoteLoginPage) CaptureSessionData() (*browser.SessionData, error) {
	p.captureOnce.Do(func() {
		if p.captureStart != nil {
			close(p.captureStart)
		}
	})
	if p.captureDone != nil {
		<-p.captureDone
	}
	p.mu.Lock()
	defer p.mu.Unlock()
	p.sessionSaves++
	return &browser.SessionData{}, nil
}

func installTestLoginJob(lm *LoginManager, provider, id string, page remoteLoginPage) {
	lm.mu.Lock()
	lm.jobs[provider] = &LoginJob{
		ID:           id,
		Provider:     provider,
		Status:       LoginStatusWaiting,
		Message:      "ready",
		StartedAt:    time.Now(),
		page:         page,
		lastClientAt: time.Now(),
		ioMu:         &sync.Mutex{},
	}
	lm.mu.Unlock()
}

func TestSafePageLocationStripsRedirectSecrets(t *testing.T) {
	got := safePageLocation("https://accounts.example.com/oauth/callback?code=secret&state=private#token")
	if got != "https://accounts.example.com" {
		t.Fatalf("safe location = %q", got)
	}
	if got := safePageLocation("javascript:alert(1)"); got != "Browser page" {
		t.Fatalf("unsafe location = %q", got)
	}
}

func TestRemoteBridgeRelaysInteractionsAndRejectsStaleJobs(t *testing.T) {
	lm := NewLoginManager()
	defer lm.Close()
	lm.saveSession = func(string, *browser.SessionData) error { return nil }
	page := &fakeRemoteLoginPage{
		frame:    []byte{0xff, 0xd8, 0xff, 0xd9},
		location: "https://accounts.example.com",
	}
	installTestLoginJob(lm, "qwen", "current-job", page)

	frame, err := lm.CaptureFrame("qwen", "current-job")
	if err != nil {
		t.Fatal(err)
	}
	if !bytes.Equal(frame.JPEG, page.frame) || frame.Location != page.location {
		t.Fatalf("unexpected frame: %#v", frame)
	}

	events := []remoteInput{
		{JobID: "current-job", Type: "click", X: .25, Y: .5, Button: "left", ClickCount: 1},
		{JobID: "current-job", Type: "pointer_down", X: .25, Y: .5, Button: "left", ClickCount: 1},
		{JobID: "current-job", Type: "pointer_move", X: .5, Y: .75},
		{JobID: "current-job", Type: "pointer_up", X: .75, Y: .5, Button: "left", ClickCount: 1},
		{JobID: "current-job", Type: "scroll", X: .5, Y: .5, DeltaY: 240},
		{JobID: "current-job", Type: "key", Key: "Enter", Code: "Enter"},
		{JobID: "current-job", Type: "text", Text: "not-recorded-by-the-bridge"},
	}
	for _, event := range events {
		if err := lm.RelayInput("qwen", event); err != nil {
			t.Fatalf("relay %s: %v", event.Type, err)
		}
	}

	if len(page.clicks) != 1 || page.clicks[0].X != 320 || page.clicks[0].Y != 360 {
		t.Fatalf("click not mapped to viewport: %#v", page.clicks)
	}
	if len(page.scrolls) != 1 || len(page.keys) != 1 || len(page.texts) != 1 {
		t.Fatalf("events were not relayed: scroll=%d key=%d text=%d", len(page.scrolls), len(page.keys), len(page.texts))
	}
	if len(page.pointers) != 5 ||
		page.pointers[0].Type != "pointer_move" || page.pointers[0].X != 320 || page.pointers[0].Y != 360 ||
		page.pointers[1].Type != "pointer_down" ||
		page.pointers[2].Type != "pointer_move" || page.pointers[2].X != 640 || page.pointers[2].Y != 540 ||
		page.pointers[3].Type != "pointer_move" || page.pointers[3].X != 960 || page.pointers[3].Y != 360 ||
		page.pointers[4].Type != "pointer_up" {
		t.Fatalf("pointer drag not relayed in order: %#v", page.pointers)
	}

	if _, err := lm.CaptureFrame("qwen", "replaced-job"); err != errStaleLoginJob {
		t.Fatalf("stale frame error = %v", err)
	}
	if err := lm.RelayInput("qwen", remoteInput{
		JobID: "replaced-job", Type: "text", Text: "x",
	}); err != errStaleLoginJob {
		t.Fatalf("stale input error = %v", err)
	}
}

func TestLoginSaveAndCancelRequireCurrentJob(t *testing.T) {
	lm := NewLoginManager()
	defer lm.Close()
	lm.saveSession = func(string, *browser.SessionData) error { return nil }
	page := &fakeRemoteLoginPage{}
	installTestLoginJob(lm, "qwen", "save-job", page)

	if err := lm.SaveSession("qwen", "old-job"); err != errStaleLoginJob {
		t.Fatalf("stale save error = %v", err)
	}
	if err := lm.SaveSession("qwen", "save-job"); err != nil {
		t.Fatal(err)
	}
	if page.sessionSaves != 1 {
		t.Fatalf("session saves = %d", page.sessionSaves)
	}
	if status := lm.Status("qwen"); status == nil || status.Status != LoginStatusSaved {
		t.Fatalf("saved status = %#v", status)
	}

	installTestLoginJob(lm, "qwen", "cancel-job", page)
	if err := lm.CancelLogin("qwen", "save-job"); err != errStaleLoginJob {
		t.Fatalf("stale cancel error = %v", err)
	}
	if err := lm.CancelLogin("qwen", "cancel-job"); err != nil {
		t.Fatal(err)
	}
	if status := lm.Status("qwen"); status != nil {
		t.Fatalf("cancelled job remains: %#v", status)
	}
}

func TestCancellationBeforeStartPreventsLateJobCreation(t *testing.T) {
	lm := NewLoginManager()
	defer lm.Close()
	jobID := "b8ea2db1-b913-4f2a-92e8-fc3b3a4eed2c"

	if err := lm.CancelLogin("qwen", jobID); err != nil {
		t.Fatal(err)
	}
	if _, err := lm.StartLogin("qwen", jobID); err != errStaleLoginJob {
		t.Fatalf("late start error = %v", err)
	}
	if status := lm.Status("qwen"); status != nil {
		t.Fatalf("cancelled late start created job: %#v", status)
	}
}

func TestSavingRejectsCancelAndReplacementUntilCommit(t *testing.T) {
	lm := NewLoginManager()
	defer lm.Close()
	commits := 0
	lm.saveSession = func(string, *browser.SessionData) error {
		commits++
		return nil
	}
	page := &fakeRemoteLoginPage{
		captureStart: make(chan struct{}),
		captureDone:  make(chan struct{}),
	}
	installTestLoginJob(lm, "metaai", "save-current", page)

	saveDone := make(chan error, 1)
	go func() {
		saveDone <- lm.SaveSession("metaai", "save-current")
	}()
	<-page.captureStart

	if err := lm.CancelLogin("metaai", "save-current"); err != errLoginNotReady {
		t.Fatalf("cancel during save error = %v", err)
	}
	replacementID := "eb48d887-114b-4e05-859f-d255cdee84a6"
	if _, err := lm.StartLogin("metaai", replacementID); err != errLoginNotReady {
		t.Fatalf("replacement during save error = %v", err)
	}

	close(page.captureDone)
	if err := <-saveDone; err != nil {
		t.Fatal(err)
	}
	if commits != 1 {
		t.Fatalf("session commits = %d", commits)
	}
	status := lm.Status("metaai")
	if status == nil || status.ID != "save-current" || status.Status != LoginStatusSaved {
		t.Fatalf("unexpected final status: %#v", status)
	}
}

func TestRemoteBridgeRoutesRequireDashboardSecret(t *testing.T) {
	gin.SetMode(gin.TestMode)
	lm := NewLoginManager()
	defer lm.Close()
	page := &fakeRemoteLoginPage{
		frame:    []byte{0xff, 0xd8, 0xff, 0xd9},
		location: "https://login.example.com",
	}
	installTestLoginJob(lm, "metaai", "route-job", page)

	h := &Handler{
		cfg:      &config.Config{Server: config.ServerConfig{DashboardSecret: "test-secret"}},
		loginMgr: lm,
	}
	router := gin.New()
	api := router.Group("/dashboard/api", h.requireSecret())
	api.GET("/providers/:name/login/status", h.LoginStatus)
	api.GET("/providers/:name/login/frame", h.LoginFrame)
	api.POST("/providers/:name/login/input", h.LoginInput)

	req := httptest.NewRequest(http.MethodGet, "/dashboard/api/providers/metaai/login/frame?job_id=route-job", nil)
	rec := httptest.NewRecorder()
	router.ServeHTTP(rec, req)
	if rec.Code != http.StatusUnauthorized {
		t.Fatalf("unauthorized frame status = %d", rec.Code)
	}

	req = httptest.NewRequest(http.MethodGet, "/dashboard/api/providers/metaai/login/frame?job_id=route-job", nil)
	req.Header.Set("Authorization", "Bearer test-secret")
	rec = httptest.NewRecorder()
	router.ServeHTTP(rec, req)
	if rec.Code != http.StatusOK {
		t.Fatalf("authorized frame status = %d body=%s", rec.Code, rec.Body.String())
	}
	if got := rec.Header().Get("X-Login-Origin"); got != "https://login.example.com" {
		t.Fatalf("origin header = %q", got)
	}
	if cache := rec.Header().Get("Cache-Control"); !strings.Contains(cache, "no-store") {
		t.Fatalf("cache control = %q", cache)
	}

	req = httptest.NewRequest(http.MethodGet, "/dashboard/api/providers/metaai/login/status?job_id=old-job", nil)
	req.Header.Set("Authorization", "Bearer test-secret")
	rec = httptest.NewRecorder()
	router.ServeHTTP(rec, req)
	if rec.Code != http.StatusConflict || strings.Contains(rec.Body.String(), "route-job") {
		t.Fatalf("stale status leaked replacement: status=%d body=%s", rec.Code, rec.Body.String())
	}

	body := `{"job_id":"route-job","type":"click","x":0.5,"y":0.5,"button":"left","click_count":1}`
	req = httptest.NewRequest(http.MethodPost, "/dashboard/api/providers/metaai/login/input", strings.NewReader(body))
	req.Header.Set("Authorization", "Bearer test-secret")
	req.Header.Set("Content-Type", "application/json")
	rec = httptest.NewRecorder()
	router.ServeHTTP(rec, req)
	if rec.Code != http.StatusNoContent {
		t.Fatalf("input status = %d body=%s", rec.Code, rec.Body.String())
	}

	req = httptest.NewRequest(http.MethodPost, "/dashboard/api/providers/metaai/login/input",
		strings.NewReader(`{"job_id":"old-job","type":"text","text":"x"}`))
	req.Header.Set("Authorization", "Bearer test-secret")
	req.Header.Set("Content-Type", "application/json")
	rec = httptest.NewRecorder()
	router.ServeHTTP(rec, req)
	if rec.Code != http.StatusConflict {
		t.Fatalf("stale input status = %d body=%s", rec.Code, rec.Body.String())
	}
}

func TestRemoteInputValidation(t *testing.T) {
	invalid := []remoteInput{
		{JobID: "job", Type: "click", X: -1, Y: 0},
		{JobID: "job", Type: "pointer_down", X: .5, Y: .5, Button: "invalid"},
		{JobID: "job", Type: "pointer_move", X: 2, Y: .5},
		{JobID: "job", Type: "scroll", X: .5, Y: .5, DeltaY: 3000},
		{JobID: "job", Type: "key", Key: "F12", Code: "F12"},
		{JobID: "job", Type: "text", Text: ""},
		{JobID: "job", Type: "unknown"},
	}
	for _, event := range invalid {
		event := event
		t.Run(event.Type, func(t *testing.T) {
			if err := validateRemoteInput(&event); err != errInvalidInput {
				t.Fatalf("validation error = %v", err)
			}
		})
	}
}
