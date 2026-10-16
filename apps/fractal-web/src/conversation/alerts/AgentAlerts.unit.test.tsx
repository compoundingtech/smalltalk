import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, it, vi } from 'vitest'
import { ClientError, type St3Client } from '@smalltalk/st3-client'
import type * as Native from '@smalltalk/st3-client/schema'

// Node rendering exercises the real composition, not the CSS compiler.
vi.mock('@stylexjs/stylex', () => ({
  create: (styles: unknown) => styles,
  defineVars: (variables: unknown) => variables,
  createTheme: () => ({}),
  keyframes: () => 'test-animation',
  props: () => ({}),
}))
vi.mock('../../data/react.tsx', () => ({ useAttention: vi.fn(), useDataSource: vi.fn() }))

import { AgentAlerts, promptAlerts, waitingCalls } from './AgentAlerts.tsx'
import { gatewayPromptAnswers } from './prompt-answer-port.ts'

const seat = 'agent/example/atlas'

const alert = (id: string, answers: readonly string[], detail = 'Claude asks to use Bash'): Native.Attention =>
  ({
    id,
    kind: 'attention',
    revision: 'r1',
    attention_kind: 'harness-prompt',
    title: `${seat} is waiting for you`,
    detail,
    priority: 'high',
    state: 'open',
    requested_at: '2026-10-13T10:00:00Z',
    person_id: 'person/avery',
    source_id: seat,
    conversation_id: seat,
    conversation_ids: [seat],
    episode: `${id}/episode`,
    alert: true,
    actions: answers.length > 0 ? ['prompt.respond'] : [],
    action_parameters: answers.length > 0
      ? { 'prompt.respond': { target_id: seat, episode: `${id}/episode`, answers } }
      : {},
  }) as unknown as Native.Attention

const call = (callId: string, name: string, input: unknown, result?: unknown) =>
  ({ _tag: 'ToolCall', id: `item/${callId}`, callId, name, input, callSeen: true, ...(result === undefined ? {} : { result }) })

const render = (alerts: readonly Native.Attention[], items: Parameters<typeof waitingCalls>[0]) =>
  renderToStaticMarkup(<AgentAlerts alerts={alerts} calls={waitingCalls(items)} onAnswer={async () => ({ _tag: 'Sent' })} />)

describe('AgentAlerts', () => {
  it('shows only the open prompt alerts of this conversation', () => {
    const other = { ...alert('attention/other', ['allow', 'deny']), conversation_id: 'agent/example/else', conversation_ids: ['agent/example/else'] } as Native.Attention
    const closed = { ...alert('attention/closed', ['allow', 'deny']), state: 'resolved' } as Native.Attention
    const request = { ...alert('attention/ask', []), attention_kind: 'agent-request' } as Native.Attention
    expect(promptAlerts([alert('attention/one', ['allow', 'deny']), other, closed, request], seat).map((item) => item.id))
      .toEqual(['attention/one'])
    expect(render([], [])).toBe('')
  })

  it('shows the one waiting call in full beside allow and deny', () => {
    const html = render([alert('attention/one', ['allow', 'deny'])], [
      call('done', 'Bash', { command: 'ls' }, { status: 'ok' }),
      call('c1', 'Bash', { command: 'rm -rf build\necho done' }),
    ])
    expect(html).toContain('command: rm -rf build')
    expect(html).toContain('  echo done')
    expect(html).toContain('>Allow<')
    expect(html).toContain('>Deny<')
  })

  it('offers allow only beside the exact call the prompt names, never a parent\'s Task call', () => {
    const named = (callId: string | null) => {
      const item = alert('attention/one', ['allow', 'deny'])
      return { ...item, action_parameters: { 'prompt.respond': { target_id: seat, episode: 'e', answers: ['allow', 'deny'], call_id: callId } } } as unknown as Native.Attention
    }
    const parentOnly = render([named('toolu_sub')], [call('toolu_task', 'Task', { prompt: 'run the build' })])
    expect(parentOnly).not.toContain('>Allow<')
    expect(parentOnly).not.toContain('run the build')
    const both = render([named('toolu_bash')], [call('toolu_task', 'Task', { prompt: 'run the build' }), call('toolu_bash', 'Bash', { command: 'make' })])
    expect(both).toContain('>Allow<')
    expect(both).toContain('command: make')
    expect(both).not.toContain('run the build')
    expect(render([named(null)], [call('toolu_bash', 'Bash', { command: 'make' })])).not.toContain('>Allow<')
  })

  it('withholds allow while it cannot say which call the prompt is about', () => {
    const none = render([alert('attention/one', ['allow', 'deny'])], [])
    expect(none).not.toContain('>Allow<')
    expect(none).toContain('>Deny<')
    expect(none).toContain('not in the conversation yet')
    const two = render([alert('attention/one', ['allow', 'deny'])], [
      call('c1', 'Bash', { command: 'make' }),
      call('c2', 'Write', { file_path: 'notes.md' }),
    ])
    expect(two).not.toContain('>Allow<')
    expect(two).toContain('2 calls are waiting')
  })

  it('offers a question\'s options to choose, and sends only once every question has a choice', () => {
    const html = render([alert('attention/q', ['answer', 'deny'], 'Claude asks you a question')], [
      call('q1', 'AskUserQuestion', { questions: [
        { question: 'Which color?', header: 'Color', multiSelect: false, options: [{ label: 'Red', description: '' }, { label: 'Blue', description: '' }] },
        { question: 'Which pets?', header: 'Pets', multiSelect: true, options: [{ label: 'Cat', description: '' }, { label: 'Fish', description: '' }] },
      ] }),
    ])
    expect(html).toContain('Color · Which color?')
    expect(html).toContain('Which pets? (choose any)')
    expect(html).toContain('>Fish<')
    expect(html).toMatch(/disabled=""[^>]*>Send</)
    expect(html).toContain('>Decline<')
  })

  it('offers a plan\'s approval beside it, and a screen prompt\'s own answers', () => {
    const plan = render([alert('attention/plan', ['approve', 'deny'])], [call('p1', 'ExitPlanMode', { plan: '1. Build\n2. Test' })])
    expect(plan).toContain('2. Test')
    expect(plan).toContain('>Approve plan<')
    const screen = render([alert('attention/mcp', ['use', 'use-all', 'skip'], 'New MCP server found in this project: probe-echo.')], [])
    for (const label of ['Use it', 'Use all', 'Skip']) expect(screen).toContain(`>${label}<`)
    const terminal = render([alert('attention/t', [], 'Codex asks a question')], [])
    expect(terminal).toContain('Answer it in the seat&#x27;s terminal.')
  })
})

