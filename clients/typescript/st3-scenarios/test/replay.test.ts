import { afterEach, describe, expect, it } from 'vitest'
import {
  applyWindow, ClientError, St3Client,
  type ActionRequest, type CollectionFrame, type CollectionStream, type CollectionWindow,
  type TerminalAttachment, type TimelineEntry,
} from '@smalltalk/st3-client'
import { CollectionFrame as FrameSchema, Envelope, ErrorEnvelope, decodeUnknownSync } from '@smalltalk/st3-client/schema'
import { applyConversation, isConversational, type Conversation } from '../../st3-views/sessionView.ts'

import { ANCHOR_MS, foldSlice, loadWorld, type Slice, type SyncEvent, type World } from '../src/index.ts'
import { createReplay, manualClock, type Replay } from '../src/replay/index.ts'

const replays: Replay[] = []
afterEach(() => { for (const replay of replays.splice(0)) replay.close() })
const base = () => loadWorld('fleet-mid-refactor', { now: ANCHOR_MS })
const scripted = (timeline: SyncEvent[], world = base()): World => world.with({ sync: { ...world.slices.sync, timeline } })
const setup = (world = base()) => {
  const clock = manualClock(world.now)
  const replay = createReplay(world, { clock })
  replays.push(replay)
  const replies: unknown[] = []
  const fetchImpl: typeof fetch = async (input, init) => {
    const response = await replay.fetch(input, init)
    if (response.headers.get('Content-Type')?.includes('application/json')) {
      const body: unknown = await response.clone().json()
      if (typeof body === 'object' && body !== null && 'error_version' in body) decodeUnknownSync(ErrorEnvelope, 'strict')(body)
      else decodeUnknownSync(Envelope, 'strict')(body)
      replies.push(body)
    }
    return response
  }
  const client = new St3Client({ baseUrl: 'http://scenario.invalid', fetchImpl })
  const frames: CollectionFrame[] = []
  const endings: (Error | undefined)[] = []
  const connect = async (): Promise<CollectionStream> => client.collectionStream({ socket: replay.socket,
    onFrame: (frame) => { decodeUnknownSync(FrameSchema, 'strict')(frame); frames.push(frame) }, onEnd: (error) => endings.push(error) })
  const attach = async (terminal = world.slices.terminal.state.terminals[0]!.terminal): Promise<TerminalAttachment> => {
    const current = foldSlice(world.slices.terminal, clock.now() - world.now).state.terminals.find((item) => item.terminal === terminal)!
    const runtime = await client.runtimesGet(current.runtime.id)
    const resource = runtime.value
    if (resource.kind !== 'runtime' || resource.incarnation_id === null) throw new Error('Terminal runtime missing incarnation')
    const result = await client.terminalAttach({ id: 'action/scenario-attach', idempotency_key: 'scenario-attach',
      parameters: { target_id: terminal }, fence: { snapshot_id: runtime.snapshot.id, subject_revisions: {}, runtime_incarnation: resource.incarnation_id } })
    const attachment = result.value.terminal_attachment
    if (attachment == null) throw new Error('Attach did not return an attachment')
    return attachment
  }
  const subscribeTerminal = (stream: CollectionStream, attachment: TerminalAttachment, id = 'term') => {
    if (attachment.stream_capability === null) throw new Error('Attach did not return a capability')
    stream.subscribeTerminal(id, attachment.terminal_id, attachment.runtime_incarnation, attachment.stream_capability)
  }
  return { world, clock, replay, client, replies, frames, endings, connect, attach, subscribeTerminal }
}
const errors = (frames: CollectionFrame[]) => frames.filter((frame) => frame.kind === 'error')
const screens = (frames: CollectionFrame[]) => frames.filter((frame) => frame.kind === 'screen')
const event = <T extends SyncEvent>(value: T): T => value

