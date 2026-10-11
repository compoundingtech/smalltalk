/**
 * Deterministic `TerminalScreen` frame generator for the renderer bake-off.
 *
 * Scenes reproduce what st republishes from real agent PTYs: Claude Code and Codex TUIs mid-turn
 * (truecolor + 256-colour + ANSI, box drawing, an animated 10 Hz spinner with shimmer, streaming
 * text, a drawn inverse cursor vs a real terminal cursor), a Unicode/width probe, and a worst-case
 * frame that restyles every cell. Frames are pure functions of `(scene, frame, size)`.
 */

import {
  decodeUnknownSync,
  TerminalScreen,
  type TerminalLine,
  type TerminalModes,
  type TerminalRun,
} from '@smalltalk/st3-client/schema'

import {
  agents,
  agentByRef,
  decisionD12,
  hosts,
  missions,
  operator,
  pullRequests,
  ciRuns,
} from '../fixtures/world.ts'
import { graphemes, graphemeWidth, textWidth } from './cellWidth.ts'

type Style = Omit<TerminalRun, 'text'>
/** A segment is a run before clipping and merging: its text plus its style attributes. */
type Seg = TerminalRun
type Row = ReadonlyArray<Seg>

/** Synthetic renderer-fidelity scenes. */
export type SceneId = 'F1' | 'F2' | 'F3' | 'F4'

/** Every synthetic scene with what it exercises. */
export const scenes: ReadonlyArray<{ readonly id: SceneId; readonly description: string }> = [
  {
    id: 'F1',
    description:
      'Claude Code mid-turn: truecolor, diff, ✻ spinner + shimmer, streaming text, drawn cursor',
  },
  {
    id: 'F2',
    description: 'Codex mid-turn: ANSI/256 colours, tree glyphs, gradient shimmer, real bar cursor',
  },
  {
    id: 'F3',
    description: 'Unicode probe: CJK, emoji, combining, box/block/braille, 256 + truecolor strips',
  },
  { id: 'F4', description: 'Worst case: every cell restyled every frame (12k runs at 200×60)' },
]

/** Bake-off terminal geometries. */
export type SizeId = 'S1' | 'S2' | 'S3'

/** Every bake-off geometry in cells. */
export const sizes: ReadonlyArray<{
  readonly id: SizeId
  readonly columns: number
  readonly rows: number
  readonly description: string
}> = [
  { id: 'S1', columns: 80, rows: 24, description: '80×24 classic' },
  { id: 'S2', columns: 120, rows: 40, description: '120×40 laptop pane' },
  { id: 'S3', columns: 200, rows: 60, description: '200×60 wide monitor (perf target)' },
]

/** An unstyled segment. */
const s = (text: string): Seg => ({ text })

const CLAUDE_ORANGE = '#d77757'
const CLAUDE_SHIMMER = '#f2b29a'
const CLAUDE_GREEN = '#4eba65'
const GRAY = 245
const DIFF_ADD_BG = '#103a1c'
const DIFF_DEL_BG = '#4a1418'

const blank: Row = []

const widthOf = (row: Row) => row.reduce((total, seg) => total + textWidth(seg.text), 0)

const padRow = ({
  row,
  width,
  style,
}: {
  readonly row: Row
  readonly width: number
  readonly style?: Style
}): Row => {
  const missing = width - widthOf(row)
  return missing > 0 ? [...row, { text: ' '.repeat(missing), ...style }] : row
}

/** Left and right content on one row, the gap filled with spaces (right-aligned status lines). */
const spread = ({
  left,
  right,
  width,
}: {
  readonly left: Row
  readonly right: Row
  readonly width: number
}): Row => {
  const gap = width - widthOf(left) - widthOf(right)
  return gap > 0 ? [...left, s(' '.repeat(gap)), ...right] : left
}

const box = ({
  content,
  width,
  border,
  rounded = true,
}: {
  readonly content: ReadonlyArray<Row>
  readonly width: number
  readonly border: Style
  readonly rounded?: boolean
}): Array<Row> => {
  const [tl, tr, bl, br] = rounded ? ['╭', '╮', '╰', '╯'] : ['┌', '┐', '└', '┘']
  const inner = Math.max(0, width - 2)
  return [
    [{ text: `${tl}${'─'.repeat(inner)}${tr}`, ...border }],
    ...content.map((row) => [
      { text: '│', ...border },
      ...padRow({ row, width: inner }),
      { text: '│', ...border },
    ]),
    [{ text: `${bl}${'─'.repeat(inner)}${br}`, ...border }],
  ]
}

const wrap = ({
  text,
  width,
}: {
  readonly text: string
  readonly width: number
}): Array<string> => {
  const lines: Array<string> = []
  let current = ''
  for (const word of text.split(' ')) {
    if (current.length > 0 && textWidth(current) + 1 + textWidth(word) > width) {
      lines.push(current)
      current = word
    } else {
      current = current.length > 0 ? `${current} ${word}` : word
    }
  }
  if (current.length > 0) lines.push(current)
  return lines
}

