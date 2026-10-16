import type * as Native from '@smalltalk/st3-client/schema'
import * as stylex from '@stylexjs/stylex'
import { DateTime } from 'effect'
import * as React from 'react'

import { cleanMessageText } from '../../../../../clients/typescript/st3-views/conversationView.ts'
import { ANSWERS, isRequest, personAnswer, report, spaced, yesNo } from '../../../../../clients/typescript/st3-views/requestView.ts'
import { ago } from '../../../../../clients/typescript/st3-views/time.ts'
import { Markdown } from '../../../../../packages/fractal-ui/src/assistant-ui/composition/Markdown.tsx'
import { AlertButton } from './AlertButton.tsx'
import { tokens } from '../../ui-compat/tokens.stylex.ts'
import type { AlertAction, AlertActionResult } from './alert-action-port.ts'

// Asks and review gates in an agent's conversation, answered in place with the answers stui and
// the phone give: a structured request's named answers, Yes and No for a yes-or-no question, the
// person's own words, Nothing to do; a gate's approve, send back or reject. Every answer is
// confirmed before it is sent, so a stray click answers nothing.

/** One answer an alert offers, and what it sends. */
export type AlertChoice = {
  readonly key: string
  readonly label: string
  /** What happens next, as the request says. */
  readonly consequence?: string
  readonly recommended?: boolean
  /** Whether this answer takes the person's words: never, if they wrote some, or only with them. */
  readonly words: 'none' | 'optional' | 'required'
  readonly tone: 'primary' | 'secondary' | 'danger'
  readonly action: (words: string) => AlertAction
}

const done = (summary: string, answer?: { readonly id?: string; readonly text?: string }): AlertAction => ({
  type: 'work.done',
  summary,
  ...(answer === undefined ? {} : { answer }),
})

/** The person's answer to a structured request, as stui types it; the words become the summary. */
const typed = (request: NonNullable<Native.Attention['request']>, chosen: string | undefined, words: string) => {
  const answer = personAnswer(request as Parameters<typeof personAnswer>[0], chosen, words)
  return typeof answer === 'object' ? answer : undefined
}

/**
 * The answers an alert offers here, or null when it is answered elsewhere. A request is answered
 * only while st offers `work.done` on it; a gate offers only the `review.*` actions st lists.
 */
export const alertChoices = (alert: Native.Attention): readonly AlertChoice[] | null => {
  const offered = alert.actions as readonly string[]
  if (isRequest(alert.attention_kind) && offered.includes('work.done')) {
    const request = alert.request
    if (request !== undefined) {
      const choices: AlertChoice[] = (request.answers ?? []).map((answer) => ({
        key: answer.id,
        label: answer.label,
        consequence: answer.consequence,
        recommended: request.recommendation?.answer === answer.id,
        // Requesting changes carries the changes, in the person's words.
        words: answer.outcome === 'request_changes' ? 'required' : 'none',
        tone: answer.outcome === 'decline' ? 'danger' : request.recommendation?.answer === answer.id ? 'primary' : 'secondary',
        action: (words) => done(answer.outcome === 'request_changes' ? words.trim() : answer.label, typed(request, answer.id, words)),
      }))
      if (request.type === 'feedback' || (request.type === 'choice' && request.custom === true)) {
        choices.push({
          key: 'words', label: 'Send', words: 'required', tone: choices.length === 0 ? 'primary' : 'secondary',
          action: (words) => done(words.trim(), typed(request, undefined, words)),
        })
      }
      // A request with named answers is set aside by name; st tells its asker it was dismissed.
      choices.push((request.answers ?? []).length > 0
        ? { key: 'dismiss', label: 'Dismiss', words: 'none', tone: 'secondary', action: () => done('Dismissed without an answer.', { id: 'dismiss' }) }
        : { key: 'nothing', label: 'Nothing to do', words: 'none', tone: 'secondary', action: () => done(ANSWERS.nothing, typed(request, undefined, ANSWERS.nothing)) })
      return choices
    }
    return [
      ...(yesNo(alert.title)
        ? [
            { key: 'yes', label: 'Yes', words: 'none', tone: 'primary', action: () => done(ANSWERS.yes) },
            { key: 'no', label: 'No', words: 'none', tone: 'secondary', action: () => done(ANSWERS.no) },
          ] satisfies AlertChoice[]
        : []),
      { key: 'words', label: 'Send', words: 'required', tone: yesNo(alert.title) ? 'secondary' : 'primary', action: (words) => done(words.trim()) },
      { key: 'nothing', label: 'Nothing to do', words: 'none', tone: 'secondary', action: () => done(ANSWERS.nothing) },
    ]
  }
  if (alert.attention_kind === 'human-gate') {
    const choices: AlertChoice[] = []
    // Notes written on a review go with an approval too.
    if (offered.includes('review.approve'))
      choices.push({ key: 'approve', label: 'Approve', words: 'optional', tone: 'primary', action: (words) => ({ type: 'review.approve', reason: words.trim() }) })
    // A feedback gate offers request-changes where an approval gate offers reject.
    if (offered.includes('review.request-changes'))
      choices.push({ key: 'changes', label: 'Send back', words: 'required', tone: 'secondary', action: (words) => ({ type: 'review.request-changes', reason: words.trim() }) })
    if (offered.includes('review.reject'))
      choices.push({ key: 'reject', label: 'Reject', words: 'required', tone: 'danger', action: (words) => ({ type: 'review.reject', reason: words.trim() }) })
    return choices.length > 0 ? choices : null
  }
  return null
}

