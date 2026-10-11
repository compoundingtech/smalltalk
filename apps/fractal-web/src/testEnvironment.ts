// jsdom omits the browser font-loading API used by the real kit's layout observers.
// Preserve actual event delivery without pretending to measure or load fonts.
if (typeof document !== 'undefined') {
  Object.defineProperty(document, 'fonts', {
    configurable: true,
    value: Object.assign(new EventTarget(), { ready: Promise.resolve(), status: 'loaded' }),
  })
}
