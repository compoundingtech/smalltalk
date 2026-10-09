import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { flushSync } from 'react-dom'
import { renderToStaticMarkup } from 'react-dom/server'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect, userEvent, within, waitFor } from 'storybook/test'
import { TerminalSurface } from './assistant-ui/terminal/TerminalSurface'
import { TerminalDrawer } from './assistant-ui/terminal/TerminalDrawer'
import { resolveTerminalColor, createTerminalPalette } from './assistant-ui/terminal/terminal-palette'
import { appendLocalHistory } from './assistant-ui/terminal/terminal-behavior'
import type { TerminalScreen, TerminalSurfaceHandle, TerminalConnection } from './assistant-ui/terminal/terminal-types'
import surfaceSource from './assistant-ui/terminal/TerminalSurface.tsx?raw'
import drawerSource from './assistant-ui/terminal/TerminalDrawer.tsx?raw'
import paletteSource from './assistant-ui/terminal/terminal-palette.ts?raw'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { lightTheme } from './assistant-ui/composition-theme'
import { colorVars as c, typeVars as t } from './assistant-ui/composition-tokens.stylex'

function terminalFixture(start = 0, rows = 4): TerminalScreen {
  return { kind: 'terminal-screen', terminal_id: 'synthetic-terminal', runtime_incarnation: 'synthetic-incarnation', revision: String(start), next_sequence: start, title: 'Synthetic shell', columns: 80, rows, cursor: { row: rows - 1, column: 2, visible: true, blinking: true, style: 'block' }, modes: { alternate_screen: false, application_cursor: true, application_keypad: true, bracketed_paste: true, focus_events: false, mouse_tracking: 'none', mouse_encoding: 'default' }, truncated: false, lines: Array.from({ length: rows }, (_, row) => ({ row, text: `local line ${start + row}`, runs: [{ text: `local line ${start + row}` }], redacted: false, truncated: false })) }
}
type Scenario = 'focus' | 'typing' | 'paste' | 'resize' | 'selection' | 'scrollback' | 'palette' | 'pixels' | 'readonly' | 'unavailable' | 'ended' | 'ssr' | 'imports' | 'cursor' | 'drawer' | 'rendercost' | 'all'
type Args = { scheme: 'dark' | 'light'; scenario: Scenario; defect: boolean }
function TerminalStory({ scheme, scenario, defect }: Args) {
  const [screen, setScreen] = React.useState(() => terminalFixture(0, scenario === 'scrollback' || scenario === 'rendercost' ? 200 : 4))
  const [input, setInput] = React.useState<string[]>([])
  const [sizes, setSizes] = React.useState<string[]>([])
  const [copy, setCopy] = React.useState('')
  const [recovered, setRecovered] = React.useState(0)
  const [height, setHeight] = React.useState(380)
  const [width, setWidth] = React.useState(640)
  const [open, setOpen] = React.useState(true)
  const [detached, setDetached] = React.useState(0)
  const [killed, setKilled] = React.useState(0)
  const handle = React.useRef<TerminalSurfaceHandle>(null)
  const paintCalls = React.useRef(0)
  const palette = React.useMemo(() => {
    const base = createTerminalPalette(scheme)
    return scenario === 'rendercost' ? { ...base, resolve: (color: number | string) => { paintCalls.current++; return base.resolve(color) } } : base
  }, [scheme, scenario])
  let chosen = screen
  if (scenario === 'selection') chosen = { ...screen, lines: [
    { row: 0, text: 'A界B', runs: [{ text: 'A' }, { text: '界', cells: 2 }, { text: 'B' }], redacted: false, truncated: false },
    { row: 1, text: 'tail', runs: [{ text: 'tail' }], redacted: false, truncated: false, wrapped: !defect },
    { row: 2, text: '', runs: [], redacted: false, truncated: false },
    { row: 3, text: 'end', runs: [{ text: 'end' }], redacted: false, truncated: false },
  ] }
  if (scenario === 'rendercost') chosen = { ...screen, lines: screen.lines.map(line => ({ ...line, runs: [{ text: line.text.slice(0, 1), fg: 4 }, { text: line.text.slice(1), fg: 2 }] })) }
  if (scenario === 'pixels' || scenario === 'palette') {
    const samples = ['#00ff00', '#7ffe00', '#00ffaa', '#a6e3a1', '#94e2d5', '#00ffff', '#61afef', '#ffffff']
    chosen = { ...screen, rows: 17, lines: [
      ...Array.from({ length: 16 }, (_, row) => ({ row, text: ' '.repeat(32), runs: Array.from({ length: 16 }, (_, col) => ({ text: '  ', fg: row * 16 + col, bg: row * 16 + col })), redacted: false, truncated: false })),
      { row: 16, text: ' '.repeat(16), runs: samples.map(color => ({ text: '  ', fg: color, bg: color })), redacted: false, truncated: false },
    ] }
  }
  if (defect && (scenario === 'typing' || scenario === 'paste')) chosen = { ...screen, modes: { ...screen.modes, application_cursor: false, application_keypad: false, bracketed_paste: false } }
  if (scenario === 'cursor' && defect) chosen = { ...chosen, cursor: { ...chosen.cursor, visible: false } }
  const connection: TerminalConnection = scenario === 'unavailable' ? { state: 'unavailable', reason: 'The terminal host cannot be reached.' } : scenario === 'ended' ? { state: 'ended', reason: 'The terminal process has exited.' } : { state: 'live' }
  const props = { screen: chosen, connection, readOnly: scenario === 'readonly' && !defect, palette: defect && (scenario === 'palette' || scenario === 'pixels') ? { ...palette, resolve: () => '#00ff00' } : palette, font: { family: 'monospace', sizePx: 13, lineHeightPx: 20, advanceEm: 0.6 }, focusRing: !(defect && scenario === 'focus'), scrollbackLines: scenario === 'scrollback' ? defect ? 1200 : 1000 : 1000, onInput: (data: string) => { if (!(defect && scenario === 'typing')) setInput(values => [...values, data]) }, onResize: (size: { cols: number; rows: number }) => { if (!(defect && scenario === 'resize')) setSizes(values => [...values, `${size.cols}x${size.rows}`]) }, onCopy: (text: string) => setCopy(text), onRecover: () => { if (!defect) setRecovered(value => value + 1) }, handleRef: handle }
  return <main data-scheme={scheme} data-defect={defect} {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}>
    <h1 {...stylex.props(styles.heading)}>Terminal · {scenario}</h1><div {...stylex.props(styles.tools)}><button onClick={() => setWidth(value => value === 640 ? 480 : 640)}>Resize container</button><button onClick={() => { for (const start of [199, 398, 597, 796, 995, 1194, 1200]) flushSync(() => setScreen(terminalFixture(start, 200))) }}>Roll 1200 lines</button><button onClick={() => setScreen(value => ({ ...value, revision: value.revision + '-modes', modes: { ...value.modes, application_cursor: false, application_keypad: false, bracketed_paste: false, focus_events: true } }))}>Normal modes</button><button onClick={() => { setCopy(handle.current?.copySelection() || '') }}>Copy selection</button><button onClick={() => handle.current?.clearSelection()}>Clear selection</button><button onClick={() => setOpen(true)}>Reopen</button></div>
    {scenario === 'cursor' && <div {...stylex.props(styles.tools)}>{(['block', 'underline', 'bar'] as const).map(style => <button key={style} onClick={() => setScreen(value => ({ ...value, cursor: { ...value.cursor, style, visible: true } }))}>{style} cursor</button>)}<button onClick={() => setScreen(value => ({ ...value, cursor: { ...value.cursor, visible: false } }))}>Hide cursor</button></div>}
    {scenario === 'rendercost' && <button data-paint-calls={paintCalls.current} onClick={event => {
      paintCalls.current = 0
      flushSync(() => setScreen(value => ({ ...value, revision: value.revision + '-cell', lines: value.lines.map((line, index) => index === 0 || defect ? { ...line, text: 'X' + line.text.slice(1) } : line) })))
      event.currentTarget.dataset.paintCalls = String(paintCalls.current)
    }}>Change one cell</button>}
    <div data-terminal-container style={{ width, height: scenario === 'drawer' ? undefined : height }} {...stylex.props(styles.frame)}>{scenario === 'drawer' ? <TerminalDrawer {...props} open={open} height={height} onHeight={setHeight} onToggle={() => setOpen(value => !value)} onDetach={() => { if (!defect) setDetached(value => value + 1) }} onKill={() => setKilled(value => value + 1)} /> : <TerminalSurface {...props} />}</div>
    <div {...stylex.props(styles.feedback)}><div data-input>{JSON.stringify(input)}</div><div data-sizes>{JSON.stringify(sizes)}</div><div data-copy>{copy}</div><div data-recovery>{recovered}</div><div data-detached>{detached}</div><div data-killed>{killed}</div></div>
    {scenario === 'all' && <div {...stylex.props(styles.states)}>{(['connecting', 'reconnecting', 'unavailable', 'ended'] as const).map(state => <TerminalSurface key={state} label={`Terminal · ${state}`} screen={null} readOnly palette={palette} connection={state === 'ended' || state === 'unavailable' ? { state, reason: state === 'ended' ? 'The terminal process has exited.' : 'The terminal host cannot be reached.' } : { state }} onRecover={() => setRecovered(value => value + 1)} />)}</div>}
  </main>
}
const meta = { title: 'Fractal UI/Terminal', component: TerminalStory, parameters: { layout: 'fullscreen' }, args: { scheme: 'dark', scenario: 'all', defect: false }, argTypes: { scheme: { options: ['dark', 'light'], control: 'radio' }, defect: { control: 'boolean', description: 'Negative control: deliberately breaks the scenario; its play must fail.' } } } satisfies Meta<typeof TerminalStory>
export default meta
type Story = StoryObj<typeof meta>
const inputValues = (root: HTMLElement) => JSON.parse(root.querySelector('[data-input]')!.textContent!) as string[]
const sizeValues = (root: HTMLElement) => JSON.parse(root.querySelector('[data-sizes]')!.textContent!) as string[]
function hue(hex: string) {
  const rgb = hex.match(/[a-f\d]{2}/gi)!.map(value => parseInt(value, 16) / 255)
  const [r, g, b] = rgb; const max = Math.max(...rgb); const min = Math.min(...rgb); const delta = max - min
  return delta < 0.01 ? -1 : ((max === r ? (g - b) / delta : max === g ? (b - r) / delta + 2 : (r - g) / delta + 4) * 60 + 360) % 360
}
function noGreen(color: string) { const value = hue(color); if (value >= 90 && value <= 160) throw new Error(`Forbidden green ${color} (${value})`) }
function luminance(hex: string) { return hex.match(/[a-f\d]{2}/gi)!.map(value => parseInt(value, 16) / 255).map(value => value <= 0.04045 ? value / 12.92 : ((value + 0.055) / 1.055) ** 2.4).reduce((sum, value, index) => sum + value * [0.2126, 0.7152, 0.0722][index], 0) }
export const FocusRing: Story = { args: { scenario: 'focus' }, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement); const input = canvas.getByRole('textbox', { name: 'Interactive terminal' })
  input.focus(); await userEvent.tab({ shift: true }); await userEvent.tab()
  await waitFor(() => expect(getComputedStyle(canvasElement.querySelector('[data-testid="terminal-surface"]')!).outlineWidth).toBe('2px'))
  const ring = getComputedStyle(canvasElement.querySelector('[data-testid="terminal-surface"]')!)
  await expect(ring.outlineStyle).toBe('solid'); await expect(ring.outlineOffset).toBe('1px'); await expect(ring.outlineColor).not.toBe('rgba(0, 0, 0, 0)')
  await userEvent.tab({ shift: true }); await waitFor(() => expect(getComputedStyle(canvasElement.querySelector('[data-testid="terminal-surface"]')!).outlineStyle).toBe('none'))
} }
export const TypingReachesOnData: Story = { args: { scenario: 'typing' }, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement); await userEvent.click(canvas.getByRole('textbox')); await userEvent.keyboard('ls{Enter}{Escape}{Backspace}{Tab}{ArrowUp}')
  await expect(inputValues(canvasElement).join('')).toBe('ls\r\x1b\x7f\t\x1bOA')
  const node = canvas.getByRole('textbox'); node.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, key: '1', code: 'Numpad1' }))
  await waitFor(() => expect(inputValues(canvasElement).at(-1)).toBe('\x1bOq'))
  await userEvent.click(canvas.getByRole('button', { name: 'Normal modes' })); await userEvent.click(node); await userEvent.keyboard('{ArrowUp}')
  await expect(inputValues(canvasElement)).toContain('\x1b[A'); await expect(inputValues(canvasElement)).toContain('\x1b[I')
} }
export const PasteBracketed: Story = { args: { scenario: 'paste' }, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement); const node = canvas.getByRole('textbox'); const paste = () => { const data = new DataTransfer(); data.setData('text/plain', 'first\r\nsecond\n界'); node.dispatchEvent(new ClipboardEvent('paste', { clipboardData: data, bubbles: true, cancelable: true })) }
  paste(); await waitFor(() => expect(inputValues(canvasElement)).toEqual(['\x1b[200~first\rsecond\r界\x1b[201~']))
  await userEvent.click(canvas.getByRole('button', { name: 'Normal modes' })); paste(); await waitFor(() => expect(inputValues(canvasElement).at(-1)).toBe('first\rsecond\r界'))
} }
export const ResizeFromMetrics: Story = { args: { scenario: 'resize' }, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement); const measure = () => { const node = canvasElement.querySelector<HTMLElement>('[data-testid="terminal-surface"]')!; return `${Math.floor(node.clientWidth / 7.8)}x${Math.floor(node.clientHeight / 20)}` }
  await waitFor(() => expect(sizeValues(canvasElement).at(-1)).toBe(measure())); const before = sizeValues(canvasElement).length
  await userEvent.click(canvas.getByRole('button', { name: 'Resize container' })); await waitFor(() => expect(sizeValues(canvasElement).at(-1)).toBe(measure())); await expect(sizeValues(canvasElement)).toHaveLength(before + 1)
  await userEvent.click(canvas.getByRole('button', { name: 'Normal modes' })); await expect(sizeValues(canvasElement)).toHaveLength(before + 1)
  const container = canvasElement.querySelector<HTMLElement>('[data-terminal-container]')!; const beforeDrag = sizeValues(canvasElement).length
  for (const width of [510, 540, 570, 600]) { container.style.width = `${width}px`; await new Promise(resolve => setTimeout(resolve, 30)) }
  await waitFor(() => expect(sizeValues(canvasElement).at(-1)).toBe(measure()))
  await expect(sizeValues(canvasElement)).toHaveLength(beforeDrag + 1)
} }
export const SelectionCopy: Story = { args: { scenario: 'selection' }, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement); const lines = canvasElement.querySelectorAll('[data-terminal-line]'); const range = document.createRange(); range.setStart(lines[0], 0); range.setEnd(lines[1], 1); const selection = document.getSelection()!; selection.removeAllRanges(); selection.addRange(range)
  await userEvent.click(canvas.getByRole('button', { name: 'Copy selection' })); await expect(canvasElement.querySelector('[data-copy]')).toHaveTextContent('A界Btail')
  const clipboard = new DataTransfer()
  canvas.getByRole('textbox').dispatchEvent(new ClipboardEvent('copy', { clipboardData: clipboard, bubbles: true, cancelable: true }))
  await expect(clipboard.getData('text/plain')).toBe('A界Btail')
  await expect(lines[0].querySelectorAll('span')[1].getBoundingClientRect().width).toBeCloseTo(15.6, 1)
  const blankRange = document.createRange(); blankRange.setStart(lines[1], 0); blankRange.setEnd(lines[3], 1)
  selection.removeAllRanges(); selection.addRange(blankRange)
  await userEvent.click(canvas.getByRole('button', { name: 'Copy selection' }))
  await expect(canvasElement.querySelector('[data-copy]')!.textContent).toBe('tail\n\nend')
  selection.removeAllRanges(); selection.addRange(range)
  await userEvent.click(canvas.getByRole('button', { name: 'Copy selection' }))
  await expect(canvasElement.querySelector('[data-copy]')!.textContent).toBe('A界Btail')
  selection.removeAllRanges(); selection.addRange(range); await userEvent.click(canvas.getByRole('button', { name: 'Clear selection' })); await expect(selection.isCollapsed).toBe(true)
  selection.addRange(range); const node = canvas.getByRole('textbox'); node.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, key: 'x' })); await waitFor(() => expect(selection.isCollapsed).toBe(true))
} }
export const ScrollbackCap: Story = { args: { scenario: 'scrollback' }, play: async ({ canvasElement }) => {
  let history = { lines: [] as TerminalScreen['lines'], truncated: false }; let previous = terminalFixture()
  const repeated = { ...terminalFixture(), lines: terminalFixture().lines.map(line => ({ ...line, text: '', runs: [] })) }
  await expect(appendLocalHistory(repeated, { ...repeated, revision: 'cursor-only', cursor: { ...repeated.cursor, column: 3 } }, { lines: [], truncated: false }, 1000).lines).toHaveLength(0)
  for (let index = 1; index <= 1200; index++) { const next = terminalFixture(index); history = appendLocalHistory(previous, next, history, 1000); previous = next }
  await expect(history.lines).toHaveLength(1000); await expect(history.lines[0].text).toBe('local line 200'); await expect(history.truncated).toBe(true)
  await expect(appendLocalHistory(previous, terminalFixture(1201), history, 0).lines).toHaveLength(0)
  await userEvent.click(within(canvasElement).getByRole('button', { name: 'Roll 1200 lines' })); await expect(canvasElement.querySelector('[data-local-lines]')).toHaveAttribute('data-local-lines', '1000'); await expect(canvasElement.querySelector('[data-local-lines]')).toHaveAttribute('data-history-truncated', 'true'); await expect(within(canvasElement).getByRole('note')).toHaveTextContent('Local scrollback')
  await expect(canvasElement.querySelector('[data-terminal-line]')).toHaveTextContent('local line 200')
} }
export const RenderCost: Story = { args: { scenario: 'rendercost' }, play: async ({ canvasElement }) => {
  const button = within(canvasElement).getByRole('button', { name: 'Change one cell' })
  await userEvent.click(button)
  await expect(button).toHaveAttribute('data-paint-calls', '2')
} }
export const RenderCostLight: Story = { ...RenderCost, args: { ...RenderCost.args, scheme: 'light' } }
export const NoGreenPalette: Story = { args: { scenario: 'palette' }, play: async ({ args }) => {
  const resolve = args.defect ? () => '#00ff00' : (value: number | string) => resolveTerminalColor(value, args.scheme)
  for (let index = 0; index < 256; index++) noGreen(resolve(index))
  for (const r of [0, 64, 128, 192, 255]) for (const g of [0, 64, 128, 192, 255]) for (const b of [0, 64, 128, 192, 255]) noGreen(resolve('#' + [r, g, b].map(value => value.toString(16).padStart(2, '0')).join('')))
  await expect(resolve(2)).toBe(args.scheme === 'dark' ? '#7295ed' : '#4269df'); await expect(resolve(2)).not.toBe(resolve(10))
  for (const index of [2, 10]) { await expect(hue(resolve(index))).toBeGreaterThan(210); await expect(hue(resolve(index))).toBeLessThan(250) }
  for (const index of [6, 14]) await expect(hue(resolve(index))).toBeGreaterThanOrEqual(210)
  await expect(Math.abs(luminance(resolve(12)) - luminance(resolve(4))) / luminance(resolve(4))).toBeGreaterThanOrEqual(0.2)
  await expect(new Set(Array.from({ length: 16 }, (_, index) => resolve(index))).size).toBe(16)
  for (const blue of [4, 12]) for (const addition of [2, 10]) await expect(Math.abs(luminance(resolve(blue)) - luminance(resolve(addition))) / luminance(resolve(addition))).toBeGreaterThanOrEqual(0.2)
  for (const sample of ['#7ffe00', '#00ffaa']) await expect(resolve(sample)).toBe(resolve(2))
  await expect(() => noGreen('#00ff00')).toThrow()
} }
export const NoGreenPixels: Story = { args: { scenario: 'pixels' }, play: async ({ canvasElement }) => {
  for (const run of canvasElement.querySelectorAll('[data-terminal-line] span')) {
    const color = getComputedStyle(run).backgroundColor; const rgb = color.match(/[\d.]+/g)!.slice(0, 3).map(Number); const canvas = document.createElement('canvas'); canvas.width = canvas.height = 1; const context = canvas.getContext('2d')!; context.fillStyle = color; context.fillRect(0, 0, 1, 1); const pixel = Array.from(context.getImageData(0, 0, 1, 1).data).slice(0, 3); await expect(pixel).toEqual(rgb); noGreen('#' + pixel.map(value => value.toString(16).padStart(2, '0')).join(''))
  }
} }
export const ReadOnly: Story = { args: { scenario: 'readonly' }, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement); await expect(canvas.getByRole('status')).toHaveTextContent('Read-only terminal'); await expect(canvas.getByRole('textbox')).toHaveAttribute('aria-readonly', 'true'); await userEvent.click(canvas.getByRole('textbox')); await userEvent.keyboard('ls{Enter}{ArrowUp}'); const data = new DataTransfer(); data.setData('text/plain', 'hello'); canvas.getByRole('textbox').dispatchEvent(new ClipboardEvent('paste', { bubbles: true, clipboardData: data })); await expect(inputValues(canvasElement)).toEqual([])
} }
const recoveryPlay: Story['play'] = async ({ canvasElement }) => { const canvas = within(canvasElement); await expect(canvas.getByRole('status')).not.toHaveTextContent('Unknown'); await userEvent.click(canvas.getByRole('button', { name: /Reconnect|Start a new terminal/ })); await expect(canvasElement.querySelector('[data-recovery]')).toHaveTextContent('1'); await userEvent.click(canvas.getByRole('textbox')); await userEvent.keyboard('ls{Enter}'); await expect(inputValues(canvasElement)).toEqual([]) }
export const Unavailable: Story = { args: { scenario: 'unavailable' }, play: recoveryPlay }
export const Ended: Story = { args: { scenario: 'ended' }, play: recoveryPlay }
export const SSR: Story = { args: { scenario: 'ssr' }, play: async ({ args }) => { const palette = createTerminalPalette(args.scheme); const empty = renderToStaticMarkup(<TerminalSurface screen={null} readOnly palette={palette} connection={{ state: 'connecting' }} />); const live = renderToStaticMarkup(<TerminalSurface screen={args.defect ? null : terminalFixture()} readOnly={false} palette={palette} connection={{ state: 'live' }} />); await expect(empty).toContain('Connecting to the terminal.'); await expect(live).toContain('local line 0') } }
export const NoXterm: Story = { args: { scenario: 'imports' }, play: async ({ args }) => { const source = [surfaceSource, drawerSource, paletteSource].join('\n') + (args.defect ? '\nimport "xterm"' : ''); await expect(source).not.toMatch(/(?:from\s*|import\s*[(']?\s*)['"](?:@xterm\/|xterm)/); await expect(source).not.toMatch(/import\s+(?!type\b)[^\n]+st3-client/) } }
export const CursorAndMotion: Story = { args: { scenario: 'cursor' }, play: async ({ canvasElement, args }) => {
  const canvas = within(canvasElement)
  await userEvent.click(canvas.getByRole('textbox'))
  await waitFor(() => expect(canvasElement.querySelector('[data-terminal-cursor="block"]')).not.toBeNull())
  for (const style of ['underline', 'bar', 'block']) {
    await userEvent.click(canvas.getByRole('button', { name: `${style} cursor` }))
    await userEvent.click(canvas.getByRole('textbox'))
    await waitFor(() => expect(canvasElement.querySelector(`[data-terminal-cursor="${style}"]`)).not.toBeNull())
    const cursor = canvasElement.querySelector('[data-terminal-cursor]')!; const bounds = cursor.getBoundingClientRect()
    await expect(getComputedStyle(cursor).backgroundColor).toBe(args.scheme === 'light' ? 'rgb(27, 78, 216)' : 'rgb(52, 107, 241)')
    await expect(bounds.width).toBeCloseTo(style === 'bar' ? 2 : 7.8, 1); await expect(bounds.height).toBe(style === 'underline' ? 2 : 20)
    if (matchMedia('(prefers-reduced-motion: reduce)').matches) await expect(getComputedStyle(canvasElement.querySelector('[data-terminal-cursor]')!).animationName).toBe('none')
  }
  await userEvent.click(canvas.getByRole('button', { name: 'Hide cursor' }))
  await userEvent.click(canvas.getByRole('textbox'))
  await expect(canvasElement.querySelector('[data-terminal-cursor]')).toBeNull()
} }
export const DrawerChrome: Story = { args: { scenario: 'drawer' }, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement)
  await expect(canvas.queryByRole('tablist')).toBeNull()
  const separator = canvas.getByRole('separator', { name: 'Terminal drawer height' })
  separator.focus()
  await userEvent.keyboard('{ArrowDown}')
  await expect(separator).toHaveAttribute('aria-valuenow', '388')
  await userEvent.click(canvas.getByRole('button', { name: 'End terminal' }))
  let dialog = within(document.body).getByRole('alertdialog')
  await expect(within(dialog).getByRole('button', { name: 'End terminal process' })).toBeVisible()
  await userEvent.click(within(dialog).getByRole('button', { name: 'Cancel' }))
  await expect(canvasElement.querySelector('[data-killed]')).toHaveTextContent('0')
  await userEvent.click(canvas.getByRole('button', { name: 'End terminal' }))
  dialog = within(document.body).getByRole('alertdialog')
  await userEvent.click(within(dialog).getByRole('button', { name: 'End terminal process' }))
  await expect(canvasElement.querySelector('[data-killed]')).toHaveTextContent('1')
  await userEvent.click(canvas.getByRole('button', { name: 'Close terminal drawer' }))
  await expect(canvasElement.querySelector('[data-detached]')).toHaveTextContent('1')
  await expect(canvasElement.querySelector('[data-killed]')).toHaveTextContent('1')
  await expect(canvas.queryByLabelText('Terminal drawer')).toBeNull()
  await userEvent.click(canvas.getByRole('button', { name: 'Reopen' }))
  await expect(canvas.getByLabelText('Terminal drawer')).toBeVisible()
  canvas.getByRole('separator', { name: 'Terminal drawer height' }).focus(); await userEvent.keyboard('{Enter}')
  await expect(canvasElement.querySelector('[data-detached]')).toHaveTextContent('2'); await expect(canvasElement.querySelector('[data-killed]')).toHaveTextContent('1')
  await userEvent.click(canvas.getByRole('button', { name: 'Reopen' }))
} }
export const AllStates: Story = { args: { scenario: 'all' } }
export const FocusRingLight: Story = { ...FocusRing, args: { ...FocusRing.args, scheme: 'light' } }
export const TypingReachesOnDataLight: Story = { ...TypingReachesOnData, args: { ...TypingReachesOnData.args, scheme: 'light' } }
export const PasteBracketedLight: Story = { ...PasteBracketed, args: { ...PasteBracketed.args, scheme: 'light' } }
export const ResizeFromMetricsLight: Story = { ...ResizeFromMetrics, args: { ...ResizeFromMetrics.args, scheme: 'light' } }
export const SelectionCopyLight: Story = { ...SelectionCopy, args: { ...SelectionCopy.args, scheme: 'light' } }
export const ScrollbackCapLight: Story = { ...ScrollbackCap, args: { ...ScrollbackCap.args, scheme: 'light' } }
export const NoGreenPaletteLight: Story = { ...NoGreenPalette, args: { ...NoGreenPalette.args, scheme: 'light' } }
export const NoGreenPixelsLight: Story = { ...NoGreenPixels, args: { ...NoGreenPixels.args, scheme: 'light' } }
export const ReadOnlyLight: Story = { ...ReadOnly, args: { ...ReadOnly.args, scheme: 'light' } }
export const UnavailableLight: Story = { ...Unavailable, args: { ...Unavailable.args, scheme: 'light' } }
export const EndedLight: Story = { ...Ended, args: { ...Ended.args, scheme: 'light' } }
export const SSRLight: Story = { ...SSR, args: { ...SSR.args, scheme: 'light' } }
export const NoXtermLight: Story = { ...NoXterm, args: { ...NoXterm.args, scheme: 'light' } }
export const CursorAndMotionLight: Story = { ...CursorAndMotion, args: { ...CursorAndMotion.args, scheme: 'light' } }
export const DrawerChromeLight: Story = { ...DrawerChrome, args: { ...DrawerChrome.args, scheme: 'light' } }
export const AllStatesLight: Story = { ...AllStates, args: { ...AllStates.args, scheme: 'light' } }
const styles = stylex.create({ root: { minHeight: '100vh', padding: 20, backgroundColor: c.canvas, color: c.fg, fontFamily: t.fontSans, boxSizing: 'border-box' }, heading: { fontSize: t.uiSize, marginBlock: 0, marginBottom: 12 }, tools: { display: 'flex', gap: 8, marginBottom: 12 }, frame: { display: 'flex', flexDirection: 'column', maxWidth: '100%', borderWidth: 1, borderStyle: 'solid', borderColor: c.borderStrong }, feedback: { display: 'flex', flexWrap: 'wrap', gap: 12, color: c.fgMuted, fontSize: t.metaSize, lineHeight: t.metaLeading }, states: { display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 12, marginTop: 12 } })
