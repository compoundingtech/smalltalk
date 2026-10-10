import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { flushSync } from 'react-dom'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { expect, fn, userEvent, within } from 'storybook/test'
import { Transcript, type TranscriptHistory, type TranscriptProps, type TranscriptTurn } from './assistant-ui/composition/Transcript'
import { EmbraceRuntimeProvider } from './assistant-ui/EmbraceRuntime'
import type { ConversationItem, TextItem } from './assistant-ui/embrace-data/model'
import { workLogTurnFromItems } from './assistant-ui/taste/work-log'
import type { SyncStatus } from './assistant-ui/st3-views/sync-status'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { lightTheme, type Scheme } from './assistant-ui/composition-theme'
import { surfaceVars as surface, textVars as ink, typeVars as t } from './assistant-ui/composition-tokens.stylex'

const now = Date.UTC(2032, 0, 18, 12)
const sync: SyncStatus = { _tag: 'Live', since: now - 5000 }
const human = { kind: 'human', label: 'Operator' } as const
const worker = { kind: 'agent', label: 'Assistant' } as const
const at = (turn: number, offset: number) => new Date(now - (400 - turn) * 60_000 + offset).toISOString()
const areas = ['selection', 'focus', 'ordering', 'filtering', 'refresh'] as const

/** A settled turn: prompt, folded tool work, and a Markdown answer with lists and a TypeScript fence. */
function benchTurn(turn: number): TranscriptTurn {
  const area = areas[turn % areas.length]!
  const file = `src/${area}/rows-${turn + 1}.ts`
  const answer = [
    `## ${area[0]!.toUpperCase()}${area.slice(1)} check ${turn + 1}`,
    '',
    `The \`${area}\` model keeps row identity stable across refreshes. **Findings:**`,
    '',
    `- The selected key survives a reorder of ${turn + 12} rows.`,
    '- Keyboard focus returns to the *active* row after a refresh.',
    '  - Tab order still matches DOM order.',
    `- No change is needed in \`${file}\`.`,
    '',
    `1. Read \`${file}\`.`,
    '2. Ran the focused tests.',
    '',
    '```ts',
    `export function select${turn + 1}(rows: readonly Row[], key: string): Row | undefined {`,
    '  const index = rows.findIndex(row => row.key === key)',
    `  return index === -1 ? rows[${turn % 4}] : rows[index]`,
    '}',
    '```',
  ].join('\n')
  const prompt: TextItem & { role: 'user' } = { _tag: 'Text', id: `bench/${turn}/prompt`, role: 'user', text: `Check the ${area} behaviour in ${file} and summarize what holds.`, streaming: false, attachments: [], at: at(turn, 0), sender: human }
  const items: ConversationItem[] = [
    { _tag: 'ToolCall', id: `bench/${turn}/read`, callId: `bench/${turn}/read`, name: 'read', input: { path: file }, status: 'success', callSeen: true, at: at(turn, 1000), sender: worker, result: { content: `export const ${area}Rows = [] as const`, isError: false, mediaType: 'text/plain', at: at(turn, 2000) } },
    { _tag: 'ToolCall', id: `bench/${turn}/run`, callId: `bench/${turn}/run`, name: 'run', input: { command: `pnpm vitest ${area}.test.ts` }, status: 'success', callSeen: true, at: at(turn, 3000), sender: worker, result: { content: '4 passed', isError: false, mediaType: 'text/plain', at: at(turn, 9000) } },
    { _tag: 'Text', id: `bench/${turn}/answer`, role: 'assistant', text: answer, streaming: false, attachments: [], at: at(turn, 12_000), sender: worker },
  ]
  const work = workLogTurnFromItems([prompt, ...items], { kindFor: name => name === 'run' ? 'run' : 'read', running: false, failed: false, interrupted: false, durationMs: 12_000, startedAt: at(turn, 0), completeHistory: true })
  return { id: `bench/${turn}`, prompt, items, work }
}
const transcripts = new Map<number, readonly TranscriptTurn[]>()
const benchTurns = (count: number) => {
  let turns = transcripts.get(count)
  if (turns === undefined) {
    turns = Array.from({ length: count }, (_, turn) => benchTurn(turn))
    transcripts.set(count, turns)
  }
  return turns
}