describe('collection windows', () => {
  it('dispatches changes and applyWindow equals the roster fold at each clock step', async () => {
    const h = setup()
    const stream = await h.connect()
    stream.subscribe('all', 'agents', 200)
    stream.subscribe('small', 'agents', 2)
    stream.subscribe('blocked', 'agents', 200, { status: 'blocked' })
    h.clock.advance(0)
    const windows = new Map<string, CollectionWindow>()
    let read = 0
    const check = () => {
      for (const frame of h.frames.slice(read)) {
        const next = applyWindow(windows.get(frame.id ?? ''), frame)
        if (next !== undefined) windows.set(frame.id ?? '', next)
      }
      read = h.frames.length
      const roster = foldSlice(h.world.slices.roster, h.clock.now() - h.world.now).state
      const ordered = roster.order.flatMap((id) => roster.agents.filter((agent) => agent.id === id))
      for (const [id, limit, status] of [['all', 200, undefined], ['small', 2, undefined], ['blocked', 200, 'blocked']] as const) {
        const all = ordered.filter((agent) => status === undefined || agent.state === status)
        expect(windows.get(id)?.items).toEqual(all.slice(0, limit))
        expect(windows.get(id)?.hasMore).toBe(all.length > limit)
      }
    }
    check()
    for (const next of h.world.slices.roster.timeline) { h.clock.advance(h.world.now + next.at_ms - h.clock.now()); check() }
    expect(h.frames.some((frame) => frame.kind === 'changes')).toBe(true)
    expect(h.replay.served()).toContain('roster')
  })

  it('removes rows that leave a limited window and leaves unaffected subscriptions alone', async () => {
    const world = base()
    const [a, b, c] = world.slices.roster.state.agents
    const h = setup(world.with({ roster: { ...world.slices.roster, timeline: [
      { _tag: 'changes', at_ms: 10, store: 0, upserts: [], removes: [], order: [c!.id, b!.id, a!.id] },
    ] } }))
    const stream = await h.connect()
    stream.subscribe('small', 'agents', 1)
    stream.subscribe('none', 'agents', 1, { status: 'never-present' })
    h.clock.advance(0)
    h.clock.advance(10)
    const change = h.frames.find((frame) => frame.kind === 'changes')
    expect(change).toMatchObject({ kind: 'changes', id: 'small', removes: [a!.id], order: [c!.id], upserts: [c] })
    expect(h.frames.filter((frame) => frame.id === 'none')).toHaveLength(1)
  })

  it('keeps equal fences without changes and nondecreasing store versions per socket', async () => {
    const h = setup()
    const stream = await h.connect()
    stream.subscribe('first', 'agents', 2)
    stream.subscribe('second', 'agents', 2)
    h.clock.advance(0)
    const first = h.frames[0]
    const second = h.frames[1]
    expect(first?.kind).toBe('snapshot')
    if (first?.kind !== 'snapshot' || second?.kind !== 'snapshot') throw new Error('Missing snapshots')
    expect(first.snapshot).toEqual(second.snapshot)
    h.clock.advance(20_000)
    stream.subscribe('third', 'missions', 20)
    const stores = h.frames.flatMap((frame) => 'snapshot' in frame ? [frame.snapshot.store_index] : [])
    expect(stores).toEqual([...stores].sort((a, b) => a - b))
    expect(stores.at(-1)).toBeGreaterThan(stores[0]!)
  })

  it.each(['empty', 'one-agent', 'loading'] as const)('serves the %s roster variant', async (variant) => {
    const h = setup(base().with({ roster: variant }))
    const stream = await h.connect()
    stream.subscribe('agents', 'agents', 200)
    h.clock.advance(0)
    if (variant === 'loading') { h.clock.advance(100_000); expect(h.frames).toEqual([]) }
    else {
      const frame = h.frames[0]
      if (frame?.kind !== 'snapshot') throw new Error('Missing snapshot')
      expect(frame.items).toEqual(h.world.slices.roster.state.agents)
    }
  })
})

