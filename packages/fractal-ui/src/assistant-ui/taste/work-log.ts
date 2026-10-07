/** Controlled presentation types for the work log. */
import type { ConversationItem } from '../embrace-data/model'
export type WorkKind = 'read' | 'run' | 'edit'
export interface WorkLogCall {
  readonly id: string
  readonly kind: WorkKind
  readonly title: string
  readonly argsSummary?: string
  readonly status: 'running' | 'success' | 'error' | 'interrupted'
  readonly startedAt: string
  readonly endedAt?: string
  readonly detail?: string
}
export interface WorkLogTurn {
  readonly calls: readonly WorkLogCall[]
  /** Verified whole-run duration only. Missing means unknown, never a tool-span estimate. */
  readonly durationMs: number | undefined
  readonly running: boolean
  readonly failed: boolean
  readonly interrupted: boolean
  readonly failureNote?: string
}
export interface WorkLogProjection {
  readonly id: string
  readonly mode: 'settled' | 'running'
  /** Exact source references, validated against the committed runtime before display. */
  readonly items: readonly ConversationItem[]
  readonly turn: WorkLogTurn
}
const summarizeArgs = (input: unknown): string | undefined => {
  if (typeof input !== 'object' || input === null) return undefined
  const record = input as Record<string, unknown>
  for (const key of ['path', 'command', 'pattern', 'query', 'url']) {
    const value = record[key]
    if (typeof value === 'string' && value.length > 0) return value
  }
  return undefined
}
/** Selected workshop call projection; the host owns classification, lifecycle and timing. */
export function workLogTurnFromItems(items: readonly ConversationItem[], facts: {
  readonly kindFor: (name: string) => WorkKind
  readonly running: boolean
  readonly failed: boolean
  readonly interrupted: boolean
  readonly durationMs?: number
  readonly failureNote?: string
}): WorkLogTurn {
  return {
    calls: items.flatMap(call => call._tag !== 'ToolCall' ? [] : [{
      id: call.id, kind: facts.kindFor(call.name), title: call.name,
      argsSummary: summarizeArgs(call.input), status: call.status, startedAt: call.at,
      endedAt: call.result?.at, detail: typeof call.result?.content === 'string' ? call.result.content : undefined,
    }]),
    durationMs: facts.durationMs !== undefined && Number.isFinite(facts.durationMs) && facts.durationMs >= 0 ? facts.durationMs : undefined,
    running: facts.running, failed: facts.failed, interrupted: facts.interrupted,
    failureNote: facts.failureNote,
  }
}
export function formatWorkDuration(milliseconds: number | undefined): string {
  if (milliseconds === undefined || !Number.isFinite(milliseconds) || milliseconds < 0) return ''
  const seconds = Math.floor(milliseconds / 1000)
  if (seconds < 1) return ''
  if (seconds < 60) return `${seconds}s`
  if (seconds < 3600) return `${Math.floor(seconds / 60)}m${seconds % 60 === 0 ? '' : ` ${seconds % 60}s`}`
  return `${Math.floor(seconds / 3600)}h${Math.floor(seconds / 60) % 60 === 0 ? '' : ` ${Math.floor(seconds / 60) % 60}m`}`
}
/** Never let a newer caller projection fold an older committed message snapshot. */
export function committedWorkLogs(items: readonly ConversationItem[], projections: readonly WorkLogProjection[]): ReadonlyMap<number, WorkLogProjection> {
  const positions = new Map(items.map((item, index) => [item.id, index]))
  const result = new Map<number, WorkLogProjection>()
  const occupied = new Set<number>()
  for (const projection of projections) {
    const start = projection.items[0] === undefined ? undefined : positions.get(projection.items[0].id)
    if (start === undefined || projection.items.some((item, offset) => items[start + offset] !== item || occupied.has(start + offset))) continue
    if (projection.mode === 'settled' && (projection.turn.calls.length !== projection.items.length || projection.turn.calls.some((call, offset) => call.id !== projection.items[offset]?.id) || projection.turn.running || projection.turn.failed || projection.turn.interrupted || projection.items.some(item => item._tag !== 'ToolCall' || item.status !== 'success'))) continue
    if (projection.mode === 'running' && (!projection.turn.running || projection.items.length !== 1)) continue
    projection.items.forEach((_, offset) => occupied.add(start + offset))
    result.set(start, projection)
  }
  return result
}