const frame = () => new Promise<void>(resolve => requestAnimationFrame(() => resolve()))
const turnCount = (root: ParentNode) => root.querySelectorAll('[data-testid="transcript-turn"]').length
const completeTurnCount = (root: ParentNode) => root.querySelectorAll('[data-testid="user-message"]').length
/** Resolves once every turn is in the DOM, fonts are loaded and two frames have painted. */
async function settled(root: ParentNode, count: number): Promise<void> {
  while (completeTurnCount(root) < count) await frame()
  await document.fonts.ready
  await frame()
  await frame()
}

/** Imperative bench surface, read by the CDP tracing runner. Every call marks the start of a traced interval. */
export interface TranscriptBenchApi {
  /** Resolves when the mounted transcript holds every turn and has painted. */
  readonly settled: () => Promise<void>
  /**
   * First open: commits the pane synchronously (`bench:mount`). Frames are sampled until the first one with a
   * turn in the DOM (`bench:turns`) and the first one with every turn (`bench:all-turns`).
   */
  readonly mount: () => void
  readonly unmount: () => void
  /** Retains the mounted pane the way the app does: `content-visibility: hidden`. */
  readonly hide: () => void
  /** Switch back: reveals the retained pane without a React commit (`bench:reveal`). */
  readonly reveal: () => void
}
declare global {
  interface Window { __transcriptBench?: TranscriptBenchApi }
}

function TranscriptPane({ turns, history, onOpenTool }: { turns: readonly TranscriptTurn[]; history?: TranscriptHistory; onOpenTool?: TranscriptProps['onOpenTool'] }) {
  const messages = React.useMemo(() => turns.flatMap(turn => turn.prompt === undefined ? turn.items : [turn.prompt, ...turn.items]), [turns])
  const options = React.useMemo(() => ({ messages, isRunning: false, onNew: async () => {} }), [messages])
  return <EmbraceRuntimeProvider options={options}><Transcript title="Row checks" turns={turns} sync={sync} now={now} observedAt={now - 8000} history={history} onOpenTool={onOpenTool} /></EmbraceRuntimeProvider>
}

function TranscriptBench({ turns: count, scheme, history, earlier = 0, onOpenTool, initiallyMounted = true }: { turns: number; scheme: Scheme; history?: TranscriptHistory; earlier?: number; onOpenTool?: TranscriptProps['onOpenTool']; initiallyMounted?: boolean }) {
  // `earlier` older turns stay behind the history boundary until "Load earlier messages" prepends them.
  const series = benchTurns(count + earlier)
  const [loaded, setLoaded] = React.useState(false)
  const turns = React.useMemo(() => loaded ? series : series.slice(earlier), [series, loaded, earlier])
  const shownHistory: TranscriptHistory | undefined = earlier > 0 && !loaded ? { _tag: 'HasOlder', onLoadEarlier: () => setLoaded(true) } : history
  const [mounted, setMounted] = React.useState(initiallyMounted)
  const attach = React.useCallback((pane: HTMLDivElement | null) => {
    if (pane === null) return
    let sampling = 0
    const sample = (generation: number) => {
      let first = false
      const step = () => {
        if (generation !== sampling) return
        const present = turnCount(pane)
        if (!first && present > 0) { first = true; performance.mark('bench:turns') }
        if (completeTurnCount(pane) >= count) { performance.mark('bench:all-turns'); return }
        requestAnimationFrame(step)
      }
      requestAnimationFrame(step)
    }
    const api: TranscriptBenchApi = {
      settled: () => settled(pane, count),
      mount: () => { performance.mark('bench:mount'); flushSync(() => setMounted(true)); sample(++sampling) },
      unmount: () => { sampling++; flushSync(() => setMounted(false)) },
      hide: () => { pane.dataset.retained = 'hidden' },
      reveal: () => { performance.mark('bench:reveal'); delete pane.dataset.retained },
    }
    window.__transcriptBench = api
    return () => { sampling++; if (window.__transcriptBench === api) delete window.__transcriptBench }
  }, [count])
  return <main data-testid="transcript-bench" data-turns={count} data-scheme={scheme} {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}>
    <div ref={attach} data-testid="bench-pane" {...stylex.props(styles.pane)}>{mounted ? <TranscriptPane turns={turns} history={shownHistory} onOpenTool={onOpenTool} /> : null}</div>
  </main>
}

