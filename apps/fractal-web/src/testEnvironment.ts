// jsdom omits the browser font-loading API used by the real kit's layout observers.
// Preserve actual event delivery without pretending to measure or load fonts.
if (typeof document !== 'undefined') {
  Object.defineProperty(document, 'fonts', {
    configurable: true,
    value: Object.assign(new EventTarget(), { ready: Promise.resolve(), status: 'loaded' }),
  })
  // jsdom has no media-query engine; these fixtures use the default motion preference.
  window.matchMedia = (media: string): MediaQueryList => Object.assign(new EventTarget(), {
    media,
    matches: false,
    onchange: null,
    addListener(this: EventTarget, listener: EventListener) { this.addEventListener('change', listener) },
    removeListener(this: EventTarget, listener: EventListener) { this.removeEventListener('change', listener) },
  })
}
