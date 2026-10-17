import { readFileSync } from 'node:fs'
import { describe, expect, it } from 'vitest'
import { catalog, SLICE_KINDS } from '../src/index.ts'
import { decodeRecording } from '../scripts/recordings.ts'
import { loadRecordedWorld, RECORDINGS_DIR } from './recorded-world.ts'

const narratives = [
  'fleet-mid-refactor', 'ci-failing-fix', 'long-debug-rabbit-hole',
  'first-run-onboarding', 'merge-conflict-standoff', 'offline-reconnect-storm',
  'release-candidate-polish',
]

describe('narrative recording inputs', () => {
  for (const name of narratives) it('imports capture-derived test-only ' + name, () => {
    const { world, script, rows } = loadRecordedWorld(name)
    expect(script.format_version).toBe(2)
    expect(world.id).toBe('recorded-' + name)
    expect(SLICE_KINDS.map((kind) => world.slices[kind].source._tag)).toEqual(SLICE_KINDS.map(() => 'recorded'))
    const oracle = decodeRecording(readFileSync(new URL(name + '.normalized.json', RECORDINGS_DIR), 'utf8'))
    expect(rows).toEqual(oracle)
    expect(rows.some((row) => row.kind === 'frame' && row.value.kind === 'conversation')).toBe(true)
    expect(rows.some((row) => row.kind === 'frame' && row.value.kind === 'screen')).toBe(true)
    expect(catalog.some((entry) => entry.id === world.id)).toBe(false)
  })

  it('keeps onboarding baseline empty and imports real future creation', () => {
    const { world } = loadRecordedWorld('first-run-onboarding')
    expect(world.slices.conversation.state.threads).toEqual([])
    expect(world.slices.terminal.state.terminals).toEqual([])
    expect(world.slices.conversation.timeline.some((event) => event._tag === 'thread-create')).toBe(true)
    expect(world.slices.terminal.timeline.some((event) => event._tag === 'terminal-create')).toBe(true)
  })

  it('keeps release conversation and terminal activity historical', () => {
    const { world, script } = loadRecordedWorld('release-candidate-polish')
    expect(script.historical_turns).toBe(script.turns.length)
    expect(world.slices.conversation.state.threads.length).toBeGreaterThan(0)
    expect(world.slices.conversation.timeline).toEqual([])
    expect(world.slices.terminal.timeline).toEqual([])
  })

  it('records three reconnect replacement cycles independently of synthetic sync timing', () => {
    const { script, rows } = loadRecordedWorld('offline-reconnect-storm')
    expect(script.reconnect_after).toHaveLength(3)
    const baseline = rows.find((row) => row.kind === 'baseline')
    const baselineSocket = rows.find((row) => row.ordinal === baseline?.value.last_pre_timeline_ordinal)?.socket_id
    if (baselineSocket === undefined || baselineSocket === null) throw new Error('Missing recorded baseline socket')
    const replacements = rows.filter((row) => row.kind === 'frame' && row.value.kind === 'conversation' && row.value.replace === true &&
      (row.receive_ms >= 0 || row.socket_id === baselineSocket))
    const sockets = new Set(replacements.map((row) => row.socket_id))
    expect(sockets.size).toBe(4)
    expect(catalog).toHaveLength(24)
  })

  it('retains the forty-minute hunt, rejected hypotheses and real older-page walk', () => {
    const { world, rows } = loadRecordedWorld('long-debug-rabbit-hole')
    const entries = world.slices.conversation.state.threads.flatMap((thread) => thread.items)
    expect(new Set(entries.filter((entry) => entry.id.startsWith('timeline-entry/')).map((entry) => entry.id)).size).toBeGreaterThan(50)
    const captured = JSON.stringify(rows.filter((row) => row.kind === 'frame' || row.kind === 'http'))
    for (const phrase of ['Wrong hypothesis 1', 'Wrong hypothesis 2', 'Wrong hypothesis 3', 'Rejected hypothesis 1', 'Rejected hypothesis 2', 'Rejected hypothesis 3', 'Forty minutes of evidence:', 'new AbortController', '100 shuffled orders pass']) expect(captured).toContain(phrase)
    const pages = rows.filter((row) => row.kind === 'http' && typeof row.value.path === 'string' && row.value.path.endsWith('/timeline'))
    expect(pages.length).toBeGreaterThan(1)
    expect(pages.some((row) => typeof row.value.query === 'string' && row.value.query.includes('cursor='))).toBe(true)
  })
})
