package dashboard

import (
	"errors"
	"fmt"
	"net/url"
	"strings"
	"time"
	"unicode/utf8"

	"go-proxy/internal/browser"

	"github.com/go-rod/rod"
	"github.com/go-rod/rod/lib/proto"
)

const (
	remoteViewportWidth  = 1280
	remoteViewportHeight = 720
	maxInputTextBytes    = 4096
	maxInputEventsSecond = 120
	minFrameInterval     = 100 * time.Millisecond
)

var (
	errLoginNotFound   = errors.New("login job not found")
	errStaleLoginJob   = errors.New("stale login job")
	errLoginNotReady   = errors.New("login browser is not ready")
	errBridgeRateLimit = errors.New("remote browser rate limit exceeded")
	errInvalidInput    = errors.New("invalid remote browser input")
)

// remoteLoginPage is deliberately narrower than Rod's page API. The dashboard
// can only request pixels and constrained input operations; it never receives
// DOM, CDP, cookie, storage, or debugger access.
type remoteLoginPage interface {
	CaptureFrame() ([]byte, string, error)
	Click(x, y float64, button proto.InputMouseButton, clickCount int) error
	PointerMove(x, y float64) error
	PointerDown(button proto.InputMouseButton, clickCount int) error
	PointerUp(button proto.InputMouseButton, clickCount int) error
	Scroll(x, y, deltaX, deltaY float64) error
	Key(key, code string, modifiers int) error
	InsertText(text string) error
	CaptureSessionData() (*browser.SessionData, error)
}

type rodRemoteLoginPage struct {
	page *rod.Page
}

func newRodRemoteLoginPage(page *rod.Page) (*rodRemoteLoginPage, error) {
	if err := page.SetViewport(&proto.EmulationSetDeviceMetricsOverride{
		Width:             remoteViewportWidth,
		Height:            remoteViewportHeight,
		DeviceScaleFactor: 1,
		Mobile:            false,
	}); err != nil {
		return nil, err
	}
	return &rodRemoteLoginPage{page: page}, nil
}

func (p *rodRemoteLoginPage) CaptureFrame() ([]byte, string, error) {
	quality := 72
	data, err := p.page.Screenshot(false, &proto.PageCaptureScreenshot{
		Format:           proto.PageCaptureScreenshotFormatJpeg,
		Quality:          &quality,
		OptimizeForSpeed: true,
	})
	if err != nil {
		return nil, "", err
	}
	location := ""
	if info, infoErr := p.page.Info(); infoErr == nil && info != nil {
		location = safePageLocation(info.URL)
	}
	return data, location, nil
}

func (p *rodRemoteLoginPage) Click(x, y float64, button proto.InputMouseButton, clickCount int) error {
	if err := p.page.Mouse.MoveTo(proto.Point{X: x, Y: y}); err != nil {
		return err
	}
	return p.page.Mouse.Click(button, clickCount)
}

func (p *rodRemoteLoginPage) PointerMove(x, y float64) error {
	return p.page.Mouse.MoveTo(proto.Point{X: x, Y: y})
}

func (p *rodRemoteLoginPage) PointerDown(button proto.InputMouseButton, clickCount int) error {
	return p.page.Mouse.Down(button, clickCount)
}

func (p *rodRemoteLoginPage) PointerUp(button proto.InputMouseButton, clickCount int) error {
	return p.page.Mouse.Up(button, clickCount)
}

func (p *rodRemoteLoginPage) Scroll(x, y, deltaX, deltaY float64) error {
	if err := p.page.Mouse.MoveTo(proto.Point{X: x, Y: y}); err != nil {
		return err
	}
	return p.page.Mouse.Scroll(deltaX, deltaY, 1)
}

