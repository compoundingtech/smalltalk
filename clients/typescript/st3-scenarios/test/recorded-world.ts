import { readFileSync } from 'node:fs'
import { ANCHOR_MS } from '../src/kit/time.ts'
import { buildWorld, type World } from '../src/kit/world.ts'
import { decodeRecording, decodeScript, importRecording, normalizeRecording, type RecordingRow, type RecordingScript } from '../scripts/recordings.ts'

export const RECORDINGS_DIR = new URL('../recordings/', import.meta.url)
export const PROOF_NAME = 'fleet-mid-refactor-proof'

export interface RecordedProof {
  readonly world: World
  readonly script: RecordingScript
  readonly rows: RecordingRow[]
}

/** Only cast metadata is drawn; every slice and wire boundary is imported, never factory-built. */
export const recordedWorld = (script: RecordingScript, rows: readonly RecordingRow[], now = ANCHOR_MS): World => {
  const imported = importRecording(script, rows)
  return buildWorld({
    id: 'recorded-fleet-mid-refactor-proof', title: 'Recorded fleet mid-refactor proof',
    narrative: 'Scratch recorder transport fidelity proof.', seed: 1,
    cast: { project: 'atlas', agents: [{ key: 'builder', role: 'builder' }], hosts: 1, people: 1, missions: [] },
    slices: () => imported.slices, recording: imported.recording,
  }, { roster: {}, details: {}, attention: {}, conversation: {}, terminal: {}, sync: {} }, now)
}

export const loadRecordedProof = (now = ANCHOR_MS): RecordedProof => {
  const script = decodeScript(JSON.parse(readFileSync(new URL(PROOF_NAME + '.script.json', RECORDINGS_DIR), 'utf8')))
  const raw = decodeRecording(readFileSync(new URL(PROOF_NAME + '.recording.jsonl', RECORDINGS_DIR), 'utf8'))
  const rows = normalizeRecording(script, raw, now)
  return { world: recordedWorld(script, rows, now), script, rows }
}
