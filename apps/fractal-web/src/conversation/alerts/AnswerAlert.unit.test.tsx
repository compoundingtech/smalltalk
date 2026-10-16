import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, it, vi } from 'vitest'
import { ClientError, type St3Client } from '@smalltalk/st3-client'
import type * as Native from '@smalltalk/st3-client/schema'
import { DateTime, Option } from 'effect'

// Node rendering exercises the real composition, not the CSS compiler.
vi.mock('@stylexjs/stylex', () => ({
  create: (styles: unknown) => styles,
  defineVars: (variables: unknown) => variables,
  createTheme: () => ({}),
  keyframes: () => 'test-animation',
  props: () => ({}),
}))
vi.mock('../../data/react.tsx', () => ({ useAttention: vi.fn(), useDataSource: vi.fn() }))

import { AgentAlerts, conversationAlerts } from './AgentAlerts.tsx'
import { AnswerAlert, alertChoices, webLink } from './AnswerAlert.tsx'
import { gatewayAlertActions } from './alert-action-port.ts'

const seat = 'agent/example/atlas'
const at = '2026-10-14T10:00:00Z'

const item = (fields: Record<string, unknown>): Native.Attention =>
  ({
    id: 'attention/ask',
    kind: 'attention',
    revision: 'r1',
    attention_kind: 'person-step',
    title: 'Ship the parser change?',
    detail: 'The parser now accepts trailing commas.',
    priority: 'high',
    state: 'open',
    requested_at: DateTime.makeUnsafe(at),
    person_id: 'person/avery',
    source_id: 'step-run/example/build',
    requester_id: seat,
    conversation_id: seat,
    conversation_ids: [seat],
    episode: 'claim/example',
    alert: true,
    actions: ['work.done'],
    ...fields,
  }) as unknown as Native.Attention

const decision = {
  version: 1,
  type: 'decision',
  question: 'Merge the parser change?',
  why_person: 'You own the release.',
  answers: [
    { id: 'merge', label: 'Merge', consequence: 'It joins the merge queue.', outcome: 'accept' },
    { id: 'hold', label: 'Hold', consequence: 'It waits for the next release.', outcome: 'decline' },
    { id: 'change', label: 'Change it', consequence: 'The builder revises it.', outcome: 'request_changes' },
  ],
  recommendation: { answer: 'merge', reason: 'Tests and review passed.' },
}

const preview = {
  goal: 'Build the nightly report',
  request_excerpt: 'every night at two',
  steps: [
    { path: 'build', title: 'Build it', assignee: Option.some('agent/example/builder'), depends: [] },
    { path: 'review', title: 'Review it', assignee: Option.none(), depends: ['build'] },
  ],
  agents: [{ id: 'agent/example/builder', host: Option.some('example-host'), worktree: Option.none() }],
  gates: { automated: 2, human: 1 },
  diagnostics_count: 0,
}

const keys = (alert: Native.Attention) => alertChoices(alert)?.map((choice) => choice.key)
const choice = (alert: Native.Attention, key: string) => alertChoices(alert)!.find((each) => each.key === key)!

