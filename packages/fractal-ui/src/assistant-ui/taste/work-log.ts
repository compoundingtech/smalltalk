/** Controlled presentation types for the work log. */
import type { ConversationItem } from '../embrace-data/model'
import { toolHumanSummary, toolOutput } from '../embrace-tool-preview'
export type WorkKind = 'read' | 'run' | 'edit'
export interface WorkLogCall {
  readonly id: string
  readonly kind: WorkKind
  readonly title: string
  readonly argsSummary?: string
  readonly summary?: string
  readonly status: 'running' | 'success' | 'error' | 'interrupted'
  readonly startedAt: string
  readonly endedAt?: string
  readonly detail?: string
  readonly outputLanguage?: string
  /** Observed edit/write path, not a guessed title. */
  readonly changedPath?: string
  /** A run's own command or script source; display it only as code inside the raw disclosure. */
  readonly command?: string
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
/** A traceback's last exception is the reason, never its frame or source statements. */
export function errorReason(detail: string): string {
  const rawLines = detail.split('\n')
  // Python exception names need not end in Error/Exception (e.g. TimeoutExpired).
  // Source is indented; retain indentation until after selecting an exception.
  let exception: string | undefined
  for (let index = rawLines.length - 1; index >= 0; index--) {
    const line = rawLines[index]!
    if (/^(?:[A-Za-z_]\w*\.)*[A-Z]\w*:\s*/.test(line)) { exception = line; break }
  }
  const exit = detail.match(/\b(?:exited with code|exit code|exit status)\s*:?\s*(-?\d+)\b/i)
  const traceback = rawLines.some(line => /^\s*(?:Traceback\b|File ["'])/.test(line))
  const lines = traceback ? [] : rawLines.map(line => line.trim()).filter(line =>
    line !== '' &&
    !/^(?:at\s+\S+|[\^~]+$)/.test(line) &&
    !/^["']?(?:[A-Za-z]:[\\/]|[~/\\]|\.{1,2}[\\/]|…[\\/])\S*["']?$/.test(line),
  )
  const reason = (exception === undefined ? undefined : subprocessSummary(exception, exit?.[1]) ?? exception) ??
    (exit === null ? undefined : `Exited with code ${exit[1]}`) ??
    lines.find(line => /^(?:[\w.]+(?:Error|Exception)|error|fatal|failed|failure)\b/i.test(line)) ?? lines[0]
  if (reason === undefined) return 'Tool failed; no readable reason was recorded.'
  const line = reason.replace(/(?:~|[A-Za-z]:)?(?:[\\/][^\s\\/:'"`]+){2,}[\\/]([^\s\\/:'"`]+)/g, '…/$1')
  return line.length > REASON_MAX ? `${line.slice(0, REASON_MAX - 1).trimEnd()}…` : line
}

/** First-level reasons are one short line; full diagnostics stay behind the raw disclosure. */
const REASON_MAX = 160

/**
 * Python subprocess exceptions embed the whole argv (often a multi-line shell script) in their message,
 * so the reason states only the outcome; `failedCommandSource` recovers the command for the disclosure.
 */
function subprocessSummary(exception: string, exitCode: string | undefined): string | undefined {
  const match = exception.match(/^(?:[A-Za-z_]\w*\.)*(TimeoutExpired|CalledProcessError):/)
  if (match === null) return undefined
  if (match[1] === 'TimeoutExpired') {
    const seconds = exception.match(/timed out after (\d+(?:\.\d+)?) seconds?\s*$/)?.[1]
    return seconds === undefined ? 'Command timed out' : `Command timed out after ${seconds}s`
  }
  const code = exception.match(/non-zero exit status (-?\d+)\.?\s*$/)?.[1] ?? exitCode
  return code === undefined ? 'Command failed' : `Command exited with code ${code}`
}

const unescapePython = (literal: string) => literal.replace(/\\(?:x([0-9a-fA-F]{2})|(.))/gs, (_, hex: string | undefined, char: string) =>
  hex !== undefined ? String.fromCharCode(parseInt(hex, 16)) : ({ n: '\n', t: '\t', r: '\r', '0': '\0' } as Record<string, string>)[char] ?? char)

/** Python string literals in a list repr; an unterminated (truncated) final literal is kept as is. */
const pythonListItems = (repr: string): string[] => {
  const items: string[] = []
  const literal = /(['"])((?:\\.|(?!\1)[^\\])*)(\1|$)/gs
  for (const match of repr.matchAll(literal)) items.push(unescapePython(match[2]!))
  return items
}

/**
 * The command embedded in a Python subprocess exception, unescaped for display as code.
 * `sh -c SCRIPT` yields the script itself; other argv lists are space-joined.
 */
export function failedCommandSource(detail: string): string | undefined {
  const lines = detail.split('\n')
  for (let index = lines.length - 1; index >= 0; index--) {
    const match = lines[index]!.match(/^(?:[A-Za-z_]\w*\.)*(?:TimeoutExpired|CalledProcessError): Command '(.*)$/s)
    if (match === null) continue
    const body = match[1]!.replace(/' (?:timed out after|returned non-zero exit status) .*$/, '')
    if (!body.startsWith('[')) return unescapePython(body.replace(/'$/, ''))
    const argv = pythonListItems(body)
    const script = argv.length === 3 && /^(?:\/\S*\/)?(?:ba|z|da)?sh$/.test(argv[0]!) && argv[1] === '-c' ? argv[2]!.replace(/^\n+/, '') : undefined
    return script ?? (argv.length > 0 ? argv.join(' ') : undefined)
  }
  return undefined
}

/** The command a failure reason stands in for: a subprocess command named by the failure, else the run's own source. */
export const readableCommand = (call: WorkLogCall): string | undefined =>
  (call.detail === undefined ? undefined : failedCommandSource(call.detail)) ?? call.command

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
      id: call.id, kind, title: call.name, summary: toolHumanSummary(call.input, call.name), argsSummary: summarizeArgs(call.input),
      status: call.status, startedAt: call.at, endedAt: call.result?.at,
      detail: toolOutput(call.result?.content) || undefined,
      outputLanguage: (media === undefined ? undefined : outputMediaLanguages[media]) ?? (kind === 'run' ? 'bash' : kind === 'read' ? extension : undefined),
      changedPath: kind === 'edit' && call.callSeen && call.status === 'success' ? path : undefined,
      command: kind === 'run' ? ['command', 'code', 'script'].map(key => input?.[key]).find((value): value is string => typeof value === 'string' && value.trim().length > 0) : undefined,
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
