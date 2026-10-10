import { ClientError, St3Client } from '@smalltalk/st3-client'
import { ResourcesPage, type ResourceObservation } from '@smalltalk/st3-client/schema'
import { Effect, Option, Queue, Schedule, Schema, Stream } from 'effect'
import * as AsyncResult from 'effect/reactivity/AsyncResult'
import * as Atom from 'effect/reactivity/Atom'

import type { Feed } from '../../data/source.ts'
import { observed, unavailable, waiting } from '../../data/source.ts'
import { resourceTitle, sortResources, type ResourcePage } from './model.ts'

/** Observed agent resources, paging controls and cached file-subject lookup. */
export interface AgentResourceSource {
  readonly byAgent: (ref: string) => Atom.Atom<Feed<ResourcePage>>
  readonly byId: (ref: string) => Atom.Atom<Feed<ResourceObservation>>
  readonly subjects: Atom.Atom<
    readonly {
      readonly ref: string
      readonly title: string
      readonly detail: string
      readonly icon: string
    }[]
  >
  readonly loadMore: (ref: string) => void
  readonly refresh: (ref: string) => void
  readonly resolveFile: (request: {
    readonly agentRef: string
    readonly path: string
  }) => string | undefined
}
const record = (value: unknown): value is Record<string, unknown> =>
  typeof value === 'object' && value !== null && !Array.isArray(value)
/** Decode gateway wire exactly once; every graph and native consumer retains these rich values. */
export const decodeResourcePage = (response: unknown): ResourcePage => {
  const value = record(response) && record(response.value) ? response.value : response
  const native = Schema.decodeUnknownSync(ResourcesPage)(value)
  const nextCursor = Option.getOrNull(native.page.next_cursor)
  if (native.page.has_more && nextCursor === null)
    throw new Error('The gateway returned an invalid paging cursor.')
  return { items: native.items, nextCursor }
}

class ResourceReadError extends Schema.TaggedError<ResourceReadError>()('ResourceReadError', {
  message: Schema.String,
  reason: Schema.Literals(['ungranted', 'failed']),
}) {}

