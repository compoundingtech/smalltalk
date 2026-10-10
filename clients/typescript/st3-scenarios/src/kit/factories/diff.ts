import type { TimelineEntry } from '@smalltalk/st3-client'

import type { FactoryContext } from '../context.ts'
import { toolCall } from './toolCall.ts'
import type { Thread } from './turn.ts'

const lines = (text: string) => text.replace(/\n$/, '').split('\n')

/** Unified diff of a whole-hunk replacement. */
export const unifiedDiff = (file: string, before: string, after: string): string => {
  const a = lines(before)
  const b = lines(after)
  return [`--- a/${file}`, `+++ b/${file}`, `@@ -1,${a.length} +1,${b.length} @@`, ...a.map((line) => `-${line}`), ...b.map((line) => `+${line}`), ''].join('\n')
}

export interface DiffInput {
  readonly atMs: number
  readonly file: string
  readonly before: string
  readonly after: string
  readonly outcome?: 'ok' | 'pending'
}

/** An edit tool call whose result carries the unified diff of the edit. */
export const diff = (ctx: FactoryContext, cursor: Thread, input: DiffInput): TimelineEntry[] =>
  toolCall(ctx, cursor, {
    atMs: input.atMs,
    name: 'edit',
    arguments: { path: input.file, old_string: input.before, new_string: input.after },
    outcome: input.outcome ?? 'ok',
    output: unifiedDiff(input.file, input.before, input.after),
    durationMs: 300,
  })
