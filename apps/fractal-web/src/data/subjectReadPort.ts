import {
  ClientError,
  St3Client,
  type ClientOptions,
  type PageOptions,
  type EnvelopeOf,
  type Page,
} from '@smalltalk/st3-client'
import * as Native from '@smalltalk/st3-client/schema'
import { Context, Data, Effect, Fiber, Option, Queue, Schedule, Schema, Stream } from 'effect'
import * as Atom from 'effect/reactivity/Atom'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'

import type { Feed } from './source.ts'
import { observed, unavailable, waiting } from './source.ts'

/** A subject read failed because access, support, or a current native observation is missing. */
export class SubjectReadFailure extends Data.TaggedError('SubjectReadFailure')<{
  readonly ref: string
  readonly reason: 'ungranted' | 'unsupported' | 'failed'
  readonly detail: string
}> {}

/**
 * Exact generated decoded DTOs, never envelopes or encoded wire. Feed subscriptions own
 * acquisition/release; last-good values revalidate immediately on remount. Getter refresh is
 * demand-mounted at 15 seconds until the SDK supplies shared authoritative per-ref streams:
 * opening another bounded collection follow would overwrite the SDK's existing follow key.
 * Registry callbacks validate Schema.toType(generatedCodec), not the wire decoder again.
 */
export interface SubjectReader<T> {
  readonly read: (ref: string) => Effect.Effect<T, SubjectReadFailure>
  readonly changes: (ref: string) => Stream.Stream<Feed<T>>
  readonly feed: (ref: string) => Atom.Atom<Feed<T>>
}

/** Adapt one existing source atom without introducing another poller, cache or publication. */
export const subjectReaderFromAtom = <T>({
  feed,
  registry,
}: {
  readonly feed: SubjectReader<T>['feed']
  readonly registry: AtomRegistry.AtomRegistry
}): SubjectReader<T> => {
  const changes = (ref: string) =>
    Stream.callback<Feed<T>>((output) =>
      Effect.acquireRelease(
        Effect.sync(() =>
          registry.subscribe(feed(ref), (state) => Queue.offerUnsafe(output, state), {
            immediate: true,
          }),
        ),
        (unsubscribe) => Effect.sync(unsubscribe),
      ),
    )
  return {
    feed,
    changes,
    read: Effect.fn('SubjectReadPort.readAtom')((ref: string) =>
      changes(ref).pipe(
        Stream.filter(
          (state) =>
            state._tag === 'Unavailable' ||
            (state._tag === 'Observed' &&
              (state.freshness === 'live' || state.error !== undefined)),
        ),
        Stream.runHead,
        Effect.flatMap((first) => {
          if (Option.isNone(first))
            return Effect.fail(
              new SubjectReadFailure({
                ref,
                reason: 'failed',
                detail: 'The native resource reader closed before observation',
              }),
            )
          const state = first.value
          if (state._tag === 'Observed' && state.freshness === 'live')
            return Effect.succeed(state.value)
          if (state._tag === 'Unavailable')
            return Effect.fail(
              new SubjectReadFailure({ ref, reason: state.reason, detail: state.detail }),
            )
          return Effect.fail(
            new SubjectReadFailure({
              ref,
              reason: state._tag === 'Observed' ? (state.error?.reason ?? 'failed') : 'failed',
              detail:
                state._tag === 'Observed'
                  ? (state.error?.detail ?? 'The native observation is stale')
                  : 'No native observation',
            }),
          )
        }),
      ),
    ),
  }
}

const retainedReader = <T>({
  read,
  registry,
}: {
  readonly read: SubjectReader<T>['read']
  readonly registry: AtomRegistry.AtomRegistry
}): SubjectReader<T> => {
  const cache = new Map<string, T>()
  const changes = (ref: string) =>
    Stream.fromEffectSchedule(
      read(ref).pipe(
        Effect.match({
          onSuccess: (value): Feed<T> => {
            cache.set(ref, value)
            return observed({ value })
          },
          onFailure: ({ reason, detail }): Feed<T> => {
            const value = cache.get(ref)
            return value === undefined
              ? unavailable({ reason, detail })
              : {
                  _tag: 'Observed',
                  value,
                  freshness: 'stale',
                  error: { reason, detail },
                }
          },
        }),
      ),
      Schedule.spaced('15 seconds'),
    )
  const feed = Atom.family((ref: string) =>
    Atom.make((get): Feed<T> => {
      const value = cache.get(ref)
      const fiber = Effect.runFork(
        changes(ref).pipe(Stream.runForEach((next) => Effect.sync(() => get.setSelf(next)))),
      )
      get.addFinalizer(() => Effect.runFork(Fiber.interrupt(fiber)))
      return value === undefined ? waiting : observed({ value, freshness: 'stale' })
    }),
  )
  return subjectReaderFromAtom({ feed, registry })
}

