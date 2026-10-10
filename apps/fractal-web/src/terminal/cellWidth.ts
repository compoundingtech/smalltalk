/**
 * Native-compatible terminal cell advances for text-only fixtures and legacy runs without cells.
 *
 * Live `TerminalScreen` runs carry libghostty's authoritative `cells`. Do not replace that value
 * with browser grapheme/emoji presentation rules: the native decoder can retain a flag or ZWJ
 * sequence as multiple wide cells, and VS16 does not necessarily widen its preceding text cell.
 */

const segmenter = new Intl.Segmenter(undefined, { granularity: 'grapheme' })

const emojiPresentation = /\p{Emoji_Presentation}/u
const zeroWidth = /[\p{Mark}\p{Cf}]/u

const isWideCodePoint = (cp: number): boolean =>
  (cp >= 0x1100 && cp <= 0x115f) ||
  (cp >= 0x2e80 && cp <= 0x303e) ||
  (cp >= 0x3041 && cp <= 0x33ff) ||
  (cp >= 0x3400 && cp <= 0x4dbf) ||
  (cp >= 0x4e00 && cp <= 0x9fff) ||
  (cp >= 0xa000 && cp <= 0xa4cf) ||
  (cp >= 0xac00 && cp <= 0xd7a3) ||
  (cp >= 0xf900 && cp <= 0xfaff) ||
  (cp >= 0xfe30 && cp <= 0xfe4f) ||
  (cp >= 0xff00 && cp <= 0xff60) ||
  (cp >= 0xffe0 && cp <= 0xffe6) ||
  (cp >= 0x20000 && cp <= 0x3fffd)

/** Native cell count, not the browser's shaped grapheme advance (a flag can occupy four cells). */
export const graphemeWidth = (grapheme: string): number => {
  let cells = 0
  for (const character of grapheme) {
    const cp = character.codePointAt(0) ?? 0
    if (zeroWidth.test(character) || (cp >= 0x1160 && cp <= 0x11ff)) continue
    cells += emojiPresentation.test(character) || isWideCodePoint(cp) ? 2 : 1
  }
  return cells
}

/** Splits text into grapheme clusters (user-perceived characters). */
export const graphemes = (text: string): ReadonlyArray<string> =>
  Array.from(segmenter.segment(text), (segment) => segment.segment)

/** Only ASCII is guaranteed one cell per UTF-16 unit; everything else needs segmentation. */
export const isPlainAscii = (text: string): boolean => /^[\x20-\x7e]*$/.test(text)

/** Cells `text` occupies on a terminal row. */
export const textWidth = (text: string): number => {
  if (isPlainAscii(text)) return text.length
  let width = 0
  for (const grapheme of graphemes(text)) width += graphemeWidth(grapheme)
  return width
}
