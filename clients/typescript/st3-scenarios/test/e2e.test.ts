import * as React from 'react'
import { composeStory, type Args, type Meta, type StoryObj } from '@storybook/react'
import { renderToStaticMarkup } from 'react-dom/server'
import { describe, expect, it } from 'vitest'
import {
  applyWindow, ClientError, St3Client,
  type CollectionFrame, type CollectionWindow, type EnvelopeOf,
  type Page, type PageOptions, type Resource, type TimelineEntry,
} from '@smalltalk/st3-client'
import { CollectionFrame as FrameSchema, Envelope, ErrorEnvelope, decodeUnknownSync } from '@smalltalk/st3-client/schema'
import { applyConversation, isConversational, type Conversation } from '../../st3-views/sessionView.ts'

import { decodeSlice } from '../scripts/decode.ts'
import {
  ANCHOR_MS, SLICE_KINDS, catalog, foldSlice, loadWorld, syncStatusAt,
  type SliceKind, type SyncEvent, type World,
} from '../src/index.ts'
import { createReplay, manualClock } from '../src/replay/index.ts'
import { ScenarioProvider, createReadTracker, useScenarioSlice } from '../src/react/index.ts'
import { scenarioGlobalTypes, scenarioStoryCheck, withScenario } from '../src/storybook/index.ts'

const REBASED_NOW = ANCHOR_MS + 123_456_789
const WINDOW_LIMIT = 200
const SOCKET_COLLECTIONS = ['agents', 'missions', 'work', 'attention'] as const
const label = (world: World, slice: string, step: string) => `${world.id}/${slice}/${step}`
const step = async <T>(world: World, slice: string, name: string, run: () => T | Promise<T>): Promise<T> => {
  try { return await run() }
  catch (cause) { throw new Error(`${label(world, slice, name)}: ${cause instanceof Error ? cause.message : String(cause)}`, { cause }) }
}
const collectionSlice = (collection: string): SliceKind => {
  switch (collection) {
    case 'agents': case 'runtimes': case 'machines': return 'roster'
    case 'missions': case 'work': return 'details'
    case 'attention': case 'messages': return 'attention'
    case 'conversation': return 'conversation'
    case 'terminal': case 'terminals': return 'terminal'
    default: return 'sync'
  }
}
const rowsAt = (world: World, at: number): Record<string, Resource[]> => {
  const roster = foldSlice(world.slices.roster, at).state
  const details = foldSlice(world.slices.details, at).state
  const attention = foldSlice(world.slices.attention, at).state
  const terminal = foldSlice(world.slices.terminal, at).state
  const order = new Map(roster.order.map((id, index) => [id, index]))
  return {
    agents: [...roster.agents].sort((a, b) => (order.get(a.id) ?? Infinity) - (order.get(b.id) ?? Infinity)),
    runtimes: [...roster.runtimes, ...terminal.terminals.map(({ runtime }) => runtime)],
    machines: roster.machines, missions: details.missions, work: details.work,
    attention: attention.attention, messages: attention.messages,
    terminals: terminal.terminals.map(({ runtime }) => runtime),
  }
}

// A deliberately plain diagnostic consumer displays every slice's wire state, not fixture constants.
// Each slice is a separate component: no variable hook order when the catalog grows.
const SliceConsumer = ({ kind }: { kind: SliceKind }) => {
  const slice = useScenarioSlice(kind)
  return React.createElement('pre', { 'data-slice': kind }, JSON.stringify({
    loading: slice.loading, state: slice.state, ...('status' in slice ? { status: slice.status } : {}),
  }))
}
const Consumer = () => React.createElement('main', {}, ...SLICE_KINDS.map((kind) => React.createElement(SliceConsumer, { key: kind, kind })))
const meta = {
  title: 'Scenarios/CatalogEndToEnd', component: Consumer,
  parameters: { scenario: { slices: SLICE_KINDS } },
} satisfies Meta<Args>
const story = {} satisfies StoryObj<typeof meta>

