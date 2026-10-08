import { readdirSync, readFileSync, statSync } from 'node:fs'
import { dirname, join, resolve } from 'node:path'
import { fileURLToPath } from 'node:url'

import { it } from '@effect/vitest'
import ts from 'typescript'
import { Schema } from 'effect'
import { describe, expect } from 'vitest'

import { ANCHOR, ANCHOR_MS, catalog, loadWorld, parseTimestamp, rebase, SLICE_KINDS, TimeRangeError, type AnySlice } from '../src/index.ts'
import type { Asciicast } from '../src/kit/asciicast.ts'
import { screenAt } from '../src/kit/screen.ts'
import { canonicalJson, generate, sliceFile, sliceTimes } from '../scripts/emit.ts'
import { rebaseVectors } from '../scripts/vectors.ts'

const packageRoot = fileURLToPath(new URL('../', import.meta.url))

describe('determinism', () => {
  it('generates the same bytes for the same seed', () => {
    expect([...generate()]).toEqual([...generate()])
  })

  it('keeps generators free of Math.random and Date.now', () => {
    const offenders = ['src/kit', 'src/worlds'].flatMap((dir) =>
      walk(join(packageRoot, dir)).filter((path) => /Math\.random|Date\.now/.test(readFileSync(path, 'utf8'))),
    )
    expect(offenders).toEqual([])
  })
})

describe('rebase', () => {
  it.each(rebaseVectors.map((vector) => [vector.name, vector] as const))('vector: %s', (_name, vector) => {
    if (vector.expected._tag === 'ok') {
      expect(rebase(vector.input, vector.times, vector.anchor, vector.now)).toEqual(vector.expected.value)
    } else {
      const pointer = vector.expected.pointer
      expect(() => rebase(vector.input, vector.times, vector.anchor, vector.now)).toThrow(
        expect.objectContaining({ pointer }) as unknown as TimeRangeError,
      )
    }
  })

  const files = catalog.flatMap(({ id }) => {
    const world = loadWorld(id, { now: ANCHOR_MS })
    return SLICE_KINDS.map((kind) => ({ id, kind, file: JSON.parse(canonicalJson(sliceFile(world, world.slices[kind] as AnySlice))) }))
  })

  // 1900..2200: wide enough to cross centuries, inside the range every committed instant supports.
  const nows = Schema.Int.check(Schema.isBetween({ minimum: Date.UTC(1900, 0, 1), maximum: Date.UTC(2200, 0, 1) }))

  it.prop('a rebased file equals the world generated at that now; differences stay exact', { now: nows }, ({ now }) => {
    const worlds = new Map(catalog.map(({ id }) => [id, loadWorld(id, { now })]))
    for (const { id, kind, file } of files) {
      const rebased = rebase({ state: file.state, timeline: file.timeline }, file.times, ANCHOR, now)
      const world = worlds.get(id)!
      const direct = JSON.parse(canonicalJson(sliceFile(world, world.slices[kind] as AnySlice)))
      expect(rebased).toEqual({ state: direct.state, timeline: direct.timeline })
      for (const { pointer, codec } of file.times as { pointer: string; codec: string }[]) {
        const before = read(file, pointer)
        const after = read(rebased, pointer)
        const delta = codec === 'timestamp' ? parseTimestamp(after as string) - parseTimestamp(before as string) : (after as number) - (before as number)
        expect(delta).toBe(now - ANCHOR_MS)
        if (codec === 'timestamp') expect(after).toMatch(/^\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}\.\d{3}Z$/)
      }
    }
  }, { timeout: 60_000 })

  it('lists every instant of the default world', () => {
    const world = loadWorld('fleet-mid-refactor', { now: ANCHOR_MS })
    const times = sliceTimes(world.slices.roster as AnySlice).map((time) => time.pointer)
    expect(times).toContain('/state/agents/0/since')
    expect(times).toContain('/state/agents/0/current_work/0/since')
    expect(times).toContain('/timeline/0/upserts/0/updated_at')
  })
})