/**
 * A request's link, only when it is an absolute http(s) URL. The URL is the asking agent's, so
 * any other scheme (javascript:, data:, …) or a relative path shows as plain text, never a link.
 */
export const webLink = (url: string | undefined): string | undefined => {
  if (url === undefined) return undefined
  try {
    const parsed = new URL(url)
    return parsed.protocol === 'http:' || parsed.protocol === 'https:' ? parsed.href : undefined
  } catch {
    return undefined
  }
}

/** Who an alert is from: the agent that asked, else the seat it is about. */
export const alertFrom = (alert: Pick<Native.Attention, 'requester_id' | 'source_id'>): string =>
  (alert.requester_id ?? alert.source_id).replace(/^agent\//, '')

const KIND_WORDS: Record<string, string> = {
  'human-gate': 'Review',
  'person-step': 'Request',
  'agent-request': 'Request',
  'launch-approval': 'Launch approval',
  'revision-approval': 'Revision approval',
  'harness-login': 'Sign-in',
}

const styles = stylex.create({
  alert: { display: 'flex', flexDirection: 'column', gap: '8px', fontSize: '0.8125rem' },
  title: { fontWeight: 600, color: tokens['--ds-gray-1000'] },
  meta: { color: tokens['--ds-gray-900'], fontSize: '0.75rem' },
  body: { color: tokens['--ds-gray-1000'], overflowWrap: 'anywhere' },
  report: { margin: 0, display: 'grid', gridTemplateColumns: 'max-content 1fr', columnGap: '8px', fontSize: '0.75rem' },
  key: { color: tokens['--ds-gray-900'] },
  fault: { color: tokens['--ds-red-900'] },
  soft: { color: tokens['--ds-gray-900'] },
  value: { margin: 0 },
  list: { margin: 0, paddingInlineStart: '1.25rem' },
  answers: { display: 'flex', flexWrap: 'wrap', gap: '8px', alignItems: 'center' },
  consequence: { color: tokens['--ds-gray-900'], fontSize: '0.75rem' },
  words: {
    width: '100%',
    boxSizing: 'border-box',
    minHeight: '3.5rem',
    resize: 'vertical',
    padding: '8px',
    borderRadius: 4,
    borderWidth: 1,
    borderStyle: 'solid',
    borderColor: tokens['--ds-gray-alpha-400'],
    backgroundColor: 'transparent',
    color: 'inherit',
    font: 'inherit',
  },
  note: { color: tokens['--ds-gray-900'], fontSize: '0.75rem' },
})

/** A request's question: a JSON report as its telling fields, anything else as Markdown. */
const Question = ({ text }: { readonly text: string }): React.ReactNode => {
  const shown = report(text)
  if (shown === null) return <div {...stylex.props(styles.body)}><Markdown text={spaced(text)} /></div>
  return (
    <div {...stylex.props(styles.body)}>
      {shown.before ? <Markdown text={shown.before} /> : null}
      <dl {...stylex.props(styles.report)}>
        {shown.rows.map((row) => (
          <React.Fragment key={row.key}>
            <dt {...stylex.props(styles.key)}>{row.key}</dt>
            <dd {...stylex.props(styles.value, row.tone === 'fault' ? styles.fault : row.tone === 'soft' ? styles.soft : null)}>{row.value}</dd>
          </React.Fragment>
        ))}
      </dl>
      {shown.more > 0 ? <div {...stylex.props(styles.soft)}>… {shown.more} more fields</div> : null}
    </div>
  )
}

const Body = ({ alert }: { readonly alert: Native.Attention }): React.ReactNode => {
  const request = alert.request
  if (request !== undefined) {
    const label = (id: string) => request.answers?.find((answer) => answer.id === id)?.label ?? id
    return (
      <>
        {request.recommendation ? (
          <div {...stylex.props(styles.meta)}>Recommends <strong>{label(request.recommendation.answer)}</strong>: {request.recommendation.reason}</div>
        ) : null}
        <Question text={request.question} />
        {request.summary ? <div {...stylex.props(styles.body)}><Markdown text={spaced(request.summary)} /></div> : null}
        {request.reasons?.length ? <ul {...stylex.props(styles.list)}>{request.reasons.map((reason, index) => <li key={index}>{reason}</li>)}</ul> : null}
        {request.subjects?.length ? (
          <ul {...stylex.props(styles.list)}>
            {request.subjects.map((subject, index) => (
              <li key={index}>
                {webLink(subject.url) !== undefined ? <a href={webLink(subject.url)} target="_blank" rel="noopener noreferrer">{subject.label}</a> : subject.label}
                {subject.ref && webLink(subject.url) === undefined ? <span {...stylex.props(styles.soft)}> {subject.ref}</span> : null}
              </li>
            ))}
          </ul>
        ) : null}
        <div {...stylex.props(styles.note)}>Why you: {request.why_person}</div>
      </>
    )
  }
  const text = cleanMessageText(alert.detail ?? '')
  return (
    <>
      {text ? isRequest(alert.attention_kind) ? <Question text={text} /> : <div {...stylex.props(styles.body)}><Markdown text={spaced(text)} /></div> : null}
      {alert.because ? <div {...stylex.props(styles.meta)}>because {alert.because}</div> : null}
    </>
  )
}

type Act = (alert: Native.Attention, action: AlertAction) => Promise<AlertActionResult>

/**
 * An ask, a review gate, or another alert st shows in this conversation. Answers show only when
 * this device can send them; otherwise the alert says where it is answered.
 */
export const AnswerAlert = ({
  alert,
  onAct,
  now = Date.now(),
}: {
  readonly alert: Native.Attention
  readonly onAct?: Act
  readonly now?: number
}): React.ReactNode => {
  const choices = alertChoices(alert)
  const from = alertFrom(alert)
  const [words, setWords] = React.useState('')
  const [confirming, setConfirming] = React.useState<AlertChoice>()
  const [sending, setSending] = React.useState(false)
  const [status, setStatus] = React.useState<string>()
  // A new revision may carry a new question under the same id: a pending confirmation or an
  // earlier answer's status belongs to the revision the person read. Their words stay theirs.
  const [read, setRead] = React.useState(alert.revision)
  if (read !== alert.revision) {
    setRead(alert.revision)
    setConfirming(undefined)
    if (!sending) setStatus(undefined)
  }
  const wordsId = React.useId()
  const takesWords = choices?.some((choice) => choice.words !== 'none') === true
  const send = (choice: AlertChoice) => {
    if (onAct === undefined) return
    setConfirming(undefined)
    setSending(true)
    setStatus(`Sending “${choice.label}”…`)
    void onAct(alert, choice.action(words)).then((result) => {
      setSending(false)
      setStatus(result._tag === 'Sent' ? 'Answered. The alert clears once st records it.' : result.detail)
    })
  }
  const step = alert.blocked?.step ?? alert.step_run_id?.split('/').pop()
  return (
    <section {...stylex.props(styles.alert)} aria-label={alert.title} data-alert-kind={alert.attention_kind}>
      <div {...stylex.props(styles.title)}>◆ {alert.title}</div>
      <div {...stylex.props(styles.meta)}>
        {KIND_WORDS[alert.attention_kind] ?? 'Alert'} · from {from}
        {step ? ` · step ${step}` : ''} · waited {ago(DateTime.formatIso(alert.requested_at), now)}
      </div>
      <Body alert={alert} />
      {choices === null ? (
        <div {...stylex.props(styles.note)}>
          {alert.attention_kind === 'harness-login'
            ? 'Sign in from the seat\'s terminal. One sign-in answers every seat that shares it.'
            : 'Answer this one in st for now.'}
        </div>
      ) : onAct === undefined ? (
        <div {...stylex.props(styles.note)}>This device cannot answer alerts. Answer it in st.</div>
      ) : (
        <>
          {takesWords ? (
            <textarea
              id={wordsId}
              aria-label={`Your answer to ${from}`}
              placeholder={choices.some((choice) => choice.words === 'required') ? 'Your words, for an answer that needs them' : 'Notes (optional)'}
              value={words}
              disabled={sending}
              onChange={(event) => setWords(event.target.value)}
              {...stylex.props(styles.words)}
            />
          ) : null}
          {confirming === undefined ? (
            <div {...stylex.props(styles.answers)}>
              {choices.map((choice) => (
                <AlertButton
                  key={choice.key}
                  tone={choice.tone}
                  isDisabled={sending || (choice.words === 'required' && words.trim() === '')}
                  aria-describedby={choice.consequence ? `${wordsId}-${choice.key}` : undefined}
                  onPress={() => setConfirming(choice)}
                >
                  {choice.recommended ? `${choice.label} (recommended)` : choice.label}
                </AlertButton>
              ))}
            </div>
          ) : (
            <div {...stylex.props(styles.answers)} role="group" aria-label="Confirm your answer">
              <span>Send “{confirming.label}” to {from}?</span>
              <AlertButton tone="primary" onPress={() => send(confirming)}>Send</AlertButton>
              <AlertButton tone="secondary" onPress={() => setConfirming(undefined)}>Cancel</AlertButton>
            </div>
          )}
          {choices.some((choice) => choice.consequence) ? (
            <ul {...stylex.props(styles.list, styles.consequence)}>
              {choices.filter((choice) => choice.consequence).map((choice) => (
                <li key={choice.key} id={`${wordsId}-${choice.key}`}><strong>{choice.label}</strong>: {choice.consequence}</li>
              ))}
            </ul>
          ) : null}
        </>
      )}
      {status ? <div role="status" {...stylex.props(styles.note)}>{status}</div> : null}
    </section>
  )
}
