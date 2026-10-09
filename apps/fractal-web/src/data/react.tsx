import { RegistryContext, useAtomValue } from '@effect/atom-react'
/** Shared derivations over generated rows, installed once per data source. */
import type { Agent as AgentRow, Attention, Mission } from '@smalltalk/st3-client/schema'
import * as Atom from 'effect/reactivity/Atom'
import type * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import { createContext, type ReactNode, useContext } from 'react'
import * as React from 'react'

import type { MissionView } from '../missions/model.ts'
import type { SubjectSummary } from '../shell/context.tsx'
import { createProjections } from './projections.ts'
import { type Agent, type DataSource, type Feed, type Fleet, observed } from './source.ts'
import type { FeedSync, FeedSyncObservation } from './feedSync.ts'

/** Navigable observations plus the actual availability of each independently followed family. */
export interface SubjectIndex {
  readonly subjects: readonly SubjectSummary[]
  readonly families: {
    readonly agents: Feed<Fleet>
    readonly missions: Feed<readonly Mission[]>
    readonly attention: Feed<readonly Attention[]>
  }
  /** Retained resource discoveries only, not a claim that a resource collection is loaded or empty. */
  readonly resources?: readonly SubjectSummary[]
}

interface DerivedData {
  readonly source: DataSource
  readonly fleet: Atom.Atom<Feed<Fleet>>
  readonly missions: Atom.Atom<Feed<readonly MissionView[]>>
  readonly subjects: Atom.Atom<readonly SubjectSummary[]>
  readonly subjectIndex: Atom.Atom<SubjectIndex>
  readonly agent: (ref: string) => Atom.Atom<Feed<Agent | undefined>>
  readonly agentRow: (ref: string) => Atom.Atom<Feed<AgentRow | undefined>>
  readonly agentAttention: (ref: string) => Atom.Atom<Feed<readonly Attention[]>>
  readonly terminalConnected: (ref: string) => Atom.Atom<boolean | undefined>
}
const derive = (source: DataSource) => {
  const { fleetFromAgents, missionViews, subjectList } = createProjections()
  const fleet = Atom.make((get) => mapFeed({ feed: get(source.agents), map: fleetFromAgents }))
  const missions = Atom.make((get) => {
    const rows = get(source.missions)
    const attention = get(source.attention)
    if (rows._tag !== 'Observed') return rows
    if (attention._tag !== 'Observed') return attention
    return observed({
      value: missionViews({
        missions: rows.value,
        attention: attention.value,
        ...(source.proposed === undefined ? {} : { proposed: source.proposed }),
      }),
      freshness: rows.freshness === 'stale' || attention.freshness === 'stale' ? 'stale' : 'live',
    })
  })
  const subjectIndex = Atom.make((get): SubjectIndex => {
    const agents = get(fleet)
    const rows = get(source.missions)
    const attention = get(source.attention)
    const resources = source.resources === undefined ? undefined : get(source.resources.subjects)
    const familySubjects = subjectList({
      ...(agents._tag === 'Observed' ? { fleet: agents.value } : {}),
      ...(rows._tag === 'Observed' ? { missions: rows.value } : {}),
      ...(attention._tag === 'Observed' ? { attention: attention.value } : {}),
    })
    // Cached reachability is an observation, never a live status or terminal authority.
    const visibleSubjects =
      agents._tag === 'Observed' && agents.freshness === 'stale'
        ? familySubjects.map((subject) => {
            if (subject.icon !== 'conversation' && subject.icon !== 'terminal') return subject
            const { status: _status, ...cachedSubject } = subject
            return { ...cachedSubject, detail: `${subject.detail ?? ''} · stale` }
          })
        : familySubjects
    return {
      subjects: [...visibleSubjects, ...(resources ?? [])],
      families: { agents, missions: rows, attention },
      ...(resources === undefined ? {} : { resources }),
    }
  })
  const subjects = Atom.map(subjectIndex, (index) => index.subjects).pipe(
    // Eight scalars; every other SubjectSummary field derives from these or belongs to a family feed.
    Atom.withEquality<readonly SubjectSummary[]>(
      (a, b) =>
        a.length === b.length &&
        a.every((subject, i) => {
          const other = b[i]
          return (
            other !== undefined &&
            subject.ref === other.ref &&
            subject.title === other.title &&
            subject.detail === other.detail &&
            subject.host === other.host &&
            subject.harness === other.harness &&
            subject.icon === other.icon &&
            subject.status === other.status &&
            subject.attention === other.attention
          )
        }),
    ),
  )
  // Index once per publication, not one linear roster scan for every visible row.
  const agentIndex = Atom.map(fleet, (feed) =>
    mapFeed({ feed, map: (value) => new Map(value.agents.map((agent) => [agent.ref, agent])) }),
  )
  const rowIndex = Atom.map(source.agents, (feed) =>
    mapFeed({ feed, map: (rows) => new Map<string, AgentRow>(rows.map((row) => [row.id, row])) }),
  )
  const agent = Atom.family((ref: string) =>
    Atom.make((get) => mapFeed({ feed: get(agentIndex), map: (rows) => rows.get(ref) })).pipe(
      Atom.withEquality<Feed<Agent | undefined>>(sameFeed),
    ),
  )
  const agentRow = Atom.family((ref: string) =>
    Atom.make((get) => mapFeed({ feed: get(rowIndex), map: (rows) => rows.get(ref) })).pipe(
      Atom.withEquality<Feed<AgentRow | undefined>>(sameFeed),
    ),
  )
  const agentAttention = Atom.family((ref: string) =>
    Atom.make((get) => {
      const row = get(agentRow(ref))
      const missionIds = new Set<string>(
        row._tag === 'Observed' ? row.value?.current_work?.map((work) => work.mission_id) : [],
      )
      return mapFeed({
        feed: get(source.attention),
        map: (cards) =>
          cards.filter(
            (card) =>
              card.state === 'open' &&
              (card.source_id === ref ||
                card.requester_id === ref ||
                (card.mission_id !== undefined && missionIds.has(card.mission_id))),
          ),
      })
    }).pipe(Atom.withEquality<Feed<readonly Attention[]>>((a, b) => sameFeed(a, b, sameItems))),
  )
  const terminalConnected = Atom.family((ref: string) =>
    Atom.make((get) => {
      const feed = get(fleet)
      if (feed._tag !== 'Observed') return undefined
      const owner = get(agent(`agent/${ref.slice('terminal/'.length)}`))
      if (owner._tag !== 'Observed' || owner.value === undefined) return undefined
      return feed.value.hosts.find((host) => host.id === owner.value?.host)?.connected
    }),
  )
  return {
    source,
    fleet,
    missions,
    subjects,
    subjectIndex,
    agent,
    agentRow,
    agentAttention,
    terminalConnected,
  }
}
const derived = new WeakMap<DataSource, DerivedData>()
const DataContext = createContext<DerivedData | null>(null)

