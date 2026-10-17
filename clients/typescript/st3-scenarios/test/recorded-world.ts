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
    id: 'recorded-' + script.world + (script.format_version === 1 ? '-proof' : ''), title: 'Recorded ' + script.world,
    narrative: 'Scratch recorder transport fidelity proof.', seed: 1,
    cast: { project: script.agent.split('/')[2] ?? 'example', agents: [{ key: 'builder', role: 'builder' }], hosts: 1, people: 1, missions: [] },
    slices: () => imported.slices, recording: imported.recording,
  }, { roster: {}, details: {}, attention: {}, conversation: {}, terminal: {}, sync: {} }, now)
}

export const loadRecordedWorld = (name: string, now = ANCHOR_MS): RecordedProof => {
  const script = decodeScript(JSON.parse(readFileSync(new URL(name + '.script.json', RECORDINGS_DIR), 'utf8')))
  const raw = decodeRecording(readFileSync(new URL(name + '.recording.jsonl', RECORDINGS_DIR), 'utf8'))
  const rows = normalizeRecording(script, raw, now)
  return { world: recordedWorld(script, rows, now), script, rows }
}

export const loadRecordedProof = (now = ANCHOR_MS): RecordedProof => loadRecordedWorld(PROOF_NAME, now)