const meta = {
  title: 'Fractal UI/Transcript Bench',
  component: TranscriptBench,
  parameters: { layout: 'fullscreen', docs: { description: { component: 'Synthetic settled transcripts for the switch budget. The pane mounts the real `Transcript` and can be retained under `content-visibility: hidden` like an app pane. `scripts/transcript-bench.mjs` traces first open and switch back through CDP.' } } },
  args: { turns: 200, scheme: 'dark' },
  argTypes: { turns: { options: [50, 100, 200], control: 'radio' }, scheme: { options: ['dark', 'light'], control: 'radio' } },
} satisfies Meta<typeof TranscriptBench>
export default meta
type Story = StoryObj<typeof meta>
export const Turns50: Story = { args: { turns: 50 } }
export const Turns100: Story = { args: { turns: 100 } }
export const Turns200: Story = {}
export const Turns200Light: Story = { args: { scheme: 'light' } }

/** Opens the pane again in one synchronous commit and returns the turn count that commit mounted. */
async function reopen(root: ParentNode): Promise<number> {
  await settled(root, 200)
  window.__transcriptBench!.unmount()
  window.__transcriptBench!.mount()
  return turnCount(root)
}
const scroller = (root: ParentNode) => root.querySelector<HTMLElement>('[data-testid="transcript-scroll"]')!
const followGap = (scroll: HTMLElement) => scroll.scrollHeight - scroll.clientHeight - scroll.scrollTop
/** The turn under the viewport's vertical center and its offset from the viewport top. */
function centerTurn(scroll: HTMLElement): { readonly element: Element; readonly offset: number } {
  const bounds = scroll.getBoundingClientRect()
  const center = bounds.top + bounds.height / 2
  const element = [...scroll.querySelectorAll('[data-testid="transcript-turn"]')].find(turn => turn.getBoundingClientRect().bottom > center)!
  return { element, offset: element.getBoundingClientRect().top - bounds.top }
}
/** Samples every frame until all turns are mounted and returns the largest drift of the anchor turn. */
async function anchorDrift(root: ParentNode, scroll: HTMLElement, total: number): Promise<number> {
  const anchor = centerTurn(scroll)
  let drift = 0
  while (completeTurnCount(root) < total) {
    await frame()
    drift = Math.max(drift, Math.abs(anchor.element.getBoundingClientRect().top - scroll.getBoundingClientRect().top - anchor.offset))
  }
  return drift
}