describe('conversation dispatch and consumer fold', () => {
  it('dispatches entries as revision deltas without has_more and folds with sessionView', async () => {
    const h = setup()
    const thread = h.world.slices.conversation.state.threads.find((thread) => h.world.slices.conversation.timeline.some((event) => event.agent === thread.agent))!
    const stream = await h.connect()
    stream.subscribeConversation('talk', thread.agent)
    stream.subscribeConversation('session', thread.session_id)
    h.clock.advance(0)
    let conversation: Conversation<TimelineEntry> | undefined
    let read = 0
    const check = () => {
      for (const frame of h.frames.slice(read)) if (frame.kind === 'conversation' && frame.id === 'talk') {
        if (!frame.replace) expect(frame).not.toHaveProperty('has_more')
        conversation = applyConversation(conversation, { replace: frame.replace, items: frame.items,
          hasMore: frame.has_more ?? conversation?.hasOlder ?? false, sessionId: frame.session_id })
      }
      read = h.frames.length
      const current = foldSlice(h.world.slices.conversation, h.clock.now() - h.world.now).state.threads.find((value) => value.agent === thread.agent)!
      const expected = current.items.filter(isConversational).sort((a, b) => a.timestamp.localeCompare(b.timestamp) || a.sequence - b.sequence)
      expect(conversation?.entries).toEqual(expected)
    }
    check()
    for (const next of h.world.slices.conversation.timeline) { h.clock.advance(h.world.now + next.at_ms - h.clock.now()); check() }
    expect(h.frames.filter((frame) => frame.kind === 'conversation' && !frame.replace)).toHaveLength(h.world.slices.conversation.timeline.length * 2)
  })

  it('dispatches replace with the newest page and its own history availability', async () => {
    const world = base()
    const thread = world.slices.conversation.state.threads[0]!
    const items = thread.items.slice(0, 3)
    const conversation: Slice<'conversation'> = { ...world.slices.conversation,
      state: { threads: [{ ...thread, page_size: 2 }] }, timeline: [{ _tag: 'replace', at_ms: 10, store: 0,
        agent: thread.agent, session_id: 'session/scenario-replaced', items, has_more: false }] }
    const h = setup(world.with({ conversation }))
    const stream = await h.connect()
    stream.subscribeConversation('talk', thread.agent)
    h.clock.advance(0)
    h.clock.advance(10)
    const frame = h.frames.at(-1)
    expect(frame).toMatchObject({ kind: 'conversation', replace: true, session_id: 'session/scenario-replaced', items: items.slice(-2), has_more: true })
    if (frame?.kind !== 'conversation') throw new Error('Missing conversation')
    const folded = applyConversation<TimelineEntry>(undefined, { replace: frame.replace, items: frame.items, hasMore: frame.has_more ?? false, sessionId: frame.session_id })
    expect(folded.entries).toEqual(items.slice(-2).filter(isConversational).sort((a, b) => a.timestamp.localeCompare(b.timestamp) || a.sequence - b.sequence))
  })
})

