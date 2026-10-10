import { preload } from 'react-dom'

import latinFontUrl from './assets/JetBrainsMono-Regular.woff2?url'
import symbolsFontUrl from './assets/JetBrainsMonoNerdFontMono-Regular.ttf?url'

/** Keep emoji presentation explicit, without allowing it to own terminal text or PUA icons. */
export const TERMINAL_FONT_FAMILY =
  "'Wf Terminal Mono', 'Wf Terminal Nerd Mono', 'Apple Color Emoji', 'Segoe UI Emoji', 'Noto Color Emoji', monospace"

for (const url of [latinFontUrl, symbolsFontUrl]) {
  preload(url, {
    as: 'font',
    type: url === symbolsFontUrl ? 'font/ttf' : 'font/woff2',
    crossOrigin: 'anonymous',
  })
}

type FontState = { readonly _tag: 'Ready' } | { readonly _tag: 'Failed' }
let readiness: Promise<FontState> | undefined

/** A terminal must not measure 1ch or paint a frame while its owned faces are still loading. */
export const terminalFontsReady = (): Promise<FontState> => {
  if (readiness !== undefined) return readiness
  const faces = Array.from(document.fonts).filter(
    (face) => face.family === 'Wf Terminal Mono' || face.family === 'Wf Terminal Nerd Mono',
  )
  // Load the registered faces themselves, including styles not yet used by this frame.
  readiness = Promise.all(faces.map((face) => face.load())).then(
    (): FontState =>
      faces.some((face) => face.family === 'Wf Terminal Mono') &&
      faces.some((face) => face.family === 'Wf Terminal Nerd Mono')
        ? { _tag: 'Ready' }
        : { _tag: 'Failed' },
    (): FontState => ({ _tag: 'Failed' }),
  )
  return readiness
}
