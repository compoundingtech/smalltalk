import { describe, expect, it } from 'vitest'
import { Effect, Metric } from 'effect'
import { decodeConversationChunk } from '@st3/sdk/effect'
import { LiveTimeline } from './fromTimeline.ts'
import { inspectPreview } from './inspect.ts'
import { degradedCounter, unsupportedCounter } from './telemetry.ts'
import { prepareTranscriptTurns } from '../web/conversationTranscript.ts'
import { viewCases, type ViewOf } from './vocabulary.ts'

const block = (kind: string, fields: object = {}) => ({ id: `block/${kind}`, kind, source_type: 'omp', payload: {}, ...fields })
const entry = (blocks: readonly unknown[], fields: object = {}) => ({ id: 'timeline-entry/raw/1', sequence: 1, revision: 1, final: true, role: 'system', timestamp: '2026-10-12T12:00:00Z', type: 'content', body: { text: '[omp raw]\n{"raw":{}}', media_type: 'text/plain', blocks }, ...fields })
const frame = (items: readonly unknown[], replace = true) => decodeConversationChunk({ kind: 'conversation', collection: 'conversation', id: 'follow/raw', session_id: 'session/raw', replace, has_more: false, items })
const project = (raw: unknown) => { const timeline = new LiveTimeline(); timeline.apply(frame([raw])); return timeline.project().items }

