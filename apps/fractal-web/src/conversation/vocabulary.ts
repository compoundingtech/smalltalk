import type { TimelineBlock, TimelineEntry, TimelineView, UnknownCase } from '@smalltalk/st3-client/schema'
import { Match } from 'effect'
import type { ConversationItem, HarnessNotice } from './model.ts'
import { q101OmpEnvelopeStopgap } from './q101OmpEnvelopeStopgap.ts'

export type Equals<TA, TB> = (<T>() => T extends TA ? 1 : 2) extends (<T>() => T extends TB ? 1 : 2) ? true : false
export const assertType = <T extends true>(): void => undefined
export type KnownView = Exclude<TimelineView, { readonly type: UnknownCase }>
export type KnownViewType = KnownView['type']
export type ViewOf<T extends KnownViewType> = Extract<KnownView, { readonly type: T }>
export const isKnownView = (view: TimelineView): view is KnownView => typeof view.type === 'string'
export const KNOWN_BLOCK_KINDS = ['text', 'reasoning', 'tool_call', 'tool_output', 'image', 'image_link', 'job', 'subagent', 'ask', 'status', 'error', 'document', 'source_record', 'raw_text', 'irc', 'unknown'] as const
export type KnownBlockKind = typeof KNOWN_BLOCK_KINDS[number]
export const KNOWN_VISIBILITIES = ['visible', 'internal', 'hidden-by-harness'] as const
assertType<Equals<TimelineBlock['kind'], string>>()
assertType<Equals<TimelineBlock['visibility'], string | undefined>>()
export type BlockRole = 'prose' | 'reasoning' | 'tool' | 'typed' | 'bookkeeping' | 'unsupported'
export const kindCases = {
  text: 'prose', raw_text: 'prose', image: 'prose', image_link: 'prose', document: 'prose',
  reasoning: 'reasoning', tool_call: 'tool', tool_output: 'tool',
  job: 'typed', irc: 'typed', subagent: 'typed', ask: 'typed', status: 'typed', error: 'typed',
  source_record: 'bookkeeping', unknown: 'unsupported',
} satisfies { readonly [K in KnownBlockKind]: BlockRole }
const isKnownBlockKind = (kind: string): kind is KnownBlockKind => Object.hasOwn(kindCases, kind)
export const blockRole = (kind: string): BlockRole | undefined =>
  isKnownBlockKind(kind) ? kindCases[kind] : undefined
export const entryCases = {
  content: true, message: true, tool_call: true, tool_result: true, status: true,
  usage: true, error: true, redaction: true, truncation: true,
} satisfies { readonly [T in Exclude<TimelineEntry, { readonly type: UnknownCase }>['type']]: boolean }