const verifyReplay = async (world: World) => {
  const clock = manualClock(world.now)
  const replay = createReplay(world, { clock })
  let abortLoadingRead = false
  let currentStep = 'anchor'
  const fetchImpl: typeof fetch = async (input, init) => {
    const response = await replay.fetch(input, abortLoadingRead ? { ...init, signal: AbortSignal.abort() } : init)
    if (response.headers.get('Content-Type')?.includes('application/json')) {
      const body: unknown = await response.clone().json()
      const mode = SLICE_KINDS.some((kind) => world.slices[kind].decode === 'tolerant') ? 'tolerant' : 'strict'
      await step(world, new URL(typeof input === 'string' ? input : input instanceof URL ? input.href : input.url).pathname, currentStep, () => {
        if (typeof body === 'object' && body !== null && 'error_version' in body) decodeUnknownSync(ErrorEnvelope, mode)(body)
        else decodeUnknownSync(Envelope, mode)(body)
      })
    }
    return response
  }
  const client = new St3Client({ baseUrl: 'http://scenario.invalid', fetchImpl })
  const frames: CollectionFrame[] = []
  const windows = new Map<string, CollectionWindow>()
  const conversations = new Map<string, Conversation<TimelineEntry>>()
  const screens = new Map<string, CollectionFrame & { kind: 'screen' }>()
  const inactive = new Set<string>()
  let ended = false
  let expectedEnd = false
  let readFrames = 0
  const endings: (Error | undefined)[] = []
  const overrides = () => {
    const active = new Map<string, Extract<SyncEvent, { _tag: 'http-error' | 'http-raw' }>>()
    for (const event of world.slices.sync.timeline) if (event.at_ms <= clock.now() - world.now) {
      if (event._tag === 'http-error' || event._tag === 'http-raw') active.set(event.route, event)
      else if (event._tag === 'http-ok') active.delete(event.route)
    }
    return active
  }
  const read = async <T>(route: string, kinds: readonly SliceKind[], run: () => Promise<T>): Promise<T | undefined> =>
    step(world, kinds.join('+'), `${currentStep}:HTTP ${route}`, async () => {
      const active = overrides()
      const override = active.get(`/v1/client/${route}`) ?? active.get(route)
        ?? (!['capabilities', 'actions', 'events', 'timeline'].includes(route) ? active.get('resources') : undefined)
      const loading = kinds.some((kind) => world.slices[kind].loading)
      abortLoadingRead = loading
      try {
        if (override !== undefined) {
          try { await run() } catch (error) {
            if (override._tag === 'http-error') {
              expect(error).toBeInstanceOf(ClientError)
              if (!(error instanceof ClientError)) throw error
              expect(error.response).toEqual(override.envelope)
              expect(error.status).toBe(override.status)
            } else expect(error).toBeInstanceOf(Error)
            return undefined
          }
          throw new Error('Scripted HTTP rejection was not delivered')
        }
        if (loading) { await expect(run()).rejects.toMatchObject({ name: 'AbortError' }); return undefined }
        return await run()
      } finally { abortLoadingRead = false }
    })
  const consume = () => {
    for (const frame of frames.slice(readFrames)) {
      const id = frame.id ?? ''
      const window = applyWindow(windows.get(id), frame)
      if (window !== undefined) windows.set(id, window)
      if (frame.kind === 'conversation') conversations.set(id, applyConversation(conversations.get(id), {
        replace: frame.replace, items: frame.items, hasMore: frame.has_more ?? conversations.get(id)?.hasOlder ?? false, sessionId: frame.session_id,
      }))
      if (frame.kind === 'screen') screens.set(id, frame)
      if (frame.kind === 'error' || frame.kind === 'resync') inactive.add(id)
    }
    readFrames = frames.length
  }
  const compare = async () => {
    consume()
    const at = clock.now() - world.now
    const rows = rowsAt(world, at)
    for (const [collection, expected] of Object.entries(rows)) {
      if (!(SOCKET_COLLECTIONS as readonly string[]).includes(collection) || ended || inactive.has(collection)) continue
      const kind = collectionSlice(collection)
      if (world.slices[kind].loading) { expect(windows.has(collection), label(world, kind, `${currentStep}:${collection}:loading`)).toBe(false); continue }
      const held = new Set<string>()
      for (const event of world.slices.sync.timeline) if (event.at_ms <= at && (event._tag === 'hold' || event._tag === 'release')) {
        if (event._tag === 'hold') held.add(event.selector.collection)
        else held.delete(event.selector.collection)
      }
      if (held.has(collection) && !windows.has(collection)) continue
      await step(world, kind, `${currentStep}:applyWindow ${collection}`, () => {
        expect(windows.get(collection)?.items).toEqual(expected.slice(0, WINDOW_LIMIT))
        expect(windows.get(collection)?.hasMore).toBe(expected.length > WINDOW_LIMIT)
      })
    }
    if (!ended && !world.slices.conversation.loading) for (const thread of foldSlice(world.slices.conversation, at).state.threads) {
      const id = `conversation:${thread.agent}`
      if (inactive.has(id)) continue
      const initial = world.slices.conversation.state.threads.find((value) => value.agent === thread.agent)
      if (initial === undefined) continue
      let visible = new Set(initial.items.slice(-initial.page_size).map(({ id }) => id))
      for (const event of world.slices.conversation.timeline) if (event.at_ms <= at) {
        if (event._tag === 'thread-create' && event.thread.agent === thread.agent) visible = new Set(event.thread.items.slice(-event.thread.page_size).map(({ id }) => id))
        else if (event._tag === 'replace' && event.agent === thread.agent) visible = new Set(event.items.slice(-thread.page_size).map(({ id }) => id))
        else if (event._tag === 'entries' && event.agent === thread.agent) for (const item of event.items) visible.add(item.id)
        else if (event._tag === 'thread-remove' && event.agent === thread.agent) visible.clear()
      }
      const received = conversations.get(id)
      // A sync hold deliberately withholds the first frame; it is not an empty conversation.
      const held = world.slices.sync.timeline.filter((event) => event.at_ms <= at && (event._tag === 'hold' || event._tag === 'release')
        && event.selector.collection === 'conversation' && event.selector.conversation === thread.agent).at(-1)
      if (held?._tag === 'hold' && received === undefined) continue
      await step(world, 'conversation', `${currentStep}:applyConversation ${thread.agent}`, () => {
        expect(received?.entries).toEqual(thread.items.filter((item) => visible.has(item.id) && isConversational(item))
          .sort((a, b) => a.timestamp.localeCompare(b.timestamp) || a.sequence - b.sequence))
        expect(received?.sessionId).toBe(thread.session_id)
      })
    }
    if (!ended && !world.slices.terminal.loading) for (const terminal of foldSlice(world.slices.terminal, at).state.terminals) {
      const id = `terminal:${terminal.terminal}`
      if (inactive.has(id)) continue
      const expected = terminal.screens.filter(({ at_ms }) => at_ms <= at).at(-1)?.screen
      if (expected !== undefined) await step(world, 'terminal', `${currentStep}:screen ${terminal.terminal}`, () => expect(screens.get(id)?.value).toEqual(expected))
    }
    // Fold *all* slices even if a scripted transport failure prevents a live oracle comparison.
    for (const kind of SLICE_KINDS) await step(world, kind, `${currentStep}:fold`, () => foldSlice(world.slices[kind], at))
  }
  try {
    const discovery = await read('capabilities', ['sync'], () => client.discover())
    if (discovery !== undefined) expect(discovery.value).toEqual(world.slices.sync.state.capabilities)
    const lists: readonly [string, readonly SliceKind[], (options: PageOptions) => Promise<EnvelopeOf<Page>>][] = [
      ['agents', ['roster'], (options) => client.agentsList(options)],
      ['runtimes', ['roster', 'terminal'], (options) => client.runtimesList(options)],
      ['machines', ['roster'], (options) => client.machinesList(options)],
      ['missions', ['details'], (options) => client.missionsList(options)],
      ['work', ['details'], (options) => client.workList(options)],
      ['attention', ['attention'], (options) => client.attentionList(options)],
      ['messages', ['attention'], (options) => client.messagesList(options)],
      ['terminals', ['terminal'], (options) => client.terminalsList(options)],
    ]
    if (discovery !== undefined) {
      const limit = Math.min(WINDOW_LIMIT, discovery.value.limits.max_page_items)
      for (const [collection, kinds, list] of lists) await step(world, kinds.join('+'), `anchor:pages ${collection}`, async () => {
        const items: Resource[] = []
        const cursors = new Set<string>()
        let cursor: string | undefined
        do {
          const result = await read(collection, kinds, () => list({ limit, ...(cursor === undefined ? {} : { cursor }) }))
          if (result === undefined) return
          items.push(...result.value.items)
          cursor = result.value.page.has_more ? result.value.page.next_cursor ?? undefined : undefined
          if (result.value.page.has_more) expect(cursor, 'has_more requires a next cursor').toBeDefined()
          if (cursor !== undefined) { expect(cursors.has(cursor), `repeated cursor ${cursor}`).toBe(false); cursors.add(cursor) }
        } while (cursor !== undefined)
        expect(items).toEqual(rowsAt(world, 0)[collection])
      })
      await step(world, 'roster+details+attention+terminal', 'anchor:pages resources', async () => {
        const facts: unknown[] = []
        const cursors = new Set<string>()
        let cursor: string | undefined
        do {
          const result = await read('resources', ['roster', 'details', 'attention', 'terminal'],
            () => client.resourcesList({}, { limit, ...(cursor === undefined ? {} : { cursor }) }))
          if (result === undefined) return
          facts.push(...result.value.items.map((item) => item.facts))
          cursor = result.value.page.has_more ? result.value.page.next_cursor ?? undefined : undefined
          if (result.value.page.has_more) expect(cursor).toBeDefined()
          if (cursor !== undefined) { expect(cursors.has(cursor)).toBe(false); cursors.add(cursor) }
        } while (cursor !== undefined)
        const rows = rowsAt(world, 0)
        expect(facts).toEqual(['agents', 'runtimes', 'machines', 'missions', 'work', 'attention', 'messages'].flatMap((collection) => rows[collection]!))
      })
      await read('events', ['sync'], () => client.eventsList())
      for (const thread of world.slices.conversation.state.threads) await step(world, 'conversation', `anchor:pages ${thread.agent}`, async () => {
        let cursor: string | undefined
        const cursors = new Set<string>()
        const pages: TimelineEntry[][] = []
        do {
          const result = await read('timeline', ['conversation'], () => client.timelineList(thread.session_id, { limit, ...(cursor === undefined ? {} : { cursor }) }))
          if (result === undefined) return
          pages.unshift(result.value.items)
          cursor = result.value.page.has_more ? result.value.page.next_cursor ?? undefined : undefined
          if (result.value.page.has_more) expect(cursor).toBeDefined()
          if (cursor !== undefined) { expect(cursors.has(cursor)).toBe(false); cursors.add(cursor) }
        } while (cursor !== undefined)
        expect(pages.flat()).toEqual(thread.items)
      })
    }
    const stream = await client.collectionStream({ socket: replay.socket,
      onFrame: (frame) => {
        const kind = collectionSlice(frame.collection ?? '')
        try { decodeUnknownSync(FrameSchema, world.slices[kind].decode)(frame) }
        catch (cause) { throw new Error(`${label(world, kind, `${currentStep}:frame ${frame.kind}/${frame.id ?? ''}`)}: ${String(cause)}`, { cause }) }
        frames.push(frame)
      },
      onEnd: (error) => { ended = true; endings.push(error) },
    })
    for (const collection of SOCKET_COLLECTIONS) stream.subscribe(collection, collection, WINDOW_LIMIT)
    for (const thread of world.slices.conversation.state.threads) stream.subscribeConversation(`conversation:${thread.agent}`, thread.agent)
    for (const terminal of world.slices.terminal.state.terminals) {
      if (terminal.runtime.state === 'exited' || world.slices.terminal.loading || world.slices.roster.loading || discovery === undefined) { inactive.add(`terminal:${terminal.terminal}`); continue }
      const runtime = await read('runtimes', ['roster', 'terminal'], () => client.runtimesGet(terminal.runtime.id))
      if (runtime === undefined || runtime.value.kind !== 'runtime') { inactive.add(`terminal:${terminal.terminal}`); continue }
      const incarnation = runtime.value.incarnation_id
      if (incarnation == null) throw new Error(`${label(world, 'terminal', currentStep)}: runtime has no incarnation`)
      const attached = await read('actions', ['terminal'], () => client.terminalAttach({
        id: `action/e2e-${terminal.terminal}`, idempotency_key: `e2e-${terminal.terminal}`,
        parameters: { target_id: terminal.terminal },
        fence: { snapshot_id: runtime.snapshot.id, subject_revisions: {}, runtime_incarnation: incarnation },
      }))
      const attachment = attached?.value.terminal_attachment
      if (attachment?.stream_capability == null) { inactive.add(`terminal:${terminal.terminal}`); continue }
      stream.subscribeTerminal(`terminal:${terminal.terminal}`, attachment.terminal_id, attachment.runtime_incarnation, attachment.stream_capability)
    }
    expectedEnd = world.slices.sync.timeline.some((event) => event.at_ms <= 0 && (event._tag === 'close' || event._tag === 'open-fail'))
    await step(world, 'sync', 'anchor:socket open and first frames', () => clock.advance(0))
    expect(endings.length === 0 || expectedEnd, label(world, 'sync', 'anchor:unexpected socket end')).toBe(true)
    await compare()
    const events = SLICE_KINDS.flatMap((kind) => world.slices[kind].timeline.map((event, index) => ({ kind, event, index })))
      .filter(({ event }) => event.at_ms > 0).sort((a, b) => a.event.at_ms - b.event.at_ms)
    for (const { kind, event, index } of events) {
      currentStep = `${event.at_ms}ms:${kind}[${index}]:${event._tag}`
      expectedEnd ||= event._tag === 'close' || event._tag === 'open-fail'
      await step(world, kind, `${currentStep}:dispatch`, () => clock.advance(world.now + event.at_ms - clock.now()))
      expect(endings.length === 0 || expectedEnd, label(world, 'sync', `${currentStep}:unexpected socket end`)).toBe(true)
      await compare()
      if (kind === 'sync' && ['http-error', 'http-raw', 'http-ok'].includes(event._tag) && 'route' in event) {
        const route = event.route
        if (route === 'capabilities') await read(route, ['sync'], () => client.capabilities())
        else if (route === 'events') await read(route, ['sync'], () => client.eventsList())
        else if (route === 'timeline') {
          const thread = world.slices.conversation.state.threads[0]
          if (thread !== undefined) await read(route, ['conversation'], () => client.timelineList(thread.session_id, { cursor: 'scenario-cursor/1' }))
        } else {
          const list = lists.find(([collection]) => route === collection || route === `/v1/client/${collection}`) ?? lists[0]!
          await read(route, list[1], () => list[2]({ limit: WINDOW_LIMIT }))
        }
      }
    }
    stream.close()
  } finally { replay.close() }
}

