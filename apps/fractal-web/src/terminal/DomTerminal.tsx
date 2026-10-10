/**
 * T1: TerminalScreen rendered straight to DOM — no emulator. One row element per line, one span per
 * run, rows memoized on content so a 10 Hz spinner frame re-renders only the rows that changed.
 * Text stays real DOM text: native selection/copy, find-in-page and screen readers read it as is.
 */

import type { TerminalLine, TerminalRun, TerminalScreen } from '@smalltalk/st3-client/schema'
import * as stylex from '@stylexjs/stylex'
import { memo, Suspense, use, type CSSProperties, type ReactNode } from 'react'

// The renderer owns its font so every host uses the same text and box-drawing cell advances.
// oxlint-disable-next-line import/no-unassigned-import -- CSS side-effect import
import './fonts.css'
import { tokens } from '../ui-compat/tokens.stylex.ts'

import { graphemes, graphemeWidth, isPlainAscii, textWidth } from './cellWidth.ts'
import { terminalRunStyle, type TerminalPalette } from './palette.ts'
import { TERMINAL_FONT_FAMILY, terminalFontsReady } from './terminalFonts.ts'

/** Shared cell font size (px) for every renderer, so the bake-off compares like with like. */
export const TERMINAL_FONT_SIZE = 13
/** Shared row height (px); rows are positioned on this grid. */
export const TERMINAL_LINE_HEIGHT = 17
/** Inputs of the DOM renderer: the screen frame and the palette its colours resolve against. */
export type DomTerminalProps = {
  readonly screen: TerminalScreen
  readonly palette: TerminalPalette
}

/** Keep loading visible instead of font-display:block turning a connected terminal all black. */
export const DomTerminal = (props: DomTerminalProps) => (
  <Suspense fallback={<p role="status">Loading terminal fonts…</p>}>
    <LoadedTerminal {...props} />
  </Suspense>
)

/** Terminal output is untrusted. Never execute script/data URLs or resolve local relative paths. */
export const terminalHyperlink = (uri: string | undefined): string | undefined => {
  // Reject control characters without a control-regex: terminal output is untrusted input.
  if (uri === undefined || [...uri].some((char) => char <= ' ' || char === '\u007f'))
    return undefined
  try {
    const url = new URL(uri)
    return ['https:', 'http:', 'mailto:'].includes(url.protocol) ? url.href : undefined
  } catch {
    return undefined
  }
}

/** One terminal grid row; runs tile columns and may wrap. */
export const TerminalLineView = memo(
  ({ line, palette }: LineProps) => {
    // Runs tile the row from column zero, so a run's character offset identifies it within the row.
    let start = 0
    return (
      <div
        data-terminal-row
        data-wrapped={line.wrapped === true ? 'true' : 'false'}
        {...stylex.props(styles.line)}
      >
        {line.redacted ? (
          <span {...stylex.props(styles.redacted)}>[redacted]</span>
        ) : (
          line.runs.map((run) => {
            const key = start
            start += run.text.length
            const uri = terminalHyperlink(run.link?.uri)
            const props = {
              ...stylex.props(styles.run),
              style: {
                color: 'inherit',
                ...terminalRunStyle({ run, palette }),
                width: `${run.cells ?? textWidth(run.text)}ch`,
                textDecorationLine:
                  [run.underline ? 'underline' : '', run.strikethrough ? 'line-through' : '']
                    .filter(Boolean)
                    .join(' ') || 'none',
              },
            }
            return uri === undefined ? (
              <span key={key} {...props}>
                <RunText text={run.text} />
              </span>
            ) : (
              <a
                key={key}
                {...props}
                href={uri}
                target="_blank"
                rel="noopener noreferrer"
                aria-label={`${run.text} (${uri})`}
              >
                <RunText text={run.text} />
              </a>
            )
          })
        )}
        {line.truncated ? (
          <span {...stylex.props(styles.truncatedMark)} aria-label="line truncated">
            …
          </span>
        ) : null}
      </div>
    )
  },
  (previous, next) =>
    previous.palette === next.palette &&
    previous.line.redacted === next.line.redacted &&
    previous.line.truncated === next.line.truncated &&
    previous.line.wrapped === next.line.wrapped &&
    previous.line.runs.length === next.line.runs.length &&
    previous.line.runs.every((run, index) => {
      const other = next.line.runs[index]
      return other !== undefined && sameRun({ a: run, b: other })
    }),
)

const blink = stylex.keyframes({
  '0%, 49%': { opacity: 1 },
  '50%, 100%': { opacity: 0 },
})