describe('gatewayPromptAnswers', () => {
  const snapshot = { id: 'snapshot/s1', node: 'node', store_index: 1, observed_at: '2026-10-13T10:00:00Z' }
  const capabilities = (granted: boolean) => ({
    value: {
      api_version: 'st3.client.v0',
      session: { id: 'session/s', actor: 'person/avery', kind: 'device', expires_at: null },
      capabilities: [{ id: 'prompt.respond', state: granted ? 'granted' : 'ungranted' }],
    },
    snapshot,
  })
  const promptAlert = { id: 'attention/one', revision: 'r1', source_id: seat, target_id: seat, episode: 'episode-1' } as const

  it('sends one answer fenced on the alert, and says why st refused one', async () => {
    const sent: unknown[] = []
    const client = {
      capabilities: async () => capabilities(true),
      promptRespond: async (request: unknown) => {
        sent.push(request)
        return { value: { id: 'action/a', status: 'accepted', targets: [seat] }, snapshot }
      },
    } as unknown as St3Client
    const port = gatewayPromptAnswers(client)
    const result = await port.respond({ alert: promptAlert as never, answer: '{"Which?":"Blue"}' })
    expect(sent).toHaveLength(1)
    expect(sent[0]).toMatchObject({
      fence: { subject_revisions: { 'attention/one': 'r1' } },
      parameters: { target_id: seat, episode: 'episode-1', answer: '{"Which?":"Blue"}' },
    })
    expect(result).toEqual({ _tag: 'Sent' })

    const gone = gatewayPromptAnswers({
      capabilities: async () => capabilities(true),
      promptRespond: async () => {
        throw new ClientError({ api_version: 'st3.client.v0', error_version: 'st3.client.error.v0', request_id: 'request/x', code: 'stale-fence', message: 'that prompt is no longer waiting: it was answered or replaced', retryable: false, details: {} }, 422)
      },
    } as unknown as St3Client)
    expect(await gone.respond({ alert: promptAlert as never, answer: 'allow' })).toEqual({
      _tag: 'Refused',
      detail: 'that prompt is no longer waiting: it was answered or replaced.',
    })

    // Only an accepted or completed action is an answer sent; any other word is no confirmation.
    for (const [status, tag] of [['completed', 'Sent'], ['rejected', 'Refused'], ['queued', 'Refused']] as const) {
      const answered = gatewayPromptAnswers({
        capabilities: async () => capabilities(true),
        promptRespond: async () => ({ value: { id: 'action/b', status, targets: [seat] }, snapshot }),
      } as unknown as St3Client)
      expect((await answered.respond({ alert: promptAlert as never, answer: 'allow' }))._tag, status).toBe(tag)
    }

    let called = false
    const ungranted = gatewayPromptAnswers({
      capabilities: async () => capabilities(false),
      promptRespond: async () => { called = true },
    } as unknown as St3Client)
    expect((await ungranted.respond({ alert: promptAlert as never, answer: 'allow' }))._tag).toBe('Refused')
    expect(called).toBe(false)
  })
})
