import { expect, it } from 'vitest'
import { decodeSlice } from '../scripts/decode.ts'
import { fleetMidRefactor } from '../src/worlds/fleetMidRefactor.ts'
import { ciFailingFix } from '../src/worlds/ciFailingFix.ts'
import { firstRunOnboarding } from '../src/worlds/firstRunOnboarding.ts'
import { mergeConflictStandoff } from '../src/worlds/mergeConflictStandoff.ts'
import { catalog, loadWorld } from '../src/index.ts'
import { buildWorld, type World, type WorldDefinition } from '../src/kit/world.ts'
import { genericVariants } from '../src/kit/variants.ts'
import { foldSlice } from '../src/kit/fold.ts'
import { ANCHOR_MS, parseTimestamp } from '../src/kit/time.ts'
import { SLICE_KINDS, type AnySlice, type TimelineEvent } from '../src/kit/slice.ts'
import { wireValues } from '../src/kit/wire.ts'

const expectDeclaredCast = (world: World, value: unknown): void => {
  const identities = new Set([...world.cast.agents.map((member) => member.id), ...world.cast.people.map((person) => person.id), ...world.cast.hosts.map((host) => host.id)])
  const inspect = (item: unknown): void => {
    if (typeof item === 'string' && /^(agent\/|person\/|host\/)/u.test(item)) {
      const identity = item.startsWith('person/') ? item.replace(/\/session\/[^/]+$/u, '') : item
      expect(identities.has(identity), item).toBe(true)
    }
    else if (Array.isArray(item)) item.forEach(inspect)
    else if (item !== null && typeof item === 'object') Object.values(item).forEach(inspect)
  }
  inspect(value)
}

it('checks the declared person owning a session actor rather than requiring the session in the cast', () => {
  const world = loadWorld('fleet-mid-refactor', { now: ANCHOR_MS })
  const person = world.cast.people[0]!
  expectDeclaredCast(world, { session_actor: `${person.id}/session/device-1` })
  expect(() => expectDeclaredCast(world, { session_actor: 'person/avery/session/device-1' })).toThrow()
  expect(() => expectDeclaredCast(world, { owner_id: 'agent/example/foreign/builder' })).toThrow()
})

const empty: WorldDefinition = { ...fleetMidRefactor, id: 'variant-empty', slices: (ctx) => {
  const base = fleetMidRefactor.slices(ctx)
  return { ...base, roster: { ...base.roster, state: { agents: [], runtimes: [], machines: [], order: [] }, timeline: [] }, details: { ...base.details, state: { missions: [], work: [] }, timeline: [] }, attention: { ...base.attention, state: { attention: [], messages: [] }, timeline: [] }, conversation: { ...base.conversation, state: { threads: [] }, timeline: [] }, terminal: { ...base.terminal, state: { terminals: [] }, timeline: [] } }
} }
const contaminated: WorldDefinition = { ...fleetMidRefactor, id: 'variant-unknown', slices: (ctx) => {
  const base = fleetMidRefactor.slices(ctx)
  return Object.fromEntries(Object.entries(base).map(([kind, slice]) => {
    if (kind === 'sync') return [kind, slice]
    const clean = structuredClone(slice)
    const value = wireValues(clean as AnySlice)[0]
    if (value === undefined || value.value === null || typeof value.value !== 'object') throw new Error('contamination fixture requires a wire object')
    Object.assign(value.value, { future_variant_field: true })
    return [kind, { ...clean, decode: 'tolerant', unknown: [{ pointer: `${value.pointer}/future_variant_field` }] }]
  })) as typeof base
} }