describe('native closed transcript boundary', () => {
  it('hides internal tool starts', () => {
    expect(project(entry([block('status', { visibility: 'internal', view: { type: 'tool_start', call_id: 'call/a', tool: 'bash', started_at: '2026-10-12T12:00:00Z' } })]))).toEqual([])
  })
  it.each(['internal', 'hidden-by-harness'])('hides %s native tool invocations', visibility => {
    expect(project(entry([], { type: 'tool_call', role: 'assistant', body: {
      call_id: 'call/hidden', name: 'bash', arguments: { command: 'INTERNAL_ONLY_COMMAND' },
      blocks: [block('tool_call', { visibility })],
    } }))).toEqual([])
  })
  it.each(['internal', 'hidden-by-harness'])('hides %s orphan native tool results', visibility => {
    expect(project(entry([], { type: 'tool_result', role: 'tool', body: {
      call_id: 'call/hidden', content: 'INTERNAL_ONLY_OUTPUT', media_type: 'text/plain', status: 'success',
      blocks: [block('tool_output', { visibility })],
    } }))).toEqual([])
  })
  it.each(['internal', 'hidden-by-harness'])('does not join %s result payloads into visible calls', visibility => {
    const timeline = new LiveTimeline()
    timeline.apply(frame([
      entry([], { type: 'tool_call', role: 'assistant', body: { call_id: 'call/joined', name: 'bash', arguments: { command: 'visible command' }, blocks: [block('tool_call')] } }),
      entry([], { id: 'timeline-entry/raw/result', sequence: 2, type: 'tool_result', role: 'tool', body: { call_id: 'call/joined', content: 'INTERNAL_ONLY_OUTPUT', media_type: 'text/plain', status: 'success', blocks: [block('tool_output', { visibility })] } }),
    ]))
    const items = timeline.project().items
    expect(items).toMatchObject([{ _tag: 'ToolCall', input: { command: 'visible command' } }])
    expect(items[0]).not.toHaveProperty('result')
    expect(JSON.stringify(items)).not.toContain('INTERNAL_ONLY_OUTPUT')
  })
  it.each(['internal', 'hidden-by-harness'])('hides %s non-content message and status bodies', visibility => {
    for (const type of ['message', 'status']) {
      expect(project(entry([], { type, body: { message_id: 'message/hidden', title: 'INTERNAL_ONLY_MESSAGE', status: 'completed', detail: 'INTERNAL_ONLY_STATUS', blocks: [block(type === 'message' ? 'text' : 'status', { visibility })] } }))).toEqual([])
    }
  })
  it('retains visible tool fallback and mixed unsupported markers', () => {
    const items = project(entry([], { type: 'tool_call', role: 'assistant', body: {
      call_id: 'call/mixed', name: 'bash', arguments: { command: 'visible command' },
      blocks: [block('tool_call', { visibility: 'internal' }), block('future-tool'), block('tool_call')],
    } }))
    expect(items).toMatchObject([{ _tag: 'Unsupported', recordKind: 'future-tool' }, { _tag: 'ToolCall', input: { command: 'visible command' } }])
  })
  it('retains visible results as orphans when their invocation is hidden', () => {
    const timeline = new LiveTimeline()
    timeline.apply(frame([
      entry([], { type: 'tool_call', role: 'assistant', body: { call_id: 'call/mixed', name: 'bash', arguments: { command: 'INTERNAL_ONLY_COMMAND' }, blocks: [block('tool_call', { visibility: 'internal' })] } }),
      entry([], { id: 'timeline-entry/raw/visible-result', sequence: 2, type: 'tool_result', role: 'tool', body: { call_id: 'call/mixed', content: 'Visible output', media_type: 'text/plain', status: 'success', blocks: [block('tool_output')] } }),
    ]))
    expect(timeline.project().items).toMatchObject([{ _tag: 'ToolCall', callSeen: false, result: { content: 'Visible output' } }])
  })
  it.each([
    ['internal', 'visible', 'call-only'],
    ['visible', 'internal', 'call-only'],
    ['internal', 'visible', 'result-first'],
    ['visible', 'internal', 'result-first'],
    ['internal', 'visible', 'call-first'],
    ['visible', 'internal', 'call-first'],
  ])('reprojects result-before-call rows when visibility changes %s → %s (%s)', (initial, next, delivery) => {
    const timeline = new LiveTimeline()
    const call = entry([], { type: 'tool_call', role: 'assistant', body: {
      call_id: 'call/revised', name: 'bash', arguments: { command: 'Visible command' },
      blocks: [block('tool_call', { visibility: initial })],
    } })
    const result = entry([], { id: 'timeline-entry/raw/revised-result', sequence: 2, type: 'tool_result', role: 'tool', body: {
      call_id: 'call/revised', content: 'Visible output', media_type: 'text/plain', status: 'success',
      blocks: [block('tool_output', { visibility: 'visible' })],
    } })
    timeline.apply(frame([result]))
    expect(timeline.project().items).toMatchObject([{ _tag: 'ToolCall', callSeen: false }])
    timeline.apply(frame([call], false))
    expect(timeline.project().items).toMatchObject([{ _tag: 'ToolCall', callSeen: initial === 'visible', result: { content: 'Visible output' } }])
    const revisedCall = { ...call, revision: 2, body: {
      ...call.body, blocks: [block('tool_call', { visibility: next })],
    } }
    const revisedResult = { ...result, revision: 2, body: { ...result.body, content: 'Revised visible output' } }
    const updates = delivery === 'call-only' ? [revisedCall]
      : delivery === 'result-first' ? [revisedResult, revisedCall] : [revisedCall, revisedResult]
    timeline.apply(frame(updates, false))
    const items = timeline.project().items
    expect(items).toHaveLength(1)
    expect(items).toMatchObject([{ _tag: 'ToolCall', callSeen: next === 'visible',
      result: { content: delivery === 'call-only' ? 'Visible output' : 'Revised visible output' } }])
    expect(items[0]?.id).toBe(next === 'visible' ? call.id : result.id)
    expect(timeline.project().items).toEqual(items)
  })
  it.each([
    [{ id: 'Impl2239-2', state: 'completed', duration_ms: 15 }, 'completed'],
    [{ id: 'process/1', name: 'Supervised process', state: 'failed', exit_code: 1 }, 'failed'],
  ])('uses typed single-job facts without a matching envelope', (job, state) => {
    expect(project(entry([block('job', { view: { type: 'job', jobs: [job] } })]))).toMatchObject([{ _tag: 'Event', kind: 'background-job', notice: { _tag: 'BackgroundJob', jobId: job.id, state, payload: '', raw: '' } }])
  })
  it('lists multiple jobs without copying the envelope', () => {
    expect(project(entry([block('job', { view: { type: 'job', jobs: [{ id: 'a', name: 'Alpha', state: 'completed' }, { id: 'b', name: 'Beta', state: 'completed' }] } })]))).toMatchObject([{ _tag: 'Event', title: '2 background jobs completed', text: 'Alpha\nBeta' }])
  })
  it.each([
    ['unknown', undefined, 'unknown', 'unknown'],
    ['cancelled', undefined, 'cancelled', 'cancelled'],
    ['failed (exit 2)', undefined, 'failed', 'failed (exit 2)'],
    ['running', undefined, 'running', 'running'],
    ['completed', undefined, 'completed', 'completed'],
    ['failed', undefined, 'failed', 'failed'],
    ['completed', 0, 'completed', 'completed'],
    ['completed', 2, 'failed', 'failed'],
    ['failed', 0, 'failed', 'failed'],
    ['failed (exit 2)', 0, 'failed', 'failed (exit 2)'],
    ['running', 0, 'running', 'running'],
    ['running', 2, 'running', 'running'],
    ['cancelled', 0, 'cancelled', 'cancelled'],
    ['cancelled', 2, 'cancelled', 'cancelled'],
    ['unknown', 0, 'unknown', 'unknown'],
    ['unknown', 2, 'unknown', 'unknown'],
    ['future-state', 0, 'unknown', 'unknown'],
  ])('preserves native job outcome %s / exit %s without inferring success', (state, exit_code, outcome, title) => {
    const job = { id: 'j', name: 'Native job', duration_ms: 123, state, ...(exit_code === undefined ? {} : { exit_code }) }
    expect(viewCases.job({ type: 'job', jobs: [job] }, { id: 'native/job', at: '2026-10-14T13:00:00Z', text: '', harness: 'omp' })).toMatchObject({
      _tag: 'Item', item: { _tag: 'Event', title: `Background job ${title}`, text: 'Native job', notice: { _tag: 'BackgroundJob', jobId: 'j', state: outcome, duration: '123 ms', payload: '', raw: '' } },
    })
    expect(project(entry([block('job', { view: { type: 'job', jobs: [job] } })]))).toMatchObject([{ _tag: 'Event', title: `Background job ${title}`, notice: { state: outcome, payload: '', raw: '' } }])
  })
  it.each([
    [[{ id: 'j', state: 'failed' }, { id: 'k', state: 'completed' }], '2 background jobs · 1 failed, 1 completed'],
    [[{ id: 'j', state: 'unknown' }, { id: 'k', state: 'cancelled' }, { id: 'l', state: 'running' }], '3 background jobs · 1 unknown, 1 cancelled, 1 running'],
    [[{ id: 'j', state: 'failed (exit 2)' }, { id: 'k', state: 'failed' }], '2 background jobs failed'],
    [[], '0 background jobs'],
  ] satisfies readonly (readonly [ViewOf<'job'>['jobs'], string])[])('summarizes every native job outcome', (jobs, title) => {
    expect(viewCases.job({ type: 'job', jobs }, { id: 'native/jobs', at: '2026-10-14T13:00:00Z', text: '', harness: 'omp' })).toMatchObject({
      _tag: 'Item', item: { _tag: 'Event', title, text: jobs.map(job => job.id).join('\n') },
    })
  })
  it.each(['unknown', 'cancelled', 'running', 'failed', 'failed (exit 2)'])('does not let a completed legacy envelope override native %s', state => {
    const text = '<system-notice>\nBackground job j has completed. Resume your work using the result below.\nLEGACY_PAYLOAD\n</system-notice>'
    expect(viewCases.job({ type: 'job', jobs: [{ id: 'j', state }] }, { id: 'native/job', at: '2026-10-14T13:00:00Z', text, harness: 'omp' })).toMatchObject({
      _tag: 'Item', item: { _tag: 'Event', title: `Background job ${state}`, text: 'j', notice: { payload: '', raw: '' } },
    })
  })
  it('enriches a legacy job only when native identity and outcome match', () => {
    const text = '<system-notice>\nBackground job j has completed. Resume your work using the result below.\nLEGACY_PAYLOAD\n</system-notice>'
    const ctx = { id: 'native/job', at: '2026-10-14T13:00:00Z', text, harness: 'omp' }
    expect(viewCases.job({ type: 'job', jobs: [{ id: 'j', state: 'completed' }] }, ctx)).toMatchObject({ _tag: 'Item', item: { notice: { jobId: 'j', state: 'completed', payload: 'LEGACY_PAYLOAD' } } })
    expect(viewCases.job({ type: 'job', jobs: [{ id: 'other', state: 'completed' }] }, ctx)).toMatchObject({ _tag: 'Item', item: { text: 'other', notice: { jobId: 'other', state: 'completed', payload: '', raw: '' } } })
    expect(viewCases.job({ type: 'job', jobs: [{ id: 'j', state: 'failed' }, { id: 'k', state: 'completed' }] }, ctx)).toMatchObject({ _tag: 'Item', item: { title: '2 background jobs · 1 failed, 1 completed', text: 'j\nk' } })
  })
  it.each(['unknown', 'failed', 'cancelled'])('does not let a completed task envelope override native %s', state => {
    const text = '<task-result id="j" agent="task" status="completed" duration="1s">\n<output>\nLEGACY_PAYLOAD\n</output>\n</task-result>'
    expect(viewCases.job({ type: 'job', jobs: [{ id: 'j', state }] }, { id: 'native/job', at: '2026-10-14T13:00:00Z', text, harness: 'omp' })).toMatchObject({
      _tag: 'Item', item: { _tag: 'Event', title: `Background job ${state}`, notice: { _tag: 'BackgroundJob', state, payload: '', raw: '' } },
    })
  })
  it('marks unsupported native and injected kinds, even with system events hidden', () => {
    for (const [kind, source, reason] of [['unknown', 'model_usage', 'unrecognized-record'], ['future-kind', 'future', 'unknown-block-kind']]) {
      const items = project(entry([block('source_record', { visibility: 'internal' }), block(kind!, { source_type: source })]))
      expect(items).toMatchObject([{ _tag: 'Unsupported', harness: 'omp', recordKind: kind === 'unknown' ? source : kind, reason }])
      expect(prepareTranscriptTurns(items, { showSystemEvents: false, firstTurnComplete: true })[0]?.items).toEqual(items)
      expect(items[0]).not.toHaveProperty('blocks')
    }
  })
  it('keeps a good block alongside a dropped block marker', () => {
    const items = project(entry([{ id: 'bad', kind: 23, source_type: 'omp', payload: {} }, block('text')], { role: 'assistant', body: { text: 'Good prose', media_type: 'text/plain', blocks: [{ id: 'bad', kind: 23, source_type: 'omp', payload: {} }, block('text')] } }))
    expect(items).toMatchObject([{ _tag: 'Unsupported', reason: 'undecodable-block' }, { _tag: 'Text', text: 'Good prose' }])
  })
  it('turns system prose into a harness notice', () => {
    const items = project(entry([block('text')], { body: { text: '<system-reminder>Remember to save</system-reminder>', media_type: 'text/plain', blocks: [block('text')] } }))
    expect(items).toMatchObject([{ _tag: 'Event', kind: 'harness-message', title: 'Harness message', text: 'Full result available in details.' }])
  })
  it('keeps native reasoning metadata', () => {
    const blocks = [block('reasoning', { metadata: { model: 'test-model' } })]
    expect(project(entry(blocks, { role: 'assistant', body: { text: '[reasoning]\nA thought', media_type: 'text/plain', blocks } }))).toMatchObject([
      { _tag: 'Reasoning', text: 'A thought', assistantMetadata: { model: 'test-model' } },
    ])
  })
  it('joins degraded results while retaining result-position markers and counting revisions once', () => {
    const counter = degradedCounter.pipe(Metric.withAttributes({ kind: 'tool_output', view_type: 'bash' }))
    const before = Effect.runSync(Metric.value(counter)).count
    const call = entry([], { type: 'tool_call', role: 'assistant', body: { call_id: 'call/test', name: 'bash', arguments: {} } })
    const result = entry([], { id: 'timeline-entry/raw/2', sequence: 2, type: 'tool_result', role: 'tool', body: {
      call_id: 'call/test', content: 'Good result', media_type: 'text/plain', status: 'success',
      blocks: [block('tool_output', { view: { type: 'bash', tool: 'bash', env_keys: [], wall_ms: 1.5 } }), { id: 'bad-result-block', kind: 2, source_type: 'omp', payload: {} }],
    } })
    const timeline = new LiveTimeline()
    timeline.apply(frame([call, result]))
    const projected = timeline.project().items
    expect(projected).toMatchObject([
      { _tag: 'ToolCall', result: { content: 'Good result', blocks: [{ kind: 'tool_output' }] } },
      { _tag: 'Unsupported', id: 'timeline-entry/raw/2#bad-result-block', reason: 'undecodable-block' },
    ])
    timeline.project()
    timeline.apply(frame([call, result])); timeline.project()
    expect(Effect.runSync(Metric.value(counter)).count - before).toBe(1)
    timeline.apply(frame([{ ...result, revision: 2 }], false)); timeline.project()
    expect(Effect.runSync(Metric.value(counter)).count - before).toBe(2)
  })
  it('updates telemetry once per entry revision, not repeated projection or activity rebuild', () => {
    const counter = unsupportedCounter.pipe(Metric.withAttributes({ harness: 'st', record_kind: 'telemetry-kind', reason: 'unknown-block-kind' }))
    const value = () => Effect.runSync(Metric.value(counter)).count
    const before = value()
    const raw = entry([block('telemetry-kind', { source_type: 'test' })])
    const timeline = new LiveTimeline()
    timeline.apply(frame([raw])); timeline.project(); timeline.project()
    timeline.apply(frame([raw])); timeline.project()
    expect(value() - before).toBe(1)
    timeline.apply(frame([{ ...raw, revision: 2 }], false)); timeline.project()
    expect(value() - before).toBe(2)
  })
})

describe('inspect preview', () => {
  it('redacts adversarial keys and token strings before UTF-8 capping', () => {
    const raw = { authorization: 'Bearer never-show', nested: { api_key: 'secret', password: 'hunter2', safe: 'ghp_abcdef sk-abcdef xoxb-abcdef eyJabc.def.ghi' }, content: Array.from({ length: 100 }, () => '界'.repeat(600)) }
    const preview = inspectPreview(raw)
    expect(preview.bytes).toBe(new TextEncoder().encode(JSON.stringify(raw)).length)
    expect(new TextEncoder().encode(preview.text).length).toBeLessThanOrEqual(4096)
    expect(preview.truncated).toBe(true)
    expect(preview.text).not.toMatch(/never-show|hunter2|abcdef|eyJabc|"secret"/)
    expect(preview.text).toContain('[redacted]')
  })
})
