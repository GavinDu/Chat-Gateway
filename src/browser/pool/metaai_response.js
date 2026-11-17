// Meta AI's RSC reader obtains response.body more than once. The pinned
// Obscura getter incorrectly throws after the first reader is acquired.
// Preserve the same stream; never reset consumption, copy a body or affect
// native browsers / unrelated providers.
(() => {
  if (typeof location === 'undefined' || location.protocol !== 'https:' ||
      !['meta.ai', 'www.meta.ai'].includes(location.hostname) ||
      !['', '443'].includes(String(location.port || ''))) return;
  const proto = globalThis.Response?.prototype;
  const descriptor = proto && Object.getOwnPropertyDescriptor(proto, 'body');
  if (!descriptor?.get || !descriptor.configurable || descriptor.get.__cgMetaBody) return;
  let probe;
  try { probe = new Response(''); } catch (_) { return; }
  if (!Object.hasOwn(probe, '_bodyBytes') || !Object.hasOwn(probe, '_bodyStream') ||
      !Object.hasOwn(probe, '_bodyUsed') || !Object.hasOwn(probe, '_bodyNull')) return;
  const get = function body() {
    // Only the pinned fallback's existing stream is reusable as a property.
    // Its reader remains locked/disturbed and body-consuming methods still
    // reject. Native getter brand checks remain in force for other receivers.
    if (Object.hasOwn(this, '_bodyStream') && this._bodyStream &&
        Object.hasOwn(this, '_bodyUsed') && Object.hasOwn(this, '_bodyBytes')) {
      return this._bodyStream;
    }
    return descriptor.get.call(this);
  };
  Object.defineProperty(get, '__cgMetaBody', {value:true});
  Object.defineProperty(proto, 'body', {...descriptor, get});
})();
