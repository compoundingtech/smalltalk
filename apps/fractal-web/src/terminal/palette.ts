// Ported from smalltalk apps/ios/terminalStyle.ts (ANSI palette, 256-colour cube, inverse/dim
// handling); extended with a design-token palette that follows the host theme.

import type { TerminalColor, TerminalRun } from '@smalltalk/st3-client/schema'
import type { CSSProperties } from 'react'

/**
 * The 16 theme colours plus default foreground/background/cursor/selection, as CSS colour values.
 * `var(--ds-*)` values follow the app theme class.
 */
export type TerminalPalette = {
  readonly id: PaletteId
  readonly ansi: ReadonlyArray<string>
  readonly foreground: string
  readonly background: string
  readonly cursor: string
  readonly selection: string
}

/** P1 follows the app theme; P2 is the fixed iOS parity palette. */
export type PaletteId = 'P1' | 'P2'

/**
 * P1: ANSI colours from the app's semantic tokens, so the terminal re-themes with the app. Normal
 * colours take the `-900` text step and bright ones the `-1000` high-contrast step; both are
 * readable on `--ds-background-100` in either scheme. Black/white map onto the gray ramp, which
 * inverts with the scheme the same way terminal light/dark themes do.
 */
export const defaultTerminalPalette: TerminalPalette = {
  id: 'P1',
  ansi: [
    'var(--ds-gray-400)',
    'var(--ds-red-900)',
    'var(--ds-green-900)',
    'var(--ds-amber-900)',
    'var(--ds-blue-900)',
    'var(--ds-purple-900)',
    'var(--ds-teal-900)',
    'var(--ds-gray-1000)',
    'var(--ds-gray-700)',
    'var(--ds-red-1000)',
    'var(--ds-green-1000)',
    'var(--ds-amber-1000)',
    'var(--ds-blue-1000)',
    'var(--ds-purple-1000)',
    'var(--ds-teal-1000)',
    'var(--ds-gray-1000)',
  ],
  foreground: 'var(--ds-gray-1000)',
  background: 'var(--ds-background-100)',
  cursor: 'var(--ds-gray-1000)',
  selection: 'var(--ds-blue-500)',
}

/** P2: the iOS app's fixed dark palette, unchanged (a parity reference; does not re-theme). */
export const iosPalette: TerminalPalette = {
  id: 'P2',
  ansi: [
    '#1b2b36',
    '#e06c75',
    '#98c379',
    '#e5c07b',
    '#61afef',
    '#c678dd',
    '#56b6c2',
    '#d6dee3',
    '#5c6f7b',
    '#ff7b86',
    '#b5e890',
    '#ffd68a',
    '#82c4ff',
    '#e09cf0',
    '#7fd6e0',
    '#f3f7fa',
  ],
  foreground: '#d6dee3',
  background: '#0d1820',
  cursor: '#f3f7fa',
  selection: '#2f4b5e',
}

/** Every terminal palette by id. */
export const palettes: Record<PaletteId, TerminalPalette> = { P1: defaultTerminalPalette, P2: iosPalette }

const hex = (value: number) => value.toString(16).padStart(2, '0')

/** xterm colour-cube channel intensity for cube step 0-5. */
const cubeLevel = (step: number) => (step === 0 ? 0 : 55 + step * 40)

/** 16-231 are the xterm colour cube and 232-255 its gray ramp — identical to xterm.js's table. */
export const terminalColor = ({
  color,
  palette,
}: {
  readonly color: TerminalColor
  readonly palette: TerminalPalette
}): string => {
  if (typeof color === 'string') return color
  if (color < 16) return palette.ansi[color] ?? palette.foreground
  if (color < 232) {
    const index = color - 16
    return `#${hex(cubeLevel(Math.floor(index / 36)))}${hex(cubeLevel(Math.floor(index / 6) % 6))}${hex(cubeLevel(index % 6))}`
  }
  const gray = 8 + (color - 232) * 10
  return `#${hex(gray)}${hex(gray)}${hex(gray)}`
}

/**
 * Inline style for one run. Run colours are data (256 palette entries plus arbitrary truecolor), so
 * they are inline styles rather than StyleX rules.
 *
 * Deviation from iOS: dim mixes the foreground toward the background instead of `opacity: 0.6`,
 * which also faded the run's background (visible on dim text over a diff highlight).
 */
export const terminalRunStyle = ({
  run,
  palette,
}: {
  readonly run: TerminalRun
  readonly palette: TerminalPalette
}): CSSProperties | undefined => {
  if (
    run.fg === undefined &&
    run.bg === undefined &&
    !run.inverse &&
    !run.bold &&
    !run.dim &&
    !run.italic &&
    !run.underline
  ) {
    return undefined
  }
  let color = run.fg === undefined ? palette.foreground : terminalColor({ color: run.fg, palette })
  let background = run.bg === undefined ? undefined : terminalColor({ color: run.bg, palette })
  if (run.inverse) [color, background] = [background ?? palette.background, color]
  const style: CSSProperties = {
    color: run.dim
      ? `color-mix(in srgb, ${color} 55%, ${background ?? palette.background})`
      : color,
  }
  if (background !== undefined) style.backgroundColor = background
  if (run.bold) style.fontWeight = 700
  if (run.italic) style.fontStyle = 'italic'
  if (run.underline) style.textDecorationLine = 'underline'
  return style
}
