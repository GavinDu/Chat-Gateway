// Paste this complete expression into DevTools on the provider's own page.
// Returns only fixed diagnostic categories. Never prints cookies or account data.
(async () => {
  const started = Date.now();
  const report = (provider, authentication, reason, http = null) => ({
    schema_version: 1, provider, authentication, reason, http_status: http,
    checked_at: new Date().toISOString(),
    duration_ms: Math.max(0, Date.now() - started),
    exported_snapshot_verified: false
  });
  const trusted = location.protocol === 'https:' &&
    (!location.port || location.port === '443');
  if (!trusted) return report(null, 'unknown', 'unsupported_origin');
  if (['meta.ai', 'www.meta.ai'].includes(location.hostname)) {
    return report('metaai', 'unknown', 'account_verifier_not_established');
  }
  if (!['chatgpt.com', 'www.chatgpt.com'].includes(location.hostname)) {
    return report(null, 'unknown', 'unsupported_origin');
  }
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), 10000);
  try {
    const response = await fetch('/api/auth/session', {
      credentials: 'include', cache: 'no-store', redirect: 'error',
      headers: {Accept: 'application/json'}, signal: controller.signal
    });
    if (response.status === 401) {
      return report('chatgpt', 'unauthenticated', 'provider_rejected_session', 401);
    }
    if (response.status !== 200) {
      return report('chatgpt', 'unknown', 'provider_or_network_blocked', response.status);
    }
    const body = await response.json();
    if (!body || typeof body !== 'object' || Array.isArray(body)) {
      return report('chatgpt', 'unknown', 'unrecognized_session_response', 200);
    }
    const user = body.user;
    const tokenPresent = typeof body.accessToken === 'string' && body.accessToken.trim().length > 0;
    const expiry = typeof body.expires === 'string' ? Date.parse(body.expires) : NaN;
    if (user && typeof user === 'object' && !Array.isArray(user) &&
        tokenPresent && Number.isFinite(expiry) && expiry > Date.now()) {
      return report('chatgpt', 'verified', 'first_party_session_confirmed', 200);
    }
    if (Object.keys(body).length === 0 || (user === null && !tokenPresent)) {
      return report('chatgpt', 'unauthenticated', 'provider_reports_no_session', 200);
    }
    return report('chatgpt', 'unknown', 'incomplete_or_expired_session_evidence', 200);
  } catch (_) {
    return report('chatgpt', 'unknown',
      controller.signal.aborted ? 'verification_timeout' : 'verification_unavailable');
  } finally {
    clearTimeout(timer);
  }
})()
