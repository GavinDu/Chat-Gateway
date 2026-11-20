use obscura_browser::BrowserContext;

#[test]
fn exported_cookie_snapshot_preserves_domain_scope_without_widening_host_only() {
    let source = BrowserContext::new("source".into());
    let home = "https://www.meta.ai/".parse().unwrap();
    source.cookie_jar.set_cookie(
        "domain-session=synthetic-domain; Domain=meta.ai; Path=/; Secure; HttpOnly",
        &home,
    );
    source.cookie_jar.set_cookie(
        "__Host-host-session=synthetic-host; Path=/; Secure; HttpOnly",
        &home,
    );
    let mut snapshot = source.cookie_jar.get_all_cookies();
    for generation in 0..2 {
        let restored = BrowserContext::new(format!("restored-{generation}"));
        for cookie in &snapshot {
            // Match the gateway's six-field export/replay boundary. A leading
            // dot carries Domain scope; otherwise the cookie remains host-only.
            let mut header = format!(
                "{}={}; Path={}; Secure; HttpOnly",
                cookie.name, cookie.value, cookie.path
            );
            if cookie.domain.starts_with('.') {
                header.push_str(&format!("; Domain={}", cookie.domain));
            }
            let origin = format!("https://{}/", cookie.domain.trim_start_matches('.'))
                .parse()
                .unwrap();
            restored.cookie_jar.set_cookie(&header, &origin);
        }
        let exact = restored.cookie_jar.get_cookie_header_same_site(&home);
        assert!(exact.contains("domain-session=synthetic-domain"));
        assert!(exact.contains("__Host-host-session=synthetic-host"));
        let subdomain = "https://gateway.meta.ai/".parse().unwrap();
        let child = restored.cookie_jar.get_cookie_header_same_site(&subdomain);
        assert!(child.contains("domain-session=synthetic-domain"));
        assert!(!child.contains("__Host-host-session"));
        snapshot = restored.cookie_jar.get_all_cookies();
    }
}
