import { readFileSync } from 'node:fs'
import { describe, expect, it } from 'vitest'
import { catalog, SLICE_KINDS } from '../src/index.ts'
import { ANCHOR_MS } from '../src/kit/time.ts'
import type { World } from '../src/kit/world.ts'
import type { RecordedTransport } from '../src/kit/recording.ts'
import { createReplay, manualClock } from '../src/replay/index.ts'
import { decodeRecording, importRecording, type JsonObject, type RecordingRow } from '../scripts/recordings.ts'
import { loadRecordedProof, PROOF_NAME, RECORDINGS_DIR, type RecordedProof } from './recorded-world.ts'
import { assertRecordingFidelity, jsonObject } from './recording-fidelity.ts'

const oracle = (): RecordingRow[] => decodeRecording(readFileSync(new URL(PROOF_NAME + '.normalized.json', RECORDINGS_DIR), 'utf8'))

/** The predecessor proof uses the same independent replay driver as all seven narratives. */
const assertFidelity = (proof: RecordedProof, expected = oracle()): Promise<void> => assertRecordingFidelity(proof, expected)

const withRecording = (world: World, recording: RecordedTransport): World => ({ ...world, recording })
const mutateItem = (world: World, revision: boolean): World => {
  let changed = false
  const conversation = world.slices.conversation
  const timeline = conversation.timeline.map((event) => {
    if (changed || (event._tag !== 'entries' && event._tag !== 'replace') || event.items.length === 0) return event
    changed = true
    return { ...event, items: event.items.map((item, index) => index !== 0 ? item : revision
      ? { ...item, revision: item.revision + 1 } : { ...item, final: !item.final }) }
  })
  if (!changed) throw new Error('Proof has no mutable conversation event')
  return world.with({ conversation: { ...conversation, timeline } })
}

/** Focused metadata tests still use capture-derived slice data and the captured command. */
const captureMetadataFrames = (proof: RecordedProof, recording: RecordedTransport): JsonObject[] => {
  const clock = manualClock(ANCHOR_MS - 100)
  const replay = createReplay({ ...proof.world, recording }, { clock })
  const frames: JsonObject[] = []
  try {
    const socket = replay.socket('ws://scenario.invalid/v1/client/collections', ['st3.client.collections.v0'], {})
    socket.onmessage = (event) => {
      if (typeof event.data !== 'string') throw new Error('Replay frame is not JSON text')
      const parsed: unknown = JSON.parse(event.data)
      frames.push(jsonObject(parsed))
    }
    clock.advance(0)
    const command = proof.rows.find((row) => row.kind === 'command' && row.value.collection === 'conversation')
    if (command === undefined) throw new Error('Missing captured conversation command')
    socket.send(JSON.stringify(command.value))
    clock.advance(1100)
    return frames
  } finally { replay.close() }
}

