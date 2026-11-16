// Preloaded only for the ChatGPT adapter, before author scripts. At runtime
// the exact HTTPS origin and pinned Obscura fallback shape are also checked.
// Never use this compatibility shim to bypass authentication, origin policy,
// request pacing, or the prompt-bound submission guard.
// The pinned embedded stream shim never calls underlyingSource.pull().
// ChatGPT's lazy response readers need it. Keep this compatibility hook
// confined to the known shim and ChatGPT; native browser streams are untouched.
(() => {
  // A non-default HTTPS port is a different origin, even on the same host.
  // Browsers normalize :443 to an empty port; tolerate explicit 443 in shims.
  if (typeof location === 'undefined' || location.protocol !== 'https:' ||
      !['chatgpt.com', 'www.chatgpt.com'].includes(location.hostname) ||
      !['', '443'].includes(String(location.port || ''))) return;
  const NativeStream = globalThis.ReadableStream;
  if (typeof NativeStream !== 'function' || NativeStream.__chatGatewayPullCompat) return;
  let probe;
  try { probe = new NativeStream({start(controller) { controller.close(); }}); }
  catch (_) { return; }
  if (!Array.isArray(probe._reads) || !Array.isArray(probe._queue) ||
      !probe._source || typeof NativeStream.prototype.getReader !== 'function') return;
  const nativeGetReader = NativeStream.prototype.getReader;
  class PullReadableStream extends NativeStream {
    constructor(source = {}, strategy = {}) {
      source ??= {};
      strategy ??= {};
      const requestedWatermark = strategy.highWaterMark;
      const highWaterMark = requestedWatermark === undefined
        ? (source.type === 'bytes' ? 0 : 1) : Number(requestedWatermark);
      if (!Number.isFinite(highWaterMark) || highWaterMark < 0) throw new RangeError('Invalid highWaterMark');
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
        }, error => {
          pulling = false;
          // A late rejected pull must never poison a stream that already
          // delivered its final queued chunk and closed, or was cancelled.
          // The pinned fallback error() would otherwise erase that chunk.
          if (terminal) { again = false; return; }
          controller.error(error);
        });
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
          // The pinned fallback forwards rejected start promises into its
          // native error(), which can destroy a final queued chunk even after
          // close() requested termination. Ignore only *late* start failures
          // when the source already closed or the reader cancelled.
          let startResult;
          try {
            startResult = source.start?.call(source, controller);
          } catch (error) {
            if (!terminal) throw error;
          }
          return Promise.resolve(startResult).then(
            () => { started = true; kick(); },
            error => { if (!terminal) throw error; }
          );
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
      // Preserve a delegated reader's arguments (e.g. a byte/BYOB view).
      // The pinned fallback may ignore them; silently dropping is incorrect.
      reader.read = (...args) => {
        const queued = this._queue.length > 0;
        const pending = nativeRead(...args);
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
