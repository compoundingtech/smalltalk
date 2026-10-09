import type { TerminalColor } from '../../../../../clients/typescript/st3-client/Models.generated.ts'
import type { TerminalPalette } from './terminal-types.ts'

type Scheme = 'dark' | 'light'

// Wire colors are ANSI/xterm indices (0–255) or six-digit sRGB hex. The deliberate
// green-to-blue deviation applies equally to foregrounds and backgrounds. These
// literals mirror statusVars.diffAddTint; CSS variable strings cannot be used by
// the pure sRGB resolver (or by consumers drawing outside the DOM).
const diffAddTint = { dark: '#7295ed', light: '#4269df' } as const

// 2/10 are separate addition-blue luminance steps. 4/12 retain a separate blue
// pair with well over 20% relative-luminance separation. Cyan is sky blue rather
// than teal; the other ANSI families retain their terminal semantics.
const ansi = {
  dark: [
    '#1b2b36', '#e06c75', diffAddTint.dark, '#e5c07b',
    '#356bcc', '#c678dd', '#5195dc', '#d6dee3',
    '#5c6f7b', '#ff7b86', '#adc1f5', '#ffd68a',
    '#c0dcff', '#e09cf0', '#8bbcff', '#f3f7fa',
  ],
  light: [
    '#1b2b36', '#b42332', diffAddTint.light, '#946514',
    '#163a93', '#9345a8', '#2165ac', '#b5bec5',
    '#5c6f7b', '#d43a48', '#708ee8', '#b88118',
    '#2360b0', '#b269c2', '#5299e6', '#f3f7fa',
  ],
} as const

const hex = (channel: number): string => channel.toString(16).padStart(2, '0')
const rgbHex = (red: number, green: number, blue: number): string => `#${hex(red)}${hex(green)}${hex(blue)}`
const cubeLevel = (step: number): number => step === 0 ? 0 : 55 + step * 40

const remapGreen = (color: string, scheme: Scheme): string => {
  const red = Number.parseInt(color.slice(1, 3), 16)
  const green = Number.parseInt(color.slice(3, 5), 16)
  const blue = Number.parseInt(color.slice(5, 7), 16)
  const maximum = Math.max(red, green, blue)
  const minimum = Math.min(red, green, blue)
  const chroma = maximum - minimum
  // Match the normalized sRGB chroma exemption used by the pixel assertions.
  if (chroma / 255 < 0.01) return color
  const hue = ((maximum === red
    ? (green - blue) / chroma
    : maximum === green
      ? (blue - red) / chroma + 2
      : (red - green) / chroma + 4) * 60 + 360) % 360
  return hue >= 90 && hue <= 160 ? diffAddTint[scheme] : color
}

/** Resolve the server's ANSI16, xterm cube/grayscale, or #rrggbb color to hex.
 * Nonneutral sRGB hues in the inclusive 90–160° range become addition blue.
 * ANSI green 2/10 intentionally keep distinct luminance steps in that family.
 */
export const resolveTerminalColor = (color: TerminalColor, scheme: Scheme = 'dark'): string => {
  if (typeof color === 'string') return remapGreen(color.toLowerCase(), scheme)
  if (color < 16) return ansi[scheme][color]!
  if (color < 232) {
    const index = color - 16
    return remapGreen(rgbHex(
      cubeLevel(Math.floor(index / 36)),
      cubeLevel(Math.floor(index / 6) % 6),
      cubeLevel(index % 6),
    ), scheme)
  }
  const gray = 8 + (color - 232) * 10
  return rgbHex(gray, gray, gray)
}

// Concrete hex defaults mirror the kit's text/surface/accent/status semantic
// tokens, so a requested scheme is deterministic without a document or theme
// class. Selection is an opaque blue wash over the respective terminal surface.
const palettes: Record<Scheme, TerminalPalette> = {
  dark: {
    resolve: color => resolveTerminalColor(color, 'dark'),
    foreground: '#f5f5f5',
    background: '#111111',
    selection: '#1a2c4c',
    cursor: '#346bf1',
    error: '#fb414a',
  },
  light: {
    resolve: color => resolveTerminalColor(color, 'light'),
    foreground: '#27272a',
    background: '#ffffff',
    selection: '#dbe5fa',
    cursor: '#1b4ed8',
    error: '#dc2626',
  },
}

/** Return the shared read-only palette for the requested scheme (dark by default). */
export const createTerminalPalette = (scheme: Scheme = 'dark'): TerminalPalette => palettes[scheme]