/** Color one word with a moving highlight window, like both agents' "working" shimmer. */
const shimmer = ({
  word,
  frame,
  base,
  highlight,
}: {
  readonly word: string
  readonly frame: number
  readonly base: Style
  readonly highlight: Style
}): Row => {
  const chars = graphemes(word)
  const head = frame % (chars.length + 6)
  return chars.map(
    (char, index): Seg =>
      Object.assign({ text: char }, Math.abs(index - head) <= 1 ? highlight : base),
  )
}

const formatTokens = (tokens: number) =>
  tokens >= 1000 ? `${(tokens / 1000).toFixed(1)}k` : `${tokens}`

const CLAUDE_SPINNER = ['·', '✢', '✳', '✶', '✻', '✽', '✽', '✻', '✶', '✳', '✢', '·']

const CLAUDE_STREAM =
  'The collections socket already delivers whole TerminalScreen frames at most every 100 ms, so the pane never ' +
  'has to emulate a terminal: each frame replaces the last, and the renderer only diffs rows. I kept the ' +
  'incarnation next to the screen so input can be fenced exactly the way the iOS client does it, and a stale ' +
  'fence ends the subscription instead of redirecting keystrokes into a restarted process.'

/** Geometry and clock one scene frame is rendered for. */
type SceneInput = { readonly frame: number; readonly columns: number; readonly rows: number }

type SceneFrame = {
  readonly rows: ReadonlyArray<Row>
  readonly cursor: TerminalScreen['cursor']
  readonly title: string
  readonly modes?: Partial<TerminalModes>
}

const hiddenCursor: TerminalScreen['cursor'] = {
  row: 0,
  column: 0,
  visible: false,
  blinking: false,
  style: 'block',
}

const claudeScene = ({ frame, columns, rows }: SceneInput): SceneFrame => {
  const gray: Style = { fg: GRAY }
  const orange: Style = { fg: CLAUDE_ORANGE }
  const header = box({
    content: [
      [
        { text: ' ✻ ', ...orange },
        { text: 'Welcome to Claude Code!', bold: true },
      ],
      blank,
      [{ text: '   /help for help, /status for your current setup', fg: GRAY, italic: true }],
      blank,
      [{ text: '   cwd: /srv/work/webfractal', ...gray }],
    ],
    width: Math.min(columns, 58),
    border: orange,
  })
  const textWidthBudget = columns - 4
  const streamed = CLAUDE_STREAM.slice(0, Math.min(CLAUDE_STREAM.length, 24 + frame * 5))
  const running = frame % 10 < 5
  const conversation: Array<Row> = [
    ...header,
    blank,
    padRow({
      row: [
        { text: '> ', fg: 250, bg: 236 },
        {
          text: 'wire the terminal pane to the collections socket and keep the cursor in sync',
          fg: 250,
          bg: 236,
        },
      ],
      width: columns,
      style: { bg: 236 },
    }),
    blank,
    [s('⏺ '), s('I’ll look at how the iOS client follows terminals first.')],
    blank,
    [{ text: '⏺ ', fg: CLAUDE_GREEN }, { text: 'Read', bold: true }, s('(apps/ios/feed.ts)')],
    [
      { text: '  ⎿  ', ...gray },
      { text: 'Read ', ...gray },
      { text: '412', fg: GRAY, bold: true },
      { text: ' lines (ctrl+r to expand)', ...gray },
    ],
    blank,
    [
      { text: '⏺ ', fg: CLAUDE_GREEN },
      { text: 'Search', bold: true },
      s('(pattern: "withFreshTerminalFence", path: "apps/ios")'),
    ],
    [
      { text: '  ⎿  ', ...gray },
      { text: 'Found ', ...gray },
      { text: '2', fg: GRAY, bold: true },
      { text: ' files (ctrl+r to expand)', ...gray },
    ],
    blank,
    [
      { text: '⏺ ', fg: CLAUDE_GREEN },
      { text: 'Update', bold: true },
      s('(src/terminal/DomTerminal.tsx)'),
    ],
    [
      { text: '  ⎿  ', ...gray },
      { text: 'Updated ', ...gray },
      { text: 'src/terminal/DomTerminal.tsx', fg: GRAY, bold: true },
      { text: ' with 3 additions and 1 removal', ...gray },
    ],
    padRow({
      row: [{ text: '       41  ', ...gray }, s('    const line = screen.lines[row]')],
      width: columns,
    }),
    padRow({
      row: [
        { text: '       42 ', ...gray },
        { text: '-', fg: 1, bg: DIFF_DEL_BG },
        { text: '    return <div>{line.text}</div>', bg: DIFF_DEL_BG },
      ],
      width: columns,
      style: { bg: DIFF_DEL_BG },
    }),
    padRow({
      row: [
        { text: '       42 ', ...gray },
        { text: '+', fg: 2, bg: DIFF_ADD_BG },
        { text: '    return (', bg: DIFF_ADD_BG },
      ],
      width: columns,
      style: { bg: DIFF_ADD_BG },
    }),
    padRow({
      row: [
        { text: '       43 ', ...gray },
        { text: '+', fg: 2, bg: DIFF_ADD_BG },
        { text: '      <TerminalRow line={line} palette={palette} />', bg: DIFF_ADD_BG },
      ],
      width: columns,
      style: { bg: DIFF_ADD_BG },
    }),
    padRow({
      row: [
        { text: '       44 ', ...gray },
        { text: '+', fg: 2, bg: DIFF_ADD_BG },
        { text: '    )', bg: DIFF_ADD_BG },
      ],
      width: columns,
      style: { bg: DIFF_ADD_BG },
    }),
    blank,
    [
      { text: '⏺ ', ...(running ? gray : { fg: GRAY, dim: true }) },
      { text: 'Bash', bold: true },
      s('(pnpm tsc -p tsconfig.json)'),
    ],
    [
      { text: '  ⎿  ', ...gray },
      { text: 'Running…', ...gray },
    ],
    blank,
    ...wrap({ text: streamed, width: textWidthBudget }).map((line, index) => [
      s(index === 0 ? '⏺ ' : '  '),
      s(line),
    ]),
  ]
  const spinner = CLAUDE_SPINNER[frame % CLAUDE_SPINNER.length] ?? '✻'
  const elapsed = 14 + Math.floor(frame / 10)
  const status: Row = [
    { text: `${spinner} `, ...orange },
    ...shimmer({ word: 'Brewing…', frame, base: orange, highlight: { fg: CLAUDE_SHIMMER } }),
    { text: ` (${elapsed}s · `, ...gray },
    { text: '↓', ...gray },
    { text: ` ${formatTokens(1300 + frame * 7)} tokens · `, ...gray },
    { text: 'esc', fg: GRAY, bold: true },
    { text: ' to interrupt)', ...gray },
  ]
  const input = box({
    content: [
      [
        { text: ' > ', fg: 250 },
        { text: ' ', inverse: true },
      ],
    ],
    width: columns,
    border: { fg: 244 },
  })
  const footer = spread({
    left: [{ text: '  ? for shortcuts', dim: true }],
    right: [{ text: '⧉ In DomTerminal.tsx ', fg: GRAY }],
    width: columns,
  })
  const bottom: Array<Row> = [blank, status, blank, ...input, footer]
  const room = Math.max(0, rows - bottom.length)
  const visible = [
    ...conversation.slice(Math.max(0, conversation.length - room)),
    ...Array.from({ length: Math.max(0, room - conversation.length) }, () => blank),
  ]
  return {
    rows: [...visible, ...bottom],
    // Claude Code hides the terminal cursor and paints an inverse cell in its input box.
    cursor: { ...hiddenCursor, row: rows - 3, column: 4 },
    title: '✳ Wire terminal pane',
  }
}