const styles = stylex.create({
  root: {
    position: 'relative',
    display: 'inline-block',
    fontSize: `${TERMINAL_FONT_SIZE}px`,
    lineHeight: `${TERMINAL_LINE_HEIGHT}px`,
    fontVariantLigatures: 'none',
    fontKerning: 'none',
    fontFeatureSettings: '"liga" 0, "calt" 0',
    tabSize: 8,
    outline: {
      default: 'none',
      ':focus-visible': `2px solid ${tokens['--ds-focus-color']}`,
    },
  },
  line: {
    height: `${TERMINAL_LINE_HEIGHT}px`,
    whiteSpace: 'pre',
    overflow: 'hidden',
  },
  run: {
    display: 'inline-block',
    height: `${TERMINAL_LINE_HEIGHT}px`,
    verticalAlign: 'top',
    overflow: 'hidden',
  },
  cell: {
    display: 'inline-block',
    // Fallback fonts have taller ascents; a fixed box keeps them from growing the row's line box
    // and pushing the row's plain text below the clipped 1-row height.
    height: `${TERMINAL_LINE_HEIGHT}px`,
    lineHeight: `${TERMINAL_LINE_HEIGHT}px`,
    textAlign: 'center',
    overflow: 'hidden',
    verticalAlign: 'top',
  },
  redacted: {
    fontStyle: 'italic',
    color: tokens['--ds-gray-700'],
  },
  truncatedMark: {
    color: tokens['--ds-gray-700'],
  },
  cursor: {
    position: 'absolute',
    pointerEvents: 'none',
    userSelect: 'none',
    width: '1ch',
    height: `${TERMINAL_LINE_HEIGHT}px`,
  },
  cursorBlock: {
    mixBlendMode: 'difference',
    backgroundColor: 'white',
  },
  cursorUnderline: {
    borderBottomWidth: '2px',
    borderBottomStyle: 'solid',
  },
  cursorBar: {
    borderLeftWidth: '2px',
    borderLeftStyle: 'solid',
  },
  blinking: {
    animationName: blink,
    animationDuration: '1s',
    animationIterationCount: 'infinite',
    animationTimingFunction: 'steps(1)',
  },
})

const LoadedTerminal = ({ screen, palette }: DomTerminalProps) => {
  const fonts = use(terminalFontsReady())
  if (fonts._tag === 'Failed') {
    return (
      <p role="alert">
        Terminal fonts could not be loaded. The session is still running. Reload this page to try
        again.
      </p>
    )
  }
  return (
    <div
      role="region"
      aria-roledescription="terminal"
      aria-label={`Terminal: ${screen.title}`}
      // Focusable so keyboard users can reach (and later type into) the pane.
      tabIndex={0}
      data-terminal-renderer="T1"
      {...stylex.props(styles.root)}
      style={{
        fontFamily: TERMINAL_FONT_FAMILY,
        width: `${screen.columns}ch`,
        height: screen.rows * TERMINAL_LINE_HEIGHT,
        color: palette.foreground,
        backgroundColor: palette.background,
      }}
    >
      {screen.lines.map((line) => (
        <TerminalLineView key={line.row} line={line} palette={palette} />
      ))}
      <Cursor cursor={screen.cursor} palette={palette} />
    </div>
  )
}

/**
 * JetBrains Mono owns terminal text, box drawing and blocks. Other symbols, braille and emoji
 * can come from fallback faces with different advances; box them to their libghostty cell width
 * so subsequent labels and the cursor stay on the same grid.
 */
const needsCellBox = ({
  grapheme,
  width,
}: {
  readonly grapheme: string
  readonly width: number
}) => {
  if (width !== 1) return true
  const cp = grapheme.codePointAt(0) ?? 0
  return cp >= 0x80 && !(cp >= 0x2500 && cp <= 0x259f)
}

const pictographic = /^\p{Extended_Pictographic}/u

const RunText = ({ text }: { readonly text: string }): ReactNode => {
  if (isPlainAscii(text)) return text
  const parts: Array<ReactNode> = []
  let flowing = ''
  for (const grapheme of graphemes(text)) {
    const width = graphemeWidth(grapheme)
    if (!needsCellBox({ grapheme, width })) {
      flowing += grapheme
      continue
    }
    if (flowing.length > 0) parts.push(flowing)
    flowing = ''
    parts.push(
      <span key={parts.length} {...stylex.props(styles.cell)} style={{ width: `${width}ch` }}>
        {width === 1 && pictographic.test(grapheme) ? (
          // Color emoji advance a full em, including text-default symbols such as ⚠.
          // Fit those native one-cell glyphs inside the base font's narrower advance.
          <span
            style={{
              display: 'inline-block',
              transform: 'scaleX(0.6)',
              marginInline: '-0.2em',
            }}
          >
            {grapheme}
          </span>
        ) : (
          grapheme
        )}
      </span>,
    )
  }
  if (flowing.length > 0) parts.push(flowing)
  return parts
}

const sameRun = ({ a, b }: { readonly a: TerminalRun; readonly b: TerminalRun }) =>
  a.text === b.text &&
  a.cells === b.cells &&
  a.fg === b.fg &&
  a.bg === b.bg &&
  a.bold === b.bold &&
  a.dim === b.dim &&
  a.italic === b.italic &&
  a.underline === b.underline &&
  a.inverse === b.inverse &&
  a.strikethrough === b.strikethrough &&
  a.link?.uri === b.link?.uri

type LineProps = { readonly line: TerminalLine; readonly palette: TerminalPalette }

const Cursor = ({
  cursor,
  palette,
}: {
  readonly cursor: TerminalScreen['cursor']
  readonly palette: TerminalPalette
}) => {
  if (!cursor.visible) return null
  const position: CSSProperties = {
    top: cursor.row * TERMINAL_LINE_HEIGHT,
    left: `${cursor.column}ch`,
    borderColor: palette.cursor,
  }
  return (
    <div
      aria-hidden
      data-terminal-cursor
      style={position}
      {...stylex.props(
        styles.cursor,
        cursor.style === 'block' && styles.cursorBlock,
        cursor.style === 'underline' && styles.cursorUnderline,
        cursor.style === 'bar' && styles.cursorBar,
        cursor.blinking && styles.blinking,
      )}
    />
  )
}