/** Only the request transport is scoped; capability discovery remains the source client's cache. */
class ScopedSubjectClient extends St3Client {
  private readonly signal: AbortSignal
  private readonly discovery: St3Client
  constructor({
    options,
    signal,
    discovery,
  }: {
    readonly options: ClientOptions
    readonly signal: AbortSignal
    readonly discovery: St3Client
  }) {
    super({
      ...options,
      fetchImpl: (input, init) => {
        signal.throwIfAborted()
        return (options.fetchImpl ?? globalThis.fetch)(input, { ...init, signal })
      },
    })
    this.signal = signal
    this.discovery = discovery
  }
  override async discover() {
    const capabilities = await this.discovery.discover()
    this.signal.throwIfAborted()
    return capabilities
  }
}

const PageHeader = Schema.Struct({
  kind: Schema.Literal('page'),
  items: Schema.Array(Schema.Unknown),
  page: Native.PageInfo,
})
/** Exhaust pages before concluding absence; opaque child IDs never encode their parent. */
const pages = async function* ({
  list,
  signal,
}: {
  readonly list: (options: PageOptions) => Promise<EnvelopeOf<Page>>
  readonly signal: AbortSignal
}) {
  let cursor: string | undefined
  do {
    signal.throwIfAborted()
    const response = await list(cursor === undefined ? {} : { cursor })
    signal.throwIfAborted()
    const page = Native.decodeUnknownSync(PageHeader)(response.value)
    yield response.value.items
    if (!page.page.has_more) return
    if (Option.isNone(page.page.next_cursor))
      throw new SubjectReadFailure({
        ref: '',
        reason: 'failed',
        detail: 'Gateway omitted the next page cursor',
      })
    cursor = page.page.next_cursor.value
  } while (true)
}

const findListed = async ({
  ref,
  list,
  signal,
}: {
  readonly ref: string
  readonly list: (options: PageOptions) => Promise<EnvelopeOf<Page>>
  readonly signal: AbortSignal
}) => {
  for await (const rows of pages({ list: list, signal: signal })) {
    const row = rows.find(
      (candidate) => typeof candidate === 'object' && candidate !== null && candidate.id === ref,
    )
    if (row !== undefined) return { value: row }
  }
  throw new SubjectReadFailure({
    ref,
    reason: 'failed',
    detail: `The gateway has no current ${ref}`,
  })
}

const findLaunchChild = async ({
  ref,
  client,
  list,
  signal,
}: {
  readonly ref: string
  readonly client: St3Client
  readonly list: (args: {
    readonly parent: string
    readonly options: PageOptions
  }) => Promise<EnvelopeOf<Page>>
  readonly signal: AbortSignal
}) => {
  for await (const launches of pages({
    list: (options) => client.launchesList(options),
    signal: signal,
  })) {
    for (const launch of launches) {
      if (typeof launch !== 'object' || launch === null || launch.kind !== 'launch') continue
      for await (const rows of pages({
        list: (options) => list({ parent: launch.id, options: options }),
        signal: signal,
      })) {
        const row = rows.find(
          (candidate) =>
            typeof candidate === 'object' && candidate !== null && candidate.id === ref,
        )
        if (row !== undefined) return { value: row }
      }
    }
  }
  throw new SubjectReadFailure({
    ref,
    reason: 'failed',
    detail: `The gateway has no current ${ref}`,
  })
}

