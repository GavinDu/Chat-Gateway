// This is installed only on Obscura ChatGPT pages. No external bridge port,
// query-string credential, or persistent browser storage is used.
(() => {
  if (!['chatgpt.com', 'www.chatgpt.com'].includes(location.hostname)) return;
  if (globalThis.top && globalThis.top !== globalThis) return;
  const config = __BRIDGE_CONFIG__;
  const eventProbe = new EventTarget();
  let dispatchesSlots = false;
  eventProbe.onchatgatewayprobe = () => { dispatchesSlots = true; };
  eventProbe.dispatchEvent(new Event('chatgatewayprobe'));
  const nativeFetch = globalThis.fetch.bind(globalThis);
  const bridgeURL = location.origin + config.path;
  const encode = bytes => {
    let result = '';
    for (let i = 0; i < bytes.length; i += 8192) {
      result += String.fromCharCode(...bytes.subarray(i, i + 8192));
    }
    return btoa(result);
  };
  const decode = value => Uint8Array.from(atob(value), c => c.charCodeAt(0));
  const invoke = async command => {
    const encoded = encode(new TextEncoder().encode(JSON.stringify(command)));
    if (encoded.length > 1398104) throw new Error('WebSocket command too large');
    const response = await nativeFetch(bridgeURL, {
      method: 'POST', cache: 'no-store', redirect: 'error',
      headers: {'X-Chat-Gateway-WS-Token': config.token, 'X-Chat-Gateway-WS-Command': encoded}
    });
    const reply = await response.json();
    if (!response.ok || !reply.ok) throw new Error('WebSocket transport unavailable');
    return reply;
  };
  const ready = invoke({op:'reset'});
  ready.catch(() => {});
  class GatewayWebSocket extends EventTarget {
    constructor(target, protocols = []) {
      super();
      const url = new URL(String(target), location.href);
      if (!['ws:', 'wss:'].includes(url.protocol) || url.hash) {
        throw new DOMException('Invalid WebSocket URL', 'SyntaxError');
      }
      if (typeof protocols === 'string') protocols = [protocols];
      if (!Array.isArray(protocols) || new Set(protocols).size !== protocols.length ||
          protocols.some(p => typeof p !== 'string' || !/^[!#$%&'*+\-.^_~0-9A-Za-z|]+$/.test(p))) {
        throw new DOMException('Invalid WebSocket protocols', 'SyntaxError');
      }
      this._url = url.href;
      this._state = 0;
      this._buffered = 0;
      this._protocol = '';
      this._binaryType = 'blob';
      this._id = null;
      this._sendTail = Promise.resolve();
      this.onopen = null; this.onmessage = null; this.onerror = null; this.onclose = null;
      this._opening = this._open(protocols);
      this._opening.catch(() => this._failed());
    }
    get [Symbol.toStringTag]() { return 'WebSocket'; }
    get url() { return this._url; }
    get readyState() { return this._state; }
    get bufferedAmount() { return this._buffered; }
    get protocol() { return this._protocol; }
    get extensions() { return ''; }
    get binaryType() { return this._binaryType; }
    set binaryType(value) { if (value === 'blob' || value === 'arraybuffer') this._binaryType = value; }
    _emit(type, event) {
      this.dispatchEvent(event);
      const handler = this['on' + type];
      if (!dispatchesSlots && typeof handler === 'function') handler.call(this, event);
    }
    async _open(protocols) {
      await ready;
      const result = await invoke({op:'open',url:this._url,protocols,ua:navigator.userAgent});
      this._id = result.id;
      this._protocol = result.protocol;
      if (this._state === 2) { await invoke({op:'release',id:this._id}); this._finish(1006,'',false); return; }
      this._state = 1;
      this._emit('open', new Event('open'));
      this._read().catch(() => this._failed());
    }
    async _read() {
      while (this._state < 3) {
        const {packet} = await invoke({op:'read',id:this._id});
        if (packet.type === 'idle') continue;
        if (packet.type === 'close') { this._finish(packet.code,packet.reason,packet.clean); break; }
        const data = packet.type === 'binary'
          ? (this._binaryType === 'arraybuffer' ? decode(packet.data).buffer : new Blob([decode(packet.data)]))
          : packet.data;
        this._emit('message',new MessageEvent('message',{data,origin:new URL(this._url).origin}));
      }
    }
    send(value) {
      if (this._state === 0) throw new DOMException('WebSocket is connecting','InvalidStateError');
      if (this._state !== 1) return;
      const binary = value instanceof Blob || value instanceof ArrayBuffer || ArrayBuffer.isView(value);
      const size = binary ? (value instanceof Blob ? value.size : value.byteLength) : new TextEncoder().encode(String(value)).length;
      if (size > 786432) throw new DOMException('WebSocket message too large','QuotaExceededError');
      this._buffered += size;
      this._sendTail = this._sendTail.then(async () => {
        let data;
        if (value instanceof Blob) data = encode(new Uint8Array(await value.arrayBuffer()));
        else if (value instanceof ArrayBuffer) data = encode(new Uint8Array(value));
        else if (ArrayBuffer.isView(value)) data = encode(new Uint8Array(value.buffer,value.byteOffset,value.byteLength));
        else data = String(value);
        await invoke({op:'send',id:this._id,binary,data});
        this._buffered -= size;
      });
      this._sendTail.catch(() => this._failed());
    }
    close(code = 1000, reason = '') {
      if (code !== 1000 && !(Number.isInteger(code) && code >= 3000 && code <= 4999)) {
        throw new DOMException('Invalid WebSocket close code','InvalidAccessError');
      }
      reason = String(reason);
      if (new TextEncoder().encode(reason).length > 123) throw new DOMException('WebSocket reason too long','SyntaxError');
      if (this._state >= 2) return;
      this._state = 2;
      if (this._id !== null) {
        this._sendTail.then(() => invoke({op:'close',id:this._id,code,reason})).catch(() => this._failed());
      }
    }
    _failed() {
      if (this._state === 3) return;
      this._emit('error',new Event('error'));
      this._finish(1006,'',false);
    }
    _finish(code,reason,wasClean) {
      if (this._state === 3) return;
      this._state = 3;
      this._buffered = 0;
      const event = new Event('close');
      Object.defineProperties(event,{code:{value:code},reason:{value:reason},wasClean:{value:wasClean}});
      this._emit('close',event);
      if (this._id !== null) invoke({op:'release',id:this._id}).catch(() => {});
    }
  }
  for (const [name,value] of Object.entries({CONNECTING:0,OPEN:1,CLOSING:2,CLOSED:3})) {
    Object.defineProperty(GatewayWebSocket,name,{value,enumerable:true});
    Object.defineProperty(GatewayWebSocket.prototype,name,{value,enumerable:true});
  }
  Object.defineProperty(GatewayWebSocket, 'name', {value:'WebSocket'});
  globalThis.WebSocket = GatewayWebSocket;
})();