describe('terminal dispatch', () => {
  it.each(['default', 'running', 'unavailable', 'exited', 'restarted'] as const)('strict-decodes ordered wire delivery for %s', async (variant) => {
    const h = setup(base().with({ terminal: variant }))
    const attachment = await h.attach()
    const stream = await h.connect()
    h.subscribeTerminal(stream, attachment)
    h.clock.advance(0)
    expect(screens(h.frames)).toHaveLength(1)
    const expected: string[] = ['screen']
    for (const next of h.world.slices.terminal.timeline) {
      h.clock.advance(h.world.now + next.at_ms - h.clock.now())
      if (next._tag === 'screen' && !expected.includes('error')) expected.push('screen')
      else if (next._tag !== 'screen') expected.push('error')
    }
    expect(h.frames.map((frame) => frame.kind)).toEqual(expected)
    const expectedCode = variant === 'unavailable' ? 'terminal-unavailable' : variant === 'exited' ? 'terminal-ended' : variant === 'restarted' ? 'stale-fence' : undefined
    if (expectedCode !== undefined) expect(errors(h.frames).at(-1)?.code).toBe(expectedCode)
    const stores = screens(h.frames).map((frame) => frame.snapshot.store_index)
    expect(stores).toEqual([...stores].sort((a, b) => a - b))
    expect(h.replay.served()).toContain('terminal')
  })

  it('the none terminal variant has no runtime or subscribable terminal', async () => {
    const h = setup(base().with({ terminal: 'none' }))
    expect((await h.client.terminalsList()).value.items).toEqual([])
    const stream = await h.connect()
    stream.subscribeTerminal('absent', 'terminal/scenario-absent', 'scenario:1', 'scenario-absent')
    h.clock.advance(0)
    expect(errors(h.frames)).toMatchObject([{ code: 'not-found', retryable: false }])
  })

  it('screen delivery does not advance the store', async () => {
    const world = base().with({ terminal: 'running', roster: 'empty', conversation: 'empty', details: 'empty', attention: 'none' })
    const h = setup(world)
    const attachment = await h.attach()
    const stream = await h.connect()
    h.subscribeTerminal(stream, attachment)
    h.clock.advance(0)
    h.clock.advance(100_000)
    expect(screens(h.frames).length).toBeGreaterThan(1)
    expect(new Set(screens(h.frames).map((frame) => frame.snapshot.store_index))).toEqual(new Set([1]))
    expect(new Set(screens(h.frames).map((frame) => JSON.stringify(frame.snapshot))).size).toBe(1)
  })

  it('unavailable ends the follow; fresh attach and reusable lease both resume', async () => {
    const h = setup(base().with({ terminal: 'unavailable' }))
    const attachment = await h.attach()
    const stream = await h.connect()
    h.subscribeTerminal(stream, attachment)
    h.clock.advance(0)
    h.clock.advance(3_000)
    expect(errors(h.frames).at(-1)).toMatchObject({ code: 'terminal-unavailable', retryable: true })
    h.subscribeTerminal(stream, attachment, 'reuse')
    h.subscribeTerminal(stream, await h.attach(), 'fresh')
    expect(h.frames.slice(-2).map((frame) => frame.kind)).toEqual(['screen', 'screen'])
  })

  it('end refuses subsequent attaches and subscribes', async () => {
    const h = setup(base().with({ terminal: 'exited' }))
    const attachment = await h.attach()
    const stream = await h.connect()
    h.subscribeTerminal(stream, attachment)
    h.clock.advance(0)
    h.clock.advance(3_000)
    expect(errors(h.frames).at(-1)).toMatchObject({ code: 'terminal-ended', retryable: false })
    await expect(h.attach()).rejects.toMatchObject({ status: 409, response: { code: 'terminal-ended', retryable: false } })
    h.subscribeTerminal(stream, attachment, 'ended')
    expect(errors(h.frames).at(-1)?.code).toBe('terminal-ended')
  })

  it('incarnation rejects stale subscriptions and attaches afresh to the new runtime', async () => {
    const h = setup(base().with({ terminal: 'restarted' }))
    const old = await h.attach()
    const stream = await h.connect()
    h.subscribeTerminal(stream, old)
    h.clock.advance(0)
    h.clock.advance(3_000)
    expect(errors(h.frames).at(-1)?.code).toBe('stale-fence')
    h.subscribeTerminal(stream, old, 'old')
    expect(errors(h.frames).at(-1)).toMatchObject({ id: 'old', code: 'stale-fence' })
    const fresh = await h.attach()
    expect(fresh.runtime_incarnation).not.toBe(old.runtime_incarnation)
    expect(fresh.stream_capability).not.toBe(old.stream_capability)
    h.subscribeTerminal(stream, fresh, 'new')
    h.clock.advance(500)
    expect(screens(h.frames).at(-1)?.value.runtime_incarnation).toBe(fresh.runtime_incarnation)
  })

  it('also works through the generated standalone terminalStream factory seam', async () => {
    const h = setup(base().with({ terminal: 'unavailable' }))
    const attachment = await h.attach()
    const received: unknown[] = []
    const ending: Error[] = []
    await h.client.terminalStream(attachment.terminal_id, { socket: h.replay.socket,
      streamCapability: attachment.stream_capability!, incarnation: attachment.runtime_incarnation,
      onScreen: (screen) => { decodeUnknownSync(Envelope, 'strict')(screen); received.push(screen) },
      onEnd: (error) => { if (error !== undefined) ending.push(error) } })
    h.clock.advance(0)
    expect(received).toHaveLength(1)
    h.clock.advance(3_000)
    expect(ending[0]).toBeInstanceOf(ClientError)
    expect(ending[0]).toMatchObject({ response: { code: 'terminal-unavailable', retryable: true } })
  })
})

