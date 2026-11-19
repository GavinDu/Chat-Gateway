// Executed ONLY by the explicit popup action through activeTab permission.
// Chrome serializes this function into the active first-party page context.
// Do not return any user, access token, cookie, body, URL or upstream error.
async function probeActiveChatgptAccount() {
  const started = Date.now();
  const checked_at = new Date(started).toISOString();
  const report = (authentication, reason, http_status = null) => ({
    provider: "chatgpt", context: "chrome_active_tab",
    authentication, reason, http_status, checked_at,
    exported_snapshot_verified: false
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
    const validUser = session.user && typeof session.user === "object" &&
      !Array.isArray(session.user);
    const tokenPresent = typeof session.accessToken === "string" &&
      session.accessToken.trim().length > 0;
    const expiry = typeof session.expires === "string" ? Date.parse(session.expires) : NaN;
    if (validUser && tokenPresent && Number.isFinite(expiry) && expiry > Date.now()) {
      return report("verified", "first_party_session_confirmed", 200);
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
