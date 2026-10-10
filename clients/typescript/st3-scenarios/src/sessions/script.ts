import { decodeUnknownSync, TimelineEntry, type TimelineEntryEncoded } from '@smalltalk/st3-client/schema'
import { Schema } from 'effect'
import { worldNow } from './clock.ts'

/** The authored step vocabulary from fractal-web/conversation/fixtures.ts, not a wire schema. */
export type Step =
  | { readonly kind: 'user'; readonly t: number; readonly text: string }
  | { readonly kind: 'say'; readonly t: number; readonly text: string; readonly final?: boolean }
  | { readonly kind: 'think'; readonly t: number; readonly ms: number; readonly text: string }
  | { readonly kind: 'tool'; readonly t: number; readonly end?: number; readonly id: string; readonly name: string; readonly input: unknown; readonly output?: string; readonly isError?: boolean }
  | { readonly kind: 'mail'; readonly t: number; readonly id: string; readonly from: string; readonly to?: string; readonly title: string }
  | { readonly kind: 'status'; readonly t: number; readonly status: 'queued' | 'running' | 'waiting' | 'completed' | 'failed' | 'cancelled'; readonly detail?: string }

export interface Script {
  readonly id: string
  readonly startedMinutesAgo: number
  readonly steps: readonly Step[]
}

export const sessionAgentRef = 'agent/example/cedar/session-lab'
export const stepTimestamp = (script: Script, seconds: number): string =>
  new Date(worldNow - script.startedMinutesAgo * 60_000 + seconds * 1000).toISOString()

/** Keep the app script's emission semantics, but use only the existing production union.
 * Native reasoning is content (`[reasoning]`), never the app's proposed D08 schema.
 */
export const encodeSteps = (script: Script): readonly (readonly TimelineEntryEncoded[])[] => {
  let sequence = 0
  const entry = (step: Step, role: TimelineEntryEncoded['role'], type: string, body: unknown, final = true, t = step.t): TimelineEntryEncoded => {
    const raw = {
      id: ['timeline-entry', `example-${script.id}`, String(++sequence)].join('/'),
      sequence, revision: 1, timestamp: stepTimestamp(script, t), role, type, body, final,
    }
    // Decoding is mandatory even for consumers that only request encoded frames.
    return Schema.encodeSync(TimelineEntry)(decodeUnknownSync(TimelineEntry, 'strict')(raw))
  }
  return script.steps.map(step => {
    switch (step.kind) {
      case 'user': return [
        entry(step, 'user', 'message', { message_id: ['message', `example-${script.id}`, String(sequence + 1)].join('/'), from: 'person/operator', to: sessionAgentRef }),
        entry(step, 'user', 'content', { media_type: 'text/markdown', text: step.text }),
      ]
      case 'say': return [entry(step, 'assistant', 'content', { media_type: 'text/markdown', text: step.text }, step.final ?? true)]
      case 'think': return [entry(step, 'assistant', 'content', { media_type: 'text/plain', text: `[reasoning]\n${step.text}` })]
      case 'tool': return [
        entry(step, 'assistant', 'tool_call', { call_id: step.id, name: step.name, arguments: step.input }),
        ...(step.output === undefined ? [] : [entry(step, 'tool', 'tool_result', { call_id: step.id, status: step.isError === true ? 'error' : 'success', media_type: 'text/plain', content: step.output }, true, step.end ?? step.t)]),
      ]
      case 'mail': return [entry(step, 'system', 'message', { message_id: step.id, from: step.from, to: step.to ?? sessionAgentRef, title: step.title })]
      case 'status': return [entry(step, 'system', 'status', { status: step.status, ...(step.detail === undefined ? {} : { detail: step.detail }) })]
    }
  })
}

export const decodeSteps = (script: Script): readonly (readonly TimelineEntry[])[] =>
  encodeSteps(script).map(entries => entries.map(decodeUnknownSync(TimelineEntry, 'strict')))
