package dashboard

import (
	"fmt"
	"sync"
	"time"

	"go-proxy/internal/browser"

	"github.com/google/uuid"
	"github.com/rs/zerolog/log"
)

// LoginStatus represents the current state of a browser login job.
type LoginStatus string

const (
	LoginStatusStarting LoginStatus = "starting"
	LoginStatusWaiting  LoginStatus = "waiting" // browser open, waiting for user
	LoginStatusSaving   LoginStatus = "saving"
	LoginStatusSaved    LoginStatus = "saved"
	LoginStatusError    LoginStatus = "error"
)

// LoginJob tracks an active browser-based login session for one provider.
type LoginJob struct {
	ID        string      `json:"job_id"`
	Provider  string      `json:"provider"`
	Status    LoginStatus `json:"status"`
	Message   string      `json:"message"`
	StartedAt time.Time   `json:"started_at"`

	pool *browser.Pool   // headed browser pool owned by this job
	page remoteLoginPage // server-side page adapter; never serialized

	// ioMu keeps frame capture, input relay, session capture, and close from
	// racing each other. The lock order is always LoginManager.mu -> ioMu.
	ioMu *sync.Mutex

	lastClientAt     time.Time
	lastFrameAt      time.Time
	inputWindowAt    time.Time
	inputWindowCount int
}

// loginSubscriber is a per-SSE-connection handle returned by Subscribe.
// ch receives LoginJob snapshots; done is closed by Unsubscribe so that
// concurrent notify calls can detect the subscriber is gone without closing ch.
type loginSubscriber struct {
	ch   chan LoginJob
	done chan struct{}
}

// LoginManager manages one in-progress headed-browser login session per provider.
type LoginManager struct {
	mu          sync.Mutex
	jobs        map[string]*LoginJob
	subscribers map[string][]*loginSubscriber
	cancelled   map[string]time.Time
	stop        chan struct{}
	closeOnce   sync.Once
	sweepDone   chan struct{}
	saveSession func(string, *browser.SessionData) error
}

func providerDisplayName(provider string) string {
	switch provider {
	case "metaai":
		return "Meta AI"
	case "qwen":
		return "Qwen"
	default:
		return provider
	}
}

// NewLoginManager creates a LoginManager.
func NewLoginManager() *LoginManager {
	lm := &LoginManager{
		jobs:        make(map[string]*LoginJob),
		subscribers: make(map[string][]*loginSubscriber),
		cancelled:   make(map[string]time.Time),
		stop:        make(chan struct{}),
		sweepDone:   make(chan struct{}),
		saveSession: browser.SaveSession,
	}
	go lm.sweepAbandonedJobs()
	return lm
}

// Subscribe returns a loginSubscriber that receives LoginJob snapshots whenever
// the job status for providerName changes.  The current state (if any) is sent
// immediately so the caller has a baseline.  Call Unsubscribe when done.
func (lm *LoginManager) Subscribe(providerName string) *loginSubscriber {
	sub := &loginSubscriber{
		ch:   make(chan LoginJob, 8),
		done: make(chan struct{}),
	}
	lm.mu.Lock()
	lm.subscribers[providerName] = append(lm.subscribers[providerName], sub)
	// Send the current state immediately — safe because nobody can Unsubscribe us yet.
	if job, ok := lm.jobs[providerName]; ok {
		snap := lm.snapshot(job)
		sub.ch <- *snap
	}
	lm.mu.Unlock()
	return sub
}

// Unsubscribe removes sub from the subscriber list for providerName and closes
// its done channel.  Closing done (not ch) avoids the send-on-closed-channel
// panic: notify() uses select on done to bail out if the subscriber has gone away.
func (lm *LoginManager) Unsubscribe(providerName string, sub *loginSubscriber) {
	lm.mu.Lock()
	subs := lm.subscribers[providerName]
	for i, s := range subs {
		if s == sub {
			lm.subscribers[providerName] = append(subs[:i], subs[i+1:]...)
			break
		}
	}
	lm.mu.Unlock()
	// Signal done so any in-flight notify() goroutine skips sending to sub.ch.
	close(sub.done)
}

// notify pushes a snapshot of the current job state to all subscribers.
// Must be called WITHOUT holding lm.mu.
func (lm *LoginManager) notify(providerName string) {
	lm.mu.Lock()
	job, ok := lm.jobs[providerName]
	if !ok {
		lm.mu.Unlock()
		return
	}
	snap := lm.snapshot(job)
	// Copy the slice so we iterate outside the lock.
	subs := make([]*loginSubscriber, len(lm.subscribers[providerName]))
	copy(subs, lm.subscribers[providerName])
	lm.mu.Unlock()

	for _, sub := range subs {
		select {
		case sub.ch <- *snap:
		case <-sub.done:
			// Subscriber was unsubscribed between copy and send — safe to skip.
		default:
			// Subscriber buffer full — drop rather than block.
		}
	}
}