export type ViewContext = { readonly id: string; readonly at: string; readonly text: string; readonly harness: string }
export type ViewMeaning = { readonly _tag: 'Tool' } | { readonly _tag: 'Drop' } | { readonly _tag: 'Item'; readonly item: ConversationItem }
type ViewCases = { readonly [T in KnownViewType]: (view: ViewOf<T>, ctx: ViewContext) => ViewMeaning }
const tool = (_view?: unknown, _ctx?: ViewContext): ViewMeaning => ({ _tag: 'Tool' })
const drop = (_view?: unknown, _ctx?: ViewContext): ViewMeaning => ({ _tag: 'Drop' })
const item = (value: ConversationItem): ViewMeaning => ({ _tag: 'Item', item: value })
const quiet = (ctx: ViewContext, text: string): ViewMeaning => item({ _tag: 'Notice', id: ctx.id, at: ctx.at, kind: 'event', text })
const stopgap = (ctx: ViewContext) => q101OmpEnvelopeStopgap({ id: ctx.id, at: ctx.at, role: 'system', text: ctx.text })
const sender = (ctx: ViewContext) => ({ kind: 'harness', label: ctx.harness } as const)
type JobOutcome = Extract<HarnessNotice, { readonly _tag: 'BackgroundJob' }>['state']
const jobOutcome = (job: ViewOf<'job'>['jobs'][number]): JobOutcome => {
  if (job.state === 'running' || job.state === 'cancelled' || job.state === 'unknown') return job.state
  if (job.state === 'failed' || /^failed \(exit -?\d+\)$/.test(job.state ?? '') || (job.exit_code !== undefined && job.exit_code !== 0)) return 'failed'
  return job.state === 'completed' ? 'completed' : 'unknown'
}
export const viewCases = {
  bash: tool, edit: tool, write: tool, read: tool, search: tool, todo: tool, ask: tool, task: tool, hub: tool, eval: tool, generic: tool,
  tool_start: drop, credential_pin: drop,
  job: (view, ctx) => {
    const job = view.jobs[0]
    if (view.jobs.length === 1 && job !== undefined) {
      const state = jobOutcome(job)
      const parsed = stopgap(ctx)
      // Legacy payloads enrich only the same job and outcome; native facts remain authoritative.
      if (parsed?._tag === 'Event' && ((parsed.notice?._tag === 'BackgroundJob' && parsed.notice.jobId === job.id && parsed.notice.state === state)
        || (parsed.notice?._tag === 'SubagentResult' && parsed.notice.taskId === job.id && parsed.notice.status === job.state && jobOutcome({ id: job.id, state: parsed.notice.status }) === state))) return item(parsed)
      const outcome = state === 'failed' && /^failed \(exit -?\d+\)$/.test(job.state ?? '') ? job.state : state
      return item({ _tag: 'Event', id: ctx.id, at: ctx.at, kind: 'background-job', title: `Background job ${outcome}`, text: job.name ?? job.id, sender: sender(ctx), data: {}, notice: {
        _tag: 'BackgroundJob', jobId: job.id, ...(job.name === undefined ? {} : { label: job.name }), state,
        ...(job.duration_ms === undefined ? {} : { duration: `${job.duration_ms} ms` }), payload: '', raw: '',
      } })
    }
    const counts = new Map<JobOutcome, number>()
    for (const job of view.jobs) {
      const state = jobOutcome(job)
      counts.set(state, (counts.get(state) ?? 0) + 1)
    }
    const summary = [...counts].map(([state, count]) => `${count} ${state}`).join(', ')
    const title = counts.size === 1 ? `${view.jobs.length} background jobs ${counts.keys().next().value}` : `${view.jobs.length} background jobs${summary === '' ? '' : ` · ${summary}`}`
    return item({ _tag: 'Event', id: ctx.id, at: ctx.at, kind: 'background-job', title, text: view.jobs.map(job => job.name ?? job.id).join('\n'), sender: sender(ctx), data: {} })
  },
  irc: (view, ctx) => {
    const parsed = stopgap(ctx)
    return parsed?._tag === 'Event' && parsed.notice?._tag === 'Irc' ? item(parsed) : item({ _tag: 'Event', id: ctx.id, at: ctx.at, kind: 'irc', title: `Message from ${view.from}`, text: view.message, sender: { kind: 'agent', label: view.from }, data: {}, notice: { _tag: 'Irc', from: view.from, payload: view.message, raw: '', ...(view.reply_to === undefined ? {} : { replyTo: view.reply_to }) } })
  },
  compaction: (view, ctx) => {
    const parsed = stopgap(ctx)
    const payload = view.summary ?? view.short_summary ?? ''
    return parsed?._tag === 'Event' && parsed.notice?._tag === 'Compaction' ? item(parsed) : item({ _tag: 'Event', id: ctx.id, at: ctx.at, kind: 'compaction', title: 'Conversation compacted', text: payload, sender: sender(ctx), data: {}, notice: { _tag: 'Compaction', payload, raw: '' } })
  },
  assistant_error: (view, ctx) => view.presentation === 'none' ? drop() : item({ _tag: 'Notice', id: ctx.id, at: ctx.at, kind: view.status === 'failed' ? 'error' : 'event', text: view.label, detail: view.message }),
  model_change: (view, ctx) => quiet(ctx, `Model changed to ${view.model}${view.fallback ? ' · fallback' : ''}`),
  thinking_level: (view, ctx) => quiet(ctx, `Thinking level: ${view.level}`),
  title: (view, ctx) => quiet(ctx, view.title),
  session_exit: (view, ctx) => quiet(ctx, `Session ${view.kind}: ${view.reason}`),
  reset_boundary: (_view, ctx) => quiet(ctx, 'Session reset'),
  skill: (view, ctx) => quiet(ctx, `Skill: ${view.name}`),
} satisfies ViewCases
export const viewMeaning = (view: KnownView, ctx: ViewContext): ViewMeaning =>
  Match.value(view).pipe(Match.discriminatorsExhaustive('type')({
    ...viewCases,
    bash: v => viewCases.bash(v, ctx), edit: v => viewCases.edit(v, ctx), write: v => viewCases.write(v, ctx),
    read: v => viewCases.read(v, ctx), search: v => viewCases.search(v, ctx), todo: v => viewCases.todo(v, ctx),
    ask: v => viewCases.ask(v, ctx), task: v => viewCases.task(v, ctx), hub: v => viewCases.hub(v, ctx), eval: v => viewCases.eval(v, ctx), generic: v => viewCases.generic(v, ctx),
    tool_start: v => viewCases.tool_start(v, ctx), credential_pin: v => viewCases.credential_pin(v, ctx),
    job: v => viewCases.job(v, ctx), irc: v => viewCases.irc(v, ctx), compaction: v => viewCases.compaction(v, ctx),
    assistant_error: v => viewCases.assistant_error(v, ctx), model_change: v => viewCases.model_change(v, ctx), thinking_level: v => viewCases.thinking_level(v, ctx), title: v => viewCases.title(v, ctx), session_exit: v => viewCases.session_exit(v, ctx), reset_boundary: v => viewCases.reset_boundary(v, ctx), skill: v => viewCases.skill(v, ctx),
  }))
