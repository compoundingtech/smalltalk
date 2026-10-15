import { RegistryContext, useAtomValue } from '@effect/atom-react'
import { DateTime, Option } from 'effect'
import * as Atom from 'effect/reactivity/Atom'
import * as React from 'react'
import { useClockTime, useClockWake } from '@smalltalk/fractal-ui/human-time/clock'
import { useAgent, useAgentRow, useDataSource, useSubjectList } from '../data/react.tsx'
import { nativeSessionStartReason, projectNativeDetails, type NativeDetailsObservation } from '../data/nativeDetails.ts'
import { unavailable, type DataSource, type Feed } from '../data/source.ts'
import type * as Native from '@smalltalk/st3-client/schema'
import type { NativeDeclarations } from '../data/nativeDeclarations.ts'

export type { NativeDetailsObservation } from '../data/nativeDetails.ts'
const notReported = Atom.make(unavailable({ reason: 'unsupported', detail: 'The selected native fact is not reported.' }))

interface SessionStartGuard {
  readonly earliest: number
  readonly rejectedReason?: string
}
// Source ownership outlives a Details mount, but never retains a discarded source.
const sessionStartGuards = new WeakMap<DataSource, Map<string, Map<string, Map<string | undefined, SessionStartGuard>>>>()

/** Immutable earliest/rejected starts are fenced by source, selected agent, session and incarnation. */
export const guardedSessionTiming = ({ input, source, agentRef }: { readonly input: Atom.Atom<Feed<Native.Session>>; readonly source: DataSource; readonly agentRef: string }): Atom.Atom<Feed<Native.Session>> => {
  let agents = sessionStartGuards.get(source)
  if (agents === undefined) { agents = new Map(); sessionStartGuards.set(source, agents) }
  let sessions = agents.get(agentRef)
  if (sessions === undefined) { sessions = new Map(); agents.set(agentRef, sessions) }
  const guards = sessions
  return Atom.make(get => {
    const feed = get(input)
    if (feed._tag !== 'Observed' || feed.error?.reason === 'ungranted') return feed
    const session = feed.value
    if (session.owner_id !== agentRef) return feed
    const start = DateTime.toEpochMillis(session.started_at)
    const incarnation = Option.getOrUndefined(session.runtime_incarnation)
    let incarnations = guards.get(session.id)
    if (incarnations === undefined) { incarnations = new Map(); guards.set(session.id, incarnations) }
    let guard = incarnations.get(incarnation)
    const reason = nativeSessionStartReason({ session, snapshotCreatedAt: feed.snapshotCreatedAt })
    if (guard === undefined || start < guard.earliest) {
      guard = { earliest: start, ...(reason === undefined ? {} : { rejectedReason: reason }) }
      incarnations.set(incarnation, guard)
    } else if (start === guard.earliest && reason !== undefined && guard.rejectedReason === undefined) {
      guard = { ...guard, rejectedReason: reason }
      incarnations.set(incarnation, guard)
    }
    const detail = start > guard.earliest
      ? 'Session start changed between observations; elapsed time and age are unreported.'
      : guard.rejectedReason
    return detail === undefined ? feed : unavailable({ reason: 'failed', detail })
  })
}

/** Mount only with the Details surface: atom lifetimes release every selected getter demand. */
export const useNativeDetails = ({ agentRef }: { readonly agentRef: string }): NativeDetailsObservation => {
  const source = useDataSource()
  const registry = React.useContext(RegistryContext)
  const now = useClockTime()
  const agent = useAgent(agentRef)
  const row = useAgentRow(agentRef)
  const selected = row._tag === 'Observed' ? row.value : undefined
  const sessionRef = selected === undefined ? undefined : Option.getOrUndefined(selected.current_session_id)
  const sessionAtom: Atom.Atom<Feed<Native.Session>> = sessionRef === undefined ? notReported : source.subjectReads.session.feed(sessionRef)
  const timingSessionAtom = React.useMemo(() => guardedSessionTiming({ input: sessionAtom, source, agentRef }), [sessionAtom, source, agentRef])
  const session = useAtomValue(timingSessionAtom)
  const periodAtom: Atom.Atom<Feed<Native.UsagePeriod>> = source.nativeDetails?.usagePeriod ?? notReported
  const period = useAtomValue(periodAtom)
  const declarationAtom: Atom.Atom<Feed<NativeDeclarations>> = selected === undefined ? notReported : source.nativeDetails?.declarations({ agentRef, revision: selected.revision }) ?? notReported
  const declarations = useAtomValue(declarationAtom)
  const missions = useAtomValue(source.missions)
  const queueAtom = source.subjectReads.agentQueue.feed(agentRef)
  const queue = useAtomValue(queueAtom)
  const subjects = useSubjectList()
  const needsMe = agent._tag === 'Observed' && agent.error?.reason !== 'ungranted' && agent.value !== undefined
    ? agent.value.status === 'waiting' || subjects.some(subject => subject.ref === agentRef && subject.attention === true) : undefined
  const onRetry = () => {
    const retrySubject = (atom: Atom.Atom<Feed<unknown>>) => {
      const feed = registry.get(atom)
      if ((feed._tag === 'Unavailable' && feed.reason === 'failed') ||
        (feed._tag === 'Observed' && feed.error?.reason === 'failed')) registry.refresh(atom)
    }
    retrySubject(sessionAtom)
    retrySubject(queueAtom)
    source.nativeDetails?.retry(periodAtom)
    source.nativeDetails?.retry(declarationAtom)
    if ((row._tag === 'Unavailable' && row.reason === 'failed') ||
      (row._tag === 'Observed' && row.freshness === 'stale')) source.reconnect?.()
  }
  const observation = projectNativeDetails({ agentRef, agent, row, session, period, declarations, missions, queue, needsMe, now, onRetry })
  useClockWake(observation.nextChangeAt)
  return observation
}
