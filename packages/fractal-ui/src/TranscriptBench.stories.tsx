import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { flushSync } from 'react-dom'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { Transcript, type TranscriptHistory, type TranscriptTurn } from './assistant-ui/composition/Transcript'
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
/** Resolves once every turn is in the DOM, fonts are loaded and two frames have painted. */
async function settled(root: ParentNode, count: number): Promise<void> {
  while (turnCount(root) < count) await frame()
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

function TranscriptPane({ turns, history }: { turns: readonly TranscriptTurn[]; history?: TranscriptHistory }) {
  const messages = React.useMemo(() => turns.flatMap(turn => turn.prompt === undefined ? turn.items : [turn.prompt, ...turn.items]), [turns])
  const options = React.useMemo(() => ({ messages, isRunning: false, onNew: async () => {} }), [messages])
  return <EmbraceRuntimeProvider options={options}><Transcript title="Row checks" turns={turns} sync={sync} now={now} observedAt={now - 8000} history={history} /></EmbraceRuntimeProvider>
}

function TranscriptBench({ turns: count, scheme, history }: { turns: number; scheme: Scheme; history?: TranscriptHistory }) {
  const turns = benchTurns(count)
  const [mounted, setMounted] = React.useState(true)
  const attach = React.useCallback((pane: HTMLDivElement | null) => {
    if (pane === null) return
    let sampling = 0
    const sample = (generation: number) => {
      let first = false
      const step = () => {
        if (generation !== sampling) return
        const present = turnCount(pane)
        if (!first && present > 0) { first = true; performance.mark('bench:turns') }
        if (present >= count) { performance.mark('bench:all-turns'); return }
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
    <div ref={attach} data-testid="bench-pane" {...stylex.props(styles.pane)}>{mounted ? <TranscriptPane turns={turns} history={history} /> : null}</div>
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

const styles = stylex.create({
  root: { height: '100vh', width: '100%', minWidth: 0, boxSizing: 'border-box', display: 'flex', flexDirection: 'column', overflow: 'hidden', backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans },
  pane: { display: 'flex', flexDirection: 'column', flex: '1 1 0', minWidth: 0, minHeight: 0, overflow: 'hidden', contentVisibility: { default: 'visible', ':is([data-retained="hidden"])': 'hidden' } },
})
