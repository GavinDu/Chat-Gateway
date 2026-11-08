// The pinned Obscura runtime has the DOMTokenList getter but omits the
// HTMLIFrameElement.sandbox [PutForwards=value] assignment behavior.
// Preserve the live token list and reflect exactly the caller's requested
// attribute value; do not add permissions or alter a native setter.
(() => {
  let prototype = Object.getPrototypeOf(document.createElement('iframe'));
  while (prototype) {
    const descriptor = Object.getOwnPropertyDescriptor(prototype, 'sandbox');
    if (descriptor) {
      if (descriptor.get && !descriptor.set && descriptor.configurable) {
        Object.defineProperty(prototype, 'sandbox', {
          ...descriptor,
          set(value) { descriptor.get.call(this).value = value; }
        });
      }
      return;
    }
    prototype = Object.getPrototypeOf(prototype);
  }
})();
