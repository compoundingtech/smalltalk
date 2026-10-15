import { readFileSync } from 'node:fs'
import { describe, expect, it } from 'vitest'
import { SLICE_KINDS } from '../src/kit/slice.ts'
import { decodeRecording, importRecording, normalizeRecording } from '../scripts/recordings.ts'
import { importNormalizedRecording, rebaseImportedSlices } from '../src/kit/importRecording.ts'
import { ANCHOR, ANCHOR_MS, rebase } from '../src/kit/time.ts'
import { assertRecordingFidelity } from './recording-fidelity.ts'
import { loadRecordedWorld, NARRATIVE_RECORDING_NAMES, RECORDINGS_DIR } from './recorded-world.ts'

it('rebases shared historical/state/event leaves once while preserving aliases and anchor input', () => {
  const shared = { timestamp: ANCHOR, epoch_ms: ANCHOR_MS, text: ANCHOR }
  const source = { state: { entry: shared }, history: [{ entry: shared }], timeline: [{ entry: shared }] }
  const now = ANCHOR_MS + 1_234_567
  const rebased = rebase(source, [
    { pointer: '/state/entry/timestamp', codec: 'timestamp' },
    { pointer: '/state/entry/epoch_ms', codec: 'epoch-ms' },
    { pointer: '/history/0/entry/timestamp', codec: 'timestamp' },
    { pointer: '/history/0/entry/epoch_ms', codec: 'epoch-ms' },
    { pointer: '/timeline/0/entry/timestamp', codec: 'timestamp' },
    { pointer: '/timeline/0/entry/epoch_ms', codec: 'epoch-ms' },
  ], ANCHOR, now)
  expect(rebased.state.entry).toEqual({ timestamp: new Date(now).toISOString(), epoch_ms: now, text: ANCHOR })
  expect(rebased.history[0]?.entry).toBe(rebased.state.entry)
  expect(rebased.timeline[0]?.entry).toBe(rebased.state.entry)
  expect(rebased.state.entry).not.toBe(shared)
  expect(shared).toEqual({ timestamp: ANCHOR, epoch_ms: ANCHOR_MS, text: ANCHOR })
})