// StartLogin launches a headed browser for providerName and navigates to its
// home URL.  Any previous job for the same provider is cancelled first.
// The browser opens in a background goroutine; subscribe via Subscribe() for
// push updates, or poll Status() for pull-based updates.
func (lm *LoginManager) StartLogin(providerName, requestedJobID string) (*LoginJob, error) {
	lm.mu.Lock()
	defer lm.mu.Unlock()

	providerURL := browser.ProviderHomeURLs[providerName]
	if providerURL == "" {
		providerURL = "about:blank"
	}

	jobID := requestedJobID
	if jobID == "" {
		jobID = uuid.NewString()
	} else if _, err := uuid.Parse(jobID); err != nil {
		return nil, fmt.Errorf("invalid login job identifier")
	}
	if expires, cancelled := lm.cancelled[jobID]; cancelled {
		delete(lm.cancelled, jobID)
		if time.Now().Before(expires) {
			return nil, errStaleLoginJob
		}
	}

	// Cancel any existing job for this provider only after the requested job ID
	// passes the pre-start cancellation check.
	if old, ok := lm.jobs[providerName]; ok {
		if old.Status == LoginStatusSaving {
			return nil, errLoginNotReady
		}
		lm.cancelLocked(old)
	}

	job := &LoginJob{
		ID:           jobID,
		Provider:     providerName,
		Status:       LoginStatusStarting,
		Message:      "Launching headed browser…",
		StartedAt:    time.Now(),
		lastClientAt: time.Now(),
		ioMu:         &sync.Mutex{},
	}
	lm.jobs[providerName] = job

	go lm.runLogin(job, providerName, providerURL)
	return lm.snapshot(job), nil
}

// runLogin is the background goroutine that opens the browser and navigates
// to the provider URL.  On success it sets Status to waiting; on failure, error.
func (lm *LoginManager) runLogin(job *LoginJob, providerName, providerURL string) {
	// Always use an isolated temporary profile. The main proxy may already own
	// the configured persistent profile, and Chromium refuses to open the same
	// user-data directory twice. CaptureSession transfers cookies/localStorage
	// into the proxy's internal session file after the user finishes login.
	loginPool := browser.NewPool(false, "")

	lm.mu.Lock()
	current, ok := lm.jobs[providerName]
	if !ok || current != job {
		lm.mu.Unlock()
		loginPool.Close()
		return
	}
	job.pool = loginPool
	lm.mu.Unlock()

	entry, err := loginPool.Get(providerName)
	if err != nil {
		lm.mu.Lock()
		current, ok := lm.jobs[providerName]
		if ok && current == job {
			job.pool = nil
			job.Status = LoginStatusError
			job.Message = fmt.Sprintf(
				"Could not launch browser: %v. "+
					"If running headlessly, use the terminal instead: ./go-proxy --login %s",
				err, providerName,
			)
		}
		lm.mu.Unlock()
		loginPool.Close()
		log.Warn().Str("provider", providerName).Err(err).Msg("login browser launch failed")
		lm.notify(providerName)
		return
	}

	lm.mu.Lock()
	current, ok = lm.jobs[providerName]
	if !ok || current != job {
		lm.mu.Unlock()
		loginPool.Close()
		return
	}
	lm.mu.Unlock()

	page, err := browser.NewPage(entry.Browser, providerURL)
	if err != nil {
		lm.mu.Lock()
		current, ok := lm.jobs[providerName]
		if ok && current == job {
			job.pool = nil
			job.Status = LoginStatusError
			job.Message = fmt.Sprintf("Could not open browser page: %v", err)
		}
		lm.mu.Unlock()
		loginPool.Close()
		lm.notify(providerName)
		return
	}
	remotePage, err := newRodRemoteLoginPage(page)
	if err != nil {
		lm.mu.Lock()
		current, ok := lm.jobs[providerName]
		if ok && current == job {
			job.pool = nil
			job.Status = LoginStatusError
			job.Message = "Could not prepare remote browser view."
		}
		lm.mu.Unlock()
		loginPool.Close()
		log.Warn().Str("provider", providerName).Err(err).Msg("login browser viewport setup failed")
		lm.notify(providerName)
		return
	}

	lm.mu.Lock()
	current, ok = lm.jobs[providerName]
	if !ok || current != job {
		lm.mu.Unlock()
		loginPool.Close()
		return
	}
	job.page = remotePage
	job.Status = LoginStatusWaiting
	job.Message = fmt.Sprintf(
		"%s is ready in the secure browser below. Complete sign-in, then click 'Save Session'.",
		providerDisplayName(providerName),
	)
	lm.mu.Unlock()

	log.Info().Str("provider", providerName).Str("url", providerURL).Msg("login browser ready")
	lm.notify(providerName)
}