func (p *rodRemoteLoginPage) Key(key, code string, modifiers int) error {
	keyCode := browserVirtualKeyCode(key)
	down := proto.InputDispatchKeyEvent{
		Type:                  proto.InputDispatchKeyEventTypeRawKeyDown,
		Key:                   key,
		Code:                  code,
		Modifiers:             modifiers,
		WindowsVirtualKeyCode: keyCode,
		NativeVirtualKeyCode:  keyCode,
	}
	if err := down.Call(p.page); err != nil {
		return err
	}
	return proto.InputDispatchKeyEvent{
		Type:                  proto.InputDispatchKeyEventTypeKeyUp,
		Key:                   key,
		Code:                  code,
		Modifiers:             modifiers,
		WindowsVirtualKeyCode: keyCode,
		NativeVirtualKeyCode:  keyCode,
	}.Call(p.page)
}

// Call CDP directly instead of Rod's traced InsertText helper so authentication
// input cannot appear in Rod trace labels even if tracing is enabled later.
func (p *rodRemoteLoginPage) InsertText(text string) error {
	return proto.InputInsertText{Text: text}.Call(p.page)
}

func (p *rodRemoteLoginPage) CaptureSessionData() (*browser.SessionData, error) {
	return browser.CaptureSessionData(p.page)
}

type remoteInput struct {
	JobID      string  `json:"job_id"`
	Type       string  `json:"type"`
	X          float64 `json:"x"`
	Y          float64 `json:"y"`
	DeltaX     float64 `json:"delta_x"`
	DeltaY     float64 `json:"delta_y"`
	Button     string  `json:"button"`
	ClickCount int     `json:"click_count"`
	Key        string  `json:"key"`
	Code       string  `json:"code"`
	Modifiers  int     `json:"modifiers"`
	Text       string  `json:"text"`
}

type remoteFrame struct {
	JPEG     []byte
	Location string
}

func (lm *LoginManager) CaptureFrame(providerName, jobID string) (*remoteFrame, error) {
	job, page, err := lm.lockActivePage(providerName, jobID, true)
	if err != nil {
		return nil, err
	}
	defer job.ioMu.Unlock()

	data, location, err := page.CaptureFrame()
	if err != nil {
		return nil, fmt.Errorf("capture remote browser frame: %w", err)
	}
	return &remoteFrame{JPEG: data, Location: location}, nil
}

func (lm *LoginManager) RelayInput(providerName string, event remoteInput) error {
	if err := validateRemoteInput(&event); err != nil {
		return err
	}
	job, page, err := lm.lockActivePage(providerName, event.JobID, false)
	if err != nil {
		return err
	}
	defer job.ioMu.Unlock()

	x := event.X * remoteViewportWidth
	y := event.Y * remoteViewportHeight
	switch event.Type {
	case "click":
		return page.Click(x, y, mouseButton(event.Button), event.ClickCount)
	case "pointer_move":
		return page.PointerMove(x, y)
	case "pointer_down":
		if err := page.PointerMove(x, y); err != nil {
			return err
		}
		return page.PointerDown(mouseButton(event.Button), event.ClickCount)
	case "pointer_up":
		if err := page.PointerMove(x, y); err != nil {
			return err
		}
		return page.PointerUp(mouseButton(event.Button), event.ClickCount)
	case "scroll":
		return page.Scroll(x, y, event.DeltaX, event.DeltaY)
	case "key":
		return page.Key(event.Key, event.Code, event.Modifiers)
	case "text":
		return page.InsertText(event.Text)
	default:
		return errInvalidInput
	}
}