/** Collection socket has no resource subscription yet. Each mounted reader owns its scoped poll. */
export const gatewayResources = ({
  baseUrl,
  fetchImpl,
}: {
  readonly baseUrl: string
  readonly fetchImpl: typeof fetch
}): AgentResourceSource => {
  const controls = new Map<string, { more: () => void; refresh: () => void }>()
  const cache = new Map<string, ResourceObservation>()
  const revision = Atom.make(0)
  let cacheRevision = 0

  /** All pages are staged; HTTP, body consumption and native decoding share one cancellation scope. */
  const read = Effect.fn('wf.resources.read')(
    (
      filter: 'opened_by' | 'subject_prefix',
      ref: string,
      count: number,
      startCursor: string | null = null,
    ) =>
      Effect.tryPromise({
        try: async (signal) => {
          // The generated getter has no signal option; this scoped transport preserves abort
          // through fetch and response.json without sharing mutable cancellation across readers.
          const client = new St3Client({
            baseUrl,
            fetchImpl: (input, init) => fetchImpl(input, { ...init, signal }),
          })
          let cursor = startCursor
          let limit = count
          let restarted = false
          const items = new Map<string, ResourceObservation>()
          for (let index = 0; index < limit; index++) {
            let response
            try {
              response = await client.resourcesList(
                filter === 'opened_by' ? { opened_by: ref } : { subject_prefix: ref },
                { limit: 50, ...(cursor === null ? {} : { cursor }) },
              )
            } catch (error) {
              if (!(error instanceof ClientError) || error.status !== 410) throw error
              if (restarted)
                throw new Error(
                  'Resources changed while paging. Refresh to load the latest observations.',
                  { cause: error },
                )
              restarted = true
              cursor = null
              items.clear()
              // An expired agent window restarts at page one; the editor scans its entire prefix.
              limit = filter === 'opened_by' ? 1 : count
              index = -1
              continue
            }
            const page = decodeResourcePage(response)
            for (const item of page.items) items.set(item.id, item)
            cursor = page.nextCursor
            if (cursor === null) break
          }
          return { items: [...items.values()], nextCursor: cursor, restarted }
        },
        catch: (error) =>
          new ResourceReadError({
            message: error instanceof Error ? error.message : 'Could not load resources.',
            reason: error instanceof ClientError && error.status === 403 ? 'ungranted' : 'failed',
          }),
      }),
  )

  const agentPoll = Atom.family((ref: string) =>
    Atom.make(
      (get) =>
        Stream.callback<Feed<ResourcePage>>((output) =>
          Effect.gen(function* () {
            const commands = yield* Queue.unbounded<'more' | 'refresh'>()
            let busy = false
            let pending = false
            let pages = 1
            let latest: Feed<ResourcePage> = waiting
            const publish = (feed: Feed<ResourcePage>) => {
              latest = feed
              Queue.offerUnsafe(
                output,
                feed._tag === 'Observed'
                  ? { ...feed, value: { ...feed.value, items: sortResources(feed.value.items) } }
                  : feed,
              )
            }
            const enqueue = (command: 'more' | 'refresh') => {
              if (busy || pending) return
              pending = true
              Queue.offerUnsafe(commands, command)
            }
            yield* Effect.acquireRelease(
              Effect.sync(() =>
                controls.set(ref, {
                  more: () => {
                    if (latest._tag === 'Observed' && latest.value.nextCursor !== null)
                      enqueue('more')
                  },
                  refresh: () => enqueue('refresh'),
                }),
              ),
              () =>
                Effect.sync(() => {
                  controls.delete(ref)
                }),
            )
            yield* Effect.repeat(
              Effect.sync(() => enqueue('refresh')),
              Schedule.spaced('15 seconds'),
            ).pipe(Effect.forkScoped)
            return yield* Effect.forever(
              Effect.gen(function* () {
                const command = yield* Queue.take(commands)
                pending = false
                busy = true
                const previous = latest._tag === 'Observed' ? latest.value : undefined
                const more = command === 'more' && previous !== undefined
                if (more) publish(observed({ value: { ...previous, loadingMore: true } }))
                yield* read(
                  'opened_by',
                  ref,
                  more ? 1 : pages,
                  more ? previous.nextCursor : null,
                ).pipe(
                  Effect.tap((result) =>
                    Effect.sync(() => {
                      const append = more && !result.restarted
                      const staged = new Map(
                        (append ? previous.items : []).map((item) => [item.id, item]),
                      )
                      for (const item of result.items) staged.set(item.id, item)
                      const items = [...staged.values()]
                      // Publish only a complete requested window. No page can mutate shared state on failure.
                      if (!append && previous !== undefined) {
                        for (const item of previous.items)
                          if (!staged.has(item.id)) cache.delete(item.id)
                      }
                      for (const item of items) cache.set(item.id, item)
                      pages = result.restarted ? 1 : more ? pages + 1 : pages
                      Atom.batch(() => {
                        get.set(revision, ++cacheRevision)
                        publish(observed({ value: { items, nextCursor: result.nextCursor } }))
                      })
                    }),
                  ),
                  Effect.catch((error) =>
                    Effect.sync(() => {
                      if (previous !== undefined)
                        publish({
                          _tag: 'Observed',
                          value: { ...previous, loadingMore: false, pagingError: error.message },
                          freshness: 'stale',
                          error: { reason: error.reason, detail: error.message },
                        })
                      else publish(unavailable({ reason: error.reason, detail: error.message }))
                    }),
                  ),
                )
                busy = false
              }),
            )
          }),
        ),
      { initialValue: waiting },
    ),
  )
  const byAgent = Atom.family((ref: string) =>
    Atom.make(
      (get): Feed<ResourcePage> => AsyncResult.getOrElse(get(agentPoll(ref)), () => waiting),
    ),
  )

  // This driver never depends on revision: other readers' cache writes cannot restart its scope.
  const resourcePoll = Atom.family((ref: string) =>
    Atom.make((get) =>
      Stream.callback<Feed<ResourceObservation>>((output) =>
        Effect.gen(function* () {
          const cached = cache.get(ref)
          let latest: Feed<ResourceObservation> =
            cached === undefined ? waiting : observed({ value: cached, freshness: 'stale' })
          const publish = (feed: Feed<ResourceObservation>) => {
            latest = feed
            Queue.offerUnsafe(output, feed)
          }
          publish(latest)
          return yield* Effect.repeat(
            read('subject_prefix', ref, Infinity).pipe(
              Effect.tap((page) =>
                Effect.sync(() => {
                  const resource = page.items.find((item) => item.id === ref)
                  if (resource === undefined) cache.delete(ref)
                  else cache.set(ref, resource)
                  Atom.batch(() => {
                    publish(
                      resource === undefined
                        ? unavailable({
                            reason: 'failed',
                            detail: 'This resource is no longer observed by the gateway.',
                          })
                        : observed({ value: resource }),
                    )
                    get.set(revision, ++cacheRevision)
                  })
                }),
              ),
              Effect.catch((error) =>
                Effect.sync(() =>
                  publish(
                    latest._tag === 'Observed'
                      ? {
                          _tag: 'Observed',
                          value: latest.value,
                          freshness: 'stale',
                          error: { reason: error.reason, detail: error.message },
                        }
                      : unavailable({ reason: error.reason, detail: error.message }),
                  ),
                ),
              ),
            ),
            Schedule.spaced('15 seconds'),
          )
        }),
      ),
    ),
  )
  const byId = Atom.family((ref: string) =>
    Atom.make((get): Feed<ResourceObservation> => {
      get(revision)
      const cached = cache.get(ref)
      const feed = AsyncResult.getOrElse(
        get(resourcePoll(ref)),
        (): Feed<ResourceObservation> =>
          cached === undefined ? waiting : observed({ value: cached, freshness: 'stale' }),
      )
      return feed._tag === 'Observed' && cached !== undefined ? { ...feed, value: cached } : feed
    }),
  )
  const subjects = Atom.make((get) => {
    get(revision)
    return [...cache.values()].map((resource) => ({
      ref: resource.id,
      title: resourceTitle(resource),
      detail: resource.kind,
      icon: 'resources',
    }))
  })
  return {
    byAgent,
    byId,
    subjects,
    loadMore: (ref) => controls.get(ref)?.more(),
    refresh: (ref) => controls.get(ref)?.refresh(),
    resolveFile: ({ agentRef, path }) =>
      [...cache.values()].find(
        (resource) =>
          Option.contains(resource.opened_by, agentRef) &&
          resource.kind === 'filesystem.file' &&
          resource.facts.path === path.replace(/:\d+(?::\d+)?$/u, ''),
      )?.id,
  }
}