describe('worlds', () => {
  it('orders every timeline by at_ms and numbers stores from 2 upward without gaps', () => {
    for (const { id } of catalog) {
      const world = loadWorld(id, { now: ANCHOR_MS })
      const stores = new Set<number>()
      for (const kind of SLICE_KINDS) {
        for (const slice of [world.slices[kind], ...world.available[kind].map((variant) => world.with({ [kind]: variant }).slices[kind])]) {
          const times = slice.timeline.map((event) => event.at_ms)
          expect(times).toEqual([...times].sort((a, b) => a - b))
        }
        world.slices[kind].timeline.forEach((event) => stores.add(event.store))
      }
      const changed = [...stores].filter((store) => store > 1).sort((a, b) => a - b)
      expect(changed).toEqual(changed.map((_, index) => index + 2))
    }
  })

  it('derives failed-sync-socket-dropped from the default world, differing only in sync', () => {
    const base = loadWorld('fleet-mid-refactor', { now: ANCHOR_MS })
    const failed = loadWorld('failed-sync-socket-dropped', { now: ANCHOR_MS })
    for (const kind of SLICE_KINDS) {
      if (kind === 'sync') expect(failed.slices.sync).not.toEqual(base.slices.sync)
      else expect(failed.slices[kind]).toEqual(base.slices[kind])
    }
  })
})

describe('terminal screen', () => {
  it('wraps a line that reaches the width onto the next row and keeps every character', () => {
    const cast = { header: { version: 2, width: 10, height: 4 }, started_at_ms: 0,
      events: [[0, 'o', 'Expected 2 arguments\r\nok\r\n']] } satisfies Asciicast
    const screen = screenAt(cast, 1, { terminalId: 'terminal/t', incarnation: 'i' })
    expect(screen.lines.map(({ text, wrapped }) => ({ text, wrapped }))).toEqual([
      { text: 'Expected 2', wrapped: true },
      { text: ' arguments', wrapped: false },
      { text: 'ok', wrapped: false },
      { text: '', wrapped: false },
    ])
  })
})

describe('size budget', () => {
  it('keeps each file under 8 MiB and the tree under 24 MiB', () => {
    const sizes = [...generate().values()].map((bytes) => Buffer.byteLength(bytes))
    expect(Math.max(...sizes)).toBeLessThanOrEqual(8 * 1024 * 1024)
    expect(sizes.reduce((a, b) => a + b, 0)).toBeLessThanOrEqual(24 * 1024 * 1024)
  })
})

describe('import boundary', () => {
  it('keeps effect out of every file reachable from ., ./replay and ./react', () => {
    const manifest = JSON.parse(readFileSync(join(packageRoot, 'package.json'), 'utf8')) as { exports: Record<string, string> }
    const entries = ['.', './replay', './react'].map((key) => join(packageRoot, manifest.exports[key]!))
    expect(effectImporters(entries)).toEqual([])
  })

  it('detects a planted effect import', () => {
    expect(effectImporters([join(packageRoot, 'test/planted/importsEffect.ts')])).toHaveLength(1)
  })
})

/** Files reachable through relative imports from `entries` that import `effect` or `@effect/*`. */
const effectImporters = (entries: readonly string[]): string[] => {
  const seen = new Set<string>()
  const offenders: string[] = []
  const visit = (path: string) => {
    if (seen.has(path)) return
    seen.add(path)
    for (const { fileName: specifier } of ts.preProcessFile(readFileSync(path, 'utf8'), true, true).importedFiles) {
      if (specifier === 'effect' || specifier.startsWith('effect/') || specifier.startsWith('@effect/') || specifier.endsWith('/schema')) {
        offenders.push(path)
      } else if (specifier.startsWith('.')) visit(resolve(dirname(path), specifier))
    }
  }
  entries.forEach(visit)
  return offenders
}

const read = (doc: unknown, pointer: string): unknown =>
  pointer
    .slice(1)
    .split('/')
    .map((token) => token.replace(/~1/g, '/').replace(/~0/g, '~'))
    .reduce<unknown>((value, token) => (value as Record<string, unknown>)[token], doc)

const walk = (dir: string): string[] =>
  readdirSync(dir).flatMap((name) => {
    const path = join(dir, name)
    return statSync(path).isDirectory() ? walk(path) : [path]
  })
