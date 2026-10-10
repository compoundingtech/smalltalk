/** Controlled presentation types for the work log. */
import type { ConversationItem } from '../embrace-data/model'
import { toolOutput } from '../embrace-tool-preview'
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
  /** Verbatim tool input (pretty JSON, or the raw string). Shown only in an expanded tool detail, never in the row label or failure overlay. */
  readonly rawInput?: string
  readonly outputLanguage?: string
  /** Observed edit/write path, not a guessed title. */
  readonly changedPath?: string
}
export interface WorkLogTurn {
  readonly calls: readonly WorkLogCall[]
  /** Verified whole-run duration only. Missing means unknown, never a tool-span estimate. */
  readonly durationMs: number | undefined
  readonly running: boolean
  readonly failed: boolean
  readonly interrupted: boolean
  readonly failureNote?: string
  readonly startedAt?: string
  /** False keeps incomplete or multi-participant history expanded. */
  readonly foldable?: boolean
  readonly commands?: number
  readonly changedFiles?: number
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
  readonly startedAt?: string
  readonly completeHistory?: boolean
}): WorkLogTurn {
  const calls: WorkLogCall[] = items.flatMap(call => {
    if (call._tag !== 'ToolCall') return []
    const kind = facts.kindFor(call.name)
    const input = typeof call.input === 'object' && call.input !== null ? call.input as Record<string, unknown> : undefined
    const path = [input?.['path'], input?.['file_path'], input?.['filePath']].find((value): value is string => typeof value === 'string' && value.length > 0)
    const media = call.result?.mediaType?.split(';')[0]?.trim().toLowerCase()
    const filename = path?.split('/').at(-1) ?? ''
    const dot = filename.lastIndexOf('.')
    const extension = dot > 0 ? filename.slice(dot + 1).toLowerCase() : undefined
    return [{
      id: call.id, kind, title: call.name, argsSummary: summarizeArgs(call.input),
      // Empty input carries nothing to inspect, so it does not make a row expandable.
      rawInput: typeof call.input === 'string' ? call.input || undefined : call.input === undefined || call.input === null || typeof call.input === 'object' && Object.keys(call.input).length === 0 ? undefined : JSON.stringify(call.input, null, 2),
      status: call.status, startedAt: call.at, endedAt: call.result?.at,
      detail: toolOutput(call.result?.content) || undefined,
      outputLanguage: (media === undefined ? undefined : outputMediaLanguages[media]) ?? (kind === 'run' ? 'bash' : kind === 'read' ? extension : undefined),
      changedPath: kind === 'edit' && call.callSeen && call.status === 'success' ? path : undefined,
    }]
  })
  const complete = facts.completeHistory === true
  const edits = calls.filter(call => call.kind === 'edit' && call.status === 'success')
  return {
    calls,
    durationMs: facts.durationMs !== undefined && Number.isFinite(facts.durationMs) && facts.durationMs >= 0 ? facts.durationMs : undefined,
    running: facts.running, failed: facts.failed, interrupted: facts.interrupted,
    failureNote: facts.failureNote, startedAt: facts.startedAt,
    foldable: facts.completeHistory === undefined ? undefined : complete,
    commands: complete ? items.filter(item => item._tag === 'ToolCall' && item.callSeen && facts.kindFor(item.name) === 'run').length : undefined,
    changedFiles: complete && edits.every(call => call.changedPath !== undefined) ? new Set(edits.map(call => call.changedPath)).size : undefined,
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
const outputMediaLanguages: Readonly<Record<string, string>> = {
  'application/json': 'json', 'text/json': 'json', 'text/markdown': 'markdown',
  'text/x-diff': 'diff', 'text/x-patch': 'diff', 'text/typescript': 'typescript',
  'application/typescript': 'typescript', 'text/javascript': 'javascript',
  'application/javascript': 'javascript', 'text/css': 'css', 'text/x-shellscript': 'bash',
  'application/x-sh': 'bash', 'text/x-python': 'python', 'text/x-rust': 'rust',
  'application/yaml': 'yaml', 'text/yaml': 'yaml', 'application/x-yaml': 'yaml',
}
export const workLogOutputLanguage = (call: WorkLogCall): string => call.outputLanguage ?? (call.kind === 'run' ? 'bash' : '')
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