// oxlint-disable-next-line overeng/named-args -- Retained-identity array comparator; fixed positional Equivalence shape.
const sameItems = <T,>(a: readonly T[], b: readonly T[]): boolean =>
  a.length === b.length && a.every((item, index) => item === b[index])

// oxlint-disable-next-line overeng/named-args -- Feed comparator for Atom.withEquality's fixed (value, next) ABI.
const sameFeed = <T,>(
  a: Feed<T>,
  b: Feed<T>,
  equal: (left: T, right: T) => boolean = Object.is,
): boolean => {
  if (a._tag !== b._tag) return false
  if (a._tag === 'Observed' && b._tag === 'Observed')
    return a.freshness === b.freshness && a.coverage?._tag === b.coverage?._tag && a.error === b.error && equal(a.value, b.value)
  if (a._tag === 'Unavailable' && b._tag === 'Unavailable')
    return a.reason === b.reason && a.detail === b.detail
  return a._tag === 'Waiting' && b._tag === 'Waiting'
}

const mapFeed = <A, B>({
  feed,
  map,
}: {
  readonly feed: Feed<A>
  readonly map: (value: A) => B
}): Feed<B> => (feed._tag === 'Observed' ? { ...feed, value: map(feed.value) } : feed)

/** Share generated-row derivations and the source registry with workbench consumers. */
export const DataSourceProvider = ({
  source,
  registry,
  children,
}: {
  readonly source: DataSource
  readonly registry: AtomRegistry.AtomRegistry
  readonly children: ReactNode
}) => {
  let value = derived.get(source)
  if (value === undefined) {
    value = derive(source)
    derived.set(source, value)
  }
  return (
    <RegistryContext.Provider value={registry}>
      <DataContext.Provider value={value}>{children}</DataContext.Provider>
    </RegistryContext.Provider>
  )
}
const useData = () => {
  const data = useContext(DataContext)
  if (data === null) throw new Error('wf data hooks need a <DataSourceProvider>')
  return data
}
/** Read the source installed by the nearest provider. */
export const useDataSource = (): DataSource => useData().source
/** Acquire follow demand only after a visible surface commits; Activity cleans this up when hidden. */
export const useFeedInterest = ({
  interest,
  visible,
}: {
  readonly interest: Atom.Atom<void> | undefined
  readonly visible: boolean
}) => {
  const registry = useContext(RegistryContext)
  React.useEffect(() => {
    return visible && interest !== undefined ? registry.mount(interest) : undefined
  }, [interest, registry, visible])
}
/** Observe the gateway connection lifecycle. */
export const useConnection = () => useAtomValue(useDataSource().connection)
const noSyncObservation = Atom.make<FeedSyncObservation | undefined>(undefined)
/** SDK-owned verdicts, not inferred from retained Feed content or connection state. */
export const useGatewaySync = (): FeedSyncObservation | undefined =>
  useAtomValue(useDataSource().sync?.gateway ?? noSyncObservation)