describe('recorded transport fidelity', () => {
  it('reconstructs captured frames and full HTTP exchanges at the pinned anchor', async () => {
    await assertFidelity(loadRecordedProof())
  })

  it('orders simultaneous frame boundaries by ordinal even when metadata is reversed', () => {
    const proof = loadRecordedProof()
    const selected = proof.world.recording?.frames.filter((frame) => frame.kind === 'conversation')
      .filter((frame) => frame.event !== undefined).slice(0, 2)
    if (selected === undefined || selected.length !== 2) throw new Error('Missing captured conversation boundaries')
    const expected = selected.map((frame) => {
      if (frame.event === undefined) throw new Error('Missing event reference')
      const event = proof.world.slices.conversation.timeline[frame.event]
      if (event === undefined || (event._tag !== 'entries' && event._tag !== 'replace')) throw new Error('Invalid captured event reference')
      return { kind: 'conversation', replace: frame.replace, items: event.items }
    })
    const frames = captureMetadataFrames(proof, { http: [], frames: selected.map((frame) => ({ ...frame, at_ms: 100 })).reverse() })
    expect(frames).toHaveLength(2)
    for (const [index, expectedFrame] of expected.entries()) expect(frames[index]).toMatchObject(expectedFrame)
  })

  it('preserves an absent retryable field on an optional recorded error boundary', () => {
    const proof = loadRecordedProof()
    const source = proof.world.recording?.frames.find((frame) => frame.kind === 'conversation')
    if (source === undefined) throw new Error('Missing captured selector')
    const frames = captureMetadataFrames(proof, { http: [], frames: [{
      kind: 'error', ordinal: 100, at_ms: 10, selector: source.selector, message: 'Recorded transport error',
    }] })
    expect(frames).toHaveLength(1)
    expect(frames[0]).toMatchObject({ kind: 'error', message: 'Recorded transport error' })
    expect(frames[0]).not.toHaveProperty('retryable')
  })

  it('keeps every slice recorded and excludes the proof from the published catalog', () => {
    const { world } = loadRecordedProof()
    expect(world.id).toBe('recorded-fleet-mid-refactor-proof')
    expect(SLICE_KINDS.map((kind) => world.slices[kind].source._tag)).toEqual(SLICE_KINDS.map(() => 'recorded'))
    expect(catalog).toHaveLength(24)
    expect(catalog.some(({ id }) => id.startsWith('recorded-'))).toBe(false)
  })

  it('preserves imported numeric stores and shared fences through world assembly and overrides', () => {
    const proof = loadRecordedProof()
    const imported = importRecording(proof.script, proof.rows)
    for (const kind of SLICE_KINDS) expect(proof.world.slices[kind].timeline.map((event) => event.store))
      .toEqual(imported.slices[kind].timeline.map((event) => event.store))
    const unchanged = proof.world.with({ conversation: proof.world.slices.conversation })
    for (const kind of SLICE_KINDS) expect(unchanged.slices[kind].timeline.map((event) => event.store))
      .toEqual(proof.world.slices[kind].timeline.map((event) => event.store))
    const screens = proof.world.recording?.frames.filter((frame) => frame.kind === 'screen') ?? []
    expect(screens.length).toBeGreaterThan(1)
    const stores = screens.map((frame) => frame.snapshot.store_index)
    expect(stores).toEqual([...stores].sort((a, b) => a - b))
    const http = proof.world.recording?.http
    // The attach screen/read and action share a fence; final GETs may observe later writes.
    expect(http?.map((entry) => entry.snapshot)).toEqual(imported.recording.http.map((entry) => entry.snapshot))
    expect(http?.[0]?.snapshot).toEqual(http?.[1]?.snapshot)
    const published = http?.find((entry) => entry.value.kind === 'agents')
    expect(published?.snapshot.store_index).toBeLessThan(stores.at(-1)!)
    expect(stores.every((store) => typeof store === 'number')).toBe(true)
  })

  for (const mutation of ['item', 'revision', 'has_more', 'boundary', 'status', 'store'] as const) {
    it('rejects an independent ' + mutation + ' mutation without altering the oracle', async () => {
      const proof = loadRecordedProof()
      const before = readFileSync(new URL(PROOF_NAME + '.normalized.json', RECORDINGS_DIR), 'utf8')
      const recording = proof.world.recording
      if (recording === undefined) throw new Error('Missing recording metadata')
      await assertFidelity(proof)
      let world = proof.world
      switch (mutation) {
        case 'item': world = mutateItem(world, false); break
        case 'revision': world = mutateItem(world, true); break
        case 'has_more': world = withRecording(world, { ...recording, frames: recording.frames.map((frame) => frame.kind === 'conversation' ? { ...frame, has_more: !frame.has_more } : frame) }); break
        case 'boundary': world = withRecording(world, { ...recording, frames: recording.frames.slice(1) }); break
        case 'status': world = withRecording(world, { ...recording, http: recording.http.map((http, index) => index === 0 ? { ...http, status: 201 } : http) }); break
        case 'store': world = withRecording(world, { ...recording, frames: recording.frames.map((frame) => frame.kind === 'screen' ? { ...frame, snapshot: { ...frame.snapshot, store_index: frame.snapshot.store_index + 1 } } : frame) }); break
      }
      await expect(assertFidelity({ ...proof, world })).rejects.toThrow()
      expect(readFileSync(new URL(PROOF_NAME + '.normalized.json', RECORDINGS_DIR), 'utf8')).toBe(before)
    })
  }
})