describe.each(catalog.map(({ id }) => [id]))('catalog consumer end-to-end: %s', (id) => {
  it.each([ANCHOR_MS, REBASED_NOW])('loads and decodes every slice at now=%s', (now) => {
    const world = loadWorld(id!, { now })
    for (const kind of SLICE_KINDS) expect(decodeSlice(world.id, world.slices[kind]), label(world, kind, `decode now=${now}`)).toEqual([])
    // TODO(strict-negative-paths): read slice.unknown JSON pointers and assert exact strict failures once that planned contract exists.
  })

  it('serves discovery, all pages, first socket frames and every timeline step to St3Client', async () => {
    await verifyReplay(loadWorld(id!, { now: ANCHOR_MS }))
  }, 30_000)

  it('renders each slice through the provider and switches the actual story toolbar', async () => {
    const world = loadWorld(id!, { now: ANCHOR_MS })
    const clock = manualClock(world.now)
    const tracker = createReadTracker()
    const render = () => renderToStaticMarkup(React.createElement(ScenarioProvider, { world, clock, tracker }, React.createElement(Consumer)))
    const initialMarkup = render()
    expect(tracker.reads()).toEqual(new Set(SLICE_KINDS))
    const Composed = composeStory(story, meta, { decorators: [withScenario], globalTypes: scenarioGlobalTypes,
      initialGlobals: { scenario: world.id, scenarioNow: world.now } })
    const toolbarMarkup = renderToStaticMarkup(React.createElement(Composed))
    expect(toolbarMarkup, label(world, 'all', 'toolbar selected world')).toContain(`data-scenario="${world.id}"`)
    expect(toolbarMarkup, label(world, 'all', 'toolbar consumer content')).toContain(initialMarkup)
    const times = [...new Set(SLICE_KINDS.flatMap((kind) => world.slices[kind].timeline.map(({ at_ms }) => at_ms)))].filter((at) => at > 0).sort((a, b) => a - b)
    for (const at of [0, ...times]) {
      clock.advance(world.now + at - clock.now())
      const expected = React.createElement('main', {}, ...SLICE_KINDS.map((kind) => {
        const slice = foldSlice(world.slices[kind], at)
        return React.createElement('pre', { key: kind, 'data-slice': kind }, JSON.stringify({ loading: slice.loading, state: slice.state,
          ...(kind === 'sync' ? { status: syncStatusAt(world.slices.sync, at, world.now) } : {}) }))
      }))
      expect(render(), label(world, 'all', `provider fold at ${at}ms`)).toBe(renderToStaticMarkup(expected))
    }
    // Pinned per-world checks avoid running the checker's O(catalog²) world-pair loop once per world.
    // Scope marker checks to one slice: shared titles/text in other slices are legitimate, not stale data.
    for (const kind of SLICE_KINDS) await step(world, kind, 'scenarioStoryCheck pinned world', () => scenarioStoryCheck({
      render: () => React.createElement(SliceConsumer, { kind }),
      parameters: { scenario: { slices: [kind], world: world.id } },
    }, meta, { scenario: world.id, scenarioNow: world.now, assert: true }))
  }, 30_000)
})

it('checks unpinned consumer data dependence across the entire catalog once', async () => {
  const world = loadWorld(catalog[0]!.id, { now: ANCHOR_MS })
  for (const kind of SLICE_KINDS) await step(world, kind, 'scenarioStoryCheck full catalog world-switch', () => scenarioStoryCheck({
    render: () => React.createElement(SliceConsumer, { kind }),
    parameters: { scenario: { slices: [kind] } },
  }, meta, { scenarioNow: ANCHOR_MS, assert: true }))
}, 30_000)