const useRetainedSync = <TValue,>(sync: Atom.Atom<FeedSync<TValue>> | undefined): FeedSyncObservation | undefined => {
  const observation = React.useMemo(
    () => Atom.make((get) => sync === undefined ? undefined : get(sync).sync),
    [sync],
  )
  return useAtomValue(observation)
}
export const useAgentsSync = (): FeedSyncObservation | undefined =>
  useRetainedSync(useDataSource().sync?.agents)
export const useMissionsSync = (): FeedSyncObservation | undefined =>
  useRetainedSync(useDataSource().sync?.missions)
export const useAttentionSync = (): FeedSyncObservation | undefined =>
  useRetainedSync(useDataSource().sync?.attention)
export const useConversationSync = (ref: string): FeedSyncObservation | undefined =>
  useRetainedSync(useDataSource().sync?.conversation(ref))
export const useTerminalSync = (ref: string): FeedSyncObservation | undefined =>
  useRetainedSync(useDataSource().sync?.terminal(ref))
/** Observe current action and terminal grants. */
export const useGrants = () => useAtomValue(useDataSource().grants)
/** Observe the fleet projected from agent rows. */
export const useFleet = () => useAtomValue(useData().fleet)
/** Select only fleet fields that affect a collection's membership/order, not row presentation. */
export const useFleetSelection = <T,>({
  select,
  equal,
}: {
  readonly select: (feed: Feed<Fleet>) => T
  readonly equal: (left: T, right: T) => boolean
}): T => {
  const { fleet } = useData()
  const atom = React.useMemo(
    () => Atom.map(fleet, select).pipe(Atom.withEquality(equal)),
    [fleet, select, equal],
  )
  return useAtomValue(atom)
}
/** A single projected row retains its freshness and unavailable state without roster fanout. */
export const useAgent = (ref: string): Feed<Agent | undefined> => useAtomValue(useData().agent(ref))
/** The native evidence for one agent, with the same lifecycle as its collection. */
export const useAgentRow = (ref: string): Feed<AgentRow | undefined> =>
  useAtomValue(useData().agentRow(ref))
/** Select native evidence at row scope (for example, a ledger root), not the full raw DTO. */
export const useAgentRowSelection = <T,>({
  ref,
  select,
}: {
  readonly ref: string
  readonly select: (feed: Feed<AgentRow | undefined>) => T
}): T => useAtomValue(useData().agentRow(ref), select)
/** Only open cards about this agent or its current missions notify its information surfaces. */
export const useAgentAttention = (ref: string): Feed<readonly Attention[]> =>
  useAtomValue(useData().agentAttention(ref))
/** Badge/count projections ignore unrelated card metadata while detail readers keep native cards. */
export const useAgentAttentionSelection = <T,>({
  ref,
  select,
  equal,
}: {
  readonly ref: string
  readonly select: (feed: Feed<readonly Attention[]>) => T
  readonly equal: (left: T, right: T) => boolean
}): T => {
  const atom = useData().agentAttention(ref)
  const selected = React.useMemo(
    () => Atom.map(atom, select).pipe(Atom.withEquality(equal)),
    [atom, select, equal],
  )
  return useAtomValue(selected)
}
/** Host reachability for one terminal, rather than a subscription to every fleet row. */
export const useTerminalConnected = (ref: string): boolean | undefined =>
  useAtomValue(useData().terminalConnected(ref))
/** Observe mission views joined with attention rows. */
export const useMissions = () => useAtomValue(useData().missions)
/** Observe every attention row independently of the loaded mission window. */
export const useAttention = () => useAtomValue(useDataSource().attention)
/** Observe available navigation subjects; an absent family never removes another family's subjects. */
export const useSubjectList = (): readonly SubjectSummary[] => useAtomValue(useData().subjects)
/** Narrow subject selection for collection filters; open content must not read the subject roster. */
export const useSubjectSelection = <T,>({
  select,
  equal,
}: {
  readonly select: (subjects: readonly SubjectSummary[]) => T
  readonly equal: (left: T, right: T) => boolean
}): T => {
  const { subjects } = useData()
  const atom = React.useMemo(
    () => Atom.map(subjects, select).pipe(Atom.withEquality(equal)),
    [subjects, select, equal],
  )
  return useAtomValue(atom)
}
/** Distinguish Waiting/Unavailable families from observed empty collections, retaining freshness. */
export const useSubjectIndex = (): SubjectIndex => useAtomValue(useData().subjectIndex)
/** Observe a retained agent conversation. */
export const useConversation = (agentRef: string) =>
  useAtomValue(useDataSource().conversation(agentRef))
/** Observe a retained terminal screen. */
export const useTerminal = (terminalRef: string) =>
  useAtomValue(useDataSource().terminal(terminalRef))
/** Observe a resource envelope. */
export const useEnvelope = (ref: string) => useAtomValue(useDataSource().envelope(ref))
/** Observe source gateway events. */
export const useEvents = () => useAtomValue(useDataSource().events)
/** Observe the source clock. */
export const useNow = (): number => useAtomValue(useDataSource().now)