const CODEX_SPINNER_BULLETS = ['•', '•', '•', '◦', '◦', '◦']

const codexScene = ({ frame, columns, rows }: SceneInput): SceneFrame => {
  const dim: Style = { dim: true }
  const header = box({
    content: [
      [
        { text: ' >_ ', dim: true },
        { text: 'OpenAI Codex', bold: true },
        { text: ' (v0.61.0)', ...dim },
      ],
      blank,
      [
        { text: ' model:     ', ...dim },
        s('gpt-5-codex high'),
        { text: '   /model', fg: 6 },
        { text: ' to change', ...dim },
      ],
      [{ text: ' directory: ', ...dim }, s('/srv/work/webfractal')],
    ],
    width: Math.min(columns, 54),
    border: dim,
  })
  const userBg = '#262626'
  const elapsed = 12 + Math.floor(frame / 10)
  const gradient = [
    '#5f5f5f',
    '#8a8a8a',
    '#b5b5b5',
    '#e0e0e0',
    '#ffffff',
    '#e0e0e0',
    '#b5b5b5',
    '#8a8a8a',
  ]
  const working = graphemes('Working').map(
    (char, index): Seg => ({
      text: char,
      fg: gradient[(index - frame + 64) % gradient.length] ?? '#ffffff',
      bold: true,
    }),
  )
  const conversation: Array<Row> = [
    ...header,
    blank,
    padRow({
      row: [
        { text: '› ', bold: true, bg: userBg },
        { text: 'add a 10Hz spinner fixture to the terminal bake-off', bg: userBg },
      ],
      width: columns,
      style: { bg: userBg },
    }),
    blank,
    [
      s('• '),
      s('I’ll add a frame generator next to the renderers, then wire it into the stories.'),
    ],
    blank,
    [
      { text: '• ', fg: 2 },
      { text: 'Explored', bold: true },
    ],
    [
      { text: '  └ ', ...dim },
      { text: 'Read ', fg: 6 },
      s('screen.ts, terminalStyle.ts, terminalControls.ts'),
    ],
    [
      s('    '),
      { text: 'Search ', fg: 6 },
      s('TerminalScreen'),
      { text: ' in ', ...dim },
      s('clients/typescript'),
    ],
    blank,
    [
      { text: '• ', fg: 2 },
      { text: 'Ran ', bold: true },
      { text: 'pnpm vitest run src/terminal', fg: 4 },
    ],
    [
      { text: '  └ ', ...dim },
      { text: ' ✓ ', fg: 2 },
      { text: 'src/terminal/repaint.test.ts ', ...dim },
      { text: '(12 tests)', fg: 3 },
      { text: ' 41ms', ...dim },
    ],
    [
      s('     '),
      { text: 'Test Files  ', ...dim },
      { text: '1 passed', fg: 2, bold: true },
      { text: ' (1)', ...dim },
    ],
    blank,
    [
      { text: '• ', fg: 2 },
      { text: 'Edited ', bold: true },
      s('src/terminal/fixtures.ts '),
      { text: '(', ...dim },
      { text: '+84', fg: 2 },
      { text: ' ', ...dim },
      { text: '-3', fg: 1 },
      { text: ')', ...dim },
    ],
    [
      { text: '    120 ', ...dim },
      { text: '+', fg: 2 },
      { text: "export const spinner = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧']", fg: 2 },
    ],
    [
      { text: '    121 ', ...dim },
      { text: '-', fg: 1 },
      { text: 'const spinner = "|/-\\\\"', fg: 1 },
    ],
    [{ text: '    122 ', ...dim }, s(' '), s('const frames = sceneFrames(scene, size)')],
  ]
  const bullet = CODEX_SPINNER_BULLETS[frame % CODEX_SPINNER_BULLETS.length] ?? '•'
  const status: Row = [
    { text: `${bullet} `, dim: true },
    ...working,
    { text: ` (${elapsed}s • `, ...dim },
    { text: 'esc', bold: true, dim: true },
    { text: ' to interrupt)', ...dim },
  ]
  const composer = padRow({
    row: [
      { text: '› ', bold: true, bg: userBg },
      { text: 'Ask Codex to do anything', dim: true, bg: userBg },
    ],
    width: columns,
    style: { bg: userBg },
  })
  const footer = spread({
    left: [],
    right: [{ text: `${100 - Math.floor(frame / 40)}% context left · ? for shortcuts `, ...dim }],
    width: columns,
  })
  const bottom: Array<Row> = [blank, status, blank, composer, blank, footer]
  const room = Math.max(0, rows - bottom.length)
  const visible = [
    ...conversation.slice(Math.max(0, conversation.length - room)),
    ...Array.from({ length: Math.max(0, room - conversation.length) }, () => blank),
  ]
  return {
    rows: [...visible, ...bottom],
    // Codex keeps the real terminal cursor in its composer.
    cursor: { row: rows - 3, column: 2, visible: true, blinking: true, style: 'bar' },
    title: 'codex',
    modes: { bracketed_paste: true, focus_events: true },
  }
}

