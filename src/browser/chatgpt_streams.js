// The pinned embedded stream shim never calls underlyingSource.pull().
// ChatGPT's lazy response readers need it. Keep this compatibility hook
// confined to the known shim and ChatGPT; native browser streams are untouched.
(() => {
  if (!['chatgpt.com', 'www.chatgpt.com'].includes(location.hostname)) return;
  const NativeStream = globalThis.ReadableStream;
  if (NativeStream.__chatGatewayPullCompat) return;
  const probe = new NativeStream({start(controller) { controller.close(); }});
  if (!Array.isArray(probe._reads) || !Array.isArray(probe._queue) || !probe._source) return;
  const nativeGetReader = NativeStream.prototype.getReader;
  class PullReadableStream extends NativeStream {
    constructor(source = {}, strategy = {}) {
      source ??= {};
      strategy ??= {};
      const requestedWatermark = strategy.highWaterMark;
      const highWaterMark = requestedWatermark === undefined
        ? (source.type === 'bytes' ? 0 : 1) : Number(requestedWatermark);
      if (Number.isNaN(highWaterMark) || highWaterMark < 0) throw new RangeError('Invalid highWaterMark');
      const size = strategy.size;
      if (size !== undefined && typeof size !== 'function') throw new TypeError('Invalid stream size algorithm');
      const queuedSizes = [];
      let queuedSize = 0;
      let failed = false;
      let stream;
      let controller;
      let started = false;
      let terminal = false;
      let pulling = false;
      let again = false;
      const kick = () => {
        if (!stream || !started || terminal || stream._state !== 'readable' || typeof source.pull !== 'function') return;
        // Normal streams prefill their queue after start, before a reader is
        // attached. Demand-only pulling can strand startup handshakes.
        if (!stream._reads.length && highWaterMark <= queuedSize) return;
        if (pulling) { again = true; return; }
        pulling = true;
        Promise.resolve().then(() => { if (!terminal) return source.pull.call(source, controller); }).then(() => {
          pulling = false;
          if (again) { again = false; kick(); }
        }, error => { pulling = false; controller.error(error); });
      };
      super({
        start(nativeController) {
          controller = {
            enqueue(value) {
              if (terminal) return nativeController.enqueue(value);
              if (!stream?._reads.length) {
                let amount;
                try {
                  amount = size === undefined ? 1 : Number(size(value));
                  if (!Number.isFinite(amount) || amount < 0) throw new RangeError('Invalid chunk size');
                } catch (error) { controller.error(error); throw error; }
                queuedSizes.push(amount);
                queuedSize += amount;
              }
              nativeController.enqueue(value);
              kick();
            },
            close() { terminal = true; nativeController.close(); },
            error(reason) { terminal = true; failed = true; queuedSizes.length = 0; queuedSize = 0; nativeController.error(reason); },
            get desiredSize() {
              if (failed || stream?._state === 'errored') return null;
              if (stream?._state === 'closed' || (terminal && !queuedSizes.length)) return 0;
              return highWaterMark - queuedSize;
            }
          };
          return Promise.resolve(source.start?.call(source, controller)).then(() => { started = true; kick(); });
        },
        cancel(reason) { terminal = true; queuedSizes.length = 0; queuedSize = 0; return source.cancel?.call(source, reason); }
      }, strategy);
      stream = this;
      this._pullIfNeeded = kick;
      this._dequeued = () => { if (queuedSizes.length) queuedSize = Math.max(0, queuedSize - queuedSizes.shift()); };
    }
    getReader(...args) {
      const reader = nativeGetReader.apply(this, args);
      const nativeRead = reader.read.bind(reader);
      reader.read = () => {
        const queued = this._queue.length > 0;
        const pending = nativeRead();
        if (queued) this._dequeued();
        this._pullIfNeeded();
        return pending;
      };
      return reader;
    }
  }
  Object.defineProperty(PullReadableStream, 'name', {value: 'ReadableStream'});
  Object.defineProperty(PullReadableStream, '__chatGatewayPullCompat', {value: true});
  globalThis.ReadableStream = PullReadableStream;
})();