/** First open commits only the newest page, follows the bottom, and backfills every turn without moving it. */
export const NewestPageFirst: Story = { play: async ({ canvasElement }) => {
  const first = await reopen(canvasElement)
  await expect(first).toBeGreaterThan(0)
  await expect(first).toBeLessThan(200)
  const scroll = scroller(canvasElement)
  await frame()
  await expect(followGap(scroll)).toBeLessThanOrEqual(1)
  await expect(turnCount(canvasElement)).toBeLessThan(200)
  await expect(await anchorDrift(canvasElement, scroll, 200)).toBeLessThanOrEqual(1)
  await frame()
  await expect(followGap(scroll)).toBeLessThanOrEqual(1)
  await expect(within(canvasElement).getAllByRole('region', { name: /^Work log bench\// })).toHaveLength(200)
} }
/** A reader who scrolled up keeps their turn in place while older turns land above it. */
export const BackfillKeepsReaderAnchor: Story = { play: async ({ canvasElement }) => {
  await expect(await reopen(canvasElement)).toBeLessThan(200)
  const scroll = scroller(canvasElement)
  while (completeTurnCount(canvasElement) < 6) await frame()
  await frame()
  scroll.dispatchEvent(new WheelEvent('wheel', { deltaY: -scroll.clientHeight / 2 }))
  scroll.scrollTop -= scroll.clientHeight / 2
  await frame()
  await expect(turnCount(canvasElement)).toBeLessThan(200)
  await expect(scroll.scrollTop).toBeGreaterThanOrEqual(scroll.clientHeight)
  await expect(await anchorDrift(canvasElement, scroll, 200)).toBeLessThanOrEqual(1)
  await expect(followGap(scroll)).toBeGreaterThan(scroll.clientHeight / 4)
} }
/** Find in page needs every turn: Mod+F mounts the rest synchronously. */
export const FindMountsAllTurns: Story = { play: async ({ canvasElement }) => {
  await expect(await reopen(canvasElement)).toBeLessThan(200)
  const scroll = scroller(canvasElement)
  await frame()
  const anchor = centerTurn(scroll)
  window.dispatchEvent(new KeyboardEvent('keydown', { key: 'f', ctrlKey: true }))
  await expect(turnCount(canvasElement)).toBe(200)
  await expect(Math.abs(anchor.element.getBoundingClientRect().top - scroll.getBoundingClientRect().top - anchor.offset)).toBeLessThanOrEqual(1)
} }
/** Opening a long transcript must follow before paint and keep the first distant placeholders at their real heights. */
export const InitialFollowWithoutLayoutShift: Story = { args: { turns: 50, initiallyMounted: false }, play: async ({ canvasElement, args }) => {
  const root = canvasElement.querySelector<HTMLElement>('[data-testid="transcript-bench"]')!
  const originalStyle = root.style.cssText
  // A short host lane exposes a late initial follow even when every distant turn keeps its correct intrinsic size.
  root.style.height = '400px'
  root.style.width = '800px'
  await frame()
  await frame()
  const shifts: number[] = []
  const recordShifts = (entries: readonly PerformanceEntry[]) => {
    for (const entry of entries) if ('value' in entry && typeof entry.value === 'number' && 'hadRecentInput' in entry && entry.hadRecentInput === false) shifts.push(entry.value)
  }
  const layout = new PerformanceObserver(list => recordShifts(list.getEntries()))
  layout.observe({ type: 'layout-shift' })
  const initial = new Map<Element, number>()
  const observed = new Set<Element>()
  const witness = new IntersectionObserver(entries => {
    for (const entry of entries) if (!initial.has(entry.target)) initial.set(entry.target, entry.boundingClientRect.height)
  })
  let beforePaint: ResizeObserver | undefined
  let prePaintFollowGap = 0
  const mutations = new MutationObserver(() => {
    const scroll = scroller(canvasElement)
    if (beforePaint === undefined && scroll?.firstElementChild !== null && scroll?.firstElementChild !== undefined) {
      // Register after the production observer: its settle callback must run before this pre-paint witness.
      beforePaint = new ResizeObserver(() => {
        if (turnCount(canvasElement) > 0) prePaintFollowGap = Math.max(prePaintFollowGap, followGap(scroll))
      })
      beforePaint.observe(scroll.firstElementChild)
    }
    for (const turn of canvasElement.querySelectorAll('[data-testid="transcript-turn"]')) if (!observed.has(turn)) {
      observed.add(turn)
      witness.observe(turn)
    }
  })
  mutations.observe(canvasElement, { childList: true, subtree: true })
  try {
    window.__transcriptBench!.mount()
    await settled(canvasElement, args.turns)
    for (let index = 0; index < 8; index++) await frame()
    const scroll = scroller(canvasElement)
    const distant = [...initial.keys()].filter(turn => turn.hasAttribute('data-distant'))
    await expect(initial.size).toBe(args.turns)
    await expect(distant.length).toBeGreaterThan(0)
    const skipped = distant.filter(turn => !turn.checkVisibility({ contentVisibilityAuto: true }))
    await expect(skipped.length).toBeGreaterThan(0)
    const drift = Math.max(...distant.map(turn => Math.abs(turn.getBoundingClientRect().height - initial.get(turn)!)))
    recordShifts(layout.takeRecords())
    const proof = { turns: args.turns, distantTurns: distant.length, skippedTurns: skipped.length, drift, viewportHeight: scroll.clientHeight, prePaintFollowGap, followGap: followGap(scroll), rawCls: shifts.reduce((total, value) => total + value, 0), shifts }
    canvasElement.dataset.layoutProof = JSON.stringify(proof)
    console.info('transcript-initial-follow-layout', proof)
    await expect(drift).toBeLessThanOrEqual(1)
    await expect(followGap(scroll)).toBeLessThanOrEqual(1)
    await expect(prePaintFollowGap).toBeLessThanOrEqual(1)
    await expect(proof.rawCls).toBeLessThanOrEqual(0.00001)
  } finally {
    mutations.disconnect()
    witness.disconnect()
    layout.disconnect()
    beforePaint?.disconnect()
    root.style.cssText = originalStyle
  }
} }
const loadEarlier = fn()
/** Reaching the top reveals a bounded older chunk; idle backfill eventually makes the boundary reachable. */
export const TopMountsOlderTurns: Story = { args: { history: { _tag: 'HasOlder', onLoadEarlier: loadEarlier } }, play: async ({ canvasElement }) => {
  loadEarlier.mockClear()
  await expect(await reopen(canvasElement)).toBeLessThan(200)
  const scroll = scroller(canvasElement)
  await frame()
  scroll.dispatchEvent(new WheelEvent('wheel', { deltaY: -scroll.scrollHeight }))
  scroll.scrollTop = 0
  await frame()
  await frame()
  await expect(turnCount(canvasElement)).toBeGreaterThan(2)
  await expect(turnCount(canvasElement)).toBeLessThan(200)
  await settled(canvasElement, 200)
  scroll.scrollTop = 0
  const boundary = await within(canvasElement).findByTestId('history-boundary')
  await expect(boundary.compareDocumentPosition(canvasElement.querySelector('[data-testid="transcript-turn"]')!) & Node.DOCUMENT_POSITION_FOLLOWING).not.toBe(0)
  await expect(canvasElement.querySelector('[data-testid="transcript-turn"]')).toHaveAttribute('data-item-id', 'bench/0')
  await userEvent.click(within(boundary).getByRole('button', { name: 'Load earlier messages' }))
  await expect(loadEarlier).toHaveBeenCalledTimes(1)
} }
/** Backfill and prepended history keep every mounted turn: its DOM node, an open disclosure and focus survive both. */
export const BackfillKeepsTurnState: Story = { args: { earlier: 40, onOpenTool: fn() }, play: async ({ canvasElement }) => {
  await expect(await reopen(canvasElement)).toBeLessThan(200)
  const turn = [...canvasElement.querySelectorAll('[data-testid="transcript-turn"]')].at(-1)!
  const disclosure = turn.querySelector<HTMLElement>('button[aria-expanded="false"]')!
  await userEvent.click(disclosure)
  disclosure.focus()
  await expect(disclosure).toHaveAttribute('aria-expanded', 'true')
  await expect(disclosure).toHaveFocus()
  while (completeTurnCount(canvasElement) < 200) await frame()
  await expect(turn.isConnected).toBe(true)
  await expect(disclosure.isConnected).toBe(true)
  await expect(disclosure).toHaveAttribute('aria-expanded', 'true')
  await expect(disclosure).toHaveFocus()
  // A virtual click presses the boundary button without moving focus off the disclosure.
  within(await within(canvasElement).findByTestId('history-boundary')).getByRole('button', { name: 'Load earlier messages' }).click()
  // Prepended turns backfill above the reader like the first older turns did.
  while (completeTurnCount(canvasElement) < 240) await frame()
  await expect(canvasElement.querySelector('[data-testid="transcript-turn"]')).toHaveAttribute('data-item-id', 'bench/0')
  await expect(turn.isConnected).toBe(true)
  await expect(disclosure.isConnected).toBe(true)
  await expect(disclosure).toHaveAttribute('aria-expanded', 'true')
  await expect(disclosure).toHaveFocus()
} }

const styles = stylex.create({
  root: { height: '100vh', width: '100%', minWidth: 0, boxSizing: 'border-box', display: 'flex', flexDirection: 'column', overflow: 'hidden', backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans },
  pane: { display: 'flex', flexDirection: 'column', flex: '1 1 0', minWidth: 0, minHeight: 0, overflow: 'hidden', contentVisibility: { default: 'visible', ':is([data-retained="hidden"])': 'hidden' } },
})