const BRAILLE = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏']

const unicodeScene = ({ frame, columns, rows }: SceneInput): SceneFrame => {
  const label = (text: string): Seg => ({ text: text.padEnd(12), fg: GRAY })
  const probeWidth = Math.min(columns, 64)
  const probe = box({
    content: [
      [{ text: ' Right borders must line up (cells as libghostty counts them)', bold: true }],
      [label(' CJK'), s('日本語のテキスト｜한국어｜中文')],
      [label(' Emoji'), s('🚀 ✅ ⚠️ 👍🏽 👩‍💻 🇩🇪 ❤️')],
      [label(' Combining'), s('é (e + ◌́)  ñ  Å  ǖ  ส้ม')],
      [label(' Box'), s('┌─┬─┐ ╔═╦═╗ ╭─╮ ┏━┳━┓ ╟─╫─╢')],
      [label(' Blocks'), s('▁▂▃▄▅▆▇█ ░▒▓ ▖▗▘▙▚▛▜▝▞▟')],
      [
        label(' Braille'),
        s(`${BRAILLE[frame % BRAILLE.length] ?? '⠋'} loading `),
        { text: '⣿⣷⣯⣟⡿⢿⣻⣽⣾', fg: 4 },
      ],
      [
        label(' Nerd/PUA'),
        { text: '\ue0b0 \ue0b2 \uf418 \uf113 (bundled Nerd Font symbols)', fg: GRAY },
      ],
      [
        label(' Attributes'),
        { text: 'bold', bold: true },
        s(' '),
        { text: 'dim', dim: true },
        s(' '),
        { text: 'italic', italic: true },
        s(' '),
        { text: 'under', underline: true },
        s(' '),
        { text: 'inverse', inverse: true },
      ],
    ],
    width: probeWidth,
    border: { fg: 4 },
    rounded: false,
  })
  const ansi: Row = Array.from(
    { length: 16 },
    (_, index): Seg => ({
      text: index.toString(16).padStart(2).padEnd(3),
      bg: index,
      fg: index === 0 ? 15 : 0,
    }),
  )
  const cube = Array.from(
    { length: 6 },
    (_, block): Row => [
      { text: block === 0 ? ' 256 cube ' : '          ', fg: GRAY },
      ...Array.from(
        { length: 36 },
        (_cell, index): Seg => ({ text: ' ', bg: 16 + block * 36 + index }),
      ),
    ],
  )
  const grays: Row = Array.from(
    { length: 24 },
    (_, index): Seg => ({ text: '  ', bg: 232 + index }),
  )
  const gradientWidth = Math.min(columns, 96)
  const truecolor: Row = Array.from({ length: gradientWidth }, (_, index) => {
    const hue = ((index / gradientWidth) * 360 + frame * 6) % 360
    const [r, g, b] = hslToRgb({ h: hue, sat: 0.7, light: 0.55 })
    return {
      text: '▀',
      fg: rgbHex([r, g, b]),
      bg: rgbHex([Math.round(r * 0.4), Math.round(g * 0.4), Math.round(b * 0.4)]),
    }
  })
  const content: Array<Row> = [
    [
      { text: ' Width & colour probe ', bold: true, inverse: true },
      { text: `  frame ${frame}`, fg: GRAY },
    ],
    blank,
    ...probe,
    blank,
    [{ text: ' 16 ANSI  ', fg: GRAY }, ...ansi],
    ...cube,
    [{ text: ' grays    ', fg: GRAY }, ...grays],
    [{ text: ' truecolor', fg: GRAY }],
    [s(' '), ...truecolor],
  ]
  return {
    rows: content.slice(0, rows),
    cursor: {
      row: Math.min(rows - 1, content.length),
      column: 1,
      visible: true,
      blinking: true,
      style: 'block',
    },
    title: 'width probe',
  }
}

