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

export function terminalFixture(start = 0): TerminalScreen {
  return { kind: 'terminal-screen', terminal_id: 'synthetic-terminal', runtime_incarnation: 'synthetic-incarnation', revision: String(start), next_sequence: start, title: 'Synthetic shell', columns: 80, rows: 4, cursor: { row: 3, column: 2, visible: true, blinking: true, style: 'block' }, modes: { alternate_screen: false, application_cursor: true, application_keypad: true, bracketed_paste: true, focus_events: false, mouse_tracking: 'none', mouse_encoding: 'default' }, truncated: false, lines: Array.from({ length: 4 }, (_, row) => ({ row, text: `local line ${start + row}`, runs: [{ text: `local line ${start + row}` }], redacted: false, truncated: false })) }
}
type Scenario = 'focus' | 'typing' | 'paste' | 'resize' | 'selection' | 'scrollback' | 'palette' | 'pixels' | 'readonly' | 'unavailable' | 'ended' | 'ssr' | 'imports' | 'cursor' | 'drawer' | 'all'
type Args = { scheme: 'dark' | 'light'; scenario: Scenario; defect: boolean }
function TerminalStory({ scheme, scenario, defect }: Args) {
  const [screen, setScreen] = React.useState(() => terminalFixture())
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
  const palette = createTerminalPalette(scheme)
  const chosen = scenario === 'selection' ? { ...screen, lines: [ { row: 0, text: 'A界B', runs: [{ text: 'A' }, { text: '界', cells: 2 }, { text: 'B' }], redacted: false, truncated: false, wrapped: !defect }, { row: 1, text: 'tail', runs: [{ text: 'tail' }], redacted: false, truncated: false } ] } : scenario === 'pixels' || scenario === 'palette' ? { ...screen, lines: Array.from({ length: 16 }, (_, row) => ({ row, text: '████████████████', runs: Array.from({ length: 16 }, (_, col) => ({ text: '█', fg: row * 16 + col, bg: row * 16 + col })), redacted: false, truncated: false })) } : defect && (scenario === 'typing' || scenario === 'paste') ? { ...screen, modes: { ...screen.modes, application_cursor: false, application_keypad: false, bracketed_paste: false } } : screen
  if (scenario === 'cursor' && defect) chosen.cursor = { ...chosen.cursor, visible: false }
  const connection: TerminalConnection = scenario === 'unavailable' ? { state: 'unavailable', reason: 'The terminal host cannot be reached.' } : scenario === 'ended' ? { state: 'ended', reason: 'The terminal process has exited.' } : { state: 'live' }
  const props = { screen: chosen, connection, readOnly: scenario === 'readonly' && !defect, palette: defect && (scenario === 'palette' || scenario === 'pixels') ? { ...palette, resolve: () => '#00ff00' } : palette, font: { family: 'monospace', sizePx: 13, lineHeightPx: 20, advanceEm: 0.6 }, focusRing: !(defect && scenario === 'focus'), scrollbackLines: scenario === 'scrollback' ? defect ? 1200 : 1000 : 1000, onInput: (data: string) => { if (!(defect && scenario === 'typing')) setInput(values => [...values, data]) }, onResize: (size: { cols: number; rows: number }) => { if (!(defect && scenario === 'resize')) setSizes(values => [...values, `${size.cols}x${size.rows}`]) }, onCopy: (text: string) => setCopy(text), onRecover: () => { if (!defect) setRecovered(value => value + 1) }, handleRef: handle }
  return <main data-scheme={scheme} data-defect={defect} {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}>
    <h1 {...stylex.props(styles.heading)}>Terminal · {scenario}</h1><div {...stylex.props(styles.tools)}><button onClick={() => setWidth(value => value === 640 ? 480 : 640)}>Resize container</button><button onClick={() => { for (let start = 1; start <= 1200; start++) flushSync(() => setScreen(terminalFixture(start))) }}>Roll 1200 lines</button><button onClick={() => setScreen(value => ({ ...value, revision: value.revision + '-modes', modes: { ...value.modes, application_cursor: false, application_keypad: false, bracketed_paste: false, focus_events: true } }))}>Normal modes</button><button onClick={() => { setCopy(handle.current?.copySelection() || '') }}>Copy selection</button><button onClick={() => handle.current?.clearSelection()}>Clear selection</button><button onClick={() => setOpen(true)}>Reopen</button></div>
    <div data-terminal-container style={{ width, height }} {...stylex.props(styles.frame)}>{scenario === 'drawer' ? <TerminalDrawer {...props} open={open} height={height} onHeight={setHeight} onToggle={() => setOpen(value => !value)} onDetach={() => { if (!defect) setDetached(value => value + 1) }} onKill={() => setKilled(value => value + 1)} /> : <TerminalSurface {...props} />}</div>
    <output data-input>{JSON.stringify(input)}</output><output data-sizes>{JSON.stringify(sizes)}</output><output data-copy>{copy}</output><output data-recovery>{recovered}</output><output data-detached>{detached}</output><output data-killed>{killed}</output>
    {scenario === 'all' && <div {...stylex.props(styles.states)}>{(['connecting', 'reconnecting', 'unavailable', 'ended'] as const).map(state => <TerminalSurface key={state} screen={null} readOnly palette={palette} connection={state === 'ended' || state === 'unavailable' ? { state, reason: state === 'ended' ? 'The terminal process has exited.' : 'The terminal host cannot be reached.' } : { state }} onRecover={() => setRecovered(value => value + 1)} />)}</div>}
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
} }
export const SelectionCopy: Story = { args: { scenario: 'selection' }, play: async ({ canvasElement }) => {
  const canvas = within(canvasElement); const lines = canvasElement.querySelectorAll('[data-terminal-line]'); const range = document.createRange(); range.setStart(lines[0], 0); range.setEnd(lines[1], 1); const selection = document.getSelection()!; selection.removeAllRanges(); selection.addRange(range)
  await userEvent.click(canvas.getByRole('button', { name: 'Copy selection' })); await expect(canvasElement.querySelector('[data-copy]')).toHaveTextContent('A界Btail')
  await expect(lines[0].querySelectorAll('span')[1].getBoundingClientRect().width).toBeCloseTo(15.6, 1)
  selection.removeAllRanges(); selection.addRange(range); await userEvent.click(canvas.getByRole('button', { name: 'Clear selection' })); await expect(selection.isCollapsed).toBe(true)
  selection.addRange(range); const node = canvas.getByRole('textbox'); node.dispatchEvent(new KeyboardEvent('keydown', { bubbles: true, key: 'x' })); await waitFor(() => expect(selection.isCollapsed).toBe(true))
} }
export const ScrollbackCap: Story = { args: { scenario: 'scrollback' }, play: async ({ canvasElement }) => {
  let history = { lines: [] as TerminalScreen['lines'], truncated: false }; let previous = terminalFixture()
  for (let index = 1; index <= 1200; index++) { const next = terminalFixture(index); history = appendLocalHistory(previous, next, history, 1000); previous = next }
  await expect(history.lines).toHaveLength(1000); await expect(history.lines[0].text).toBe('local line 200'); await expect(history.truncated).toBe(true)
  await expect(appendLocalHistory(previous, terminalFixture(1201), history, 0).lines).toHaveLength(0)
  await userEvent.click(within(canvasElement).getByRole('button', { name: 'Roll 1200 lines' })); await expect(canvasElement.querySelector('[data-local-lines]')).toHaveAttribute('data-local-lines', '1000'); await expect(canvasElement.querySelector('[data-local-lines]')).toHaveAttribute('data-history-truncated', 'true'); await expect(within(canvasElement).getByRole('note')).toHaveTextContent('Local scrollback')
} }
export const NoGreenPalette: Story = { args: { scenario: 'palette' }, play: async ({ args }) => {
  const resolve = args.defect ? () => '#00ff00' : (value: number | string) => resolveTerminalColor(value, args.scheme)
  for (let index = 0; index < 256; index++) noGreen(resolve(index))
  for (const r of [0, 64, 128, 192, 255]) for (const g of [0, 64, 128, 192, 255]) for (const b of [0, 64, 128, 192, 255]) noGreen(resolve('#' + [r, g, b].map(value => value.toString(16).padStart(2, '0')).join('')))
  await expect(resolve(2)).toBe(args.scheme === 'dark' ? '#7295ed' : '#4269df'); await expect(resolve(2)).not.toBe(resolve(10))
  for (const index of [2, 10]) { await expect(hue(resolve(index))).toBeGreaterThan(210); await expect(hue(resolve(index))).toBeLessThan(250) }
  for (const index of [6, 14]) await expect(hue(resolve(index))).toBeGreaterThanOrEqual(210)
  await expect(Math.abs(luminance(resolve(12)) - luminance(resolve(4))) / luminance(resolve(4))).toBeGreaterThanOrEqual(0.2)
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
export const CursorAndMotion: Story = { args: { scenario: 'cursor' }, play: async ({ canvasElement }) => { await userEvent.click(within(canvasElement).getByRole('textbox')); await waitFor(() => expect(canvasElement.querySelector('[data-terminal-cursor="block"]')).not.toBeNull()); if (matchMedia('(prefers-reduced-motion: reduce)').matches) await expect(getComputedStyle(canvasElement.querySelector('[data-terminal-cursor]')!).animationName).toBe('none') } }
export const DrawerChrome: Story = { args: { scenario: 'drawer' }, play: async ({ canvasElement, args }) => {
  const canvas = within(canvasElement); await expect(canvas.queryByRole('tablist')).toBeNull(); await userEvent.click(canvas.getByRole('button', { name: 'End terminal' })); const dialog = within(document.body).getByRole('alertdialog'); await expect(within(dialog).getByRole('button', { name: 'End terminal process' })).toBeVisible(); await userEvent.click(within(dialog).getByRole('button', { name: 'Cancel' })); await expect(canvasElement.querySelector('[data-killed]')).toHaveTextContent('0'); await userEvent.click(canvas.getByRole('button', { name: 'Close terminal drawer' })); await expect(canvasElement.querySelector('[data-detached]')).toHaveTextContent('1'); await expect(canvas.queryByLabelText('Terminal drawer')).toBeNull(); await userEvent.click(canvas.getByRole('button', { name: 'Reopen' })); await expect(canvas.getByLabelText('Terminal drawer')).toBeVisible()
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
const styles = stylex.create({ root: { minHeight: '100vh', padding: 20, backgroundColor: c.canvas, color: c.fg, fontFamily: t.fontSans, boxSizing: 'border-box' }, heading: { fontSize: t.uiSize, marginBlock: 0, marginBottom: 12 }, tools: { display: 'flex', gap: 8, marginBottom: 12 }, frame: { display: 'flex', maxWidth: '100%', border: `1px solid ${c.borderStrong}` }, states: { display: 'grid', gridTemplateColumns: '1fr 1fr', gap: 12, marginTop: 12 } })
