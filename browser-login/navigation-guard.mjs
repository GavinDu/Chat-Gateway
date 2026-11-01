// Browser-level CDP interception. A popup's first request can precede its
// Playwright Page/Frame event, so a page-only interceptor is insufficient.
import { EventEmitter } from 'node:events';
import { allowedDocument } from './policy.mjs';

export class Session extends EventEmitter {
  constructor(parent, id) {
    super();
    this.parent = parent;
    this.id = id;
    this.serial = 0;
    this.pending = new Map();
  }
  send(method, params = {}) {
    if (this.closed) return Promise.reject(new Error('guard_closed'));
    const id = ++this.serial;
    // Return the response promise immediately. A child error/detach can
    // precede its parent's transport ACK; awaiting the ACK first would leave
    // the rejected child promise unhandled and terminate the worker.
    return new Promise((resolve, reject) => {
      this.pending.set(id, { resolve, reject });
      Promise.resolve().then(() => {
        if (!this.closed) return this.parent.send('Target.sendMessageToTarget', {
          sessionId: this.id, message: JSON.stringify({ id, method, params })
        });
      }).catch(() => {
        this.pending.get(id)?.reject(new Error('guard_unavailable'));
        this.pending.delete(id);
      });
    });
  }
  receive(message) {
    const m = JSON.parse(message);
    if (m.id) {
      const request = this.pending.get(m.id);
      this.pending.delete(m.id);
      if (m.error) request?.reject(new Error('guard_unavailable'));
      else request?.resolve(m.result);
    } else if (m.method) this.emit(m.method, m.params);
  }
  dispose() {
    this.closed = true;
    for (const p of this.pending.values()) p.reject(new Error('guard_closed'));
    this.pending.clear();
  }
}

export async function navigationGuard(browser, provider, blocked, broken) {
  const root = await browser.newBrowserCDPSession();
  const sessions = new Map();
  const installed = new Set();
  const rootTargets = new Set();
  let closed = false;
  const autoAttach = { autoAttach: true, waitForDebuggerOnStart: true, flatten: false };
  function attachEvents(parent) {
    parent.on('Target.receivedMessageFromTarget', m => sessions.get(m.sessionId)?.receive(m.message));
    parent.on('Target.detachedFromTarget', m => {
      sessions.get(m.sessionId)?.dispose();
      sessions.delete(m.sessionId);
    });
    parent.on('Target.attachedToTarget', async event => {
      let id = event.sessionId;
      if (parent === root) {
        // Chromium requires flat browser auto-attach, while Playwright's
        // public CDPSession API cannot dispatch arbitrary flat session IDs.
        // Keep that target paused and explicitly attach a routable session.
        if (rootTargets.has(event.targetInfo.targetId)) return;
        rootTargets.add(event.targetInfo.targetId);
        try {
          ({ sessionId: id } = await root.send('Target.attachToTarget', {
            targetId: event.targetInfo.targetId, flatten: false
          }));
        } catch { if (!closed) broken('target_attach'); return; }
      }
      const client = new Session(parent, id);
      sessions.set(id, client);
      attachEvents(client);
      let phase = 'page_enable';
      void (async () => {
        const documentTarget = ['page', 'iframe'].includes(event.targetInfo.type);
        if (documentTarget) {
          await client.send('Page.enable');
          // New popup targets may not yet have a frame tree while paused.
          // Unknown frames are top-level (strict); only observed child-frame
          // IDs may use the narrower CAPTCHA exception.
          const children = new Set();
          client.on('Page.frameAttached', frame => children.add(frame.frameId));
          client.on('Page.frameDetached', frame => children.delete(frame.frameId));
          client.on('Fetch.requestPaused', request => {
            const top = event.targetInfo.type === 'page' &&
              (!request.frameId || !children.has(request.frameId));
            const accept = allowedDocument(provider, request.request.url, top);
            if (!accept) blocked();
            void client.send(accept ? 'Fetch.continueRequest' : 'Fetch.failRequest',
              accept ? { requestId: request.requestId } :
                { requestId: request.requestId, errorReason: 'BlockedByClient' })
              .catch(() => { if (!closed && !client.closed) broken('document_guard'); });
          });
          await client.send('Fetch.enable', {
            patterns: [{ urlPattern: '*', resourceType: 'Document', requestStage: 'Request' }]
          });
          installed.add(event.targetInfo.targetId);
        }
        phase = 'child_attach';
        await client.send('Target.setAutoAttach', autoAttach);
        phase = 'target_resume';
        await client.send('Runtime.runIfWaitingForDebugger');
        if (parent === root && id !== event.sessionId) {
          // A closer related-target auto-attacher can already have detached
          // this redundant flat session. Guard installation is complete; that
          // cleanup race must not terminate the protected browser.
          await root.send('Target.detachFromTarget', { sessionId: event.sessionId }).catch(() => {});
        }
      })().catch(() => { if (!closed && !client.closed) broken(phase); });
    });
  }
  attachEvents(root);
  await root.send('Target.setAutoAttach', { ...autoAttach, flatten: true });
  const { targetInfos } = await root.send('Target.getTargets');
  const initial = targetInfos.filter(t => t.type === 'page').map(t => t.targetId);
  const deadline = Date.now() + 5000;
  while (initial.some(id => !installed.has(id))) {
    if (Date.now() > deadline) {
      closed = true;
      await root.detach().catch(() => {});
      throw new Error('guard_unavailable');
    }
    await new Promise(r => setTimeout(r, 10));
  }
  return {
    async close() {
      closed = true;
      for (const client of sessions.values()) client.dispose();
      sessions.clear();
      await root.detach().catch(() => {});
    }
  };
}