/** mulberry32: a tiny deterministic PRNG so the worst-case frames are reproducible. */
const prng = (seed: number) => () => {
  seed = (seed + 0x6d2b79f5) | 0
  let t = Math.imul(seed ^ (seed >>> 15), 1 | seed)
  t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t
  return ((t ^ (t >>> 14)) >>> 0) / 4294967296
}

const NOISE = 'abcdefghijklmnopqrstuvwxyz0123456789#$%&*+=<>?─│┼▓▒░'

const stressScene = ({ frame, columns, rows }: SceneInput): SceneFrame => {
  const random = prng(frame + 1)
  const content = Array.from(
    { length: rows },
    (): Row =>
      Array.from(
        { length: columns },
        (): Seg => ({
          text: NOISE[Math.floor(random() * NOISE.length)] ?? 'x',
          fg: 16 + Math.floor(random() * 216),
          bg: 232 + Math.floor(random() * 24),
          ...(random() < 0.2 ? { bold: true as const } : {}),
        }),
      ),
  )
  return {
    rows: content,
    cursor: {
      row: frame % rows,
      column: (frame * 7) % columns,
      visible: true,
      blinking: false,
      style: 'block',
    },
    title: 'stress',
    modes: { alternate_screen: true },
  }
}

const hslToRgb = ({
  h,
  sat,
  light,
}: {
  readonly h: number
  readonly sat: number
  readonly light: number
}): [number, number, number] => {
  const k = (n: number) => (n + h / 30) % 12
  const a = sat * Math.min(light, 1 - light)
  const channel = (n: number) =>
    Math.round(255 * (light - a * Math.max(-1, Math.min(k(n) - 3, Math.min(9 - k(n), 1)))))
  return [channel(0), channel(8), channel(4)]
}

const rgbHex = (rgb: readonly [number, number, number]) =>
  `#${rgb.map((value) => value.toString(16).padStart(2, '0')).join('')}`

const sceneFrame: Record<SceneId, (input: SceneInput) => SceneFrame> = {
  F1: claudeScene,
  F2: codexScene,
  F3: unicodeScene,
  F4: stressScene,
}

const styleKey = (style: Style) =>
  `${style.fg ?? ''}|${style.bg ?? ''}|${style.bold ? 1 : 0}${style.dim ? 1 : 0}${style.italic ? 1 : 0}${style.underline ? 1 : 0}${style.inverse ? 1 : 0}`

/** A run whose trailing blanks are visible: the server keeps them, it trims all others. */
const showsBlanks = (style: Style) =>
  style.bg !== undefined || style.inverse === true || style.underline === true

