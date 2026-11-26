// Keep the session and its credentials inside the first-party page. Only fixed
// result/reason categories and HTTP status cross the browser boundary.
(() => {
  const attempt = __ATTEMPT__;
  if (location.origin !== __ORIGIN__) return {state:'off_origin', reason:'off_origin'};
  // A full URL may contain private account/thread data. Keep it in this realm.
  const initialHref = location.href;
  const samePage = () => location.origin === __ORIGIN__ && location.href === initialHref;
  const key = '__chatGatewayChatgptSessionReadiness';
  const previous = globalThis[key];
  if (previous) {
    clearTimeout(previous.timer);
    try { previous.controller.abort(); } catch (_) {}
  }
  const probe = {attempt, state:'pending', reason:'pending', http:null,
    controller:new AbortController(), timer:null};
  globalThis[key] = probe;
  const finish = (state, reason) => {
    if (globalThis[key] !== probe || probe.state !== 'pending') return;
    // Earlier account evidence, including a 401, cannot certify a later page.
    const current = samePage();
    probe.state = current ? state : 'unavailable';
    probe.reason = current ? reason : 'page_changed';
    clearTimeout(probe.timer);
  };
  probe.timer = setTimeout(() => {
    finish('timeout', 'deadline');
    try { probe.controller.abort(); } catch (_) {}
  }, __TIMEOUT_MS__);
  try {
    // Pinned Obscura requires "follow" to expose redirect metadata. Reject
    // redirected or foreign responses rather than treating a 200 as proof.
    fetch('/api/auth/session', {credentials:'include', cache:'no-store',
      redirect:'follow', signal:probe.controller.signal})
      .then(async response => {
        probe.http = response.status;
        if (response.redirected) { finish('unavailable', 'response_redirected'); return; }
        if (__ORIGIN__ !== 'null' && response.url !== __ORIGIN__ + '/api/auth/session') {
          finish('unavailable', response.url ? 'response_endpoint_mismatch' : 'response_url_missing'); return;
        }
        if (response.status === 401) { finish('needs_login', 'explicit_unauthenticated'); return; }
        if (!response.ok) { finish('unavailable', 'http_unavailable'); return; }
        let session;
        try { session = await response.json(); }
        catch (_) { finish('unavailable', 'invalid_json_or_body'); return; }
        if (!session || typeof session !== 'object' || Array.isArray(session)) {
          finish('unavailable', 'invalid_session_schema'); return;
        }
        // Refresh errors and expired claims reject evidence; JWT claims never
        // establish account proof. Preserve support for nonempty opaque tokens.
        if (session.error) { finish('unavailable', 'session_reported_error'); return; }
        if (Object.keys(session).length === 0) { finish('needs_login', 'explicit_unauthenticated'); return; }
        const token = typeof session.accessToken === 'string' && session.accessToken.trim().length > 0;
        const tokenFailure = (() => {
          if (!token) return 'access_token_missing';
          const parts = session.accessToken.split('.');
          if (parts.length !== 3) return null;
          try {
            const encoded = parts[1].replace(/-/g, '+').replace(/_/g, '/');
            if (!encoded || encoded.length > 32768) return 'access_token_invalid';
            const payload = JSON.parse(atob(encoded + '='.repeat((4 - encoded.length % 4) % 4)));
            if (!payload || typeof payload !== 'object' || Array.isArray(payload)) return 'access_token_invalid';
            if (payload.exp === undefined) return null;
            if (typeof payload.exp !== 'number' || !Number.isFinite(payload.exp)) return 'access_token_invalid';
            return payload.exp * 1000 > Date.now() ? null : 'access_token_expired';
          } catch (_) { return 'access_token_invalid'; }
        })();
        const user = !!session.user && typeof session.user === 'object' &&
                     !Array.isArray(session.user) && Object.keys(session.user).length > 0;
        const expiry = typeof session.expires === 'string' ? Date.parse(session.expires) : NaN;
        if (!user) finish('unavailable', 'user_missing');
        else if (tokenFailure) finish('unavailable', tokenFailure);
        else if (!Number.isFinite(expiry)) finish('unavailable', 'session_expiry_invalid');
        else if (expiry <= Date.now()) finish('unavailable', 'session_expiry_expired');
        else finish('verified', 'verified');
      }).catch(() => finish('unavailable', 'request_failed'));
  } catch (_) { finish('unavailable', 'request_failed'); }
  return {state:probe.state, reason:probe.reason};
})();