describe('sync dispatch', () => {
  it('open-fail reports browser onerror then onclose 1006 and only fails the next open', async () => {
    const h = setup(scripted([{ _tag: 'open-fail', at_ms: 0, store: 0 }, { _tag: 'open-fail', at_ms: 10, store: 0 }]))
    const calls: string[] = []
    const socket = h.replay.socket('ws://scenario.invalid/v1/client/collections/stream', [], {})
    socket.onerror = () => calls.push('error')
    socket.onclose = (close) => { expect(close).toEqual({ code: 1006, reason: '' }); calls.push('close') }
    expect(calls).toEqual([])
    h.clock.advance(0)
    expect(calls).toEqual(['error', 'close'])
    h.clock.advance(10)
    await h.connect()
    h.clock.advance(0)
    expect(h.endings[0]?.message).toBe('The collections socket failed')
    const stream = await h.connect()
    stream.subscribe('agents', 'agents')
    h.clock.advance(0)
    expect(h.frames[0]?.kind).toBe('snapshot')
  })

  it('close reaches the real client with the configured close code', async () => {
    const h = setup(scripted([{ _tag: 'close', at_ms: 10, store: 0, code: 1012, reason: 'scenario restart' }]))
    await h.connect()
    h.clock.advance(0)
    h.clock.advance(10)
    expect(h.endings[0]?.message).toContain('1012 scenario restart')
  })

  it('reopen fails attempts until after_ms, then permits a fresh snapshot', async () => {
    const h = setup(base().with({ sync: 'reconnected' }))
    await h.connect()
    h.clock.advance(0)
    h.clock.advance(4_000)
    await h.connect()
    h.clock.advance(0)
    expect(h.endings).toHaveLength(2)
    h.clock.advance(2_999)
    await h.connect()
    h.clock.advance(0)
    expect(h.endings).toHaveLength(3)
    h.clock.advance(1)
    const stream = await h.connect()
    stream.subscribe('new', 'agents')
    h.clock.advance(0)
    expect(h.frames.at(-1)).toMatchObject({ kind: 'snapshot', id: 'new' })
  })

  it('http-error persists until http-ok restores normal reads', async () => {
    const envelope = { api_version: 'st3.client.v0', error_version: 'st3.client.error.v0', code: 'remote-unavailable',
      details: {}, request_id: 'request/scenario-error', retryable: true, message: 'Scenario owner unavailable' } as const
    const h = setup(scripted([
      { _tag: 'http-error', at_ms: 10, store: 0, route: 'resources', status: 503, envelope },
      { _tag: 'http-ok', at_ms: 20, store: 0, route: 'resources' },
    ]))
    await h.client.agentsList()
    h.clock.advance(10)
    await expect(h.client.agentsList()).rejects.toMatchObject({ status: 503, response: envelope })
    await expect(h.client.agentsList()).rejects.toBeInstanceOf(ClientError)
    h.clock.advance(10)
    expect((await h.client.agentsList()).value.items).toEqual(h.world.slices.roster.state.agents)
  })

  it('http-raw returns the exact non-envelope response until http-ok', async () => {
    const h = setup(scripted([
      { _tag: 'http-raw', at_ms: 0, store: 0, route: 'capabilities', status: 502, content_type: 'text/html', body: '<p>Scenario proxy unavailable</p>' },
      { _tag: 'http-ok', at_ms: 10, store: 0, route: 'capabilities' },
    ]))
    const response = await h.replay.fetch('http://scenario.invalid/v1/client/capabilities')
    expect(response.status).toBe(502)
    expect(response.headers.get('Content-Type')).toBe('text/html')
    expect(await response.text()).toBe('<p>Scenario proxy unavailable</p>')
    h.clock.advance(10)
    expect((await h.client.capabilities()).value.kind).toBe('capabilities')
  })

  it('hold withholds only the first reply and release builds it at the current state', async () => {
    const h = setup(scripted([
      { _tag: 'hold', at_ms: 0, store: 0, selector: { collection: 'agents' } },
      { _tag: 'release', at_ms: 20_000, store: 0, selector: { collection: 'agents' } },
    ]))
    const stream = await h.connect()
    stream.subscribe('held', 'agents')
    stream.subscribe('free', 'missions')
    h.clock.advance(0)
    expect(h.frames.map((frame) => frame.id)).toEqual(['free'])
    h.clock.advance(20_000)
    const frame = h.frames.find((frame) => frame.id === 'held')
    expect(frame?.kind).toBe('snapshot')
    if (frame?.kind !== 'snapshot') throw new Error('Missing released snapshot')
    expect(frame.items).toEqual(foldSlice(h.world.slices.roster, 20_000).state.agents)
  })

  it.each([false, true])('resync emits the %s coded shape and keeps the subscription', async (coded) => {
    const h = setup(scripted([event({ _tag: 'resync', at_ms: 10, store: 0, selector: { collection: 'agents' },
      ...(coded ? { code: 'remote-unavailable', message: 'Scenario member unavailable' } : {}) })]))
    const stream = await h.connect()
    stream.subscribe('agents', 'agents')
    h.clock.advance(0)
    h.clock.advance(10)
    const frame = h.frames.at(-1)
    expect(frame).toMatchObject({ kind: 'resync', id: 'agents', collection: 'agents', retryable: true })
    if (coded) expect(frame).toMatchObject({ code: 'remote-unavailable', message: 'Scenario member unavailable' })
    else { expect(frame).not.toHaveProperty('code'); expect(frame).not.toHaveProperty('message') }
    h.clock.advance(20_000)
    expect(h.frames.some((frame) => frame.kind === 'changes')).toBe(true)
  })

  it.each([false, true])('error emits with selector=%s and permanent errors stop the selected follow', async (selected) => {
    const h = setup(scripted([{ _tag: 'error', at_ms: 10, store: 0, message: 'Scenario subscription refused', retryable: false,
      ...(selected ? { selector: { collection: 'agents' as const }, code: 'forbidden' } : {}) }]))
    const stream = await h.connect()
    stream.subscribe('agents', 'agents')
    h.clock.advance(0)
    h.clock.advance(10)
    const frame = errors(h.frames).at(-1)
    expect(frame).toMatchObject({ kind: 'error', message: 'Scenario subscription refused', retryable: false })
    if (selected) expect(frame).toMatchObject({ collection: 'agents', id: 'agents', code: 'forbidden' })
    else { expect(frame).not.toHaveProperty('collection'); expect(frame).not.toHaveProperty('code') }
    h.clock.advance(20_000)
    expect(h.frames.some((frame) => frame.kind === 'changes')).toBe(!selected)
  })

  it('notice and notice-clear change every subsequent page but send no socket frame', async () => {
    const peers = [{ host_id: 'host/harbor', local_only_envelopes: 2, peer_only_envelopes: 1 }]
    const h = setup(scripted([
      { _tag: 'notice', at_ms: 10, store: 0, peers }, { _tag: 'notice-clear', at_ms: 20, store: 0 },
    ]))
    await h.connect()
    h.clock.advance(0)
    h.clock.advance(10)
    expect((await h.client.agentsList()).value.sync).toEqual({ state: 'catching-up', peers })
    expect((await h.client.resourcesList()).value.sync).toEqual({ state: 'catching-up', peers })
    expect(h.frames).toEqual([])
    h.clock.advance(10)
    expect((await h.client.agentsList()).value).not.toHaveProperty('sync')
    expect((await h.client.resourcesList()).value).not.toHaveProperty('sync')
  })

  it('loading withholds first replies indefinitely and close cancels work', async () => {
    const h = setup(base().with({ roster: 'loading' }))
    const stream = await h.connect()
    stream.subscribe('agents', 'agents')
    h.clock.advance(0)
    let replied = false
    const pending = h.client.agentsList().then(() => { replied = true }, (error: unknown) => error)
    await Promise.resolve()
    await Promise.resolve()
    h.clock.advance(100_000)
    expect(replied).toBe(false)
    expect(h.frames).toEqual([])
    h.replay.close()
    expect(await pending).toMatchObject({ name: 'AbortError' })
    h.clock.advance(100_000)
    expect(h.frames).toEqual([])
  })
})

