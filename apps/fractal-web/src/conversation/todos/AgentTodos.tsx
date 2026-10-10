import { useAtomValue } from '@effect/atom-react'
import * as stylex from '@stylexjs/stylex'
import { DateTime, Option } from 'effect'
import * as React from 'react'

import { scale, tokens } from '../../ui-compat/tokens.stylex.ts'
import { colorVars as c, geometryVars as g, spaceVars as s } from '../../../../../packages/fractal-ui/src/assistant-ui/composition-tokens.stylex.ts'

import { useDataSource } from '../../data/react.tsx'
import { agentTodosAtom, type TodoState } from './model.ts'

const occurrenceKey = ({ counts, parts }: { counts: Map<string, number>; parts: readonly string[] }): string => {
  const key = JSON.stringify(parts)
  const occurrence = counts.get(key) ?? 0
  counts.set(key, occurrence + 1)
  return `${key}:${occurrence}`
}

const styles = stylex.create({
  resources: { color: 'inherit', fontSize: '0.75rem' },
  pinned: {
    width: '100%',
    minWidth: 0,
    boxSizing: 'border-box',
    paddingInline: s.section,
    flexShrink: 0,
  },
  detail: { padding: scale.space4, color: tokens['--ds-gray-1000'], fontSize: '0.8125rem' },
  strip: {
    maxWidth: g.lane,
    marginInline: 'auto',
    color: tokens['--ds-gray-1000'],
    fontSize: '0.75rem',
    borderTopWidth: 1,
    borderTopStyle: 'solid',
    borderTopColor: tokens['--ds-gray-alpha-400'],
  },
  summary: {
    cursor: 'pointer',
    paddingBlock: s.md,
    minWidth: 0,
    outline: { default: 'none', ':focus-visible': `2px solid ${tokens['--ds-blue-700']}` },
    borderRadius: scale.radiusDefault,
  },
  summaryRow: {
    display: 'inline-flex',
    verticalAlign: 'middle',
    width: 'calc(100% - 18px)',
    alignItems: 'center',
    gap: s.md,
    minWidth: 0,
  },
  phase: {
    flexGrow: 1,
    minWidth: 0,
    overflow: 'hidden',
    textOverflow: 'ellipsis',
    whiteSpace: 'nowrap',
  },
  count: { fontVariantNumeric: 'tabular-nums', flexShrink: 0 },
  source: { color: tokens['--ds-gray-900'], flexShrink: 0, fontSize: '0.6875rem' },
  stale: { color: tokens['--ds-amber-900'] },
  body: { maxHeight: 'min(40vh, 320px)', overflowY: 'auto', paddingBottom: scale.space2 },
  heading: {
    margin: 0,
    display: 'flex',
    justifyContent: 'space-between',
    gap: s.md,
    fontSize: '0.75rem',
    fontWeight: 500,
    paddingBlock: s.md,
  },
  list: { listStyle: 'none', margin: 0, padding: 0 },
  task: { display: 'flex', gap: s.md, paddingBlock: scale.space1, lineHeight: 1.5 },
  content: { minWidth: 0, overflowWrap: 'anywhere' },
  done: { color: c.fg },
  status: { color: tokens['--ds-gray-900'], width: 16, flexShrink: 0, textAlign: 'center' },
  active: { color: tokens['--ds-blue-900'] },
  note: {
    margin: 0,
    color: tokens['--ds-gray-900'],
    fontSize: '0.6875rem',
    lineHeight: 1.5,
    overflowWrap: 'anywhere',
  },
  provenance: {
    borderTopWidth: 1,
    borderTopStyle: 'solid',
    borderTopColor: tokens['--ds-gray-alpha-400'],
    marginTop: scale.space2,
    paddingTop: scale.space2,
  },
})

const statusLabels = {
  pending: 'Pending',
  in_progress: 'In progress',
  completed: 'Completed',
  blocked: 'Blocked',
}
const statusMarks = { pending: '○', in_progress: '◉', completed: '✓', blocked: '!' }

