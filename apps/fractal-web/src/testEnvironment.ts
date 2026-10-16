// jsdom omits the browser font-loading API used by the real kit's layout observers.
// Preserve actual event delivery without pretending to measure or load fonts.
if (typeof document !== 'undefined') {
  Object.defineProperty(document, 'fonts', {
    configurable: true,
    value: Object.assign(new EventTarget(), { ready: Promise.resolve(), status: 'loaded' }),
  })
}

// jsdom has no matchMedia. Width queries answer from the test's window.innerWidth (jsdom's default is 1024);
// every other query does not match. Change events are not simulated: tests set the width before mounting.
if (typeof window !== 'undefined' && window.matchMedia === undefined) {
  Object.defineProperty(window, 'matchMedia', {
    configurable: true,
    value: (media: string): MediaQueryList => {
      const max = /^\(max-width:\s*(\d+)px\)$/.exec(media)?.[1]
      const min = /^\(min-width:\s*(\d+)px\)$/.exec(media)?.[1]
      const matches = max !== undefined ? window.innerWidth <= Number(max) : min !== undefined ? window.innerWidth >= Number(min) : false
      return Object.assign(new EventTarget(), { media, matches, onchange: null, addListener: () => {}, removeListener: () => {} }) as MediaQueryList
    },
  })
}
