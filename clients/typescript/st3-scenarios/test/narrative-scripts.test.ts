import { describe, expect, it } from 'vitest'
import { ANCHOR_MS, loadWorld } from '../src/index.ts'
import { NARRATIVE_RECORDINGS, narrativeScript } from '../scripts/narrative-scripts.ts'
import { decodeScript } from '../scripts/recordings.ts'

describe('generated narrative recording scripts', () => {
  it.each(NARRATIVE_RECORDINGS)('%s derives its cast without changing host ownership', (id) => {
    const script = narrativeScript(id)
    const world = loadWorld(id, { now: ANCHOR_MS })
    expect(decodeScript(script)).toEqual(script)
    expect(script.format_version).toBe(2)
    expect(script.agents).toEqual(world.cast.agents.filter((a) => a.host.id === script.host).map((a) => ({ agent: a.id, host: a.host.id })))
    expect(script.turns.length).toBeGreaterThan(0)
    expect(script.turns.some((t) => t.command !== undefined)).toBe(true)
    expect(script.turns.every((t) => t.command === undefined || t.agent === script.agent)).toBe(true)
    expect(narrativeScript(id)).toEqual(script)
  })
  it('keeps the hunt historical and the release entirely completed', () => {
    const hunt = narrativeScript('long-debug-rabbit-hole')
    expect(Date.parse(hunt.turns[0]!.timestamp)).toBe(ANCHOR_MS - 40 * 60_000)
    expect(hunt.historical_turns).toBe(19)
    expect(hunt.turns).toHaveLength(22)
    expect(hunt.walk_pages).toBe(true)
    const story = JSON.stringify(hunt)
    for (const phrase of ['network retry latency', 'fake timers', 'stale search cache', 'new AbortController']) expect(story).toContain(phrase)
    const release = narrativeScript('release-candidate-polish')
    expect(release.historical_turns).toBe(release.turns.length)
    expect(release.turns.some((t) => t.agent !== release.agent)).toBe(true)
    expect(JSON.stringify(release)).toContain('6/6 checks passed')
  })
  it('records empty onboarding and three distinct future reconnect points', () => {
    expect(narrativeScript('first-run-onboarding').empty_baseline).toBe(true)
    const offline = narrativeScript('offline-reconnect-storm')
    expect(offline.reconnect_after).toHaveLength(3)
    expect(new Set(offline.reconnect_after).size).toBe(3)
    expect(offline.turns.some((t) => t.revisions?.length)).toBe(true)
  })
  it('carries the exact shared three-way merge through a native tool', () => {
    const merge = narrativeScript('merge-conflict-standoff')
    expect(merge.agents).toHaveLength(2)
    const conflicts = merge.turns.flatMap((t) => t.tool?.name === 'shell' ? [t.tool.input] : []).filter((input) => input !== null && typeof input === 'object' && !Array.isArray(input) && 'base' in input)
    expect(conflicts).toHaveLength(2)
    expect(conflicts[0]).toEqual(conflicts[1])
    expect(JSON.stringify(merge)).toContain('ttl: 300, stale: true')
  })
  it('rejects foreign ownership, secondary terminal commands and invalid indices', () => {
    const script = narrativeScript('merge-conflict-standoff')
    expect(() => decodeScript({ ...script, agents: [{ agent: 'agent/example/harbor/reviewer', host: 'host/orchard' }] })).toThrow('primary host')
    expect(() => decodeScript({ ...script, turns: [{ ...script.turns[0], agent: script.agents?.[1]?.agent, command: 'pwd' }] })).toThrow('primary agent')
    expect(() => decodeScript({ ...script, historical_turns: -1 })).toThrow('nonnegative')
    expect(() => decodeScript({ ...script, historical_turns: script.turns.length + 1 })).toThrow('turn count')
    expect(() => decodeScript({ ...script, reconnect_after: [script.turns.length] })).toThrow('future turn index')
    expect(() => decodeScript({ ...script, turns: [{ ...script.turns[0], tool: { id: 'tool-call/example-negative', name: 'edit', input: null } }] })).toThrow('required JSON')
  })
})