/** Clip to `columns` cells (a wide grapheme straddling the edge becomes a space), merge equal styles. */
const toLine = ({
  row,
  index,
  columns,
}: {
  readonly row: Row
  readonly index: number
  readonly columns: number
}): TerminalLine => {
  // The first segment of each merged run carries the run's style; `text` grows as equal styles merge.
  const runs: Array<{ readonly run: { text: string } & Style; readonly key: string }> = []
  let used = 0
  for (const seg of row) {
    if (used >= columns) break
    let text = seg.text
    const width = textWidth(text)
    if (used + width > columns) {
      text = ''
      for (const grapheme of graphemes(seg.text)) {
        const cells = graphemeWidth(grapheme)
        if (used + textWidth(text) + cells > columns) {
          if (used + textWidth(text) < columns) text += ' '
          break
        }
        text += grapheme
      }
    }
    used += textWidth(text)
    if (text.length === 0) continue
    const key = styleKey(seg)
    const last = runs.at(-1)
    if (last !== undefined && last.key === key) last.run.text += text
    else runs.push({ run: { ...seg, text }, key })
  }
  while (runs.length > 0) {
    const last = runs.at(-1)
    if (last === undefined || showsBlanks(last.run)) break
    last.run.text = last.run.text.trimEnd()
    if (last.run.text.length > 0) break
    runs.pop()
  }
  return {
    row: index,
    text: runs
      .map(({ run }) => run.text)
      .join('')
      .trimEnd(),
    runs: runs.map(({ run }) => run),
    redacted: false,
    truncated: false,
  }
}

const defaultModes: TerminalModes = {
  alternate_screen: false,
  application_cursor: false,
  application_keypad: false,
  bracketed_paste: false,
  focus_events: false,
  mouse_encoding: 'default',
  mouse_tracking: 'none',
}

/** Which synthetic scene frame to build, at what geometry, for which runtime incarnation. */
export type FrameOptions = {
  readonly scene: SceneId
  readonly columns: number
  readonly rows: number
  readonly frame: number
  readonly incarnation?: string
}

/** One synthetic `TerminalScreen` frame; a pure function of its options. */
export const makeScreen = ({
  scene,
  columns,
  rows,
  frame,
  incarnation = 'inc_7f3a9c',
}: FrameOptions): TerminalScreen => {
  const built = sceneFrame[scene]({ frame, columns, rows })
  return decodeUnknownSync(
    TerminalScreen,
    'strict',
  )({
    kind: 'terminal-screen',
    terminal_id: `terminal/${scene.toLowerCase()}-agent`,
    runtime_incarnation: incarnation,
    next_sequence: 1040 + frame,
    revision: `rev_${scene}_${columns}x${rows}_${frame}`,
    title: built.title,
    columns,
    rows,
    cursor: built.cursor,
    modes: { ...defaultModes, ...built.modes },
    lines: Array.from({ length: rows }, (_, index) =>
      toLine({ row: built.rows[index] ?? blank, index, columns }),
    ),
    truncated: false,
  })
}

/** One 10 Hz loop worth of frames; generation stays out of the measured render path. */
export const makeFrames = ({
  count = 100,
  ...options
}: Omit<FrameOptions, 'frame'> & { readonly count?: number }): ReadonlyArray<TerminalScreen> =>
  Array.from({ length: count }, (_, frame) => makeScreen({ ...options, frame }))