const findMissionRun = async ({
  ref,
  client,
  signal,
}: {
  readonly ref: string
  readonly client: St3Client
  readonly signal: AbortSignal
}) => {
  for await (const missions of pages({
    list: (page) => client.missionsList(page),
    signal: signal,
  })) {
    for (const parent of missions) {
      if (typeof parent !== 'object' || parent === null || parent.kind !== 'mission') continue
      signal.throwIfAborted()
      const mission = (await client.missionsGet(parent.id)).value
      signal.throwIfAborted()
      if (mission.kind !== 'mission') continue
      const run = mission.run_details?.find((candidate) => candidate.id === ref)
      if (run !== undefined) return { value: run }
    }
  }
  throw new SubjectReadFailure({
    ref,
    reason: 'failed',
    detail: `The gateway has no current ${ref}`,
  })
}
/** Feed consumers must use this captured registry, shared with public reads and streams. */
export const gatewaySubjectReads = ({
  options,
  resource = unsupportedSubjectReader('This source has no native resource driver'),
  discovery = new St3Client(options),
  registry = AtomRegistry.make(),
}: {
  readonly options: ClientOptions
  readonly resource?: SubjectReader<Native.ResourceObservation>
  readonly discovery?: St3Client
  readonly registry?: AtomRegistry.AtomRegistry
}): SubjectReads => {
  const fromGetter = <T>({
    decode,
    getter,
  }: {
    readonly decode: (value: unknown) => T
    readonly getter: (args: {
      readonly client: St3Client
      readonly ref: string
      readonly signal: AbortSignal
    }) => Promise<{ readonly value: unknown }>
  }) =>
    retainedReader({
      read: Effect.fn('SubjectReadPort.read')((ref: string) =>
        Effect.tryPromise({
          try: async (signal) => {
            signal.throwIfAborted()
            const response = await getter({
              client: new ScopedSubjectClient({
                options: options,
                signal: signal,
                discovery: discovery,
              }),
              ref: ref,
              signal: signal,
            })
            signal.throwIfAborted()
            return decode(response.value)
          },
          catch: (cause) =>
            new SubjectReadFailure({
              ref,
              reason: cause instanceof ClientError && cause.status === 403 ? 'ungranted' : 'failed',
              detail: cause instanceof Error ? cause.message : String(cause),
            }),
        }),
      ),
      registry: registry,
    })
  return {
    resource,
    launch: fromGetter({
      decode: Native.decodeUnknownSync(Native.Launch),
      getter: ({ client, ref }) => client.launchesGet(ref),
    }),
    launchVariant: fromGetter({
      decode: Native.decodeUnknownSync(Native.LaunchVariant),
      getter: ({ client, ref, signal }) =>
        findLaunchChild({
          ref: ref,
          client: client,
          list: ({ parent, options: page }) => client.launchVariantsList(parent, page),
          signal: signal,
        }),
    }),
    launchDecision: fromGetter({
      decode: Native.decodeUnknownSync(Native.LaunchDecision),
      getter: ({ client, ref, signal }) =>
        findLaunchChild({
          ref: ref,
          client: client,
          list: ({ parent, options: page }) => client.launchDecisionsList(parent, page),
          signal: signal,
        }),
    }),
    launchApproval: fromGetter({
      decode: Native.decodeUnknownSync(Native.LaunchApproval),
      getter: ({ client, ref, signal }) =>
        findLaunchChild({
          ref: ref,
          client: client,
          list: ({ parent, options: page }) => client.launchApprovalsList(parent, page),
          signal: signal,
        }),
    }),
    device: fromGetter({
      decode: Native.decodeUnknownSync(Native.Device),
      getter: ({ client, ref, signal }) =>
        findListed({ ref: ref, list: (page) => client.devicesList(page), signal: signal }),
    }),
    machine: fromGetter({
      decode: Native.decodeUnknownSync(Native.Machine),
      getter: ({ client, ref, signal }) =>
        findListed({ ref: ref, list: (page) => client.machinesList(page), signal: signal }),
    }),
    blob: fromGetter({
      decode: Native.decodeUnknownSync(Native.BlobChunk),
      getter: ({ client, ref }) => client.blobChunk(ref.startsWith('blob/') ? ref.slice(5) : ref),
    }),
    mission: fromGetter({
      decode: Native.decodeUnknownSync(Native.Mission),
      getter: ({ client, ref }) => client.missionsGet(ref),
    }),
    missionRun: fromGetter({
      decode: Native.decodeUnknownSync(Native.MissionRunSummary),
      getter: ({ client, ref, signal }) =>
        findMissionRun({ ref: ref, client: client, signal: signal }),
    }),
    work: fromGetter({
      decode: Native.decodeUnknownSync(Native.Work),
      getter: ({ client, ref }) => client.workGet(ref),
    }),
    lane: fromGetter({
      decode: Native.decodeUnknownSync(Native.Lane),
      getter: ({ client, ref }) => client.lanesGet(ref),
    }),
    agentQueue: fromGetter({
      decode: Native.decodeUnknownSync(Native.AgentQueue),
      getter: ({ client, ref }) => client.agentQueueGet(ref),
    }),
    agent: fromGetter({
      decode: Native.decodeUnknownSync(Native.Agent),
      getter: ({ client, ref }) => client.agentsGet(ref),
    }),
    attention: fromGetter({
      decode: Native.decodeUnknownSync(Native.Attention),
      getter: ({ client, ref }) => client.attentionGet(ref),
    }),
    message: fromGetter({
      decode: Native.decodeUnknownSync(Native.Message),
      getter: ({ client, ref }) => client.messagesGet(ref),
    }),
    runtime: fromGetter({
      decode: Native.decodeUnknownSync(Native.Runtime),
      getter: ({ client, ref }) => client.runtimesGet(ref),
    }),
    observer: fromGetter({
      decode: Native.decodeUnknownSync(Native.Observer),
      getter: ({ client, ref }) => client.observersGet(ref),
    }),
    subscription: fromGetter({
      decode: Native.decodeUnknownSync(Native.Subscription),
      getter: ({ client, ref }) => client.subscriptionsGet(ref),
    }),
    operation: fromGetter({
      decode: Native.decodeUnknownSync(Native.Operation),
      getter: ({ client, ref }) => client.operationsGet(ref),
    }),
    history: fromGetter({
      decode: Native.decodeUnknownSync(Native.History),
      getter: ({ client, ref }) => client.historyGet(ref),
    }),
    session: fromGetter({
      decode: Native.decodeUnknownSync(Native.Session),
      getter: ({ client, ref }) => client.sessionsGet(ref),
    }),
    terminalScreen: fromGetter({
      decode: Native.decodeUnknownSync(Native.TerminalScreen),
      getter: ({ client, ref }) => client.terminalScreen(ref),
    }),
    document: fromGetter({
      decode: Native.decodeUnknownSync(Native.DocumentContent),
      getter: ({ client, ref: name }) => client.documentGet(name),
    }),
    glass: fromGetter({
      decode: Native.decodeUnknownSync(Native.Glass),
      getter: ({ client, ref }) => client.glassesGet(ref),
    }),
  }
}

