import type * as Native from '@smalltalk/st3-client/schema'
import * as stylex from '@stylexjs/stylex'
import * as React from 'react'

import {
  alertConversations,
  answerLabel,
  offeredAnswers,
  pendingCalls,
  pendingQuestions,
  promptAnswers,
  promptAlertKey,
  promptCalls,
  questionAnswer,
  type PendingCall,
} from '../../../../../clients/typescript/st3-views/homeView.ts'
import { geometryVars as g, spaceVars as s } from '../../../../../packages/fractal-ui/src/assistant-ui/composition-tokens.stylex.ts'
import { useAttention, useDataSource } from '../../data/react.tsx'
import { scale, tokens } from '../../ui-compat/tokens.stylex.ts'
import { Button } from '../../ui-compat/components.tsx'

/** What the strip reads of a conversation's items: its tool calls, joined with their results. */
export type ConversationCall = { readonly _tag: string; readonly id: string } & Partial<{
  readonly callId: string
  readonly name: string
  readonly input: unknown
  readonly result: unknown
  readonly callSeen: boolean
}>
import type { PromptAnswerResult } from './prompt-answer-port.ts'

const styles = stylex.create({
  pinned: { width: '100%', minWidth: 0, boxSizing: 'border-box', paddingInline: s.section, flexShrink: 0 },
  strip: {
    maxWidth: g.lane,
    marginInline: 'auto',
    display: 'flex',
    flexDirection: 'column',
    gap: scale.space2,
    paddingBlock: s.md,
    borderTopWidth: 1,
    borderTopStyle: 'solid',
    borderTopColor: tokens['--ds-gray-alpha-400'],
  },
  alert: { display: 'flex', flexDirection: 'column', gap: scale.space1, fontSize: '0.8125rem' },
  title: { fontWeight: 600, color: tokens['--ds-gray-1000'] },
  detail: { color: tokens['--ds-gray-900'], whiteSpace: 'pre-wrap' },
  call: {
    margin: 0,
    padding: scale.space2,
    borderRadius: 4,
    backgroundColor: tokens['--ds-gray-alpha-100'],
    whiteSpace: 'pre-wrap',
    overflowWrap: 'anywhere',
    fontSize: '0.75rem',
  },
  answers: { display: 'flex', flexWrap: 'wrap', gap: scale.space2 },
  note: { color: tokens['--ds-gray-900'], fontSize: '0.75rem' },
})

/** The native prompt alerts of one agent's conversation, open ones only. */
export const promptAlerts = (items: readonly Native.Attention[], agentRef: string): readonly Native.Attention[] =>
  items.filter(
    (item) =>
      item.attention_kind === 'harness-prompt' &&
      item.state === 'open' &&
      alertConversations(item).includes(agentRef),
  )

/** The tool calls a conversation shows with no result yet, in full, as st3-views reads them. */
export const waitingCalls = (items: readonly ConversationCall[]): PendingCall[] =>
  pendingCalls(
    items.flatMap((item) =>
      item._tag === 'ToolCall' && item.callSeen === true && item.result === undefined && item.callId !== undefined
        ? [{ id: item.id, type: 'tool_call' as const, body: { call_id: item.callId, name: item.name ?? 'tool', arguments: item.input } }]
        : [],
    ),
  )

type Answer = (alert: Native.Attention, answer: string, sent: string) => Promise<PromptAnswerResult>

/**
 * The prompts an agent's harness waits on, above its composer, with the answers st can deliver:
 * the one waiting call in full beside `allow` (or a plan beside `approve`), a question's options
 * to choose and send together, a screen prompt's own answers. Anything else is answered in the
 * seat's terminal. An answer in the terminal still wins.
 */
export const AgentAlerts = ({
  alerts,
  calls,
  onAnswer,
}: {
  readonly alerts: readonly Native.Attention[]
  readonly calls: readonly PendingCall[]
  readonly onAnswer?: Answer
}): React.ReactNode => {
  if (alerts.length === 0) return null
  return (
    <div {...stylex.props(styles.pinned)} data-testid="conversation-alerts">
      <div {...stylex.props(styles.strip)}>
        {alerts.map((alert) => (
          // A new prompt or revision behind the same alert starts with nothing chosen.
          <PromptAlert key={promptAlertKey(alert)} alert={alert} calls={calls} onAnswer={onAnswer} />
        ))}
      </div>
    </div>
  )
}

