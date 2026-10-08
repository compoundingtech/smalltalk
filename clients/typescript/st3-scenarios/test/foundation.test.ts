import { afterEach, describe, expect, it } from 'vitest'
import type { Session } from '@smalltalk/st3-client'
import { ANCHOR_MS, agent, buildWorld, foldSlice, genericVariants, loadWorld, terminalRecord, terminalRun, wireValues, type RawResource, type Slice, type SyncEvent, type TerminalRecord, type WorldDefinition } from '../src/index.ts'
import { drawCast } from '../src/kit/cast.ts'
import { rngFromSeed } from '../src/kit/rng.ts'
import { entry, thread } from '../src/kit/factories/turn.ts'
import { timeContext } from '../src/kit/time.ts'
import type { FactoryContext } from '../src/kit/context.ts'
import { createReplay, manualClock, type Replay } from '../src/replay/index.ts'
import { decodeSlice } from '../scripts/decode.ts'
import { sliceFile, sliceTimes } from '../scripts/emit.ts'

const replays: Replay[] = []
afterEach(() => { for (const replay of replays.splice(0)) replay.close() })
const base = () => loadWorld('fleet-mid-refactor', { now: ANCHOR_MS })
const setup = (timeline: SyncEvent[] = []) => {
  const original = base()
  const world = original.with({ sync: { ...original.slices.sync, timeline } })
  const clock = manualClock(world.now)
  const replay = createReplay(world, { clock })
  replays.push(replay)
  return { world, replay, clock }
}
const context = (): FactoryContext => { const world = base(); return { cast: world.cast, world: world.id, rng: rngFromSeed(1), t: timeContext(world.now) } }
const attachCapability = async (replay: Replay, record: TerminalRecord): Promise<string> => {
  const response = await replay.fetch('http://scenario.invalid/v1/client/actions', { method: 'POST', body: JSON.stringify({
    id: 'action/foundation-review', type: 'terminal.attach', parameters: { target_id: record.terminal },
    fence: { runtime_incarnation: record.incarnation },
  }) })
  expect(response.status).toBe(200)
  const body = await response.json() as { value: { terminal_attachment: { stream_capability: string } } }
  return body.value.terminal_attachment.stream_capability
}

const emptyDefinition: WorldDefinition = {
  id: 'foundation-empty', title: 'Empty contract test', narrative: 'Empty contract test', seed: 1,
  cast: { project: 'atlas', roles: ['builder'], hosts: 1, people: 1, missions: [] },
  slices: (ctx) => {
    const world = base()
    return {
      roster: { ...world.slices.roster, state: { agents: [], runtimes: [], machines: [], order: [] }, timeline: [] },
      details: { ...world.slices.details, state: { missions: [], work: [] }, timeline: [] },
      attention: { ...world.slices.attention, state: { attention: [], messages: [] }, timeline: [] },
      conversation: { ...world.slices.conversation, state: { threads: [] }, timeline: [] },
      terminal: { ...world.slices.terminal, state: { terminals: [] }, timeline: [] },
      sync: { ...world.slices.sync, timeline: [], state: { ...world.slices.sync.state, expected: [] } },
    }
  },
}

