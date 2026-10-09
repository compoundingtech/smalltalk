import * as React from 'react'
import type { ConversationItem, TextItem } from '../embrace-data/model'
import type { ConversationRuntimeOptions } from '../EmbraceRuntime'
import type { TranscriptTurn } from '../composition/Transcript'
import type { WorkLogTurn } from '../taste/work-log'
import type { DiffFile } from '../composition/DiffPanel'
import type { ThreadResource, WorkbenchResources } from './workbench-model'
import { group, split, paneTitle } from './workbench-model'
import type { WorkbenchAppearance, WorkbenchPaneDetails } from './workbench-appearance'
import { terminalFixtureFrame } from './terminal-fixtures'

type NewMessage = Parameters<NonNullable<ConversationRuntimeOptions['onNew']>>[0]
/** Only the stateful story host can turn these facts into a resource with a real submit capability. */
type FixtureThreadSnapshot = Omit<ThreadResource, 'runtime'> & { readonly runtime: Omit<ConversationRuntimeOptions, 'onNew'> }

export const decidedAppearance: WorkbenchAppearance = { width: 'W3', header: 'H1', chrome: 'P3', dropZones: 'D2' }
export const fixtureNow = Date.parse('2026-10-09T12:00:30.000Z')
const at = '2026-10-09T12:00:00.000Z'
const settledWork: WorkLogTurn = { calls: [], durationMs: 2000, running: false, failed: false, interrupted: false, foldable: true }
const textItem = (id: string, role: 'user' | 'assistant', text: string): TextItem => ({ _tag: 'Text', id, role, text, attachments: [], streaming: false, at })
const initialTurns = (worker: string): readonly TranscriptTurn[] => Array.from({ length: 6 }, (_, index) => {
  const id = `${worker}-turn-${index}`
  const prompt = textItem(`${id}-prompt`, 'user', `Review sample rows, pass ${index + 1}.`) as TextItem & { role: 'user' }
  const answer = textItem(`${id}-answer`, 'assistant', `The sample contains four rows. Pass ${index + 1} keeps the ordering and preserves the caller-owned snapshot.

The change is limited to the selected file; no transport or repository operation is performed by this deterministic fixture.

\`\`\`typescript
const rows = [1, 2, 3, 4]
\`\`\``)
  return { id, prompt, items: [answer], work: settledWork, senderCaptions: { [answer.id]: worker } }
})
const livePrompt = textItem('worker-1-live-prompt', 'user', 'Check the workbench snapshot before finishing.') as TextItem & { role: 'user' }
const liveCall: ConversationItem = { _tag: 'ToolCall', id: 'worker-1-live-call', callId: 'fixture-read', name: 'Read', input: { path: 'sample/rows.ts' }, status: 'running', callSeen: true, at }
const liveTurn: TranscriptTurn = { id: 'worker-1-live', prompt: livePrompt, items: [liveCall], work: { calls: [{ id: liveCall.id, kind: 'read', title: 'Read sample/rows.ts', argsSummary: 'sample/rows.ts', status: 'running', startedAt: at, detail: 'Reading the observed fixture snapshot.' }], durationMs: undefined, running: true, failed: false, interrupted: false, startedAt: at, foldable: true } }
const projectThread = (turns: readonly TranscriptTurn[], running: boolean): FixtureThreadSnapshot => ({
  runtime: { messages: turns.flatMap(turn => [...(turn.prompt ? [turn.prompt] : []), ...turn.items]), isRunning: running },
  transcript: { turns, sync: { _tag: 'Live', since: fixtureNow - 30000 }, now: fixtureNow, observedAt: fixtureNow },
})
const currentTurnFiles: readonly DiffFile[] = [{ path: 'sample/rows.ts', diff: ['@@ -1,2 +1,3 @@', '-const rows = [1, 2, 3]', '+const rows = [1, 2, 3, 4]', '+', ' export { rows }'], added: 2, removed: 1 }]
const branchFiles: readonly DiffFile[] = [...currentTurnFiles, { path: 'sample/README.md', diff: ['@@ -1 +1,3 @@', ' # Synthetic sample', '+', '+Four rows are ready for review.'], added: 2, removed: 0 }]
const initialWorkbenchResources = {
  threads: new Map([['agent:worker-1', projectThread([...initialTurns('worker-1'), liveTurn], true)], ['agent:worker-2', projectThread(initialTurns('worker-2'), false)]]),
  diffs: new Map([['diff:sample/rows.ts', { lines: currentTurnFiles[0]!.diff, path: 'sample/rows.ts', added: 2, removed: 1, currentTurnFiles, branchFiles }]]),
  terminals: terminalFixtureFrame,
}
export const onePaneLayout = group([{ uri: 'agent:worker-1' }])
export const twoPaneLayout = split('right', onePaneLayout, group([{ uri: 'diff:sample/rows.ts', form: 'unified' }]), 0.62)
/** Same resource URI in independent host-owned view instances; no details surface is imported. */
export const nestedLayout = split('right', onePaneLayout,
  split('below', split('right', group([{ uri: 'agent:worker-1', view: 'secondary' }]), group([{ uri: 'agent:worker-2' }]), 0.7), group([{ uri: 'diff:sample/rows.ts', form: 'unified' }]), 0.3), 0.62)

