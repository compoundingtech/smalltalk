/**
 * Synthetic native omp records, shaped the way st3 serves them on the conversation timeline.
 *
 * `toolExecutionStartEntry` follows `external_sessions.rs` for an omp `custom` record that has a
 * typed view: `push_native_block` writes the fallback text `[omp tool start <tool>]\n{"raw":…}`, the
 * `status` block takes view `tool_start` and visibility `internal` from `native_views.rs`
 * (`omp_record_view`), and a `source_record` block carries the native record
 * (fixtures/clients/transcripts/omp-parity.json). All ids, timestamps and text are invented.
 */
export const toolExecutionStartRecord = {
  type: 'custom',
  customType: 'tool_execution_start',
  data: {
    toolCallId: 'toolu_example_eval_1',
    toolName: 'eval',
    startedAt: '2026-01-15T12:00:01.000Z',
    intent: 'Example intent',
  },
  id: 'a1b2c3d4',
  parentId: 'a0b1c2d3',
  timestamp: '2026-01-15T12:00:01.000Z',
} as const

/** The assistant's call that `toolExecutionStartRecord` announces. */
export const toolExecutionStartCall = {
  id: 'timeline-entry/native-example-call',
  sequence: 40,
  revision: 1,
  final: true,
  timestamp: '2026-01-15T12:00:00.000Z',
  type: 'tool_call',
  role: 'assistant',
  body: { call_id: 'toolu_example_eval_1', name: 'eval', arguments: { i: 'Example intent' } },
} as const

export const toolExecutionStartEntry = {
  id: 'timeline-entry/native-example-start',
  sequence: 41,
  revision: 1,
  final: true,
  timestamp: '2026-01-15T12:00:01.000Z',
  type: 'content',
  role: 'system',
  body: {
    media_type: 'text/plain',
    text: `[omp tool start eval]\n${JSON.stringify({ raw: toolExecutionStartRecord })}`,
    blocks: [
      {
        id: 'native-41/0',
        kind: 'status',
        source_type: 'custom',
        visibility: 'internal',
        payload: { raw: toolExecutionStartRecord },
        view: {
          type: 'tool_start',
          call_id: 'toolu_example_eval_1',
          tool: 'eval',
          started_at: '2026-01-15T12:00:01.000Z',
        },
      },
      {
        id: 'native-41/source',
        kind: 'source_record',
        source_type: 'omp',
        visibility: 'internal',
        payload: { raw: toolExecutionStartRecord },
      },
    ],
  },
} as const

/** A custom record st3 has no view for: `push_unrecognized` marks it, with no visibility hint. */
export const unknownCustomRecord = {
  type: 'custom',
  customType: 'plan_mode_toggled',
  data: { enabled: true },
  id: 'b2c3d4e5',
  parentId: 'a1b2c3d4',
  timestamp: '2026-01-15T12:00:02.000Z',
} as const

export const unknownCustomEntry = {
  id: 'timeline-entry/native-example-unknown',
  sequence: 42,
  revision: 1,
  final: true,
  timestamp: '2026-01-15T12:00:02.000Z',
  type: 'content',
  role: 'system',
  body: {
    media_type: 'text/plain',
    text: `[unrecognized omp entry \`custom\`]\n${JSON.stringify({ raw: unknownCustomRecord })}`,
    blocks: [
      { id: 'native-42/0', kind: 'unknown', source_type: 'custom', payload: { raw: unknownCustomRecord } },
    ],
  },
} as const

/** A custom record with a typed view but no visibility hint keeps rendering as it did. */
export const visibleCustomEntry = {
  ...toolExecutionStartEntry,
  id: 'timeline-entry/native-visible',
  sequence: 43,
  body: {
    ...toolExecutionStartEntry.body,
    blocks: toolExecutionStartEntry.body.blocks.map(({ visibility: _visibility, ...block }) => block),
  },
} as const
