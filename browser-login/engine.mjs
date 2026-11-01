// Browser context boundary: no HTTP listener, evaluation API, debug port,
// credential forms, logs, or durable profile. All controls are allowlisted.
import { chromium } from 'playwright';
import { chmod, rm } from 'node:fs/promises';
import { join } from 'node:path';
import { allowed, allowedDocument, dimensions, domainMatches,
  providerDomains, vaultDomains, safeAddress } from './policy.mjs';
import { navigationGuard } from './navigation-guard.mjs';

export class LoginEngine {
  constructor(emit) {
    this.emit = emit;
    this.viewport = { width: 1000, height: 700 };
    this.entries = new Map();
    this.registration = new WeakMap();
    this.nextId = 0;
    this.booting = true;
  }
  fail(code) { this.emit({ type: 'error', code }); }
  async close() {
    if (this.closing) return this.closing;
    this.closed = true;
    clearInterval(this.timer);
    this.closing = (async () => {
      try { await this.context?.close(); } catch {}
      await this.guard?.close();
      if (this.directory) await rm(this.directory, { recursive: true, force: true }).catch(() => {});
    })();
    return this.closing;
  }
  async init(m) {
    if (this.provider || !providerDomains[m.provider] || !allowed(m.provider, m.url))
      throw new Error('invalid_start');
    this.provider = m.provider;
    this.directory = process.env.CHAT_GATEWAY_LOGIN_TMPDIR;
    if (!/^\/dev\/shm\/chat-gateway-login-[a-f0-9-]+$/.test(this.directory || ''))
      throw new Error('invalid_start');
    await chmod(this.directory, 0o700);
    this.context = await chromium.launchPersistentContext(this.directory, {
      executablePath: process.env.CHAT_GATEWAY_CHROMIUM_PATH || undefined,
      headless: true, chromiumSandbox: true,
      env: { ...process.env, HOME: this.directory,
        XDG_CACHE_HOME: join(this.directory, 'cache'), XDG_CONFIG_HOME: join(this.directory, 'config') },
      viewport: this.viewport, acceptDownloads: false, serviceWorkers: 'block',
      permissions: [], ignoreHTTPSErrors: false
    });
    this.context.browser().on('disconnected', () => {
      if (!this.closed) { this.fail('browser_runtime_unavailable'); void this.close(); }
    });
    // Closing the final tab of a persistent Chromium context can terminate
    // the browser. Adopt its fresh blank page instead of closing every tab.
    const initialPage = this.context.pages()[0];
    for (const extra of this.context.pages().slice(1)) await extra.close();
    this.guard = await navigationGuard(this.context.browser(), this.provider,
      () => this.fail('navigation_blocked'),
      phase => { this.emit({type:'error',code:'browser_runtime_unavailable',phase}); void this.close(); });
    await this.context.route('**/*', async route => {
      const request = route.request();
      if (request.isNavigationRequest()) {
        let main = true;
        try { main = !request.frame().parentFrame(); } catch {}
        if (!allowedDocument(this.provider, request.url(), main)) {
          this.fail('navigation_blocked');
          return route.abort('blockedbyclient');
        }
        // The browser-level guard is installed before targets are resumed,
        // even when Playwright does not yet expose the popup's Frame.
      }
      return route.fallback();
    });
    this.context.on('page', p => {
      void this.register(p).then(e => {
        if (e && !this.booting) return this.activate(e.id);
      }).catch(() => this.fail('browser_runtime_unavailable'));
    });
    const cookies = (m.restore?.cookies || []).filter(c =>
      domainMatches(c.domain.replace(/^\./, ''), vaultDomains[this.provider]) &&
      (c.expires == null || c.expires === -1 || c.expires > Date.now() / 1000));
    await this.context.addCookies(cookies.map(c => ({
      name: c.name, value: c.value, domain: c.domain, path: c.path || '/',
      secure: c.secure, httpOnly: c.http_only,
      ...(c.same_site ? { sameSite: c.same_site } : {}),
      ...(c.expires != null ? { expires: c.expires } : {})
    })));
    const page = initialPage || await this.context.newPage();
    const first = await this.register(page);
    await this.activate(first.id);
    this.booting = false;
    this.emit({ type: 'ready' });
    await page.goto(m.url, { waitUntil: 'domcontentloaded', timeout: 45000 });
    await this.status();
    this.timer = setInterval(() => this.status().catch(() => {}), 1500);
    this.timer.unref();
  }
  async register(page) {
    if (this.registration.has(page)) return this.registration.get(page);
    if (this.closed || this.entries.size >= 4) {
      this.fail('page_limit');
      await page.close().catch(() => {});
      throw new Error('page_limit');
    }
    const promise = (async () => {
      const e = { id: String(++this.nextId), page };
      this.entries.set(e.id, e);
      page.on('download', d => d.cancel().catch(() => {}));
      e.cdp = await this.context.newCDPSession(page);
      await e.cdp.send('Page.enable');
      e.cdp.on('Page.screencastFrame', event => {
        if (!this.closed && this.activeId === e.id) {
          this.emit({ type: 'frame', data: event.data, pageId: e.id,
            width: this.viewport.width, height: this.viewport.height });
        }
        void e.cdp.send('Page.screencastFrameAck', { sessionId: event.sessionId }).catch(() => {});
      });
      page.on('close', () => {
        this.entries.delete(e.id);
        if (!this.closed && this.activeId === e.id) {
          const other = [...this.entries.values()].at(-1);
          if (other) void this.activate(other.id).catch(() => this.close());
          else { this.fail('browser_closed'); void this.close(); }
        }
        this.pages();
      });
      return e;
    })();
    this.registration.set(page, promise);
    return promise;
  }
  pages() {
    if (!this.closed) this.emit({ type: 'pages', activePage: this.activeId,
      pages: [...this.entries.values()].map(e => ({
        id: e.id, url: safeAddress(e.page.url()), active: e.id === this.activeId
      })) });
  }
  async activate(id) {
    const e = this.entries.get(id);
    if (!e?.cdp || e.page.isClosed()) throw new Error('invalid_input');
    const previous = this.entries.get(this.activeId);
    this.activeId = id;
    this.pages();
    if (previous && previous !== e)
      await previous.cdp.send('Page.stopScreencast').catch(() => {});
    await e.page.setViewportSize(this.viewport);
    await e.cdp.send('Page.startScreencast', {
      format: 'jpeg', quality: 75, maxWidth: 1920, maxHeight: 1080, everyNthFrame: 1
    });
    this.pages();
    await this.status();
  }
  async status() {
    const e = this.entries.get(this.activeId);
    if (!e?.cdp || this.closed) return;
    const history = await e.cdp.send('Page.getNavigationHistory');
    this.emit({ type: 'status', url: safeAddress(e.page.url()),
      canBack: history.currentIndex > 0,
      canForward: history.currentIndex < history.entries.length - 1 });
    this.pages();
  }
  point(m) {
    if (![m.x, m.y].every(Number.isFinite) || m.x < 0 || m.y < 0 ||
        m.x > this.viewport.width || m.y > this.viewport.height) throw new Error('invalid_input');
    return { x: m.x, y: m.y };
  }
  async dispatch(m) {
    if (m.type === 'init') return this.init(m);
    if (m.type === 'page') return this.activate(m.id);
    const e = this.entries.get(this.activeId);
    if (!e || this.closed) throw new Error('browser_not_ready');
    const { page, cdp } = e;
    switch (m.type) {
      case 'resize':
        this.viewport = dimensions(m.width, m.height);
        await page.setViewportSize(this.viewport); return;
      case 'navigate':
        if (m.url) {
          if (!allowed(this.provider, m.url)) throw new Error('navigation_blocked');
          await page.goto(m.url, { waitUntil: 'domcontentloaded', timeout: 45000 });
        } else if (m.direction === 'back') await page.goBack();
        else if (m.direction === 'forward') await page.goForward();
        else if (m.direction === 'reload') await page.reload();
        else throw new Error('invalid_input');
        return this.status();
      case 'mouse':
        if (!['mouseMoved', 'mousePressed', 'mouseReleased'].includes(m.event) ||
            !['none', 'left', 'right', 'middle'].includes(m.button)) throw new Error('invalid_input');
        return cdp.send('Input.dispatchMouseEvent', { ...this.point(m),
          type: m.event, button: m.button, buttons: Number(m.buttons) & 7,
          clickCount: m.event === 'mouseMoved' ? 0 : 1 });
      case 'wheel':
        if (![m.deltaX, m.deltaY].every(n => Number.isFinite(n) && Math.abs(n) < 20000))
          throw new Error('invalid_input');
        return cdp.send('Input.dispatchMouseEvent', { ...this.point(m),
          type: 'mouseWheel', deltaX: m.deltaX, deltaY: m.deltaY });
      case 'text':
        if (typeof m.text !== 'string' || m.text.length > 4096) throw new Error('invalid_input');
        return page.keyboard.insertText(m.text);
      case 'key':
        if (!['keyDown', 'keyUp'].includes(m.event) || typeof m.key !== 'string' ||
            m.key.length > 40 || typeof m.code !== 'string' || m.code.length > 40)
          throw new Error('invalid_input');
        return cdp.send('Input.dispatchKeyEvent', { type: m.event, key: m.key, code: m.code,
          modifiers: Number(m.modifiers) & 15,
          windowsVirtualKeyCode: ({ Backspace: 8, Tab: 9, Enter: 13, Shift: 16, Control: 17,
            Alt: 18, Escape: 27, ' ': 32, PageUp: 33, PageDown: 34, End: 35, Home: 36,
            ArrowLeft: 37, ArrowUp: 38, ArrowRight: 39, ArrowDown: 40, Delete: 46 })[m.key] ||
            (/^Key[A-Z]$/.test(m.code) ? m.code.charCodeAt(3) : 0) });
      case 'save': {
        if (!allowed(this.provider, page.url())) throw new Error('navigation_blocked');
        const cookies = (await this.context.cookies()).filter(c =>
          domainMatches(c.domain.replace(/^\./, ''), vaultDomains[this.provider]));
        if (!cookies.length) throw new Error('no_provider_session');
        this.emit({ type: 'snapshot', session: {
          ts: new Date().toISOString(), local_storage: {},
          cookies: cookies.map(c => ({ name: c.name, value: c.value, domain: c.domain,
            path: c.path, secure: c.secure, http_only: c.httpOnly,
            expires: c.expires, same_site: c.sameSite }))
        } }); return;
      }
      default: throw new Error('invalid_input');
    }
  }
}