for (const name of NARRATIVE_RECORDING_NAMES) {
  describe(name + ' independent recorded replay', () => {
    const proof = loadRecordedWorld(name)
    const oracleBytes = readFileSync(new URL(name + '.normalized.json', RECORDINGS_DIR), 'utf8')
    const oracle = decodeRecording(oracleBytes)

    it('reconstructs every historical/future socket frame and full HTTP exchange', async () => {
      expect(SLICE_KINDS.map((kind) => proof.world.slices[kind].source._tag)).toEqual(SLICE_KINDS.map(() => 'recorded'))
      expect(proof.world.recording?.socket_ids?.length).toBeGreaterThan(0)
      expect(oracle.some((row) => row.kind === 'frame' && row.receive_ms < 0)).toBe(true)
      await assertRecordingFidelity(proof, oracle)
    }, 30_000)

    it('rebases every source-backed slice value through the Effect-free JSON importer', () => {
      const now = ANCHOR_MS + 1_234_567
      const raw = decodeRecording(readFileSync(new URL(name + '.recording.jsonl', RECORDINGS_DIR), 'utf8'))
      const expected = importRecording(proof.script, normalizeRecording(proof.script, raw, now))
      expect(importNormalizedRecording(proof.script, oracle, now)).toEqual(expected)
      expect(rebaseImportedSlices(proof.world.slices, now)).toEqual(expected.slices)
    }, 30_000)

    for (const mutation of ['drop', 'reorder', 'alter', 'frame-reorder', 'http-status'] as const) {
      it('rejects an independent ' + mutation + ' mutation with the oracle untouched', async () => {
        await assertRecordingFidelity(proof, oracle)
        const recording = proof.world.recording
        if (recording === undefined) throw new Error('Missing captured transport')
        let world = proof.world
        if (mutation === 'drop') {
          world = { ...world, recording: { ...recording, frames: recording.frames.slice(1) } }
        } else if (mutation === 'http-status') {
          world = { ...world, recording: { ...recording, http: recording.http.map((http, index) => index === 0 ? { ...http, status: 201 } : http) } }
        } else if (mutation === 'frame-reorder') {
          const first = recording.frames.find((frame) => frame.kind === 'conversation')
          const second = recording.frames.find((frame) => frame.kind === 'conversation' && frame.socket_id === first?.socket_id && frame.subscription_id === first?.subscription_id && frame.ordinal !== first?.ordinal)
          if (first === undefined || second === undefined) throw new Error('Missing two captured boundaries on one subscription')
          world = { ...world, recording: { ...recording, frames: recording.frames.map((frame) =>
            frame === first ? { ...frame, at_ms: second.at_ms, ordinal: second.ordinal }
              : frame === second ? { ...frame, at_ms: first.at_ms, ordinal: first.ordinal } : frame) } }
        } else {
          const conversation = world.slices.conversation
          const history = conversation.state.recorded_history
          const index = history?.findIndex((window) => window.items.length >= 2) ?? -1
          if (history !== undefined && index !== -1) {
            world = world.with({ conversation: { ...conversation, state: { ...conversation.state,
              recorded_history: history.map((window, at) => at !== index ? window : { ...window,
                items: mutation === 'reorder' ? [...window.items].reverse()
                  : window.items.map((item, at) => at !== 0 ? item : { ...item, revision: item.revision + 1 }),
              }),
            } } })
          } else {
            const eventIndex = conversation.timeline.findIndex((event) => (event._tag === 'entries' || event._tag === 'replace') && event.items.length >= 2)
            if (eventIndex === -1) throw new Error('Missing nonempty captured future window')
            world = world.with({ conversation: { ...conversation, timeline: conversation.timeline.map((event, at) => {
              if (at !== eventIndex || event._tag !== 'entries' && event._tag !== 'replace') return event
              return { ...event, items: mutation === 'reorder' ? [...event.items].reverse()
                : event.items.map((item, at) => at !== 0 ? item : { ...item, revision: item.revision + 1 }) }
            }) } })
          }
        }
        await expect(assertRecordingFidelity({ ...proof, world }, oracle)).rejects.toThrow()
        expect(readFileSync(new URL(name + '.normalized.json', RECORDINGS_DIR), 'utf8')).toBe(oracleBytes)
      }, 30_000)
    }

    if (name === 'long-debug-rabbit-hole') {
      it('retains the real older-page cursor itinerary and rejects a cursor mutation', async () => {
        const recording = proof.world.recording
        if (recording === undefined) throw new Error('Missing captured transport')
        const pages = recording.http.filter((http) => http.value.kind === 'timeline')
        expect(pages.map((http) => http.value.kind === 'timeline' ? http.value.items.length : -1)).toEqual([50, 50, 20])
        expect(pages.map((http) => http.value.kind === 'timeline' ? http.value.page.has_more : undefined)).toEqual([true, true, false])
        expect(pages.map((http) => new URLSearchParams(http.query).get('cursor'))).toEqual([null, 'cursor#1', 'cursor#2'])
        const expiringPages = pages.flatMap((http) => http.value.kind === 'timeline' && http.value.page.cursor_expires_at !== undefined ? [http] : [])
        expect(expiringPages).toHaveLength(2)
        await assertRecordingFidelity(proof, oracle)
        const world = { ...proof.world, recording: { ...recording, http: recording.http.map((http) => http.value.kind !== 'timeline' || http.value.page.next_cursor === undefined ? http
          : { ...http, value: { ...http.value, page: { ...http.value.page, next_cursor: 'cursor#999' } } }) } }
        await expect(assertRecordingFidelity({ ...proof, world }, oracle)).rejects.toThrow()
        const expiredWorld = { ...proof.world, recording: { ...recording, http: recording.http.map((http) =>
          http.value.kind !== 'timeline' || http.value.page.cursor_expires_at === undefined ? http
            : { ...http, value: { ...http.value, page: { ...http.value.page,
              cursor_expires_at: new Date(Date.parse(http.value.page.cursor_expires_at) + 1).toISOString(),
            } } }) } }
        await expect(assertRecordingFidelity({ ...proof, world: expiredWorld }, oracle)).rejects.toThrow()
      }, 30_000)
    }

    if (name === 'offline-reconnect-storm') {
      it('isolates each reconnect replacement socket and rejects cross-socket routing', async () => {
        const recording = proof.world.recording
        if (recording === undefined) throw new Error('Missing captured transport')
        const futureSockets = new Set(recording.frames.filter((frame) => frame.kind === 'conversation' && frame.at_ms >= 0).map((frame) => frame.socket_id))
        expect(futureSockets.size).toBe(4)
        await assertRecordingFidelity(proof, oracle)
        const historicalSocket = recording.socket_ids?.[0]
        if (historicalSocket === undefined) throw new Error('Missing historical socket')
        const world = { ...proof.world, recording: { ...recording, frames: recording.frames.map((frame) => frame.at_ms <= 0 ? frame : { ...frame, socket_id: historicalSocket }) } }
        await expect(assertRecordingFidelity({ ...proof, world }, oracle)).rejects.toThrow()
      }, 30_000)
    }
  })
}