const kinds = ['roster', 'details', 'attention', 'conversation', 'terminal'] as const
for (const definition of [fleetMidRefactor, empty, ciFailingFix, firstRunOnboarding, mergeConflictStandoff]) for (const now of [ANCHOR_MS, ANCHOR_MS + 123_456_789]) {
  const world = buildWorld(definition, genericVariants, now)
  for (const kind of kinds) for (const variant of world.available[kind]) it(`${definition.id}/${kind}/${variant}/${now}`, () => {
    const variantWorld = world.with({ [kind]: variant })
    const slice = variantWorld.slices[kind] as AnySlice
    expect(decodeSlice(world.id, slice)).toEqual([])
    expectDeclaredCast(world, slice)
    for (const event of slice.timeline) {
      const folded = foldSlice(slice, event.at_ms)
      expect(decodeSlice(world.id, folded as AnySlice)).toEqual([])
      expectDeclaredCast(world, folded)
    }
    if (kind === 'roster' && variant === 'all-states') {
      const roster = variantWorld.slices.roster
      const states = new Set([roster.state, ...roster.timeline.map((event) => foldSlice(roster, event.at_ms).state)].flatMap((state) => state.agents.map((agent) => agent.state)))
      expect([...states].sort()).toEqual(['desired', 'failed', 'running', 'starting', 'stopped', 'suspended', 'waiting'])
    }
    if (kind === 'details' && (variant === 'stalled' || variant === 'failed')) expect(variantWorld.slices.details.state.work.every((work) => work.state === (variant === 'stalled' ? 'blocked' : 'failed'))).toBe(true)
    if (kind === 'attention' && variant === 'many') expect(variantWorld.slices.attention.state.attention).toHaveLength(50)
    if (kind === 'attention' && variant === 'one-of-each-kind') expect(new Set(variantWorld.slices.attention.state.attention.map((card) => card.attention_kind)).size).toBe(4)
    if (kind === 'conversation') {
      const conversation = variantWorld.slices.conversation
      if (variant === 'long') expect(conversation.state.threads.every((thread) => thread.has_more && thread.items.length > thread.page_size)).toBe(true)
      if (variant === 'streaming') {
        expect(conversation.state.threads.every((thread) => thread.items[0]?.final === false)).toBe(true)
        expect(foldSlice(conversation, 2_000).state.threads.every((thread) => thread.items.length === 1 && thread.items[0]?.revision === 3 && thread.items[0].final)).toBe(true)
      }
      if (variant === 'tool-heavy' || variant === 'failed-tools') expect(conversation.state.threads.every((thread) => thread.items.filter((entry) => entry.type === 'tool_call').length > thread.items.filter((entry) => entry.type === 'tool_result').length)).toBe(true)
      if (variant === 'remote-only-mail') for (const thread of conversation.state.threads) {
        const member = world.cast.agents.find((candidate) => candidate.id === thread.agent)
        if (member === undefined) throw new Error('mail recipient must belong to the declared cast')
        const sender = world.cast.agents.find((candidate) => candidate.id !== member.id && candidate.host.id !== member.host.id)
          ?? world.cast.agents.find((candidate) => candidate.id !== member.id)
          ?? member
        const messages = thread.items.filter((entry) => entry.type === 'message')
        expect(messages).toHaveLength(3)
        for (const message of messages) {
          expect(message.body.from).toBe(sender.id)
          expect(message.body.to).toBe(member.id)
          expect(message.body.tags).toEqual(['mail'])
          expect(message.body.title).toBe(sender.id === member.id ? 'Session-aware migration continuation' : 'Session-aware migration handoff')
        }
      }
    }
    if (kind === 'terminal' && variant !== 'default' && variant !== 'none') expect(variantWorld.slices.terminal.state.terminals).toHaveLength(1)
  })
}

it('removes obsolete unknown declarations from regenerated slices and preserves loading declarations', () => {
  const world = buildWorld(contaminated, genericVariants, ANCHOR_MS)
  for (const kind of kinds) for (const variant of world.available[kind].filter((name) => name !== 'default' && name !== 'loading')) expect(decodeSlice(world.id, world.with({ [kind]: variant }).slices[kind] as AnySlice)).toEqual([])
  for (const kind of ['roster', 'details', 'conversation'] as const) {
    const loading = world.with({ [kind]: 'loading' }).slices[kind]
    expect(loading.unknown).toHaveLength(1)
    expect(decodeSlice(world.id, loading as AnySlice)).toEqual([])
  }
})

for (const { id } of catalog) for (const now of [ANCHOR_MS, ANCHOR_MS + 123_456_789]) it(`generates all declared variants coherently: ${id}/${now}`, () => {
  const base = loadWorld(id, { now })
  for (const kind of SLICE_KINDS) for (const variant of base.available[kind]) {
    const world = base.with({ [kind]: variant })
    const slice = world.slices[kind]
    expect(decodeSlice(id, slice)).toEqual([])
    expectDeclaredCast(world, slice)
    const offsets = slice.timeline.map((event) => event.at_ms)
    expect(offsets).toEqual([...offsets].sort((a, b) => a - b))
    const events = SLICE_KINDS.flatMap<TimelineEvent>((sliceKind) => world.slices[sliceKind].timeline)
      .sort((a, b) => a.at_ms - b.at_ms)
    let store = 1
    for (const event of events) {
      if (['changes', 'thread-create', 'thread-remove', 'terminal-create', 'terminal-remove', 'entries', 'replace', 'incarnation', 'end'].includes(event._tag)) store += 1
      expect(event.store).toBe(store)
    }
    if (kind === 'conversation' && variant !== 'default' && variant !== 'loading') {
      for (const thread of world.slices.conversation.state.threads) {
        const instants = thread.items.map((entry) => parseTimestamp(entry.timestamp))
        expect(instants).toEqual([...instants].sort((a, b) => a - b))
        expect(instants.every((instant) => instant <= now)).toBe(true)
        expect(thread.items.map((entry) => entry.sequence)).toEqual(thread.items.map((_entry, index) => index + 1))
      }
    }
    if (kind === 'terminal' && variant === 'running') {
      const [record] = world.slices.terminal.state.terminals
      if (record === undefined) throw new Error('running terminal requires a declared cast owner')
      expect(parseTimestamp(record.runtime.updated_at)).toBe(now + record.cast.started_at_ms)
      expect(record.screens.every((screen) => screen.at_ms <= 0)).toBe(true)
      expect(record.cast.events.at(-1)?.[2].endsWith('$ ')).toBe(false)
    }
    if (kind === 'sync' && (variant === 'resync-coded' || variant === 'resync-uncoded')) {
      const agent = base.slices.conversation.state.threads[0]?.agent
      const event = world.slices.sync.timeline.find((candidate) => candidate._tag === 'resync')
      if (event?._tag !== 'resync') throw new Error('resync variant must emit a resync frame')
      expect(event.selector).toEqual(agent === undefined ? { collection: 'agents' } : { collection: 'conversation', conversation: agent })
      expect(world.slices.sync.state.expected.at(-1)?.surface).toBe(agent === undefined ? 'agents' : `conversation:${agent}`)
      expect(world.slices.conversation).toEqual(base.slices.conversation)
    }
  }
})