// SaveSession captures cookies + localStorage from the tracked page and writes
// the session file.  The exact page opened during StartLogin is used — no
// heuristic page scanning.
//
// Locking discipline: browser state is captured into memory while the job I/O
// lock is held. Cancel and replacement reject the saving state. Ownership is
// revalidated before the captured state is committed to disk.
func (lm *LoginManager) SaveSession(providerName, jobID string) error {
	lm.mu.Lock()
	job, ok := lm.jobs[providerName]
	if !ok {
		lm.mu.Unlock()
		return fmt.Errorf("no active login session for %q", providerName)
	}
	if jobID == "" || job.ID != jobID {
		lm.mu.Unlock()
		return errStaleLoginJob
	}
	if job.Status != LoginStatusWaiting || job.page == nil {
		lm.mu.Unlock()
		return fmt.Errorf("login session for %q is not ready to save (status: %s)", providerName, job.Status)
	}
	// Snapshot the page reference while we hold the lock, then transition to
	// "saving" before releasing so the UI updates immediately.
	job.ioMu.Lock()
	page := job.page
	job.Status = LoginStatusSaving
	job.Message = "Capturing session…"
	lm.mu.Unlock()
	lm.notify(providerName)

	// Browser I/O runs without the manager lock, but produces only in-memory
	// data. Nothing is persisted until ownership is revalidated below.
	data, err := page.CaptureSessionData()
	job.ioMu.Unlock()
	if err != nil {
		lm.mu.Lock()
		if currentJob, exists := lm.jobs[providerName]; exists && currentJob == job {
			job.Status = LoginStatusWaiting
			job.Message = "Failed to save session: " + err.Error()
		}
		lm.mu.Unlock()
		lm.notify(providerName)
		return err
	}

	lm.mu.Lock()
	// Re-validate before committing the in-memory capture.
	currentJob, exists := lm.jobs[providerName]
	if !exists || currentJob != job {
		lm.mu.Unlock()
		return fmt.Errorf("login for %q was cancelled during save", providerName)
	}
	if err := lm.saveSession(providerName, data); err != nil {
		job.Status = LoginStatusWaiting
		job.Message = "Failed to save session: " + err.Error()
		lm.mu.Unlock()
		lm.notify(providerName)
		return err
	}

	// Close the browser pool asynchronously so the HTTP response returns quickly.
	p := job.pool
	if p != nil {
		go p.Close()
	}

	job.pool = nil
	job.page = nil
	job.Status = LoginStatusSaved
	job.Message = "Session saved! The proxy will pick up the new session within 15 seconds."
	lm.mu.Unlock()

	log.Info().Str("provider", providerName).Str("file", browser.SessionPath(providerName)).
		Msg("login session saved via dashboard")
	lm.notify(providerName)
	return nil
}

// Status returns a snapshot of the current job state for providerName, or nil.
func (lm *LoginManager) Status(providerName string) *LoginJob {
	lm.mu.Lock()
	defer lm.mu.Unlock()
	job, ok := lm.jobs[providerName]
	if !ok {
		return nil
	}
	return lm.snapshot(job)
}

// StatusFor returns state only when the caller presents the current job
// capability. It never discloses a replacement job identifier to an old client.
func (lm *LoginManager) StatusFor(providerName, jobID string) (*LoginJob, error) {
	lm.mu.Lock()
	defer lm.mu.Unlock()
	job, ok := lm.jobs[providerName]
	if !ok {
		return nil, errLoginNotFound
	}
	if jobID == "" || job.ID != jobID {
		return nil, errStaleLoginJob
	}
	return lm.snapshot(job), nil
}