/** Story host owns explicit turn/command facts and deterministic local mutations. */
export function useWorkbenchFixture() {
  const [threads, setThreads] = React.useState(initialWorkbenchResources.threads)
  const sequence = React.useRef(0)
  const send = React.useCallback((uri: string, text: string) => {
    const number = ++sequence.current
    const id = `fixture-send-${number}`
    const prompt = textItem(`${id}-prompt`, 'user', text) as TextItem & { role: 'user' }
    const answer = textItem(`${id}-answer`, 'assistant', 'Recorded by the local workbench fixture. The host owns this new turn.')
    setThreads(current => {
      const thread = current.get(uri)!
      const turns = [...thread.transcript.turns, { id, prompt, items: [answer], work: settledWork }]
      const next = new Map(current)
      next.set(uri, projectThread(turns, thread.runtime.isRunning === true))
      return next
    })
  }, [])
  const stop = React.useCallback((uri: string) => setThreads(current => {
    const thread = current.get(uri)!
    const turns = thread.transcript.turns.map(turn => turn.work.running ? { ...turn, items: turn.items.map(item => item._tag === 'ToolCall' && item.status === 'running' ? { ...item, status: 'interrupted' as const } : item), work: { ...turn.work, running: false, interrupted: true, calls: turn.work.calls.map(call => call.status === 'running' ? { ...call, status: 'interrupted' as const, endedAt: new Date(fixtureNow).toISOString() } : call) } } : turn)
    const next = new Map(current)
    next.set(uri, projectThread(turns, false))
    return next
  }), [])
  const resources = React.useMemo<WorkbenchResources>(() => ({
    ...initialWorkbenchResources,
    threads: new Map([...threads].map(([uri, thread]) => [uri, {
      ...thread,
      runtime: {
        ...thread.runtime,
        onNew: async (message: NewMessage) => send(uri, message.content.filter(part => part.type === 'text').map(part => part.text).join('\n')),
        onCancel: async () => stop(uri),
      },
    }])),
    threadControls: new Map([...threads].map(([uri]) => [uri, {
      folder: 'sample', branch: 'fixture/workbench',
      onSend: (text: string) => send(uri, text),
      onSteer: (text: string) => send(uri, text),
      onStop: () => stop(uri),
    }])),
  }), [threads, send, stop])
  const describePane = React.useCallback((pane: { uri: string; view?: string }): WorkbenchPaneDetails => {
    const thread = threads.get(pane.uri)
    return { title: pane.uri.startsWith('diff:') ? 'Changes' : paneTitle(pane), status: thread === undefined ? undefined : thread.runtime.isRunning ? 'Working' : thread.transcript.turns.some(turn => turn.work.interrupted) ? 'Stopped' : 'Done', statusTone: thread?.runtime.isRunning ? 'running' : 'neutral', onStop: thread?.runtime.isRunning ? () => stop(pane.uri) : undefined }
  }, [threads, stop])
  return { resources, describePane }
}