// lockActivePage validates job ownership and rate limits while holding manager
// state, then takes the job I/O lock before releasing that state. Cancellation
// uses the same lock order, so the page cannot close during an operation.
func (lm *LoginManager) lockActivePage(providerName, jobID string, frame bool) (*LoginJob, remoteLoginPage, error) {
	lm.mu.Lock()
	defer lm.mu.Unlock()

	job, ok := lm.jobs[providerName]
	if !ok {
		return nil, nil, errLoginNotFound
	}
	if jobID == "" || job.ID != jobID {
		return nil, nil, errStaleLoginJob
	}
	if job.Status != LoginStatusWaiting || job.page == nil {
		return nil, nil, errLoginNotReady
	}

	now := time.Now()
	if frame {
		if !job.lastFrameAt.IsZero() && now.Sub(job.lastFrameAt) < minFrameInterval {
			return nil, nil, errBridgeRateLimit
		}
		job.lastFrameAt = now
	} else {
		if job.inputWindowAt.IsZero() || now.Sub(job.inputWindowAt) >= time.Second {
			job.inputWindowAt = now
			job.inputWindowCount = 0
		}
		if job.inputWindowCount >= maxInputEventsSecond {
			return nil, nil, errBridgeRateLimit
		}
		job.inputWindowCount++
	}
	job.lastClientAt = now
	job.ioMu.Lock()
	return job, job.page, nil
}

func validateRemoteInput(event *remoteInput) error {
	if event.JobID == "" || len(event.JobID) > 128 {
		return errInvalidInput
	}
	switch event.Type {
	case "click":
		fallthrough
	case "pointer_down", "pointer_up":
		if !validNormalizedPoint(event.X, event.Y) {
			return errInvalidInput
		}
		if event.ClickCount == 0 {
			event.ClickCount = 1
		}
		if event.ClickCount < 1 || event.ClickCount > 2 {
			return errInvalidInput
		}
		if event.Button == "" {
			event.Button = "left"
		}
		if event.Button != "left" && event.Button != "middle" && event.Button != "right" {
			return errInvalidInput
		}
	case "pointer_move":
		if !validNormalizedPoint(event.X, event.Y) {
			return errInvalidInput
		}
	case "scroll":
		if !validNormalizedPoint(event.X, event.Y) ||
			event.DeltaX < -2000 || event.DeltaX > 2000 ||
			event.DeltaY < -2000 || event.DeltaY > 2000 {
			return errInvalidInput
		}
	case "key":
		if !validRemoteKey(event.Key, event.Code, event.Modifiers) {
			return errInvalidInput
		}
	case "text":
		if event.Text == "" || len(event.Text) > maxInputTextBytes || !utf8.ValidString(event.Text) {
			return errInvalidInput
		}
	default:
		return errInvalidInput
	}
	return nil
}

func validNormalizedPoint(x, y float64) bool {
	return x >= 0 && x <= 1 && y >= 0 && y <= 1
}

func validRemoteKey(key, code string, modifiers int) bool {
	if modifiers < 0 || modifiers > 15 || len(key) > 20 || len(code) > 32 {
		return false
	}
	switch key {
	case "Enter", "Tab", "Escape", "Backspace", "Delete",
		"ArrowUp", "ArrowDown", "ArrowLeft", "ArrowRight",
		"Home", "End", "PageUp", "PageDown":
		return true
	}
	return modifiers != 0 && utf8.RuneCountInString(key) == 1
}

func mouseButton(name string) proto.InputMouseButton {
	switch name {
	case "middle":
		return proto.InputMouseButtonMiddle
	case "right":
		return proto.InputMouseButtonRight
	default:
		return proto.InputMouseButtonLeft
	}
}

func browserVirtualKeyCode(key string) int {
	codes := map[string]int{
		"Backspace": 8, "Tab": 9, "Enter": 13, "Escape": 27,
		"Home": 36, "ArrowLeft": 37, "ArrowUp": 38, "ArrowRight": 39,
		"ArrowDown": 40, "PageUp": 33, "PageDown": 34, "End": 35,
		"Delete": 46,
	}
	if code, ok := codes[key]; ok {
		return code
	}
	r, _ := utf8.DecodeRuneInString(strings.ToUpper(key))
	return int(r)
}

func safePageLocation(raw string) string {
	parsed, err := url.Parse(raw)
	if err != nil || (parsed.Scheme != "http" && parsed.Scheme != "https") || parsed.Host == "" {
		return "Browser page"
	}
	// Query strings, fragments, and paths may contain OAuth state or codes.
	return parsed.Scheme + "://" + parsed.Host
}
