// Used by the explicit popup action and the consent/permission-scoped native diagnostic.
// Chrome serializes this function into the active first-party page context.
// Do not return any user, access token, cookie, body, URL or upstream error.
async function probeActiveChatgptAccount(context = "chrome_active_tab") {
  const nativeDiagnostic = context === "chrome_native_diagnostic";
  const started = Date.now();
  const checked_at = new Date(started).toISOString();
  const report = (authentication, reason, http_status = null, auth_mode = "unknown") => ({
    provider: "chatgpt", context: nativeDiagnostic ? "chrome_native_diagnostic" : "chrome_active_tab",
    authentication, reason, http_status, checked_at,
    exported_snapshot_verified: false,
    ...(nativeDiagnostic ? {auth_mode} : {})
  });
  if (location.protocol !== "https:" ||
      !["chatgpt.com", "www.chatgpt.com"].includes(location.hostname) ||
      (location.port && location.port !== "443")) {
    return report("unknown", "unsupported_origin");
  }
  // An existing first-party tab can navigate while the asynchronous fetch
  // or JSON body parsing is pending. Never attach a result observed in an
  // older document/account route to the currently active tab. The URL stays
  // strictly inside this execution context; even a fragment may be private.
  const initialHref = location.href;
  const documentUnchanged = () =>
    location.protocol === "https:" &&
    ["chatgpt.com", "www.chatgpt.com"].includes(location.hostname) &&
    (!location.port || location.port === "443") &&
    location.href === initialHref;
  const controller = new AbortController();
  const timer = setTimeout(() => controller.abort(), 10_000);
  try {
    const response = await fetch("/api/auth/session", {
      credentials: "include", cache: "no-store", redirect: "error",
      signal: controller.signal, headers: {Accept: "application/json"}
    });
    // Same-tab SPA route/port changes are also stale, not merely off-origin
    // redirects. A later signed-in page cannot prove the prior page's state.
    if (!documentUnchanged()) return report("unknown", "tab_changed");
    if (response.status === 401) {
      return report("unauthenticated", "provider_rejected_session", 401);
    }
    if (response.status !== 200) {
      return report("unknown", "provider_or_network_blocked",
        Number.isInteger(response.status) ? response.status : null);
    }
    const session = await response.json();
    // Body parsing is a separate await. Do not certify a previous account
    // after a same-origin SPA route or a cross-origin document change.
    if (!documentUnchanged()) return report("unknown", "tab_changed");
    if (!session || typeof session !== "object" || Array.isArray(session)) {
      return report("unknown", "unrecognized_session_response", 200);
    }
    // A refresh error or expired access token cannot certify this account,
    // even when the outer session expiry remains in the future. Claims stay
    // private and can only reject stale evidence, never prove authentication.

    const tokenCurrent = (() => {
      if (typeof session.accessToken !== 'string') return false;
      const parts = session.accessToken.split('.');
      if (parts.length !== 3) return true; // Preserve opaque-token support.
      try {
        const encoded = parts[1].replace(/-/g, '+').replace(/_/g, '/');
        if (!encoded || encoded.length > 32768) return false;
        const payload = JSON.parse(atob(encoded + '='.repeat((4 - encoded.length % 4) % 4)));
        return payload && typeof payload === 'object' && !Array.isArray(payload) &&
          (payload.exp === undefined || (typeof payload.exp === 'number' &&
            Number.isFinite(payload.exp) && payload.exp * 1000 > Date.now()));
      } catch (_) { return false; }
    })();
    // An empty user object is a partial/schema-drift response, not positive
    // first-party account evidence. Never expose the fields or their values.
    const validUser = session.user && typeof session.user === "object" &&
      !Array.isArray(session.user) && Object.keys(session.user).length > 0;
    const tokenPresent = typeof session.accessToken === "string" &&
      session.accessToken.trim().length > 0;
    const expiry = typeof session.expires === "string" ? Date.parse(session.expires) : NaN;
    if (session.error) {
      // A native diagnostic may test the observed token against the provider's
      // real account endpoint. Token claims alone never prove authentication.
      // All account fields and the bearer remain inside this first-party page.
      if (nativeDiagnostic && validUser && tokenPresent && tokenCurrent &&
          Number.isFinite(expiry) && expiry > Date.now() &&
          typeof session.user.id === "string" && session.user.id.trim()) {
        const account = await fetch("/backend-api/me", {
          credentials:"include", cache:"no-store", redirect:"error", signal:controller.signal,
          headers:{Accept:"application/json", Authorization:"Bearer " + session.accessToken}
        });
        if (!documentUnchanged()) return report("unknown", "tab_changed");
        if (account.status === 401) return report("unknown", "access_token_rejected", 401);
        if (account.status !== 200) return report("unknown", "provider_or_network_blocked", account.status);
        const user = await account.json();
        if (!documentUnchanged()) return report("unknown", "tab_changed");
        if (!user || typeof user !== "object" || Array.isArray(user) ||
            typeof user.id !== "string" || !user.id.trim()) {
          return report("unknown", "unrecognized_account_response", 200);
        }
        if (user.id !== session.user.id) return report("unknown", "account_principal_mismatch", 200);
        return report("verified", "bearer_account_confirmed", 200, "bearer");
      }
      return report("unknown", nativeDiagnostic ? "session_reported_error" : "incomplete_or_expired_session_evidence", 200);
    }
    if (validUser && tokenPresent && tokenCurrent && Number.isFinite(expiry) && expiry > Date.now()) {
      return report("verified", "first_party_session_confirmed", 200, "session");
    }
    // A partial/changed 200 response can contain user:null alongside
    // upstream error fields. Only the reviewed empty or exact-null shapes
    // establish absence of a session; never turn arbitrary JSON into logout.
    const sessionKeys = Object.keys(session);
    if (sessionKeys.length === 0 ||
        (sessionKeys.length === 1 && sessionKeys[0] === "user" && session.user === null)) {
      return report("unauthenticated", "provider_reports_no_session", 200);
    }
    return report("unknown", "incomplete_or_expired_session_evidence", 200);
  } catch (_) {
    return report("unknown",
      controller.signal.aborted ? "verification_timeout" : "verification_unavailable");
  } finally {
    clearTimeout(timer);
  }
}
