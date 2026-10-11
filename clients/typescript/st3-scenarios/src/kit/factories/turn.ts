import type { TimelineEntry } from '@smalltalk/st3-client'

import type { CastAgent, CastPerson } from '../cast.ts'
import type { FactoryContext } from '../context.ts'
import { scenarioId } from '../context.ts'

/** Sequence cursor of one agent's conversation; factories append to it in order. */
export interface Thread {
  readonly agent: CastAgent
  sequence: number
}

export const thread = (agent: CastAgent, sequence = 0): Thread => ({ agent, sequence })

export interface EntryInput {
  readonly atMs: number
  readonly role: TimelineEntry['role']
  readonly type: string
  readonly body: Record<string, unknown>
  readonly final?: boolean
  readonly revision?: number
}

/** Appends one entry with the next sequence. */
export const entry = (ctx: FactoryContext, cursor: Thread, input: EntryInput): TimelineEntry => {
  cursor.sequence += 1
  return {
    id: scenarioId(ctx, 'timeline-entry', `${cursor.agent.key}-${cursor.sequence}`),
    sequence: cursor.sequence,
    revision: input.revision ?? 1,
    timestamp: ctx.t.at(input.atMs),
    role: input.role,
    type: input.type,
    final: input.final ?? true,
    body: input.body,
  } as TimelineEntry
}

/** A later revision of `previous`: same id, sequence, role and type. */
export const revise = (ctx: FactoryContext, previous: TimelineEntry, atMs: number, body: Record<string, unknown>, final: boolean): TimelineEntry =>
  ({ ...previous, revision: previous.revision + 1, timestamp: ctx.t.at(atMs), final, body }) as TimelineEntry

export type TurnStep =
  | { readonly _tag: 'say'; readonly text: string }
  | { readonly _tag: 'entries'; readonly build: (atMs: number) => TimelineEntry[] }

export interface TurnInput {
  readonly atMs: number
  /** A person's message, or mail from another agent. */
  readonly from: { readonly _tag: 'person'; readonly person: CastPerson } | { readonly _tag: 'mail'; readonly agent: CastAgent; readonly title: string }
  readonly text: string
  readonly steps: readonly TurnStep[]
  /** Final status; omitted while the turn is still running. */
  readonly status?: 'completed' | 'failed' | 'waiting'
  /** Milliseconds between consecutive entries. */
  readonly stepMs?: number
}

/** One exchange: the inbound message, assistant content and tool calls, then a status entry. */
export const turn = (ctx: FactoryContext, cursor: Thread, input: TurnInput): TimelineEntry[] => {
  const stepMs = input.stepMs ?? 4_000
  let at = input.atMs
  const out: TimelineEntry[] = []
  const messageId = scenarioId(ctx, 'message', `${cursor.agent.key}-${cursor.sequence + 1}`)
  const from = input.from._tag === 'person' ? input.from.person.id : input.from.agent.id
  out.push(
    entry(ctx, cursor, {
      atMs: at,
      role: 'user',
      type: 'message',
      body: {
        message_id: messageId,
        from,
        to: cursor.agent.id,
        ...(input.from._tag === 'mail' ? { title: input.from.title, tags: ['mail'] } : {}),
      },
    }),
    entry(ctx, cursor, { atMs: at, role: 'user', type: 'content', body: { media_type: 'text/plain', text: input.text } }),
  )
  for (const step of input.steps) {
    at += stepMs
    if (step._tag === 'say') {
      out.push(entry(ctx, cursor, { atMs: at, role: 'assistant', type: 'content', body: { media_type: 'text/markdown', text: step.text } }))
    } else out.push(...step.build(at))
  }
  if (input.status !== undefined) {
    at += stepMs
    out.push(entry(ctx, cursor, { atMs: at, role: 'system', type: 'status', body: { status: input.status } }))
  }
  return out
}