describe('alertChoices', () => {
  it('answers a free-text yes-or-no question with Yes, No, words or Nothing to do, as stui does', () => {
    const ask = item({})
    expect(keys(ask)).toEqual(['yes', 'no', 'words', 'nothing'])
    expect(choice(ask, 'yes').action('')).toEqual({ type: 'work.done', summary: 'Yes' })
    expect(choice(ask, 'words').words).toBe('required')
    expect(choice(ask, 'words').action('  Only after the release.  ')).toEqual({ type: 'work.done', summary: 'Only after the release.' })
    expect(choice(ask, 'nothing').action('')).toEqual({ type: 'work.done', summary: 'Nothing for me to do here.' })
    expect(keys(item({ title: 'Read the release notes' }))).toEqual(['words', 'nothing'])
  })

  it('sends a structured decision by its named answer, with words only for changes', () => {
    const ask = item({ request: decision })
    expect(keys(ask)).toEqual(['merge', 'hold', 'change', 'dismiss'])
    expect(choice(ask, 'merge')).toMatchObject({ recommended: true, words: 'none', tone: 'primary' })
    expect(choice(ask, 'merge').action('ignored')).toEqual({ type: 'work.done', summary: 'Merge', answer: { id: 'merge' } })
    expect(choice(ask, 'hold').tone).toBe('danger')
    expect(choice(ask, 'change').words).toBe('required')
    expect(choice(ask, 'change').action('Rename the flag.')).toEqual({
      type: 'work.done', summary: 'Rename the flag.', answer: { id: 'change', text: 'Rename the flag.' },
    })
    expect(choice(ask, 'dismiss').action('')).toEqual({ type: 'work.done', summary: 'Dismissed without an answer.', answer: { id: 'dismiss' } })
  })

  it('takes words for feedback and a custom choice', () => {
    const feedback = item({ request: { version: 1, type: 'feedback', question: 'What should the banner say?', why_person: 'Your words.' } })
    expect(keys(feedback)).toEqual(['words', 'nothing'])
    expect(choice(feedback, 'words').action('Welcome back')).toEqual({ type: 'work.done', summary: 'Welcome back', answer: { text: 'Welcome back' } })
    const custom = item({ request: { ...decision, type: 'choice', custom: true, answers: decision.answers.map(({ outcome: _, ...rest }) => rest) } })
    expect(keys(custom)).toEqual(['merge', 'hold', 'change', 'words', 'dismiss'])
  })

  it('offers only the review answers st lists on a gate, words required to send one back', () => {
    const gate = item({ attention_kind: 'human-gate', source_id: 'gate/example/review', actions: ['review.approve', 'review.request-changes'] })
    expect(keys(gate)).toEqual(['approve', 'changes'])
    expect(choice(gate, 'approve')).toMatchObject({ words: 'optional' })
    expect(choice(gate, 'approve').action('')).toEqual({ type: 'review.approve', reason: '' })
    expect(choice(gate, 'changes').action(' Split the commit. ')).toEqual({ type: 'review.request-changes', reason: 'Split the commit.' })
    expect(keys(item({ attention_kind: 'human-gate', actions: ['review.approve', 'review.reject'] }))).toEqual(['approve', 'reject'])
    expect(alertChoices(item({ attention_kind: 'human-gate', actions: [] }))).toBeNull()
  })

  it('approves a launch only with a preview to approve, sends it back with words, or cancels it', () => {
    const launch = item({ attention_kind: 'launch-approval', source_id: 'planning-session/launch/nightly', actions: ['launch.approve', 'launch.cancel'], preview: Option.some(preview) })
    expect(keys(launch)).toEqual(['approve', 'revise', 'cancel'])
    expect(choice(launch, 'approve').action('')).toEqual({ type: 'launch.approve' })
    expect(choice(launch, 'revise')).toMatchObject({ words: 'required' })
    expect(choice(launch, 'revise').action(' Add a review step. ')).toEqual({ type: 'launch.revise', feedback: 'Add a review step.' })
    expect(choice(launch, 'cancel')).toMatchObject({ tone: 'danger', words: 'none' })
    expect(keys(item({ attention_kind: 'launch-approval', actions: ['launch.approve', 'launch.cancel'], preview: Option.none() }))).toEqual(['revise', 'cancel'])
  })

  it('answers nothing st does not offer here', () => {
    expect(alertChoices(item({ actions: [] }))).toBeNull()
    expect(alertChoices(item({ attention_kind: 'launch-approval', actions: [], preview: Option.some(preview) }))).toBeNull()
    expect(alertChoices(item({ attention_kind: 'revision-approval', actions: ['mission.approve-revision'] }))).toBeNull()
  })
})