/** Native subject families available through the same read, stream, and atom contract. */
export interface SubjectReads {
  readonly resource: SubjectReader<Native.ResourceObservation>
  readonly launch: SubjectReader<Native.Launch>
  readonly launchVariant: SubjectReader<Native.LaunchVariant>
  readonly launchDecision: SubjectReader<Native.LaunchDecision>
  readonly launchApproval: SubjectReader<Native.LaunchApproval>
  readonly device: SubjectReader<Native.Device>
  readonly machine: SubjectReader<Native.Machine>
  readonly blob: SubjectReader<Native.BlobChunk>
  readonly mission: SubjectReader<Native.Mission>
  readonly missionRun: SubjectReader<Native.MissionRunSummary>
  readonly work: SubjectReader<Native.Work>
  readonly lane: SubjectReader<Native.Lane>
  readonly agentQueue: SubjectReader<Native.AgentQueue>
  readonly agent: SubjectReader<Native.Agent>
  readonly attention: SubjectReader<Native.Attention>
  readonly message: SubjectReader<Native.Message>
  readonly runtime: SubjectReader<Native.Runtime>
  readonly observer: SubjectReader<Native.Observer>
  readonly subscription: SubjectReader<Native.Subscription>
  readonly operation: SubjectReader<Native.Operation>
  readonly history: SubjectReader<Native.History>
  readonly session: SubjectReader<Native.Session>
  readonly terminalScreen: SubjectReader<Native.TerminalScreen>
  readonly document: SubjectReader<Native.DocumentContent>
  readonly glass: SubjectReader<Native.Glass>
}
/** Effect service providing the source's native subject readers. */
export class SubjectReadPort extends Context.Service<SubjectReadPort, SubjectReads>()(
  'wf/SubjectReadPort',
) {}

