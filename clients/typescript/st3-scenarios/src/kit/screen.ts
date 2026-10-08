import type { TerminalLine, TerminalRun, TerminalScreen } from '@smalltalk/st3-client'

import type { Asciicast } from './asciicast.ts'

interface Style {
  fg?: number
  bold?: true
  dim?: true
}

interface Cell {
  ch: string
  style: Style
}

const applySgr = (style: Style, params: string): Style => {
  let next: Style = { ...style }
  for (const raw of params === '' ? ['0'] : params.split(';')) {
    const code = Number(raw)
    if (code === 0) next = {}
    else if (code === 1) next.bold = true
    else if (code === 2) next.dim = true
    else if (code === 22) {
      delete next.bold
      delete next.dim
    } else if (code >= 30 && code <= 37) next.fg = code - 30
    else if (code >= 90 && code <= 97) next.fg = code - 90 + 8
    else if (code === 39) delete next.fg
  }
  return next
}

const sameStyle = (a: Style, b: Style) => a.fg === b.fg && a.bold === b.bold && a.dim === b.dim

const toLine = (cells: Cell[], row: number, wrapped: boolean): TerminalLine => {
  const runs: TerminalRun[] = []
  for (const cell of cells) {
    const last = runs.at(-1)
    const lastStyle: Style | undefined = last && { fg: last.fg as number | undefined, bold: last.bold, dim: last.dim }
    if (last !== undefined && lastStyle !== undefined && sameStyle(lastStyle, cell.style)) {
      last.text += cell.ch
      last.cells = (last.cells ?? 0) + 1
    } else runs.push({ text: cell.ch, cells: 1, ...cell.style })
  }
  return { row, text: cells.map((cell) => cell.ch).join(''), runs, wrapped, redacted: false, truncated: false }
}

export interface ScreenOptions {
  readonly terminalId: string
  readonly incarnation: string
}

/**
 * Line model for line-oriented casts (text, `\r\n`, `\r`, SGR): the screen after all events up to
 * `untilSeconds`. Full-screen programs come only from recordings (spec "Terminal data").
 */
export const screenAt = (cast: Asciicast, untilSeconds: number, options: ScreenOptions): TerminalScreen => {
  const { width, height } = cast.header
  const lines: Cell[][] = [[]]
  /** Rows that continue automatically onto the next row (client-v0 `wrapped`). */
  const wrapped = new Set<Cell[]>()
  let column = 0
  let style: Style = {}
  let sequence = 0
  for (const [time, , data] of cast.events) {
    if (time > untilSeconds) break
    sequence += 1
    for (let i = 0; i < data.length; i++) {
      const ch = data[i]!
      if (ch === '\u001b' && data[i + 1] === '[') {
        const end = data.slice(i + 2).search(/[A-Za-z]/)
        if (end >= 0) {
          if (data[i + 2 + end] === 'm') style = applySgr(style, data.slice(i + 2, i + 2 + end))
          i += 2 + end
          continue
        }
      }
      if (ch === '\r') column = 0
      else if (ch === '\n') {
        lines.push([])
        column = 0
      } else {
        if (column >= width) {
          wrapped.add(lines.at(-1)!)
          lines.push([])
          column = 0
        }
        lines.at(-1)![column] = { ch, style }
        column += 1
      }
    }
  }
  const visible = lines.slice(-height)
  return {
    kind: 'terminal-screen',
    terminal_id: options.terminalId,
    runtime_incarnation: options.incarnation,
    revision: `cast-${sequence}`,
    rows: height,
    columns: width,
    cursor: { row: visible.length - 1, column, visible: true, style: 'block', blinking: true },
    title: cast.header.title ?? '',
    modes: {
      alternate_screen: false,
      application_cursor: false,
      application_keypad: false,
      bracketed_paste: false,
      focus_events: false,
      mouse_tracking: 'none',
      mouse_encoding: 'default',
    },
    lines: visible.map((cells, row) => toLine(cells, row, wrapped.has(cells))),
    next_sequence: sequence,
    truncated: lines.length > height,
  }
}
