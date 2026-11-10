// Obscura's Window may omit WindowOrWorkerGlobalScope.origin. React Router
// uses new URL(path, window.origin), so a missing value makes normal provider
// routing fail before the composer can initialize.
//
// Install only when the runtime has no native string-valued origin. The getter
// derives the value from the *current* document URL on every read, including
// after navigation; it does not broaden origin permissions or forge events.
(() => {
  if (typeof globalThis.origin === 'string') return;
  try {
    Object.defineProperty(globalThis, 'origin', {
      configurable: true,
      enumerable: true,
      get() {
        try {
          const location = globalThis.location;
          if (typeof location?.origin === 'string') return location.origin;
          return new URL(location.href).origin;
        } catch (_) {
          return 'null';
        }
      }
    });
  } catch (_) {
    // A non-configurable native property must never be overwritten.
  }
})();