describe('HTTP reads and actions', () => {
  it('strict-decodes discovery, pages, resources, gets, timeline, screen, errors and actions', async () => {
    const h = setup()
    await h.client.capabilities()
    const reads = [h.client.agentsList(), h.client.machinesList(), h.client.runtimesList(), h.client.missionsList(),
      h.client.workList(), h.client.attentionList(), h.client.messagesList(), h.client.resourcesList(), h.client.terminalsList(), h.client.eventsList()]
    await Promise.all(reads)
    const roster = h.world.slices.roster.state
    await h.client.agentsGet(roster.agents[0]!.id)
    await h.client.runtimesGet(roster.runtimes[0]!.id)
    await h.client.missionsGet(h.world.slices.details.state.missions[0]!.id)
    await h.client.workGet(h.world.slices.details.state.work[0]!.id)
    await h.client.attentionGet(h.world.slices.attention.state.attention[0]!.id)
    await h.client.messagesGet(h.world.slices.attention.state.messages[0]!.id)
    const thread = h.world.slices.conversation.state.threads[0]!
    const page = (await h.client.timelineList(thread.session_id, { limit: 2 })).value
    expect(page.items).toEqual(thread.items.slice(-2))
    if (page.page.has_more) {
      const older = (await h.client.timelineList(thread.session_id, { limit: 2, cursor: page.page.next_cursor! })).value
      expect(older.items).toEqual(thread.items.slice(-4, -2))
    }
    await h.client.terminalScreen(h.world.slices.terminal.state.terminals[0]!.terminal)
    await expect(h.client.agentsGet('agent/example/atlas/absent')).rejects.toMatchObject({ status: 404, response: { code: 'not-found' } })
    const action: ActionRequest = { api_version: 'st3.client.v0', type: 'message.send', id: 'action/scenario-message',
      idempotency_key: 'scenario-message', fence: { snapshot_id: (await h.client.capabilities()).snapshot.id, subject_revisions: {} },
      parameters: { to: roster.agents[0]!.id, content: 'Please review the rename.' } }
    const result = await h.client.submitAction(action)
    expect(result.value.status).toBe('accepted')
    expect(h.replay.actions).toEqual([action])
    const operation = await h.client.followOperation(result.value.operation_id)
    expect(operation.value).toMatchObject({ kind: 'operation', state: 'completed' })
    await h.attach()
    expect(h.replay.actions).toEqual([action])
    expect(h.replies.length).toBeGreaterThan(20)
    expect(h.replay.served()).toEqual(new Set(['roster', 'details', 'attention', 'conversation', 'terminal', 'sync']))
  })

  it('pages within max_page_items without duplicates and reflects current-store snapshots', async () => {
    const world = base()
    const original = world.slices.roster
    const template = original.state.agents[0]!
    const agents = Array.from({ length: 1_000 }, (_, index) => ({
      ...template, id: `agent/example/atlas/worker-${index}`, name: `Atlas Worker ${index}`,
    }))
    const roster: Slice<'roster'> = { ...original, timeline: [], state: { ...original.state, agents, order: agents.map((agent) => agent.id) } }
    const h = setup(world.with({ roster }))
    const items: string[] = []
    let cursor: string | undefined
    do {
      const page = await h.client.agentsList({ limit: 37, ...(cursor === undefined ? {} : { cursor }) })
      expect(page.value.items.length).toBeLessThanOrEqual(37)
      expect(page.snapshot.store_index).toBe(1)
      items.push(...page.value.items.map((item) => item.id))
      cursor = page.value.page.next_cursor ?? undefined
    } while (cursor !== undefined)
    expect(items).toEqual(h.world.slices.roster.state.order)
    expect(new Set(items).size).toBe(items.length)
    await expect(h.replay.fetch('http://scenario.invalid/v1/client/agents?limit=0').then(async (response) => {
      expect(response.status).toBe(400)
      decodeUnknownSync(ErrorEnvelope, 'strict')(await response.json())
    })).resolves.toBeUndefined()
  })
})