/** World sessions use their own transcript; the synthetic scenes above only measure renderer fidelity. */
export const screenFor = ({
  agentRef,
  columns = 96,
  rows = 30,
  frame = 0,
}: {
  readonly agentRef: string
  readonly columns?: number
  readonly rows?: number
  readonly frame?: number
}): TerminalScreen => {
  const agent = agentByRef(agentRef)
  if (agent === undefined) throw new Error(`Unknown terminal agent: ${agentRef}`)
  const host = hosts.find((candidate) => candidate.id === agent.host)!
  const tick = host.connected && agent.activity === 'working' ? frame : 0
  const { session } = agent
  const pull = pullRequests.find((candidate) => candidate.agent === agent.ref)
  const mission = missions.find((candidate) => candidate.ref === agent.mission)
  const clock = session.lastActivityAt.slice(11, 19)
  const tool = ({
    command,
    output,
  }: {
    readonly command: string
    readonly output: readonly string[]
  }): Row[] => [
    [
      {
        text:
          session.harness === 'claude'
            ? '⏺ Bash '
            : session.harness === 'codex'
              ? '• Ran '
              : 'tool · bash ',
        bold: true,
      },
      s(command),
    ],
    ...output.flatMap((line) =>
      wrap({ text: line, width: Math.max(1, columns - 4) }).map(
        (part): Row => [
          { text: '  ', dim: true },
          {
            text: part,
            ...(line.includes('error:') || line.includes('ENOSPC') ? { fg: 1 } : { fg: GRAY }),
          },
        ],
      ),
    ),
    blank,
  ]
  const prose = (text: string): Row[] =>
    wrap({ text, width: Math.max(1, columns - 2) }).map((line) => [s(line)])
  const conversation: Row[] = [
    ...box({
      content: [
        [
          {
            text: ` ${session.client} · ${agent.name}`,
            bold: true,
            fg: session.harness === 'claude' ? CLAUDE_ORANGE : 6,
          },
        ],
        [{ text: ` model: ${session.model}`, dim: true }],
        ...prose(` cwd: ${session.cwd}`),
        ...prose(` branch: ${session.branch}`),
      ],
      width: columns,
      border: { fg: GRAY },
    }),
    blank,
    ...prose(mission?.goal ?? agent.status),
    blank,
  ]
  switch (agent.slug) {
    case 'workbench-shell':
      conversation.push(
        ...tool({
          command: 'pnpm storybook build',
          output: [
            `${clock} · ${agent.repo} · ${session.branch}`,
            'info => Building manager..',
            'info => Building preview..',
            'vite v7.1.7 building for production...',
            `transforming (${1840 + Math.floor(tick / 5)}) src/shell/Workbench.tsx`,
            `PR #${pull!.number} · storybook build pending`,
          ],
        }),
      )
      break
    case 'terminal-renderer':
      conversation.push(
        ...tool({
          command: 'pnpm playwright test terminal-frame-bench',
          output: [
            'TerminalScreen · 10 Hz · 200×60 · p95 main-thread/frame',
            'renderer         normal     every-cell     selection',
            'DOM runs          1.4 ms      8.6 ms        native',
            'xterm.js WebGL    2.2 ms     10.8 ms        emulator',
            'ghostty-web       1.8 ms      6.9 ms        emulator',
            `sampling ghostty-web · frame ${1040 + tick}`,
          ],
        }),
      )
      break
    case 'missions-ui':
      conversation.push(
        ...prose(decisionD12.title),
        blank,
        ...prose(decisionD12.question),
        blank,
        ...decisionD12.options.flatMap((option) => [
          ...prose(`${option.id} · ${option.label}`),
          ...prose(`  ${option.tradeoff}`),
          blank,
        ]),
        ...prose(`Recommended: ${decisionD12.recommendation} · waiting for ${operator.name}`),
        ...prose(`${decisionD12.ref} · raised ${decisionD12.raisedAt.slice(11, 16)} UTC`),
      )
      break
    case 'palette-review':
      conversation.push(
        ...tool({
          command: `gh pr diff ${pull!.number} --repo ${agent.repo}`,
          output: [`${pull!.title}`, `+${pull!.additions} -${pull!.deletions}`],
        }),
        ...prose(
          'Review posted: keep the subject revision fence across confirmation and dispatch. Keyboard focus returns to the invoking editor.',
        ),
        blank,
        ...prose(`Review posted on #${pull!.number}; approval remains with ${operator.name}.`),
      )
      break
    case 'gateway-schema':
      conversation.push(
        ...tool({
          command: 'cargo run -p schema-codegen -- --typescript --rust',
          output: [
            'reading st3.client v1 envelopes',
            'writing clients/typescript/Models.generated.ts',
            'writing clients/rust/src/models.rs',
            'checking generated model freshness...',
            `draft PR #${pull!.number} · +${pull!.additions} -${pull!.deletions}`,
          ],
        }),
        ...tool({
          command: 'cargo test -p client-models',
          output: [
            'running model round-trip checks...',
            'test terminal_screen_round_trip ... ok',
            'test resource_envelope_round_trip ... ok',
          ],
        }),
      )
      break
    case 'deps-steward':
      conversation.push(
        ...tool({
          command: 'nix flake update',
          output: [
            "Updated input 'nixpkgs':",
            '- previous pinned revision',
            '+ verified weekly revision',
            "Updated input 'home-manager'",
            `PR #${pull!.number} · ${pull!.title}`,
          ],
        }),
        ...tool({
          command: 'nix build .#checks.x86_64-linux.eval-hosts',
          output: [
            `evaluating ${hosts
              .filter((candidate) => candidate.os.includes('NixOS'))
              .map((candidate) => candidate.id)
              .join(', ')}`,
            'building host evaluation check...',
            'darwin-activation CI failure handed to CiDoctor',
          ],
        }),
      )
      break
    case 'health-probe':
      conversation.push(
        ...tool({
          command: `fleet-health probe --repo ${agent.repo}`,
          output: [
            ...hosts.map(
              (candidate) =>
                `${candidate.id.padEnd(16)} invariants passed at ${session.lastActivityAt.slice(11, 16)} UTC`,
            ),
            agent.status,
          ],
        }),
        ...prose('Probe completed. No process is running; waiting for the next scheduled run.'),
      )
      break
    case 'cas-janitor':
      conversation.push(
        ...tool({
          command: 'cas migrate --staging /srv/cas-staging --verify',
          output: [
            `${clock} copying verified blobs into staging`,
            'error: write /srv/cas-staging/blobs/sha256: ENOSPC: no space left on device',
            'migration stopped · exit code 1',
          ],
        }),
        ...tool({
          command: 'df -h /srv/cas-staging',
          output: [
            'Filesystem      Size   Used  Avail Use% Mounted on',
            '/dev/mapper/cas  2.0T   2.0T     0 100% /srv/cas-staging',
          ],
        }),
        ...prose(
          'Stopped before deletion. Originals remain intact. Free staging space before resuming the verified copy.',
        ),
      )
      break
    case 'ci-doctor': {
      const run = ciRuns.find((candidate) =>
        candidate.jobs.some((job) => job.conclusion === 'failure'),
      )!
      conversation.push(
        ...tool({
          command: `gh run view ${run.id} --repo ${run.repo} --log-failed`,
          output: run.jobs
            .filter((job) => job.conclusion === 'failure')
            .map((job) => `${job.name}: failure after ${job.durationS}s`),
        }),
        ...tool({
          command: 'nix build .#checks.x86_64-linux.darwin-activation',
          output: [
            'building activation regression check...',
            'error: activation probe exceeded its deadline',
            'check failed · exit code 1',
          ],
        }),
        ...prose(
          `Bisecting the failing check before quarantining it in draft PR #${pull!.number}.`,
        ),
      )
      break
    }
    case 'docs-writer':
      conversation.push(
        ...tool({
          command: `gh pr view ${pull!.number} --repo ${agent.repo}`,
          output: [
            pull!.title,
            `+${pull!.additions} -${pull!.deletions}`,
            ...pull!.reviews.map(
              (review) => `review requested: ${review.reviewer} · ${review.state}`,
            ),
          ],
        }),
        ...prose(
          'Gateway restart and builder disk-pressure runbooks updated. Waiting for review; no tool is running.',
        ),
      )
      break
    case 'sdk-interop':
      conversation.push(
        ...tool({
          command: 'pnpm vitest run sdk-interop',
          output: [
            'gateway release build complete',
            '✓ collections socket',
            '✓ terminal input fence',
            '⠋ follow-budget interop matrix running...',
          ],
        }),
      )
      break
    case 'web-scout':
      conversation.push(
        ...prose('Virtualization options for React Aria collections:'),
        blank,
        ...prose(
          'React Aria Virtualizer · preserves collection keyboard + screen-reader semantics',
        ),
        ...prose('TanStack Virtual · low-level windowing; focus integration required'),
        ...prose('react-window · fixed/variable rows; collection semantics required'),
        blank,
        ...prose(
          'Research complete. Recommend proving React Aria Virtualizer against the conversation and missions lists.',
        ),
      )
      break
    default:
      conversation.push(...prose(agent.status))
  }
  const status =
    agent.activity === 'working'
      ? `${BRAILLE[tick % BRAILLE.length]} ${agent.status} · esc to interrupt`
      : agent.activity === 'waiting'
        ? `Waiting for input · ${agent.status}`
        : agent.activity === 'errored'
          ? `Stopped · ${agent.status}`
          : `Idle · ${agent.status}`
  const footer: Row[] = [
    [{ text: status, fg: agent.activity === 'errored' ? 1 : agent.activity === 'waiting' ? 3 : 6 }],
    blank,
    [
      { text: session.harness === 'codex' ? '› ' : '> ', bold: true },
      {
        text: agent.slug === 'missions-ui' ? 'Answer D12: A, B or C' : 'Send a message…',
        dim: true,
      },
      ...(session.harness === 'claude' ? [{ text: ' ', inverse: true as const }] : []),
    ],
    blank,
    [
      {
        text: `${session.model} · ${Math.round(session.contextFill * 100)}% context · ${formatTokens(session.tokens.output)} output tokens`,
        dim: true,
      },
    ],
  ]
  const content = [
    ...conversation,
    ...Array.from({ length: Math.max(0, rows - conversation.length - footer.length) }, () => blank),
  ]
  // Preserve the harness/session header and the latest transcript when the pane is short.
  const room = Math.max(0, rows - footer.length)
  const headerLength = Math.min(8, room)
  const visible =
    content.length <= room
      ? content
      : [
          ...content.slice(0, headerLength),
          ...(room > headerLength ? content.slice(-(room - headerLength)) : []),
        ]
  const rendered = [...visible, ...footer]
  return decodeUnknownSync(
    TerminalScreen,
    'strict',
  )({
    kind: 'terminal-screen',
    terminal_id: agent.terminal,
    runtime_incarnation: `inc_${session.id.split('/')[1]}`,
    next_sequence: session.toolCalls + tick,
    revision: `rev_${agent.slug}_${columns}x${rows}_${tick}`,
    title: `${agent.name} · ${session.client}`,
    columns,
    rows,
    cursor: {
      row: rows - 3,
      column: 2,
      visible: session.harness !== 'claude',
      blinking: true,
      style: session.harness === 'codex' ? 'bar' : 'block',
    },
    modes: { ...defaultModes, bracketed_paste: true, alternate_screen: session.harness === 'omp' },
    lines: Array.from({ length: rows }, (_, index) =>
      toLine({ row: rendered[index] ?? blank, index, columns }),
    ),
    truncated: false,
  })
}

/** Terminal collection projection consumed by the data seam, keyed by world terminal ref. */
export const agentScreens: Readonly<Record<string, TerminalScreen>> = Object.fromEntries(
  agents.map((agent) => [agent.terminal, screenFor({ agentRef: agent.ref })]),
)