const PromptAlert = ({
  alert,
  calls,
  onAnswer,
}: {
  readonly alert: Native.Attention
  readonly calls: readonly PendingCall[]
  readonly onAnswer?: Answer
}): React.ReactNode => {
  const prompt = promptAnswers(alert)
  // Only the exact call the prompt asks about, when the daemon names it.
  const shown = prompt === null ? calls : promptCalls(prompt, calls)
  const [chosen, setChosen] = React.useState<number[][]>([])
  const [status, setStatus] = React.useState<string>()
  const [sending, setSending] = React.useState(false)
  const offered = prompt === null || onAnswer === undefined ? [] : offeredAnswers(prompt, calls)
  const questions = pendingQuestions(shown)
  const gated = prompt?.answers.find((answer) => answer === 'allow' || answer === 'approve' || answer === 'answer')
  const sendable = questions === null ? null : questionAnswer(questions, chosen)
  const choose = (question: number, option: number, multi: boolean) =>
    setChosen((previous) => {
      const next = [...previous]
      const picked = next[question] ?? []
      next[question] = multi
        ? picked.includes(option) ? picked.filter((each) => each !== option) : [...picked, option].sort()
        : [option]
      return next
    })
  const send = (answer: string) => {
    const sent = answer === 'answer' ? sendable : answer
    if (sent === null || onAnswer === undefined) return
    setSending(true)
    setStatus(`Answering ${answerLabel(answer).toLowerCase()}…`)
    void onAnswer(alert, answer, sent).then((result) => {
      setSending(false)
      setStatus(result._tag === 'Sent' ? 'Answered. The alert clears once the harness moves on.' : result.detail)
    })
  }
  return (
    <section {...stylex.props(styles.alert)} aria-label={alert.title}>
      <div {...stylex.props(styles.title)}>◆ {alert.title}</div>
      {alert.detail ? <div {...stylex.props(styles.detail)}>{alert.detail}</div> : null}
      {prompt === null ? (
        <div {...stylex.props(styles.note)}>Answer it in the seat's terminal.</div>
      ) : questions !== null && gated === 'answer' ? (
        questions.map((question, index) => (
          <div key={index} {...stylex.props(styles.alert)}>
            <div {...stylex.props(styles.title)}>
              {question.header ? `${question.header} · ` : ''}
              {question.question}
              {question.multiSelect ? ' (choose any)' : ''}
            </div>
            <div {...stylex.props(styles.answers)}>
              {question.options.map((option, choice) => {
                const on = (chosen[index] ?? []).includes(choice)
                return (
                  <Button key={choice} size="sm" variant={on ? 'primary' : 'secondary'} aria-pressed={on} isDisabled={sending}
                    onPress={() => choose(index, choice, question.multiSelect)}>
                    {on ? `✓ ${option.label}` : option.label}
                  </Button>
                )
              })}
            </div>
          </div>
        ))
      ) : (
        shown.map((call) => (
          <pre key={call.id} {...stylex.props(styles.call)}>{[call.tool, ...call.lines].join('\n')}</pre>
        ))
      )}
      {prompt !== null && gated !== undefined && !offered.includes(gated) ? (
        <div {...stylex.props(styles.note)}>
          {shown.length > 1
            ? `${shown.length} calls are waiting and the prompt does not say which one it asks about: answer it in the seat's terminal.`
            : 'What it would let through is not in the conversation yet: answer it in the seat\'s terminal.'}
        </div>
      ) : null}
      {offered.length > 0 ? (
        <div {...stylex.props(styles.answers)}>
          {offered.map((answer) => (
            <Button key={answer} size="sm" variant={answer === 'deny' ? 'danger' : answer === gated ? 'primary' : 'secondary'}
              isDisabled={sending || (answer === 'answer' && sendable === null)} onPress={() => send(answer)}>
              {answer === 'deny' && gated === 'answer' ? 'Decline' : answerLabel(answer)}
            </Button>
          ))}
        </div>
      ) : null}
      {status ? <div role="status" {...stylex.props(styles.note)}>{status}</div> : null}
    </section>
  )
}

/** The agent's prompt alerts from the active data source, answered through its prompt port. */
export const LiveAgentAlerts = ({
  agentRef,
  items,
}: {
  readonly agentRef: string
  readonly items: readonly ConversationCall[]
}): React.ReactNode => {
  const source = useDataSource()
  const attention = useAttention()
  const calls = React.useMemo(() => waitingCalls(items), [items])
  const alerts = React.useMemo(
    () => (attention._tag === 'Observed' ? promptAlerts(attention.value, agentRef) : []),
    [attention, agentRef],
  )
  const prompts = source.prompts
  const onAnswer: Answer | undefined =
    prompts === undefined
      ? undefined
      : (alert, _answer, sent) => {
          const prompt = promptAnswers(alert)
          return prompt === null
            ? Promise.resolve({ _tag: 'Refused', detail: 'This prompt is answered in the seat\'s terminal.' })
            : prompts.respond({
                alert: { id: alert.id, revision: alert.revision, source_id: alert.source_id, target_id: prompt.target_id, episode: prompt.episode },
                answer: sent,
              })
        }
  return <AgentAlerts alerts={alerts} calls={calls} onAnswer={onAnswer} />
}
