// Keep the session and its credentials inside the first-party page. Only a
// fixed result category and HTTP status cross the browser boundary.
(() => {
  const attempt = __ATTEMPT__;
  if (location.origin !== __ORIGIN__) return {state:'off_origin'};
  // The full URL may carry a private thread/account token. Keep it in this
  // first-party realm and use it only to fence an asynchronous page change.
  const initialHref = location.href;
  const samePage = () => location.origin === __ORIGIN__ &&
                         location.href === initialHref;
  const key = '__chatGatewayChatgptSessionReadiness';
  const previous = globalThis[key];
  if (previous) {
    clearTimeout(previous.timer);
    try { previous.controller.abort(); } catch (_) {}
  }
  const probe = {attempt, state:'pending', http:null, controller:new AbortController(), timer:null};
  globalThis[key] = probe;
  const finish = state => {
    if (globalThis[key] !== probe || probe.state !== 'pending') return;
    // A completed request for an older same-origin SPA route or navigated
    // document cannot prove the currently displayed account. Also prevent an
    // earlier 401 from demanding a new login in the later page.
    probe.state = samePage() ? state : 'unavailable';
    clearTimeout(probe.timer);
  };
  probe.timer = setTimeout(() => {
    finish('timeout');
    try { probe.controller.abort(); } catch (_) {}
  }, __TIMEOUT_MS__);
  try {
    // Pinned Obscura op_fetch_url follows redirects regardless of Request's
    // "error" setting, while its bootstrap masks Response.redirected unless
    // mode is "follow". Explicit follow exposes final URL + redirect metadata
    // so this verifier can reject every redirected/foreign endpoint response
    // instead of mistaking a redirected 200 for first-party session proof.
    fetch('/api/auth/session', {credentials:'include', cache:'no-store',
      redirect:'follow', signal:probe.controller.signal})
      .then(async response => {
        probe.http = response.status;
        if (response.redirected ||
            (__ORIGIN__ !== 'null' &&
              response.url !== __ORIGIN__ + '/api/auth/session')) {
          finish('unavailable'); return;
        }
        if (response.status === 401) { finish('needs_login'); return; }
        if (!response.ok) { finish('unavailable'); return; }
        const session = await response.json();
        if (!session || typeof session !== 'object' || Array.isArray(session)) {
          finish('unavailable'); return;
        }
        const user = !!session.user && typeof session.user === 'object' && !Array.isArray(session.user);
        const token = typeof session.accessToken === 'string' && session.accessToken.length > 0;
        const future = typeof session.expires === 'string' && Date.parse(session.expires) > Date.now();
        if (user && token && future) finish('verified');
        else if (Object.keys(session).length === 0) finish('needs_login');
        else finish('unavailable');
      }).catch(() => finish('unavailable'));
  } catch (_) { finish('unavailable'); }
  return {state:probe.state};
})();