// CancelLogin closes the browser, discards the job for providerName, and
// delivers a terminal error event to any open SSE subscribers so their
// streams close cleanly.
func (lm *LoginManager) CancelLogin(providerName, jobID string) error {
	lm.mu.Lock()
	job, ok := lm.jobs[providerName]
	if !ok {
		if _, err := uuid.Parse(jobID); err != nil {
			lm.mu.Unlock()
			return errStaleLoginJob
		}
		lm.recordCancellationLocked(jobID)
		lm.mu.Unlock()
		return nil
	}
	if jobID == "" || job.ID != jobID {
		lm.mu.Unlock()
		return errStaleLoginJob
	}
	if job.Status == LoginStatusSaving {
		lm.mu.Unlock()
		return errLoginNotReady
	}
	// Take a snapshot of subscribers before cancelLocked deletes the job entry.
	subs := make([]*loginSubscriber, len(lm.subscribers[providerName]))
	copy(subs, lm.subscribers[providerName])
	lm.cancelLocked(job)
	lm.mu.Unlock()

	// Push a terminal event so open SSE streams exit cleanly.
	cancelled := &LoginJob{
		Provider: providerName,
		Status:   LoginStatusError,
		Message:  "Login cancelled.",
	}
	for _, sub := range subs {
		select {
		case sub.ch <- *cancelled:
		case <-sub.done:
		default:
		}
	}
	return nil
}

// Close cancels every active login job and closes its browser pool.
func (lm *LoginManager) Close() {
	lm.closeOnce.Do(func() {
		close(lm.stop)
		<-lm.sweepDone
		lm.mu.Lock()
		for _, job := range lm.jobs {
			lm.cancelLocked(job)
		}
		lm.mu.Unlock()
	})
}

// snapshot returns a copy of the job safe for JSON serialisation (no unexported fields).
func (lm *LoginManager) snapshot(job *LoginJob) *LoginJob {
	return &LoginJob{
		ID:        job.ID,
		Provider:  job.Provider,
		Status:    job.Status,
		Message:   job.Message,
		StartedAt: job.StartedAt,
	}
}

func (lm *LoginManager) cancelLocked(job *LoginJob) {
	job.ioMu.Lock()
	p := job.pool
	job.pool = nil
	job.page = nil
	delete(lm.jobs, job.Provider)
	job.ioMu.Unlock()

	if p != nil {
		go p.Close()
	}
}

// sweepAbandonedJobs closes remote login browsers after their dashboard client
// stops requesting frames. Starting jobs get a longer grace period because
// Chromium startup can be slow on small hosts.
func (lm *LoginManager) sweepAbandonedJobs() {
	defer close(lm.sweepDone)
	ticker := time.NewTicker(10 * time.Second)
	defer ticker.Stop()

	for {
		select {
		case <-lm.stop:
			return
		case now := <-ticker.C:
			type abandoned struct {
				provider string
				id       string
			}
			var stale []abandoned
			lm.mu.Lock()
			for id, expires := range lm.cancelled {
				if now.After(expires) {
					delete(lm.cancelled, id)
				}
			}
			for provider, job := range lm.jobs {
				timeout := 2 * time.Minute
				if job.Status == LoginStatusWaiting {
					timeout = 35 * time.Second
				}
				if now.Sub(job.lastClientAt) > timeout {
					stale = append(stale, abandoned{provider: provider, id: job.ID})
				}
			}
			lm.mu.Unlock()

			for _, item := range stale {
				lm.cancelIfCurrent(item.provider, item.id, "Login browser closed after the dashboard disconnected.")
			}
		}
	}
}

func (lm *LoginManager) recordCancellationLocked(jobID string) {
	// Bound unauthenticated deployments as well as authenticated ones against
	// unbounded tombstone growth.
	if len(lm.cancelled) >= 1024 {
		var oldestID string
		var oldest time.Time
		for id, expires := range lm.cancelled {
			if oldestID == "" || expires.Before(oldest) {
				oldestID, oldest = id, expires
			}
		}
		delete(lm.cancelled, oldestID)
	}
	lm.cancelled[jobID] = time.Now().Add(2 * time.Minute)
}

func (lm *LoginManager) cancelIfCurrent(providerName, jobID, message string) {
	lm.mu.Lock()
	job, ok := lm.jobs[providerName]
	if !ok || job.ID != jobID {
		lm.mu.Unlock()
		return
	}
	subs := append([]*loginSubscriber(nil), lm.subscribers[providerName]...)
	lm.cancelLocked(job)
	lm.mu.Unlock()

	terminal := LoginJob{
		ID:       jobID,
		Provider: providerName,
		Status:   LoginStatusError,
		Message:  message,
	}
	for _, sub := range subs {
		select {
		case sub.ch <- terminal:
		case <-sub.done:
		default:
		}
	}
}
