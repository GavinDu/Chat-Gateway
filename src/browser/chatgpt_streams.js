// The pinned embedded stream shim never calls underlyingSource.pull().
// ChatGPT's lazy response readers need it. Keep this compatibility hook
// confined to the known shim and ChatGPT; native browser streams are untouched.
(() => {
  if (!['chatgpt.com', 'www.chatgpt.com'].includes(location.hostname)) return;
  const NativeStream = globalThis.ReadableStream;
  const probe = new NativeStream({start(controller) { controller.close(); }});
  if (!Array.isArray(probe._reads) || !Array.isArray(probe._queue) || !probe._source) return;
  const nativeGetReader = NativeStream.prototype.getReader;
  class PullReadableStream extends NativeStream {
    constructor(source = {}, strategy = {}) {
      let stream;
      let controller;
      let started = false;
      let terminal = false;
      let pulling = false;
      let again = false;
      const kick = () => {
        if (!stream || !started || terminal || !stream._reads.length || typeof source.pull !== 'function') return;
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
            enqueue(value) { nativeController.enqueue(value); kick(); },
            close() { terminal = true; nativeController.close(); },
            error(reason) { terminal = true; nativeController.error(reason); },
            get desiredSize() { return nativeController.desiredSize; }
          };
          return Promise.resolve(source.start?.call(source, controller)).then(() => { started = true; kick(); });
        },
        cancel(reason) { terminal = true; return source.cancel?.call(source, reason); }
      }, strategy);
      stream = this;
      this._pullIfNeeded = kick;
    }
    getReader(...args) {
      const reader = nativeGetReader.apply(this, args);
      const nativeRead = reader.read.bind(reader);
      reader.read = () => { const pending = nativeRead(); this._pullIfNeeded(); return pending; };
      return reader;
    }
  }
  Object.defineProperty(PullReadableStream, 'name', {value: 'ReadableStream'});
  globalThis.ReadableStream = PullReadableStream;
})();