/** Missing native producers are explicit; fixture absence must never claim live support. */
export const unsupportedSubjectReader = <T>(detail: string): SubjectReader<T> => {
  const state = unavailable({ reason: 'unsupported', detail })
  const atom = Atom.make(state)
  return {
    read: (ref) => Effect.fail(new SubjectReadFailure({ ref, reason: 'unsupported', detail })),
    changes: () => Stream.make(state),
    feed: () => atom,
  }
}

/** Decoded fixture feeds indexed by subject family and exact native reference. */
export type SubjectReadFixtures = {
  readonly [K in keyof SubjectReads]?: Readonly<
    Record<string, SubjectReads[K] extends SubjectReader<infer T> ? Feed<T> : never>
  >
}

/** The fixture/source wire boundary uses the very same generated decoder as gateway reads. */
export const nativeSubjectFixture = <TType, TEncoded>({
  schema,
  wire,
}: {
  readonly schema: Schema.Codec<TType, TEncoded>
  readonly wire: unknown
}): Feed<TType> => observed({ value: Native.decodeUnknownSync(schema)(wire) })

/** Stories inject already-decoded native DTOs and the same availability states as live. */
export const fixtureSubjectReads = ({
  fixtures = {},
  overrides = {},
}: {
  readonly fixtures?: SubjectReadFixtures
  readonly overrides?: SubjectReadFixtures
} = {}): SubjectReads => {
  const reader = <T>({
    table = {},
    pinned = {},
  }: {
    readonly table: Readonly<Record<string, Feed<T>>> | undefined
    readonly pinned: Readonly<Record<string, Feed<T>>> | undefined
  }): SubjectReader<T> => {
    const state = (ref: string): Feed<T> =>
      pinned[ref] ??
      table[ref] ??
      unavailable({ reason: 'unsupported', detail: `No native fixture for ${ref}` })
    return {
      read: (ref) => {
        const feed = state(ref)
        if (feed._tag === 'Observed') return Effect.succeed(feed.value)
        return Effect.fail(
          new SubjectReadFailure({
            ref,
            reason: feed._tag === 'Unavailable' ? feed.reason : 'failed',
            detail:
              feed._tag === 'Unavailable'
                ? feed.detail
                : 'The fixture is waiting for its first observation',
          }),
        )
      },
      changes: (ref) => Stream.make(state(ref)),
      feed: Atom.family((ref: string) => Atom.make(state(ref))),
    }
  }
  return {
    resource: reader({ table: fixtures.resource, pinned: overrides.resource }),
    launch: reader({ table: fixtures.launch, pinned: overrides.launch }),
    mission: reader({ table: fixtures.mission, pinned: overrides.mission }),
    work: reader({ table: fixtures.work, pinned: overrides.work }),
    missionRun: reader({ table: fixtures.missionRun, pinned: overrides.missionRun }),
    launchVariant: reader({ table: fixtures.launchVariant, pinned: overrides.launchVariant }),
    launchDecision: reader({ table: fixtures.launchDecision, pinned: overrides.launchDecision }),
    launchApproval: reader({ table: fixtures.launchApproval, pinned: overrides.launchApproval }),
    device: reader({ table: fixtures.device, pinned: overrides.device }),
    machine: reader({ table: fixtures.machine, pinned: overrides.machine }),
    blob: reader({ table: fixtures.blob, pinned: overrides.blob }),
    lane: reader({ table: fixtures.lane, pinned: overrides.lane }),
    agentQueue: reader({ table: fixtures.agentQueue, pinned: overrides.agentQueue }),
    agent: reader({ table: fixtures.agent, pinned: overrides.agent }),
    attention: reader({ table: fixtures.attention, pinned: overrides.attention }),
    message: reader({ table: fixtures.message, pinned: overrides.message }),
    runtime: reader({ table: fixtures.runtime, pinned: overrides.runtime }),
    observer: reader({ table: fixtures.observer, pinned: overrides.observer }),
    subscription: reader({ table: fixtures.subscription, pinned: overrides.subscription }),
    operation: reader({ table: fixtures.operation, pinned: overrides.operation }),
    history: reader({ table: fixtures.history, pinned: overrides.history }),
    session: reader({ table: fixtures.session, pinned: overrides.session }),
    terminalScreen: reader({ table: fixtures.terminalScreen, pinned: overrides.terminalScreen }),
    document: reader({ table: fixtures.document, pinned: overrides.document }),
    glass: reader({ table: fixtures.glass, pinned: overrides.glass }),
  }
}
