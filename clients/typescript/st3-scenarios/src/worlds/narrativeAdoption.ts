import type { TimelineEntry } from '@smalltalk/st3-client'
import ciRows from '../../recordings/ci-failing-fix.normalized.json' with { type: 'json' }
import ciScript from '../../recordings/ci-failing-fix.script.json' with { type: 'json' }
import firstRows from '../../recordings/first-run-onboarding.normalized.json' with { type: 'json' }
import firstScript from '../../recordings/first-run-onboarding.script.json' with { type: 'json' }
import fleetRows from '../../recordings/fleet-mid-refactor.normalized.json' with { type: 'json' }
import fleetScript from '../../recordings/fleet-mid-refactor.script.json' with { type: 'json' }
import longRows from '../../recordings/long-debug-rabbit-hole.normalized.json' with { type: 'json' }
import longScript from '../../recordings/long-debug-rabbit-hole.script.json' with { type: 'json' }
import mergeRows from '../../recordings/merge-conflict-standoff.normalized.json' with { type: 'json' }
import mergeScript from '../../recordings/merge-conflict-standoff.script.json' with { type: 'json' }
import offlineRows from '../../recordings/offline-reconnect-storm.normalized.json' with { type: 'json' }
import offlineScript from '../../recordings/offline-reconnect-storm.script.json' with { type: 'json' }
import releaseRows from '../../recordings/release-candidate-polish.normalized.json' with { type: 'json' }
import releaseScript from '../../recordings/release-candidate-polish.script.json' with { type: 'json' }
import { importNormalizedRecording, rebaseImportedSlices } from '../kit/importRecording.ts'
import type { Slice } from '../kit/slice.ts'
import { ANCHOR_MS } from '../kit/time.ts'
import type { Slices, WorldDefinition } from '../kit/world.ts'

type NarrativeSlices = Pick<Slices, 'conversation' | 'terminal'>
interface Capture {
  readonly script: unknown
  readonly rows: unknown
  anchor?: NarrativeSlices
  latest?: { readonly now: number; readonly slices: NarrativeSlices }
}
const captures: Readonly<Record<string, Capture>> = {
  'fleet-mid-refactor': { script: fleetScript, rows: fleetRows },
  'ci-failing-fix': { script: ciScript, rows: ciRows },
  'long-debug-rabbit-hole': { script: longScript, rows: longRows },
  'first-run-onboarding': { script: firstScript, rows: firstRows },
  'merge-conflict-standoff': { script: mergeScript, rows: mergeRows },
  'offline-reconnect-storm': { script: offlineScript, rows: offlineRows },
  'release-candidate-polish': { script: releaseScript, rows: releaseRows },
}

/** At most two imported values per capture: the pinned anchor and the last other clock. */
const capturedSlices = (capture: Capture, now: number): NarrativeSlices => {
  if (capture.latest?.now === now) return capture.latest.slices
  if (!capture.anchor) {
    const imported = importNormalizedRecording(capture.script, capture.rows, ANCHOR_MS).slices
    capture.anchor = {
      conversation: {
        ...imported.conversation,
        // Product replay pages the accumulated history; this is not a captured frame flag.
        state: { threads: imported.conversation.state.threads.map((thread) => ({
          ...thread, has_more: thread.has_more || thread.items.length > thread.page_size,
        })) },
      },
      terminal: { ...imported.terminal, state: { terminals: imported.terminal.state.terminals } },
    }
  }
  if (now === ANCHOR_MS) return capture.anchor
  const slices = rebaseImportedSlices(capture.anchor, now)
  capture.latest = { now, slices }
  return slices
}

/** One source per slice: a splice is synthetic even when its unchanged prefix is recorded. */
export const NARRATIVE_SPLICES = {
  'fleet-mid-refactor': { point: 'thread ownership', recorded: 'builder conversation and terminal', synthetic: 'entire migrator thread and revision/diff timeline' },
  'offline-reconnect-storm': { point: 'before Readiness: 2 of 3 replicas', recorded: 'historical deployment introduction and terminal', synthetic: 'report revision 1–5, transient observation, authoritative replacements at 6000/13000/22000 ms' },
} as const

const fleetConversation = (base: Slice<'conversation'>, captured: Slice<'conversation'>, migrator: string): Slice<'conversation'> => ({
  ...base,
  state: { threads: [...captured.state.threads.filter((thread) => thread.agent !== migrator), ...base.state.threads.filter((thread) => thread.agent === migrator)] },
  timeline: [...captured.timeline.filter((event) => event._tag === 'thread-create' ? event.thread.agent !== migrator : event.agent !== migrator),
    ...base.timeline.filter((event) => event._tag === 'thread-create' ? event.thread.agent === migrator : event.agent === migrator)]
    .sort((a, b) => a.at_ms - b.at_ms),
})

const offlineConversation = (base: Slice<'conversation'>, captured: Slice<'conversation'>): Slice<'conversation'> => {
  const original = base.state.threads[0]!
  const report = original.items.at(-1)!
  const recorded = captured.state.threads.find((thread) => thread.agent === original.agent)!
  const boundary = recorded.items.find((item) => item.type === 'content' &&
    typeof item.body === 'object' && item.body !== null && 'text' in item.body &&
    item.body.text === 'Readiness: 2 of 3 replicas')
  if (!boundary) throw new Error('Offline recording has no declared report splice point')
  const prefix = recorded.items.filter((item) => item.sequence < boundary.sequence)
  const shift = Math.max(0, ...prefix.map((item) => item.sequence)) + 1 - report.sequence
  const synthetic = (item: TimelineEntry): TimelineEntry => ({ ...item, sequence: item.sequence + shift })
  const historyIds = new Set(original.items.slice(0, -1).map((item) => item.id))
  return {
    ...base,
    state: { threads: [{ ...original, items: [...prefix, synthetic(report)] }] },
    timeline: base.timeline.map((event) => event._tag === 'replace' ? {
      ...event, items: [...prefix, ...event.items.filter((item) => !historyIds.has(item.id)).map(synthetic)],
    } : event._tag === 'entries' ? { ...event, items: event.items.map(synthetic) } : event),
  }
}

/** Legacy authoring remains the immutable capture-script source; runtime adopts normalized inputs. */
export const adoptNarrativeRecording = (definition: WorldDefinition): WorldDefinition => {
  const capture = captures[definition.id]
  if (!capture) throw new Error(`Missing narrative recording: ${definition.id}`)
  return {
    ...definition,
    slices: (ctx) => {
      const base = definition.slices(ctx)
      // Historical wire-window copies belong to fidelity proofs, not the product's full history.
      const captured = capturedSlices(capture, ctx.t.now)
      const recordedConversation = captured.conversation
      const terminal = captured.terminal
      const conversation = definition.id === 'fleet-mid-refactor'
        ? fleetConversation(base.conversation, recordedConversation, ctx.cast.agents.find((member) => member.key === 'migrator')!.id)
        : definition.id === 'offline-reconnect-storm' ? offlineConversation(base.conversation, recordedConversation) : recordedConversation
      // No recorded transport metadata: production deliberately keeps synthetic fleet/sync clocks.
      return { ...base, conversation, terminal }
    },
  }
}