/** Read-only accepted harness facts; no task mutation or transcript-derived authority. */
export const AgentTodos = ({
  state,
  detail = false,
  placement = 'pinned',
}: {
  readonly state: TodoState
  readonly detail?: boolean
  /** `resources`: a section of the Resources view; it names every state instead of hiding unobserved ones. */
  readonly placement?: 'pinned' | 'resources'
}): React.ReactNode => {
  if (state._tag !== 'Observed') {
    if (!detail && placement !== 'resources') return null
    return (
      <p role="status" {...stylex.props(placement === 'resources' ? styles.resources : styles.detail)}>
        {state._tag === 'Waiting'
          ? 'Waiting for todo observations…'
          : state._tag === 'Unavailable'
            ? `Todos unavailable: ${state.detail}`
            : 'No harness todo snapshot observed. This is not an empty todo list.'}
      </p>
    )
  }
  const { todo, completed, total, phase, binding } = state
  const stale = binding === 'stale'
  const snapshot = todo.snapshot
  const label =
    phase?.name ??
    (total === 0
      ? 'No tasks'
      : completed === total
        ? 'Complete'
        : 'No open phase in displayed tasks')
  const phaseOccurrences = new Map<string, number>()
  const list = (
    <>
      {snapshot.phases.map((listedPhase) => {
        const phaseKey = occurrenceKey({ counts: phaseOccurrences, parts: [listedPhase.name] })
        const taskOccurrences = new Map<string, number>()
        return (
          <section key={phaseKey} aria-label={listedPhase.name}>
            <h3 {...stylex.props(styles.heading)}>
              <span>{listedPhase.name}</span>
              <span {...stylex.props(styles.count)}>
                {listedPhase.tasks.filter((task) => task.status === 'completed').length}/
                {listedPhase.tasks.length}
              </span>
            </h3>
            <ul {...stylex.props(styles.list)}>
              {listedPhase.tasks.map((task) => (
                <li
                  key={occurrenceKey({ counts: taskOccurrences, parts: [task.content] })}
                  {...stylex.props(styles.task)}
                >
                  <span
                    aria-label={statusLabels[task.status]}
                    {...stylex.props(
                      styles.status,
                      task.status === 'in_progress' && styles.active,
                      task.status === 'blocked' && styles.stale,
                    )}
                  >
                    {statusMarks[task.status]}
                  </span>
                  <span
                    {...stylex.props(styles.content, task.status === 'completed' && styles.done)}
                  >
                    {task.content}
                    {Option.isSome(task.blocker) ? (
                      <p {...stylex.props(styles.note, styles.stale)}>
                        Blocked: {task.blocker.value}
                      </p>
                    ) : null}
                  </span>
                </li>
              ))}
            </ul>
          </section>
        )
      })}
      {snapshot.phases.length === 0 ? (
        <p {...stylex.props(styles.note)}>No displayed tasks in this snapshot.</p>
      ) : null}
      {snapshot.truncated ? (
        <p {...stylex.props(styles.note)}>
          Task list truncated by the harness. Progress uses full-source totals.
        </p>
      ) : null}
      {(snapshot.totals.abandoned ?? 0) > 0 ? (
        <p {...stylex.props(styles.note)}>
          {snapshot.totals.abandoned} abandoned · not counted as completed.
        </p>
      ) : null}
      <div {...stylex.props(styles.provenance)}>
        <p {...stylex.props(styles.note)}>
          Source: fact.todo · {snapshot.harness} / {snapshot.source_op} ·{' '}
          {binding === 'pending' ? 'binding verification pending' : stale ? 'stale snapshot' : 'current binding'}
        </p>
        <p {...stylex.props(styles.note)}>
          Observed{' '}
          <time dateTime={DateTime.formatIso(snapshot.observed_at)}>
            {DateTime.formatIso(snapshot.observed_at)}
          </time>{' '}
          · accepted {DateTime.formatIso(todo.accepted_at)}
        </p>
        <p {...stylex.props(styles.note)}>
          Claim {todo.claim_id} · session {snapshot.session_id} · incarnation{' '}
          {snapshot.incarnation_id}
        </p>
      </div>
    </>
  )
  return detail ? (
    <section aria-label="Harness todos" {...stylex.props(styles.detail)}>
      <h2 {...stylex.props(styles.heading)}>
        <span>Todos · {label}</span>
        <span {...stylex.props(styles.count)}>
          {completed}/{total} completed{binding === 'pending' ? ' · Pending' : stale ? ' · Stale' : ''}
        </span>
      </h2>
      {list}
    </section>
  ) : (
    <div {...stylex.props(placement === 'resources' ? styles.resources : styles.pinned)}>
      <details aria-label="Harness todos" {...stylex.props(placement === 'pinned' && styles.strip)}>
        <summary {...stylex.props(styles.summary)}>
          <span {...stylex.props(styles.summaryRow)}>
            <span title={label} {...stylex.props(styles.phase)}>
              Todos · {label}
            </span>
            <span
              aria-label={`${completed} of ${total} tasks completed`}
              {...stylex.props(styles.count)}
            >
              {completed}/{total}
            </span>
            <span {...stylex.props(styles.source, stale && styles.stale)}>
              {binding === 'pending' ? 'Pending · ' : stale ? 'Stale · ' : ''}
              {snapshot.harness}
              {snapshot.truncated ? ' · Partial' : ''}
            </span>
          </span>
        </summary>
        <div {...stylex.props(styles.body)}>{list}</div>
      </details>
    </div>
  )
}

/** Renders the shared todo projection for an agent from the active gateway data source. */
export const LiveAgentTodos = ({
  agentRef,
  detail = false,
  placement,
}: {
  readonly agentRef: string
  readonly detail?: boolean
  readonly placement?: 'pinned' | 'resources'
}): React.ReactNode => {
  const source = useDataSource()
  const state = useAtomValue(agentTodosAtom({ source, agentRef }))
  return <AgentTodos state={state} detail={detail} {...(placement === undefined ? {} : { placement })} />
}