describe('conversationAlerts', () => {
  it('chooses this conversation\'s open alerts of every kind, as stui and the phone do', () => {
    const rows = [
      item({ id: 'attention/ask' }),
      item({ id: 'attention/gate', attention_kind: 'human-gate', actions: ['review.approve'] }),
      item({ id: 'attention/prompt', attention_kind: 'harness-prompt', source_id: seat }),
      item({ id: 'attention/elsewhere', conversation_id: 'agent/example/else', conversation_ids: ['agent/example/else'] }),
      item({ id: 'attention/resolved', state: 'resolved' }),
      item({ id: 'attention/update', alert: false, update: { about: 'run/example' } }),
      item({ id: 'attention/message', attention_kind: 'unread-message', alert: false }),
      item({ id: 'attention/fault', attention_kind: 'fault' }),
    ]
    expect(conversationAlerts(rows, seat).map((row) => row.id)).toEqual(['attention/ask', 'attention/gate', 'attention/prompt'])
  })
})

describe('AnswerAlert', () => {
  const now = Date.parse(at) + 5 * 60_000

  it('shows who asks, the step it holds and how long it waited, with its answers', () => {
    const html = renderToStaticMarkup(<AnswerAlert alert={item({ blocked: { step: 'build', step_run_id: 'step-run/example/build', goal: 'Build it', attempt: 1 } })} onAct={async () => ({ _tag: 'Sent' })} now={now} />)
    expect(html).toContain('Request · from example/atlas · step build · waited 5m')
    expect(html).toContain('The parser now accepts trailing commas.')
    expect(html).toContain('>Yes<')
    expect(html).toContain('>Nothing to do<')
  })

  it('leads a structured request with its recommendation, then the question, reasons and what each answer does', () => {
    const html = renderToStaticMarkup(<AnswerAlert alert={item({ request: { ...decision, reasons: ['CI is green'] } })} onAct={async () => ({ _tag: 'Sent' })} now={now} />)
    expect(html).toContain('Recommends <strong>Merge</strong>: Tests and review passed.')
    expect(html).toContain('Merge the parser change?')
    expect(html).toContain('CI is green')
    expect(html).toContain('Merge (recommended)')
    expect(html).toContain('It joins the merge queue.')
    expect(html).toContain('Why you: You own the release.')
  })

  it('links a request\'s subjects only to http(s) URLs; any other scheme or a relative path is text', () => {
    expect(webLink('https://example.com/pull/1')).toBe('https://example.com/pull/1')
    expect(webLink('http://example.com/a b')).toBe('http://example.com/a%20b')
    for (const unsafe of ['javascript:alert(1)', 'JAVASCRIPT:alert(1)', 'data:text/html,<script>alert(1)</script>', 'vbscript:x', 'file:///etc/passwd', '/relative/path', 'doc/notes', '']) {
      expect(webLink(unsafe)).toBeUndefined()
    }
    const html = renderToStaticMarkup(<AnswerAlert alert={item({ request: { ...decision, subjects: [
      { kind: 'link', label: 'The pull request', url: 'https://example.com/pull/1' },
      { kind: 'link', label: 'Run this', url: 'javascript:alert(1)' },
      { kind: 'link', label: 'Inline page', url: 'data:text/html,<b>x</b>' },
      { kind: 'document', label: 'Notes', url: '/relative/notes', ref: 'doc/notes' },
    ] } })} onAct={async () => ({ _tag: 'Sent' })} now={now} />)
    expect(html).toContain('<a href="https://example.com/pull/1" target="_blank" rel="noopener noreferrer">The pull request</a>')
    expect(html).not.toMatch(/href="(javascript|data):/i)
    expect(html).not.toContain('href="/relative/notes"')
    expect(html).toContain('<li>Run this</li>')
    expect(html).toContain('Notes<span> doc/notes</span>')
  })

  it('shows what a launch would start, or why it has nothing to approve', () => {
    const html = renderToStaticMarkup(<AnswerAlert alert={item({ attention_kind: 'launch-approval', title: 'Launch nightly', actions: ['launch.approve'], preview: Option.some(preview) })} onAct={async () => ({ _tag: 'Sent' })} now={now} />)
    expect(html).toContain('Launch approval · from example/atlas')
    expect(html).toContain('<strong>Goal:</strong> Build the nightly report')
    expect(html).toContain('Review it<span> · after build</span>')
    expect(html).toContain('Agents: example/builder on example-host')
    expect(html).toContain('Gates: 1 for a person, 2 automated')
    expect(html).toContain('>Approve launch<')
    const none = renderToStaticMarkup(<AnswerAlert alert={item({ attention_kind: 'launch-approval', detail: 'The mission file does not parse.', actions: ['launch.approve'], preview: Option.none() })} onAct={async () => ({ _tag: 'Sent' })} now={now} />)
    expect(none).toContain('There is no valid preview of this launch yet, so it cannot be approved. The mission file does not parse.')
    expect(none).not.toContain('>Approve launch<')
    expect(renderToStaticMarkup(<AnswerAlert alert={item({ attention_kind: 'revision-approval', detail: 'Add a deploy step', actions: ['mission.approve-revision'] })} onAct={async () => ({ _tag: 'Sent' })} now={now} />))
      .toContain('st work revision approve or cancel')
  })

  it('shows a JSON report as its telling fields', () => {
    const html = renderToStaticMarkup(<AnswerAlert alert={item({ detail: 'Build finished {"status":"failed","error":"linker","extra":1}' })} now={now} />)
    expect(html).toContain('<dt>status</dt>')
    expect(html).toContain('linker')
  })

  it('says where an alert is answered when this device cannot answer it here', () => {
    expect(renderToStaticMarkup(<AnswerAlert alert={item({})} now={now} />)).toContain('This device cannot answer alerts.')
    expect(renderToStaticMarkup(<AnswerAlert alert={item({ attention_kind: 'harness-login', actions: [] })} onAct={async () => ({ _tag: 'Sent' })} now={now} />))
      .toContain('Sign in from the seat&#x27;s terminal.')
    expect(renderToStaticMarkup(<AnswerAlert alert={item({ attention_kind: 'fault', actions: [] })} onAct={async () => ({ _tag: 'Sent' })} now={now} />))
      .toContain('Answer this one in st for now.')
  })

  it('opens the first of several alerts and folds the rest under their titles', () => {
    const html = renderToStaticMarkup(
      <AgentAlerts alerts={[item({ id: 'attention/one', title: 'First?' }), item({ id: 'attention/two', title: 'Second?' })]} calls={[]} onAct={async () => ({ _tag: 'Sent' })} />,
    )
    expect(html).toContain('◆ 2 alerts')
    expect(html.indexOf('First?')).toBeLessThan(html.indexOf('<details'))
    expect(html).toMatch(/<details[^>]*><summary[^>]*>◆ Second\?<\/summary>/)
  })
})

describe('gatewayAlertActions', () => {
  const envelope = (value: Record<string, unknown>) => ({ snapshot: { id: 'snapshot/now' }, value })
  const open = { kind: 'attention', state: 'open', revision: 'r1', actions: ['work.done', 'review.approve'] }
  const answered = { id: 'attention/ask', revision: 'r1', source_id: 'step-run/example/build', episode: 'claim/example' } as never

  it('reads the alert again and sends one answer fenced on the revision the person read', async () => {
    const workDone = vi.fn(async (_request: unknown) => ({ value: { status: 'completed' } }))
    const reviewApprove = vi.fn(async (_request: unknown) => ({ value: { status: 'completed' } }))
    const client = { attentionGet: vi.fn(async () => envelope(open)), workDone, reviewApprove } as unknown as St3Client
    const port = gatewayAlertActions(client)
    expect(await port.act({ alert: answered, action: { type: 'work.done', summary: 'Merge', answer: { id: 'merge' } } })).toEqual({ _tag: 'Sent' })
    expect(workDone).toHaveBeenCalledTimes(1)
    expect(workDone.mock.calls[0]![0]).toMatchObject({
      fence: { snapshot_id: 'snapshot/now', subject_revisions: { 'attention/ask': 'r1' } },
      parameters: { target_id: 'step-run/example/build', episode: 'claim/example', summary: 'Merge', answer: { id: 'merge' } },
    })
    expect(await port.act({ alert: answered, action: { type: 'review.approve', reason: '' } })).toEqual({ _tag: 'Sent' })
    expect(reviewApprove.mock.calls[0]![0]).toMatchObject({ parameters: { target_id: 'step-run/example/build' } })
    expect((reviewApprove.mock.calls[0]![0] as { parameters: object }).parameters).not.toHaveProperty('reason')
  })

  it('sends nothing for an alert that closed, changed or no longer offers the answer', async () => {
    const workDone = vi.fn()
    const reads = [
      envelope({ ...open, state: 'resolved' }),
      envelope({ ...open, revision: 'r2' }),
      envelope({ ...open, actions: [] }),
    ]
    const client = { attentionGet: vi.fn(async () => reads.shift()), workDone } as unknown as St3Client
    const port = gatewayAlertActions(client)
    const action = { type: 'work.done', summary: 'Yes' } as const
    expect(await port.act({ alert: answered, action })).toEqual({ _tag: 'Refused', detail: 'This was already answered or closed.' })
    expect(await port.act({ alert: answered, action })).toEqual({ _tag: 'Refused', detail: 'This changed since you read it. Read it again before answering.' })
    expect(await port.act({ alert: answered, action })).toEqual({ _tag: 'Refused', detail: 'st no longer offers this answer here.' })
    expect(workDone).not.toHaveBeenCalled()
  })

  it('approves exactly the variant and preview the alert shows, fenced on the alert, the launch and that variant', async () => {
    const launchApprove = vi.fn(async (_request: unknown) => ({ value: { status: 'completed' } }))
    const launchRevise = vi.fn(async (_request: unknown) => ({ value: { status: 'completed' } }))
    const launchCancel = vi.fn(async (_request: unknown) => ({ value: { status: 'completed' } }))
    const token = (digit: string) => `lpv0:${digit.repeat(64)}`
    const client = {
      // As st raises it: the alert names its variant, and its episode is the preview's hash.
      attentionGet: vi.fn(async () => envelope({ ...open, actions: ['launch.approve', 'launch.cancel'], source_id: 'planning-session/launch/nightly', variant_id: 'variant/two', episode: token('b') })),
      launchesGet: vi.fn(async () => ({ snapshot: { id: 'snapshot/launch' }, value: { kind: 'launch', id: 'launch/nightly', revision: 'l3' } })),
      launchVariantsList: vi.fn(async () => ({ snapshot: { id: 'snapshot/launch' }, value: { items: [
        { kind: 'launch-variant', id: 'variant/one', revision: 'v1', ordinal: 1, preview_token: token('a') },
        { kind: 'launch-variant', id: 'variant/two', revision: 'v2', ordinal: 2, preview_token: token('b') },
        { kind: 'launch-variant', id: 'variant/three', revision: 'v3', ordinal: 3, preview_token: token('c') },
      ] } })),
      launchApprove, launchRevise, launchCancel,
    } as unknown as St3Client
    const port = gatewayAlertActions(client)
    expect(await port.act({ alert: answered, action: { type: 'launch.approve' } })).toEqual({ _tag: 'Sent' })
    expect(client.launchesGet).toHaveBeenCalledWith('launch/nightly')
    // A newer previewed variant (three) exists, and is not approved in place of the one shown.
    expect(launchApprove.mock.calls[0]![0]).toMatchObject({
      fence: { snapshot_id: 'snapshot/launch', subject_revisions: { 'attention/ask': 'r1', 'launch/nightly': 'l3', 'variant/two': 'v2' }, preview_token: token('b') },
      parameters: { launch_id: 'launch/nightly', variant_id: 'variant/two' },
    })
    expect(await port.act({ alert: answered, action: { type: 'launch.revise', feedback: 'Add a review step.' } })).toEqual({ _tag: 'Sent' })
    expect(launchRevise.mock.calls[0]![0]).toMatchObject({ fence: { subject_revisions: { 'launch/nightly': 'l3' } }, parameters: { launch_id: 'launch/nightly', feedback: 'Add a review step.' } })
    expect(await port.act({ alert: answered, action: { type: 'launch.cancel' } })).toEqual({ _tag: 'Sent' })
    expect(launchCancel.mock.calls[0]![0]).toMatchObject({ parameters: { target_id: 'launch/nightly' } })
  })

  it('approves nothing when the shown preview changed, is gone, or the alert names none', async () => {
    const token = (digit: string) => `lpv0:${digit.repeat(64)}`
    const approve = async (alertFields: Record<string, unknown>, items: readonly unknown[]) => {
      const launchApprove = vi.fn()
      const client = {
        attentionGet: vi.fn(async () => envelope({ ...open, actions: ['launch.approve'], launch_id: 'launch/nightly', ...alertFields })),
        launchesGet: vi.fn(async () => ({ snapshot: { id: 'snapshot/launch' }, value: { kind: 'launch', id: 'launch/nightly', revision: 'l3' } })),
        launchVariantsList: vi.fn(async () => ({ snapshot: { id: 'snapshot/launch' }, value: { items } })),
        launchApprove,
      } as unknown as St3Client
      const result = await gatewayAlertActions(client).act({ alert: answered, action: { type: 'launch.approve' } })
      expect(launchApprove).not.toHaveBeenCalled()
      return result
    }
    const two = { kind: 'launch-variant', id: 'variant/two', revision: 'v2', ordinal: 2, preview_token: token('b') }
    const changed = { _tag: 'Refused', detail: 'The preview this alert shows has changed. Read it again before approving.' }
    // The same variant previewed again under a new token.
    expect(await approve({ variant_id: 'variant/two', episode: token('a') }, [two])).toEqual(changed)
    // The variant shown is gone.
    expect(await approve({ variant_id: 'variant/one', preview_token: token('a') }, [two])).toEqual(changed)
    // No variant or preview named.
    expect(await approve({ episode: token('b') }, [two])).toEqual({
      _tag: 'Refused', detail: 'This alert does not say which preview it shows, so it cannot be approved here. Review it in st.',
    })
  })

  it('says why st refused, and never sends an unconfirmed answer again', async () => {
    const refusal = new ClientError({ api_version: 'st3.client.v0', error_version: 'st3.client.error.v0', request_id: 'request/x', code: 'invalid', message: 'the step already has an answer', retryable: false, details: {} }, 422)
    const workDone = vi.fn(async () => { throw refusal })
    const port = gatewayAlertActions({ attentionGet: vi.fn(async () => envelope(open)), workDone } as unknown as St3Client)
    const result = await port.act({ alert: answered, action: { type: 'work.done', summary: 'Yes' } })
    expect(result).toMatchObject({ _tag: 'Refused', detail: expect.stringContaining('the step already has an answer') })
    const lost = gatewayAlertActions({ attentionGet: vi.fn(async () => envelope(open)), workDone: vi.fn(async () => { throw new TypeError('network') }) } as unknown as St3Client)
    expect(await lost.act({ alert: answered, action: { type: 'work.done', summary: 'Yes' } })).toEqual({
      _tag: 'Refused', detail: 'The answer could not be confirmed; check the alert before answering again. It was not sent again.',
    })
    expect(workDone).toHaveBeenCalledTimes(1)
  })
})
