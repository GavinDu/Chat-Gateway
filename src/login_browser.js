'use strict';

(() => {
  const providers = [
    { id: 'chatgpt', name: 'ChatGPT' },
    { id: 'claude', name: 'Claude' },
    { id: 'gemini', name: 'Gemini' },
    { id: 'qwen', name: 'Qwen' },
    { id: 'metaai', name: 'Meta AI' },
    { id: 'grok', name: 'Grok' },
    { id: 'mistral', name: 'Mistral' },
    { id: 'perplexity', name: 'Perplexity' }
  ];
  const elements = {
    authForm: document.getElementById('authorization-form'),
    credential: document.getElementById('gateway-authorization'),
    refresh: document.getElementById('refresh-status'),
    notice: document.getElementById('notice'),
    providerList: document.getElementById('provider-list'),
    start: document.getElementById('start-session'),
    cancel: document.getElementById('cancel-session'),
    save: document.getElementById('save-session'),
    forget: document.getElementById('forget-session'),
    back: document.getElementById('go-back'),
    forward: document.getElementById('go-forward'),
    reload: document.getElementById('reload-page'),
    addressForm: document.getElementById('address-form'),
    address: document.getElementById('address'),
    addressGo: document.getElementById('address-go'),
    canvas: document.getElementById('browser-viewport'),
    viewportWrap: document.getElementById('viewport-wrap'),
    empty: document.getElementById('viewport-empty'),
    connection: document.getElementById('connection-state'),
    safeUrl: document.getElementById('safe-url'),
    frameSize: document.getElementById('frame-size'),
    sessionNote: document.getElementById('session-note'),
    pageTabs: document.getElementById('page-tabs')
  };
  const context = elements.canvas.getContext('2d');
  const providerStatus = new Map();
  let authorization = '';
  let selectedProvider = null;
  let socket = null;
  let socketProvider = null;
  let running = false;
  let pendingSave = false;
  let saveConfirmed = false;
  let startRequest = 0;
  let canBack = false;
  let canForward = false;
  let resizeObserver = null;
  let resizeFrame = 0;
  let lastResize = '';
  let addressDirty = false;
  let pages = [];
  let activePage = '';
  let heartbeatTimer = 0;
  let forgettingSession = false;
  let frameIssued = 0;
  let framePainted = 0;

  function setNotice(text, tone) {
    elements.notice.textContent = text;
    elements.notice.dataset.tone = tone || 'neutral';
  }

  function safeFailure(responseStatus) {
    if (responseStatus === 401 || responseStatus === 403) {
      return 'Gateway authorization was rejected. Check the gateway admin API credential.';
    }
    if (responseStatus === 404) return 'This gateway does not provide the requested login endpoint.';
    if (responseStatus === 429) return 'The gateway is busy. Wait a moment, then retry.';
    if (responseStatus >= 500) return 'The gateway reported a server-side failure. Try again later.';
    return 'The gateway request could not be completed. Check that the local gateway is available.';
  }

  async function api(path, options) {
    if (!authorization) throw new Error('Enter the gateway admin API credential first.');
    const config = options || {};
    const headers = { Authorization: 'Bearer ' + authorization };
    if (config.body !== undefined) headers['Content-Type'] = 'application/json';
    let response;
    try {
      response = await fetch(path, {
        method: config.method || 'GET',
        headers,
        body: config.body === undefined ? undefined : JSON.stringify(config.body),
        credentials: 'same-origin'
      });
    } catch (_) {
      throw new Error('Could not reach the local gateway. Check its address and try again.');
    }
    if (!response.ok) {
      const code = (await response.json().catch(() => null))?.error?.code;
      const failure = new Error(code === 'embedded_browser_not_packaged'
        ? 'This runtime does not package embedded Chromium. Use the documented Replit-native setup or the extension relay.'
        : safeFailure(response.status));
      failure.status = response.status;
      throw failure;
    }
    try {
      return await response.json();
    } catch (_) {
      throw new Error('The gateway returned an unreadable response.');
    }
  }

  function setConnection(label, live) {
    elements.connection.textContent = label;
    elements.connection.dataset.live = live ? 'true' : 'false';
  }

  function updateControls() {
    const connected = Boolean(socket && socket.readyState === WebSocket.OPEN && running);
    const locked = pendingSave || !connected;
    elements.start.disabled = !authorization || !selectedProvider || Boolean(socket) || pendingSave || forgettingSession;
    elements.cancel.disabled = !socket;
    elements.save.disabled = !connected || pendingSave || forgettingSession;
    elements.forget.disabled = !authorization || !selectedProvider || forgettingSession;
    elements.back.disabled = locked || !canBack;
    elements.forward.disabled = locked || !canForward;
    elements.reload.disabled = locked;
    elements.address.disabled = locked;
    elements.addressGo.disabled = locked;
    elements.canvas.setAttribute('aria-disabled', locked ? 'true' : 'false');
  }

  function renderProviders() {
    elements.providerList.replaceChildren();
    providers.forEach((provider) => {
      const status = providerStatus.get(provider.id);
      const choice = document.createElement('button');
      choice.type = 'button';
      choice.className = 'provider-choice';
      choice.dataset.provider = provider.id;
      choice.setAttribute('aria-pressed', String(selectedProvider === provider.id));
      choice.disabled = !authorization || forgettingSession;
      const info = document.createElement('span');
      const name = document.createElement('span');
      name.className = 'provider-name';
      name.textContent = provider.name;
      const meta = document.createElement('span');
      meta.className = 'provider-meta';
      if (!authorization) meta.textContent = 'Connect gateway first';
      else if (!status) meta.textContent = 'Routing not reported · unavailable';
      else if (status.enabled !== true) meta.textContent = status.session_present === true ? 'Routing disabled · session saved' : 'Routing disabled · no saved session';
      else meta.textContent = status.session_present === true ? 'Routing enabled · session saved' : 'Routing enabled · no saved session';
      info.append(name, meta);
      const dot = document.createElement('span');
      dot.className = 'dot' + (status && status.enabled === true ? ' enabled' : '') + (status && status.session_present === true ? ' saved' : '');
      dot.setAttribute('aria-hidden', 'true');
      choice.append(info, dot);
      choice.addEventListener('click', () => selectProvider(provider.id));
      elements.providerList.append(choice);
    });
    updateControls();
  }

  function selectProvider(providerId) {
    if (forgettingSession) return;
    if (selectedProvider === providerId) return;
    startRequest += 1;
    if (socket) cancelSocket('Provider changed. The previous browser session was cancelled.');
    selectedProvider = providerId;
    pages = [];
    activePage = '';
    renderPages();
    elements.safeUrl.textContent = 'No page loaded';
    elements.address.value = '';
    elements.frameSize.textContent = 'Viewport idle';
    elements.sessionNote.textContent = 'Ready to start a private ' + providers.find((item) => item.id === providerId).name + ' browser session.';
    clearViewport();
    renderProviders();
    setConnection('Not started', false);
  }

  async function refreshStatus() {
    if (!authorization) {
      setNotice('Enter your gateway credential to load provider availability.', 'neutral');
      renderProviders();
      return;
    }
    elements.refresh.disabled = true;
    setNotice('Checking gateway status…', 'neutral');
    try {
      const result = await api('/login/api/status');
      if (!result || !Array.isArray(result.providers)) throw new Error('The gateway status response is incomplete.');
      providerStatus.clear();
      result.providers.forEach((provider) => {
        if (provider && typeof provider.name === 'string' && providers.some((item) => item.id === provider.name.toLowerCase())) {
          providerStatus.set(provider.name.toLowerCase(), {
            enabled: provider.enabled === true,
            session_present: provider.session_present === true
          });
        }
      });
      renderProviders();
      if (!selectedProvider) selectedProvider = providers[0].id;
      renderProviders();
      const available = providers.filter((provider) => {
        const state = providerStatus.get(provider.id);
        return state && state.enabled;
      }).length;
      setNotice('Gateway connected · ' + available + ' of 8 providers enabled.', 'success');
    } catch (error) {
      setNotice(error.message === 'The gateway status response is incomplete.' ? error.message : error.message, 'error');
    } finally {
      elements.refresh.disabled = false;
      renderProviders();
    }
  }

  function clearViewport() {
    context.clearRect(0, 0, elements.canvas.width, elements.canvas.height);
    elements.empty.hidden = false;
  }

  function safePageAddress(value) {
    if (typeof value !== 'string') return '';
    try {
      const parsed = new URL(value, window.location.origin);
      if (parsed.protocol !== 'https:' && parsed.protocol !== 'http:') return '';
      return parsed.origin + parsed.pathname;
    } catch (_) {
      return '';
    }
  }

  function renderPages() {
    elements.pageTabs.replaceChildren();
    const validPages = pages.filter((page) => page && typeof page.id === 'string' && page.id.length > 0 && typeof page.url === 'string' && page.url.length <= 4096);
    if (validPages.length && !validPages.some((page) => page.id === activePage)) activePage = validPages[0].id;
    elements.pageTabs.hidden = validPages.length === 0;
    if (validPages.length) {
      elements.viewportWrap.setAttribute('aria-labelledby', 'browser-page-tab-' + validPages.findIndex((page) => page.id === activePage));
    } else {
      elements.viewportWrap.removeAttribute('aria-labelledby');
    }
    validPages.forEach((page, index) => {
      const button = document.createElement('button');
      button.type = 'button';
      button.className = 'page-tab';
      button.setAttribute('role', 'tab');
      button.setAttribute('aria-selected', String(page.id === activePage));
      button.setAttribute('aria-controls', 'viewport-wrap');
      button.tabIndex = page.id === activePage ? 0 : -1;
      button.id = 'browser-page-tab-' + index;
      button.textContent = safePageAddress(page.url) || 'Provider page';
      button.title = button.textContent;
      button.addEventListener('click', () => send({ type: 'page', id: page.id }));
      button.addEventListener('keydown', (event) => {
        if (!['ArrowLeft', 'ArrowRight', 'Home', 'End'].includes(event.key)) return;
        event.preventDefault();
        const tabs = Array.from(elements.pageTabs.querySelectorAll('[role="tab"]'));
        const current = tabs.indexOf(button);
        const nextIndex = event.key === 'Home' ? 0 : event.key === 'End' ? tabs.length - 1 :
          (current + (event.key === 'ArrowRight' ? 1 : -1) + tabs.length) % tabs.length;
        if (tabs[nextIndex]) {
          tabs[nextIndex].focus();
          tabs[nextIndex].click();
        }
      });
      elements.pageTabs.append(button);
    });
  }

  function updatePages(message) {
    if (Array.isArray(message.pages)) {
      pages = message.pages.slice(0, 32).filter((page) => page && typeof page.id === 'string' && page.id.length > 0 && page.id.length <= 256 && typeof page.url === 'string' && page.url.length <= 4096);
      const markedActive = pages.find((page) => page.active === true);
      if (markedActive) activePage = markedActive.id;
    }
    if (typeof message.activePage === 'string') activePage = message.activePage;
    renderPages();
  }

  function stopHeartbeat() {
    if (heartbeatTimer) window.clearInterval(heartbeatTimer);
    heartbeatTimer = 0;
  }

  function startHeartbeat(connection) {
    stopHeartbeat();
    heartbeatTimer = window.setInterval(() => {
      if (!document.hidden && socket === connection && connection.readyState === WebSocket.OPEN) {
        try { connection.send(JSON.stringify({ type: 'heartbeat' })); } catch (_) { /* close handler will report loss */ }
      }
    }, 15000);
  }

  function send(message) {
    if (!socket || socket.readyState !== WebSocket.OPEN || pendingSave) return false;
    try {
      socket.send(JSON.stringify(message));
      return true;
    } catch (_) {
      setNotice('The browser connection is no longer available. Cancel it and start again.', 'error');
      return false;
    }
  }

  function cancelSocket(reason, sendCancel) {
    const previous = socket;
    socket = null;
    socketProvider = null;
    running = false;
    pendingSave = false;
    saveConfirmed = false;
    startRequest += 1;
    stopHeartbeat();
    pages = [];
    activePage = '';
    renderPages();
    removeResizeObserver();
    clearViewport();
    lastResize = '';
    if (previous) {
      if (sendCancel !== false && previous.readyState === WebSocket.OPEN) {
        try { previous.send(JSON.stringify({ type: 'cancel' })); } catch (_) { /* socket already closed */ }
      }
      try { previous.close(); } catch (_) { /* socket already closed */ }
    }
    if (reason) setNotice(reason, 'neutral');
    setConnection('Not started', false);
    elements.sessionNote.textContent = reason || 'The private browser session has ended.';
    canBack = false;
    canForward = false;
    updateControls();
  }

  async function startSession() {
    const provider = providerStatus.get(selectedProvider);
    if (!authorization || !provider || socket) return;
    const requestedProvider = selectedProvider;
    const requestId = ++startRequest;
    elements.start.disabled = true;
    setNotice('Requesting a private browser session…', 'neutral');
    elements.sessionNote.textContent = 'Opening a private browser for this provider.';
    try {
      let result;
      // A previous browser can still be closing after its socket ends. Keep
      // provider exclusivity server-side; retry only this explicit busy result.
      for (let attempt = 0; attempt < 12; attempt += 1) {
        try {
          result = await api('/login/api/' + encodeURIComponent(requestedProvider) + '/browser/ticket', { method: 'POST', body: {} });
          break;
        } catch (error) {
          if (error.status !== 409 || attempt === 11) throw error;
          await new Promise(resolve => setTimeout(resolve, 200));
          if (requestId !== startRequest || requestedProvider !== selectedProvider) return;
        }
      }
      if (!result || typeof result.ticket !== 'string' || !result.ticket) throw new Error('The gateway did not issue a browser ticket.');
      if (requestId !== startRequest || requestedProvider !== selectedProvider) return;
      const protocol = 'cg-login.' + result.ticket;
      const scheme = window.location.protocol === 'https:' ? 'wss:' : 'ws:';
      const url = scheme + '//' + window.location.host + '/login/api/' + encodeURIComponent(selectedProvider) + '/browser/stream';
      const connection = new WebSocket(url, [protocol]);
      socket = connection;
      socketProvider = selectedProvider;
      running = false;
      pendingSave = false;
      saveConfirmed = false;
      clearViewport();
      setConnection('Connecting', false);
      updateControls();
      connection.addEventListener('open', () => {
        if (socket !== connection || socketProvider !== selectedProvider) return;
        setConnection('Connecting', false);
        setNotice('Browser connected. Waiting for the first frame…', 'neutral');
        startHeartbeat(connection);
        installResizeObserver();
        scheduleResize();
      });
      connection.addEventListener('message', (event) => {
        if (socket === connection) handleSocketMessage(event);
      });
      connection.addEventListener('error', () => {
        if (socket === connection && socketProvider === selectedProvider) {
          setNotice('Browser connection failed. Check the gateway runtime and try again.', 'error');
          setConnection('Connection error', false);
        }
      });
      connection.addEventListener('close', () => {
        const wasCurrent = socket === connection && socketProvider === selectedProvider;
        if (wasCurrent) {
          socket = null;
          socketProvider = null;
          stopHeartbeat();
          pages = [];
          activePage = '';
          renderPages();
          running = false;
          pendingSave = false;
          removeResizeObserver();
          clearViewport();
          updateControls();
          if (saveConfirmed) {
            saveConfirmed = false;
            setConnection('Saved', false);
            refreshStatus();
            return;
          }
          setConnection('Disconnected', false);
          elements.sessionNote.textContent = 'The browser connection closed. Start a new session to continue.';
          setNotice('The browser connection closed. The session was not confirmed saved.', 'error');
        }
      });
    } catch (error) {
      if (requestId !== startRequest) return;
      setNotice(error.message, 'error');
      elements.sessionNote.textContent = 'Could not start the browser session.';
      updateControls();
    }
  }

  function handleSocketMessage(event) {
    if (!socket || typeof event.data !== 'string') return;
    let message;
    try { message = JSON.parse(event.data); } catch (_) {
      setNotice('The browser sent an unreadable update.', 'error');
      return;
    }
    if (!message || typeof message.type !== 'string') return;
    if (message.type === 'frame') {
      drawFrame(message);
    } else if (message.type === 'status') {
      running = true;
      canBack = message.canBack === true;
      canForward = message.canForward === true;
      if (typeof message.url === 'string') {
        const safePath = safePageAddress(message.url);
        if (safePath) {
          elements.safeUrl.textContent = safePath;
          if (!addressDirty && document.activeElement !== elements.address) {
            elements.address.value = safePath;
          }
        } else {
          elements.safeUrl.textContent = 'Provider page active';
        }
      }
      updatePages(message);
      setConnection('Browser active', true);
      elements.sessionNote.textContent = 'Private session active. Enter credentials directly in the browser viewport.';
      updateControls();
    } else if (message.type === 'ready') {
      running = true;
      setConnection('Browser ready', true);
      setNotice('Browser ready. Use the viewport to complete sign-in.', 'success');
      elements.sessionNote.textContent = 'Private session active. Passwords, MFA and CAPTCHA are entered in the remote browser.';
      updateControls();
      scheduleResize();
    } else if (message.type === 'saved') {
      const wasActive = message.active === true;
      saveConfirmed = true;
      pendingSave = true;
      clearViewport();
      if (wasActive) {
        setNotice('Session save confirmed by the gateway.', 'success');
        elements.sessionNote.textContent = 'Session saved by the gateway. Cookies are handled only by its encrypted save flow.';
        refreshStatus();
      } else {
        setNotice('Encrypted session saved. Provider runtime activation was not confirmed.', 'success');
        elements.sessionNote.textContent = 'The browser was closed after saving. Enable the provider or retry its runtime activation separately.';
      }
      updateControls();
    } else if (message.type === 'error') {
      pendingSave = false;
      setConnection('Browser error', false);
      setNotice(describeBrowserFailure(message.code), 'error');
      elements.sessionNote.textContent = 'The gateway reported a browser runtime failure. No raw backend details are shown.';
      updateControls();
    } else if (message.type === 'pages') {
      updatePages(message);
    } else if (message.type === 'popup_blocked' || message.type === 'browser_busy' ||
      message.type === 'invalid_input' || message.type === 'input_queue_full' ||
      message.type === 'session_expired' || message.type === 'session_revoked' ||
      message.type === 'browser_runtime_unavailable') {
      const code = typeof message.code === 'string' ? message.code : message.type;
      setNotice(describeBrowserFailure(code), 'error');
    }
  }

  function describeBrowserFailure(code) {
    const safeMessages = {
      embedded_browser_not_packaged: 'Embedded Chromium is not packaged in this runtime. Use the documented Replit-native setup; the browser extension relay remains a separate option.',
      unauthorized: 'Browser authorization failed. Refresh the gateway status and start again.',
      ticket_expired: 'The browser ticket expired before the session started. Start again.',
      provider_disabled: 'This provider is disabled by the gateway.',
      browser_unavailable: 'The browser runtime is unavailable. Check gateway browser configuration.',
      browser_runtime_unavailable: 'The browser runtime stopped or could not start. Cancel and check the private gateway runtime.',
      navigation_blocked: 'Navigation was blocked. Use this provider or an approved identity-provider domain.',
      popup_blocked: 'A separate login popup was blocked. Choose a sign-in method that stays in this viewport.',
      browser_busy: 'The provider browser is busy. Wait for the current action to finish, then try again.',
      invalid_input: 'That browser action was not accepted. Check the input and try again.',
      input_queue_full: 'The browser input queue is full. Wait briefly, then try again.',
      session_expired: 'This browser session expired. Start a new session to continue.',
      session_revoked: 'The browser session was revoked. Refresh status before starting again.',
      no_provider_session: 'No provider cookies are available to save. Complete sign-in in the viewport first.',
      vault_save_failed: 'Encrypted save failed. The browser remains open so you can retry safely.',
      launch_failed: 'The gateway could not launch its browser. Check the gateway runtime.',
      session_failed: 'The browser session could not be created. Try again later.',
      timeout: 'The browser session timed out. Start a new session.',
      save_failed: 'The gateway could not save this session. It has not been confirmed.',
      unsupported: 'This browser action is not supported by the gateway.'
    };
    return safeMessages[typeof code === 'string' ? code.toLowerCase() : ''] || 'The gateway reported a browser runtime failure. No raw error details are shown.';
  }

  function drawFrame(message) {
    if (typeof message.data !== 'string' || !Number.isFinite(message.width) || !Number.isFinite(message.height)) return;
    const width = Math.max(1, Math.min(4096, Math.floor(message.width)));
    const height = Math.max(1, Math.min(4096, Math.floor(message.height)));
    const image = new Image();
    const expectedSocket = socket;
    const expectedGeneration = startRequest;
    const expectedPage = activePage;
    const issued = ++frameIssued;
    image.onload = () => {
      if (!socket || socket !== expectedSocket || startRequest !== expectedGeneration ||
          activePage !== expectedPage || issued < framePainted ||
          (message.pageId && activePage && message.pageId !== activePage)) return;
      framePainted = issued;
      elements.canvas.width = width;
      elements.canvas.height = height;
      context.drawImage(image, 0, 0, width, height);
      elements.empty.hidden = true;
      elements.frameSize.textContent = width + ' × ' + height;
      if (!running) {
        running = true;
        setConnection('Browser active', true);
        updateControls();
      }
    };
    image.onerror = () => {
      if (socket !== expectedSocket || startRequest !== expectedGeneration) return;
      setNotice('A browser frame could not be displayed. Waiting for the next frame.', 'error');
    };
    image.src = 'data:image/jpeg;base64,' + message.data;
  }

  function installResizeObserver() {
    removeResizeObserver();
    if (typeof ResizeObserver !== 'function') return;
    resizeObserver = new ResizeObserver(scheduleResize);
    resizeObserver.observe(elements.viewportWrap);
  }

  function removeResizeObserver() {
    if (resizeObserver) resizeObserver.disconnect();
    resizeObserver = null;
    if (resizeFrame) cancelAnimationFrame(resizeFrame);
    resizeFrame = 0;
  }

  function scheduleResize() {
    if (resizeFrame) return;
    resizeFrame = requestAnimationFrame(() => {
      resizeFrame = 0;
      if (!socket || socket.readyState !== WebSocket.OPEN || pendingSave) return;
      const bounds = elements.viewportWrap.getBoundingClientRect();
      const width = Math.round(Math.max(640, Math.min(1920, bounds.width)));
      const height = Math.round(Math.max(480, Math.min(1080, bounds.height)));
      const signature = width + 'x' + height;
      if (signature === lastResize) return;
      lastResize = signature;
      send({ type: 'resize', width, height });
    });
  }

  function canvasPoint(event) {
    const bounds = elements.canvas.getBoundingClientRect();
    const sourceWidth = elements.canvas.width || 1;
    const sourceHeight = elements.canvas.height || 1;
    const scale = Math.min(bounds.width / sourceWidth, bounds.height / sourceHeight);
    const drawnWidth = sourceWidth * scale;
    const drawnHeight = sourceHeight * scale;
    const offsetX = (bounds.width - drawnWidth) / 2;
    const offsetY = (bounds.height - drawnHeight) / 2;
    const x = Math.max(0, Math.min(sourceWidth - 1, Math.round((event.clientX - bounds.left - offsetX) / scale)));
    const y = Math.max(0, Math.min(sourceHeight - 1, Math.round((event.clientY - bounds.top - offsetY) / scale)));
    return { x, y };
  }

  function pointerButton(button) {
    if (button === 0) return 'left';
    if (button === 1) return 'middle';
    if (button === 2) return 'right';
    return 'none';
  }

  function pointerEventName(type) {
    if (type === 'pointermove') return 'mouseMoved';
    if (type === 'pointerdown') return 'mousePressed';
    return 'mouseReleased';
  }

  elements.canvas.addEventListener('pointermove', (event) => {
    const point = canvasPoint(event);
    send({ type: 'mouse', event: 'mouseMoved', x: point.x, y: point.y, button: 'none', buttons: event.buttons });
  });
  elements.canvas.addEventListener('pointerdown', (event) => {
    elements.canvas.focus({ preventScroll: true });
    if (elements.canvas.setPointerCapture) elements.canvas.setPointerCapture(event.pointerId);
    const point = canvasPoint(event);
    send({ type: 'mouse', event: pointerEventName(event.type), x: point.x, y: point.y, button: pointerButton(event.button), buttons: event.buttons, clickCount: 1 });
    event.preventDefault();
  });
  elements.canvas.addEventListener('pointerup', (event) => {
    const point = canvasPoint(event);
    send({ type: 'mouse', event: pointerEventName(event.type), x: point.x, y: point.y, button: pointerButton(event.button), buttons: event.buttons, clickCount: 1 });
    event.preventDefault();
  });
  elements.canvas.addEventListener('contextmenu', (event) => event.preventDefault());
  elements.canvas.addEventListener('wheel', (event) => {
    const point = canvasPoint(event);
    send({ type: 'wheel', x: point.x, y: point.y, deltaX: event.deltaX, deltaY: event.deltaY });
    event.preventDefault();
  }, { passive: false });

  function modifierMask(event) {
    return (event.altKey ? 1 : 0) | (event.ctrlKey ? 2 : 0) | (event.metaKey ? 4 : 0) | (event.shiftKey ? 8 : 0);
  }

  function isPrintable(event) {
    return event.key && event.key.length === 1 && !event.ctrlKey && !event.metaKey && !event.altKey;
  }

  elements.canvas.addEventListener('keydown', (event) => {
    if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'v') {
      event.preventDefault();
      return;
    }
    if (isPrintable(event)) {
      send({ type: 'text', text: event.key });
      event.preventDefault();
      return;
    }
    send({ type: 'key', event: 'keyDown', key: event.key, code: event.code, modifiers: modifierMask(event) });
    event.preventDefault();
  });
  elements.canvas.addEventListener('keyup', (event) => {
    if ((event.ctrlKey || event.metaKey) && event.key.toLowerCase() === 'v') {
      event.preventDefault();
      return;
    }
    if (isPrintable(event)) {
      event.preventDefault();
      return;
    }
    send({ type: 'key', event: 'keyUp', key: event.key, code: event.code, modifiers: modifierMask(event) });
    event.preventDefault();
  });
  elements.canvas.addEventListener('paste', (event) => {
    if (typeof ClipboardEvent !== 'function' || !(event instanceof ClipboardEvent)) return;
    event.preventDefault();
    const plainText = event.clipboardData && event.clipboardData.getData('text/plain');
    if (typeof plainText === 'string' && plainText.length > 0) {
      send({ type: 'text', text: plainText.slice(0, 4096) });
    }
  });

  elements.authForm.addEventListener('submit', (event) => {
    event.preventDefault();
    const value = elements.credential.value.trim();
    if (!value) return;
    startRequest += 1;
    if (socket) cancelSocket('Gateway credential changed. The previous browser session was cancelled.');
    authorization = value;
    elements.credential.value = '';
    providerStatus.clear();
    renderProviders();
    refreshStatus();
  });
  elements.refresh.addEventListener('click', refreshStatus);
  elements.start.addEventListener('click', startSession);
  elements.cancel.addEventListener('click', () => cancelSocket('Browser session cancelled. No save was requested.'));
  elements.forget.addEventListener('click', async () => {
    if (!authorization || !selectedProvider || forgettingSession) return;
    const provider = providers.find((item) => item.id === selectedProvider);
    const confirmed = window.confirm('Forget the saved ' + (provider ? provider.name : 'provider') + ' session? The gateway will close its current browser session and remove the saved session.');
    if (!confirmed) return;
    forgettingSession = true;
    updateControls();
    try {
      const result = await api('/login/api/' + encodeURIComponent(selectedProvider) + '/browser/session', { method: 'DELETE' });
      if (!result || result.ok !== true || typeof result.removed !== 'boolean') {
        throw new Error('The gateway did not confirm the saved-session removal.');
      }
      // The gateway owns cancellation during revocation; do not send a second cancel.
      cancelSocket('The gateway closed the browser session after the forget request.', false);
      await refreshStatus();
      setNotice(result.removed ? 'Saved session forgotten by the gateway.' : 'The gateway confirmed that no saved session remained.', 'success');
      elements.sessionNote.textContent = result.removed
        ? 'The saved session was removed and the current browser session was closed.'
        : 'The current browser session was closed; the gateway reported no saved session to remove.';
    } catch (error) {
      setNotice(error.message || 'The saved session could not be forgotten. No removal was confirmed.', 'error');
    } finally {
      forgettingSession = false;
      updateControls();
    }
  });
  elements.save.addEventListener('click', () => {
    if (!socket || socket.readyState !== WebSocket.OPEN || pendingSave) return;
    if (!send({ type: 'save' })) return;
    pendingSave = true;
    elements.sessionNote.textContent = 'Waiting for the gateway to confirm the encrypted session save…';
    setNotice('Save requested. Input is paused until the gateway responds.', 'neutral');
    updateControls();
  });
  elements.back.addEventListener('click', () => send({ type: 'navigate', direction: 'back' }));
  elements.forward.addEventListener('click', () => send({ type: 'navigate', direction: 'forward' }));
  elements.reload.addEventListener('click', () => send({ type: 'navigate', direction: 'reload' }));
  elements.addressForm.addEventListener('submit', (event) => {
    event.preventDefault();
    const target = elements.address.value.trim();
    if (!target || !socket) return;
    let parsed;
    try { parsed = new URL(target); } catch (_) {
      setNotice('Enter a complete HTTPS address for a provider domain you are authorized to use.', 'error');
      return;
    }
    if (parsed.protocol !== 'https:') {
      setNotice('For safety, use HTTPS for provider domains.', 'error');
      return;
    }
    send({ type: 'navigate', url: parsed.href });
    addressDirty = false;
    elements.canvas.focus({ preventScroll: true });
  });
  elements.address.addEventListener('input', () => { addressDirty = true; });

  document.addEventListener('visibilitychange', () => {
    if (!document.hidden && socket && socket.readyState === WebSocket.OPEN) scheduleResize();
  });

  renderProviders();
})();