describe('foundation contracts', () => {
  it('introduces and removes full conversation and terminal records after offset zero', async () => {
    const original = base()
    const conversation = original.slices.conversation.state.threads[0]!
    const terminal = original.slices.terminal.state.terminals[0]!
    const world = original.with({
      conversation: { ...original.slices.conversation, state: { threads: [] }, timeline: [
        { _tag: 'thread-create', at_ms: 1_000, store: 0, thread: conversation },
        { _tag: 'thread-remove', at_ms: 2_000, store: 0, agent: conversation.agent },
      ] },
      terminal: { ...original.slices.terminal, state: { terminals: [] }, timeline: [
        { _tag: 'terminal-create', at_ms: 1_000, store: 0, record: terminal },
        { _tag: 'terminal-remove', at_ms: 2_000, store: 0, terminal: terminal.terminal },
      ] },
    })
    expect(foldSlice(world.slices.conversation, 0).state.threads).toEqual([])
    expect(foldSlice(world.slices.terminal, 0).state.terminals).toEqual([])
    expect(foldSlice(world.slices.conversation, 1_000).state.threads).toEqual([conversation])
    expect(foldSlice(world.slices.terminal, 1_000).state.terminals).toEqual([terminal])
    expect(world.slices.conversation.timeline[0]!.store).toBeGreaterThan(1)
    expect(world.slices.terminal.timeline[0]!.store).toBeGreaterThan(world.slices.conversation.timeline[0]!.store)
    expect(wireValues(world.slices.terminal).some((value) => value.pointer === '/timeline/0/record/runtime')).toBe(true)
    expect(sliceTimes(world.slices.terminal).some((value) => value.pointer === '/timeline/0/record/runtime/updated_at')).toBe(true)
    expect(decodeSlice(world.id, world.slices.conversation)).toEqual([])
    expect(decodeSlice(world.id, world.slices.terminal)).toEqual([])
    const clock = manualClock(world.now)
    const replay = createReplay(world, { clock })
    replays.push(replay)
    const timelineUrl = `http://scenario.invalid/v1/client/sessions/${encodeURIComponent(conversation.session_id)}/timeline`
    const runtimeUrl = `http://scenario.invalid/v1/client/runtimes/${encodeURIComponent(terminal.runtime.id)}`
    expect((await replay.fetch(timelineUrl)).status).toBe(404)
    expect((await replay.fetch(runtimeUrl)).status).toBe(404)
    clock.advance(1_000)
    expect((await replay.fetch(timelineUrl)).status).toBe(200)
    expect((await replay.fetch(runtimeUrl)).status).toBe(200)
    const action = { id: 'action/foundation-attach', type: 'terminal.attach', idempotency_key: 'foundation-attach',
      parameters: { target_id: terminal.terminal }, fence: { snapshot_id: 'snapshot/foundation', subject_revisions: {}, runtime_incarnation: terminal.incarnation } }
    const attach = await replay.fetch('http://scenario.invalid/v1/client/actions', { method: 'POST', body: JSON.stringify(action) })
    const attached = await attach.json() as { value: { terminal_attachment: { stream_capability: string } } }
    const frames: unknown[] = []
    const socket = replay.socket('ws://scenario.invalid/v1/client/collections', ['st3.client.collections.v0'], {})
    socket.onmessage = ({ data }) => frames.push(JSON.parse(String(data)))
    clock.advance(0)
    socket.send(JSON.stringify({ kind: 'subscribe', collection: 'conversation', id: 'conversation', conversation: conversation.agent }))
    socket.send(JSON.stringify({ kind: 'subscribe', collection: 'terminal', id: 'terminal', terminal: terminal.terminal, incarnation: terminal.incarnation, capability: attached.value.terminal_attachment.stream_capability }))
    expect(frames).toEqual(expect.arrayContaining([expect.objectContaining({ kind: 'conversation', session_id: conversation.session_id }), expect.objectContaining({ kind: 'screen' })]))
    clock.advance(1_000)
    expect(foldSlice(world.slices.conversation, 2_000).state.threads).toEqual([])
    expect(foldSlice(world.slices.terminal, 2_000).state.terminals).toEqual([])
    expect(frames).toEqual(expect.arrayContaining([expect.objectContaining({ kind: 'error', id: 'conversation', code: 'not-found' }), expect.objectContaining({ kind: 'error', id: 'terminal', code: 'not-found' })]))
  })

  it('checks declared root-enum failures individually and rejects planted undeclared contamination', () => {
    const world = base()
    const known = world.slices.roster.state.agents[0]!
    const value = { ...known, state: 'future-agent-state', future_field: { opaque: true } }
    const unknown = [{ pointer: '/state/agents/0/state', known_value: known.state }, { pointer: '/state/agents/0/future_field' }]
    const slice: Slice<'roster'> = { ...world.slices.roster, timeline: [], decode: 'tolerant', unknown, state: { ...world.slices.roster.state, agents: [value] } }
    expect(decodeSlice(world.id, slice)).toEqual([])
    const planted = { ...value, reachability: 'future-reachability' }
    expect(decodeSlice(world.id, { ...slice, state: { ...slice.state, agents: [planted] } })).not.toEqual([])
    expect(sliceFile(world, slice).unknown).toEqual(unknown)
    expect(decodeSlice(world.id, { ...slice, unknown: [{ pointer: '/state/agents/0/name', known_value: known.name }], state: { ...slice.state, agents: [known] } })).toEqual([expect.objectContaining({ pointer: '/state/agents/0/name' })])
    expect(decodeSlice(world.id, { ...slice, unknown: [{ pointer: '/state/no-such-value' }] })).not.toEqual([])
  })

  it('folds future resource kinds separately and delivers their original encoded values', async () => {
    const original = base()
    const known = original.slices.roster.state.agents[0]!
    const raw: RawResource = { id: 'agent/example/atlas/future', kind: 'future-resource', revision: '1', updated_at: known.updated_at, payload: { future: true } }
    const roster: Slice<'roster'> = { ...original.slices.roster, decode: 'tolerant', unknown: [{ pointer: '/timeline/0/upserts/0', known_value: known }], timeline: [{ _tag: 'changes', at_ms: 1_000, store: 0, upserts: [raw], removes: [] }] }
    expect(decodeSlice(original.id, roster)).toEqual([])
    expect(sliceTimes(roster)).toContainEqual({ pointer: '/timeline/0/upserts/0/updated_at', codec: 'timestamp' })
    expect(foldSlice(roster, 1_000).state.resources).toEqual([raw])
    expect(foldSlice(roster, 1_000).state.agents).toEqual(original.slices.roster.state.agents)
    const world = original.with({ roster })
    const clock = manualClock(world.now)
    const replay = createReplay(world, { clock })
    replays.push(replay)
    const frames: unknown[] = []
    const socket = replay.socket('ws://scenario.invalid/v1/client/collections', ['st3.client.collections.v0'], {})
    socket.onmessage = ({ data }) => frames.push(JSON.parse(String(data)))
    clock.advance(0)
    socket.send(JSON.stringify({ kind: 'subscribe', collection: 'agents', id: 'agents' }))
    clock.advance(1_000)
    expect(frames).toEqual(expect.arrayContaining([expect.objectContaining({ kind: 'changes', upserts: [raw] })]))
    const response = await replay.fetch('http://scenario.invalid/v1/client/resources?kind=future-resource')
    const body = await response.json() as { value: { items: { facts: unknown }[] } }
    expect(body.value.items.map((item) => item.facts)).toEqual([raw])
    const removed = { ...roster, timeline: [...roster.timeline, { _tag: 'changes' as const, at_ms: 2_000, store: 0, upserts: [], removes: [raw.id] }] }
    expect(foldSlice(removed, 2_000).state.resources).toEqual([])
  })

  it.each([2, 'all'] as const)('fails %s socket opens until exhausted or explicitly cleared', (opens) => {
    const { replay, clock } = setup([{ _tag: 'open-fail', at_ms: 0, store: 0, opens }, { _tag: 'open-ok', at_ms: 1_000, store: 0 }])
    const outcomes: string[] = []
    for (let index = 0; index < 3; index += 1) {
      const socket = replay.socket('ws://scenario.invalid/v1/client/collections', [], {})
      socket.onerror = () => outcomes.push('error')
      socket.onclose = (event) => outcomes.push(`close:${event.code}`)
      socket.onopen = () => outcomes.push('open')
      clock.advance(0)
    }
    expect(outcomes).toEqual(opens === 2 ? ['error', 'close:1006', 'error', 'close:1006', 'open'] : ['error', 'close:1006', 'error', 'close:1006', 'error', 'close:1006'])
    clock.advance(1_000)
    const socket = replay.socket('ws://scenario.invalid/v1/client/collections', [], {})
    socket.onopen = () => outcomes.push('recovered')
    clock.advance(0)
    expect(outcomes.at(-1)).toBe('recovered')
  })

  it('keeps socket opens CONNECTING until open-release resumes them', () => {
    const { replay, clock } = setup([
      { _tag: 'open-hold', at_ms: 0, store: 0 },
      { _tag: 'open-release', at_ms: 1_000, store: 0 },
    ])
    const outcomes: string[] = []
    const socket = replay.socket('ws://scenario.invalid/v1/client/collections', [], {})
    socket.onopen = () => outcomes.push('open')
    socket.onerror = () => outcomes.push('error')
    socket.onclose = () => outcomes.push('close')
    socket.onmessage = () => outcomes.push('message')
    clock.advance(0)
    socket.send(JSON.stringify({ kind: 'subscribe', collection: 'agents', id: 'held' }))
    clock.advance(999)
    expect(outcomes).toEqual([])
    clock.advance(1)
    expect(outcomes).toEqual(['open'])
    socket.send(JSON.stringify({ kind: 'subscribe', collection: 'agents', id: 'released' }))
    expect(outcomes).toEqual(['open', 'message'])
  })

  it('does not reopen a pending socket closed before release', () => {
    const { replay, clock } = setup([
      { _tag: 'open-hold', at_ms: 0, store: 0 },
      { _tag: 'open-release', at_ms: 1_000, store: 0 },
    ])
    const outcomes: string[] = []
    const socket = replay.socket('ws://scenario.invalid/v1/client/collections', [], {})
    socket.onopen = () => outcomes.push('open')
    socket.onclose = () => outcomes.push('close')
    clock.advance(0)
    socket.close()
    clock.advance(1_000)
    expect(outcomes).toEqual(['close'])
  })

  it('repeats the legacy unqualified uncoded error on every resubscribe until cleared', () => {
    const { replay, clock } = setup([{ _tag: 'error', at_ms: 0, store: 0, message: 'invalid subscription or subscription limit exceeded', retryable: false, repeat: true }, { _tag: 'error-clear', at_ms: 1_000, store: 0 }])
    const frames: unknown[] = []
    const socket = replay.socket('ws://scenario.invalid/v1/client/collections', [], {})
    socket.onmessage = ({ data }) => frames.push(JSON.parse(String(data)))
    clock.advance(0)
    for (let index = 0; index < 3; index += 1) socket.send(JSON.stringify({ kind: 'subscribe', collection: 'agents', id: `follow-${index}` }))
    expect(frames).toEqual(Array.from({ length: 3 }, () => ({ kind: 'error', message: 'invalid subscription or subscription limit exceeded', retryable: false })))
    clock.advance(1_000)
    socket.send(JSON.stringify({ kind: 'subscribe', collection: 'agents', id: 'recovered' }))
    expect(frames.at(-1)).toEqual(expect.objectContaining({ kind: 'snapshot' }))
  })

  it('qualifies older-page faults by cursor and query without poisoning the first page', async () => {
    const { replay, clock } = setup([{ _tag: 'http-error', at_ms: 0, store: 0, route: 'timeline', when: { cursor: 'present', query: { limit: '2' } }, status: 410,
      envelope: { api_version: 'st3.client.v0', error_version: 'st3.client.error.v0', request_id: 'request/foundation', code: 'page-cursor-expired', message: 'Older page expired', retryable: false, details: {} } }, { _tag: 'http-ok', at_ms: 1_000, store: 0, route: 'timeline', when: { cursor: 'present', query: { limit: '2' } } }])
    const session = base().slices.conversation.state.threads[0]!.session_id
    const url = `http://scenario.invalid/v1/client/sessions/${encodeURIComponent(session)}/timeline`
    expect((await replay.fetch(`${url}?limit=2`)).status).toBe(200)
    expect((await replay.fetch(`${url}?limit=3&cursor=scenario-cursor/2`)).status).toBe(200)
    expect((await replay.fetch(`${url}?limit=2&cursor=scenario-cursor/2`)).status).toBe(410)
    clock.advance(1_000)
    expect((await replay.fetch(`${url}?limit=2&cursor=scenario-cursor/2`)).status).toBe(200)
  })

  it('keeps attach and subscribe working when only the advertised capability is omitted', async () => {
    const original = base()
    const world = original.with({ sync: { ...original.slices.sync, state: { ...original.slices.sync.state, capabilities: { ...original.slices.sync.state.capabilities, capabilities: original.slices.sync.state.capabilities.capabilities.filter((entry) => entry.id !== 'terminal.attach') } } } })
    const clock = manualClock(world.now)
    const replay = createReplay(world, { clock })
    replays.push(replay)
    const terminal = world.slices.terminal.state.terminals[0]!
    const response = await replay.fetch('http://scenario.invalid/v1/client/actions', { method: 'POST', body: JSON.stringify({ id: 'action/foundation', type: 'terminal.attach', parameters: { target_id: terminal.terminal }, fence: { runtime_incarnation: terminal.incarnation } }) })
    expect(response.status).toBe(200)
    const body = await response.json() as { value: { terminal_attachment: { stream_capability: string } } }
    const frames: unknown[] = []
    const socket = replay.socket('ws://scenario.invalid/v1/client/collections', [], {})
    socket.onmessage = ({ data }) => frames.push(JSON.parse(String(data)))
    clock.advance(0)
    socket.send(JSON.stringify({ kind: 'subscribe', collection: 'terminal', id: 'terminal', terminal: terminal.terminal, incarnation: terminal.incarnation, capability: body.value.terminal_attachment.stream_capability }))
    expect(frames).toEqual([expect.objectContaining({ kind: 'screen' })])
  })

  it('uses stable unique agent keys independently of role, including generated entry ids', () => {
    const spec = { project: 'atlas', agents: [{ key: 'builder-1', role: 'builder' as const, name: 'Example display', workspace: '~/src/atlas/a/very/long/worktree', branch: 'example/branch' }, { key: 'builder-2', role: 'builder' as const }], hosts: 1, people: 1, missions: [] }
    const cast = drawCast(rngFromSeed(1), spec)
    expect(cast.agents.map((member) => member.id)).toEqual(['agent/example/atlas/builder-1', 'agent/example/atlas/builder-2'])
    expect(cast.agents[0]).toMatchObject({ key: 'builder-1', role: 'builder', name: 'Example display', workspace: spec.agents[0]!.workspace, branch: 'example/branch' })
    const ctx = { ...context(), cast }
    const entries = cast.agents.map((member) => entry(ctx, thread(member), { atMs: 0, role: 'assistant', type: 'content', body: { media_type: 'text/plain', text: 'Example' } }))
    expect(new Set(entries.map((item) => item.id)).size).toBe(2)
    expect(() => drawCast(rngFromSeed(1), { ...spec, agents: [spec.agents[0]!, spec.agents[0]!] })).toThrow('unique')
    expect(() => drawCast(rngFromSeed(1), { ...spec, agents: [{ key: '../bad', role: 'builder' }] })).toThrow('public slug')
    expect(() => drawCast(rngFromSeed(1), { ...spec, agents: [{ key: 'builder', role: 'builder', workspace: '~/src/foreign/path' }] })).toThrow('public project')
  })

  it('builds all existing variants over empty default slices using only the declared cast', () => {
    const world = buildWorld(emptyDefinition, genericVariants, ANCHOR_MS)
    for (const variant of ['running', 'unavailable', 'exited', 'restarted']) {
      const slice = world.with({ terminal: variant }).slices.terminal
      expect(slice.state.terminals[0]?.owner).toBe(world.cast.agents[0]!.id)
      expect(decodeSlice(world.id, slice)).toEqual([])
    }
    expect(world.with({ roster: 'one-agent' }).slices.roster.state.agents).toHaveLength(1)
    expect(world.with({ conversation: 'empty' }).slices.conversation.state.threads[0]?.agent).toBe(world.cast.agents[0]!.id)
  })

  it('creates desired agents without a running runtime', () => {
    const ctx = context()
    expect(agent(ctx, ctx.cast.agents[0]!, { state: 'desired', sinceMs: 0 })).toMatchObject({
      agent: { state: 'desired', runtime_ids: [], incarnation_id: null }, runtime: null,
    })
  })

  it('provides the shared terminal record helper and desired agents without runtimes', () => {
    const ctx = context()
    const member = ctx.cast.agents[0]!
    const run = terminalRun(ctx, member, { startedAtMs: -1_000, command: 'echo example', lines: ['example'] })
    const record = terminalRecord(ctx, member, run, -1_000)
    expect(record.screens.every((screen) => screen.at_ms <= 0)).toBe(true)
    expect(record.runtime.owner_id).toBe(member.id)
    expect(agent(ctx, member, { state: 'desired', sinceMs: 0 })).toMatchObject({ agent: { state: 'desired', runtime_ids: [], incarnation_id: null }, runtime: null })
  })

  it.each(['root', 'nested'] as const)('rejects a known-object %s ancestor repair hiding undeclared descendants', (scope) => {
    const original = base()
    const known = original.slices.roster.state.agents[0]!
    if (known.checkout == null) throw new Error('Ancestor-repair fixture requires a checkout')
    const contaminated = scope === 'root'
      ? { ...known, state: 'future-agent-state', reachability: 'future-reachability' }
      : { ...known, checkout: { ...known.checkout, undeclared_future_field: true } }
    const slice: Slice<'roster'> = { ...original.slices.roster, decode: 'tolerant', timeline: [],
      unknown: [{ pointer: scope === 'root' ? '/state/agents/0' : '/state/agents/0/checkout', known_value: scope === 'root' ? known : known.checkout }],
      state: { ...original.slices.roster.state, agents: [contaminated] } }
    expect(decodeSlice(original.id, slice)).not.toEqual([])
  })

  it('refuses a held terminal subscription immediately when membership is removed', async () => {
    const original = base()
    const terminal = original.slices.terminal.state.terminals[0]!
    const world = original.with({
      terminal: { ...original.slices.terminal, timeline: [{ _tag: 'terminal-remove', at_ms: 1_000, store: 0, terminal: terminal.terminal }] },
      sync: { ...original.slices.sync, timeline: [
        { _tag: 'hold', at_ms: 0, store: 0, selector: { collection: 'terminal', terminal: terminal.terminal } },
        { _tag: 'release', at_ms: 2_000, store: 0, selector: { collection: 'terminal', terminal: terminal.terminal } },
      ] },
    })
    const clock = manualClock(world.now)
    const replay = createReplay(world, { clock })
    replays.push(replay)
    const capability = await attachCapability(replay, terminal)
    const frames: unknown[] = []
    const socket = replay.socket('ws://scenario.invalid/v1/client/collections', [], {})
    socket.onmessage = ({ data }) => frames.push(JSON.parse(String(data)))
    clock.advance(0)
    socket.send(JSON.stringify({ kind: 'subscribe', collection: 'terminal', id: 'held-terminal', terminal: terminal.terminal, incarnation: terminal.incarnation, capability }))
    expect(frames).toEqual([])
    clock.advance(1_000)
    expect(frames).toEqual([expect.objectContaining({ kind: 'error', id: 'held-terminal', code: 'not-found', retryable: false })])
    clock.advance(1_000)
    expect(frames).toHaveLength(1)
  })

  it('dispatches thread record replacement to ready agent and previous-session subscriptions', () => {
    const original = base()
    const old = original.slices.conversation.state.threads[0]!
    const replacement = { ...old, session_id: `${old.session_id}-replacement`, items: old.items.slice(0, 2), page_size: 1, has_more: false }
    const delta = old.items[2]!
    const world = original.with({ conversation: { ...original.slices.conversation, timeline: [
      { _tag: 'thread-create', at_ms: 1_000, store: 0, thread: replacement },
      { _tag: 'entries', at_ms: 2_000, store: 0, agent: old.agent, items: [delta] },
    ] } })
    const clock = manualClock(world.now)
    const replay = createReplay(world, { clock })
    replays.push(replay)
    const frames: unknown[] = []
    const socket = replay.socket('ws://scenario.invalid/v1/client/collections', [], {})
    socket.onmessage = ({ data }) => frames.push(JSON.parse(String(data)))
    clock.advance(0)
    socket.send(JSON.stringify({ kind: 'subscribe', collection: 'conversation', id: 'agent', conversation: old.agent }))
    socket.send(JSON.stringify({ kind: 'subscribe', collection: 'conversation', id: 'session', conversation: old.session_id }))
    frames.length = 0
    clock.advance(1_000)
    expect(frames).toEqual(['agent', 'session'].map((id) => expect.objectContaining({
      kind: 'conversation', id, session_id: replacement.session_id, replace: true, items: replacement.items.slice(-1), has_more: true,
    })))
    frames.length = 0
    clock.advance(1_000)
    expect(frames).toEqual(['agent', 'session'].map((id) => expect.objectContaining({
      kind: 'conversation', id, session_id: replacement.session_id, replace: false, items: [delta],
    })))
  })

  it.each(['same', 'new'] as const)('refreshes or invalidates ready terminal subscriptions for a %s incarnation replacement', async (incarnation) => {
    const original = base()
    const terminal = original.slices.terminal.state.terminals[0]!
    const nextIncarnation = incarnation === 'same' ? terminal.incarnation : `${terminal.incarnation}-replacement`
    const screen = { ...terminal.screens.at(-1)!.screen, runtime_incarnation: nextIncarnation, revision: 'replacement-screen' }
    const replacement = { ...terminal, incarnation: nextIncarnation, runtime: { ...terminal.runtime, incarnation_id: nextIncarnation },
      screens: [{ at_ms: 1_000, screen }] }
    const world = original.with({ terminal: { ...original.slices.terminal, timeline: [{ _tag: 'terminal-create', at_ms: 1_000, store: 0, record: replacement }] } })
    const clock = manualClock(world.now)
    const replay = createReplay(world, { clock })
    replays.push(replay)
    const capability = await attachCapability(replay, terminal)
    const frames: unknown[] = []
    const socket = replay.socket('ws://scenario.invalid/v1/client/collections', [], {})
    socket.onmessage = ({ data }) => frames.push(JSON.parse(String(data)))
    clock.advance(0)
    socket.send(JSON.stringify({ kind: 'subscribe', collection: 'terminal', id: 'terminal', terminal: terminal.terminal, incarnation: terminal.incarnation, capability }))
    expect(frames).toHaveLength(1)
    frames.length = 0
    clock.advance(1_000)
    expect(frames).toEqual([expect.objectContaining(incarnation === 'same'
      ? { kind: 'screen', id: 'terminal', value: screen }
      : { kind: 'error', id: 'terminal', code: 'stale-fence', retryable: false })])
  })

  it('rejects a strict-valid known session upsert in a roster rather than storing it as future data', () => {
    const original = base()
    const member = original.cast.agents[0]!
    const session: Session = { kind: 'session', id: member.session, revision: '1', updated_at: new Date(original.now).toISOString(),
      owner_id: member.id, started_at: new Date(original.now).toISOString(), state: 'running', timeline_cursor: 'cursor/foundation' }
    const roster: Slice<'roster'> = { ...original.slices.roster, timeline: [{ _tag: 'changes', at_ms: 1_000, store: 0, upserts: [session], removes: [] }] }
    expect(decodeSlice(original.id, roster)).toEqual([])
    expect(() => foldSlice(roster, 1_000)).toThrow('a session upsert has no place in this slice')
  })

  it('drops discarded unknown declarations when regenerating the empty roster variant', () => {
    const definition: WorldDefinition = { ...emptyDefinition, slices: (ctx) => {
      const own = emptyDefinition.slices(ctx)
      const known = base().slices.roster.state.agents[0]!
      return { ...own, roster: { ...own.roster, decode: 'tolerant',
        unknown: [{ pointer: '/state/agents/0/future_field' }], state: { ...own.roster.state, agents: [{ ...known, future_field: true }] } } }
    } }
    const world = buildWorld(definition, genericVariants, ANCHOR_MS)
    expect(decodeSlice(world.id, world.slices.roster)).toEqual([])
    const cleared = world.with({ roster: 'empty' }).slices.roster
    expect(cleared.unknown).toBeUndefined()
    expect(cleared.decode).toBe('strict')
    expect(decodeSlice(world.id, cleared)).toEqual([])
  })
})
