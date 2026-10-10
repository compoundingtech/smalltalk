import { readFileSync } from 'node:fs'
import { fileURLToPath } from 'node:url'
import { decodeUnknownSync, TimelineEntry } from '@smalltalk/st3-client/schema'
import { describe, expect, it } from 'vitest'
import { scanContent, scanIdentities } from '../scripts/scan.ts'
import { decodeSteps, encodeSteps, sessionCounts, sessionEntries, sessionNames, sessions, worldNow } from '../src/sessions/index.ts'

const decode = decodeUnknownSync(TimelineEntry, 'strict')

describe('hand-authored session corpus', () => {
  it('exports all eight stable names without repeated scripts', () => {
    expect(sessions.map(session => session.id)).toEqual(sessionNames)
    expect(new Set(sessions.map(session => session.steps.find(step => step.kind === 'user')?.text)).size).toBe(8)
    expect(new Set(sessions.map(session => sessionCounts(session).durationMs)).size).toBe(8)
    expect(new Set(sessions.map(session => session.steps.length)).size).toBeGreaterThan(4)
  })

  for (const session of sessions) {
    it(`${session.id}: every authored step emits production-decodable items`, () => {
      const encoded = encodeSteps(session)
      const decoded = decodeSteps(session)
      expect(encoded).toHaveLength(session.steps.length)
      expect(decoded).toHaveLength(session.steps.length)
      encoded.forEach((entries, index) => {
        expect(entries.length).toBeGreaterThan(0)
        expect(entries.map(decode)).toEqual(decoded[index])
      })
      const items = encoded.flat()
      expect(sessionEntries(session)).toEqual(items.map(decode))
      expect(new Set(items.map(item => item.id)).size).toBe(items.length)
      expect(items.map(item => item.sequence)).toEqual(items.map((_, index) => index + 1))
      expect(items.every(item => Date.parse(item.timestamp) <= worldNow)).toBe(true)
      const counts = sessionCounts(session)
      expect(counts.entries).toBe(items.length)
      expect(counts.toolCalls).toBe(items.filter(item => item.type === 'tool_call').length)
      expect(counts.userMessages).toBe(items.filter(item => item.type === 'content' && item.role === 'user').length)
      expect(scanContent(session.id, JSON.stringify(items))).toEqual([])
      expect(scanIdentities(session.id, JSON.stringify(items))).toEqual([])
    })
  }

  it('rejects a planted invalid step entry through the same production decoder', () => {
    const valid = encodeSteps(sessions[0]!).flat()[0]!
    expect(() => decode(valid)).not.toThrow()
    expect(() => decode({ ...valid, revision: 0 })).toThrow()
    expect(() => decode({ ...valid, timestamp: 'not-an-instant' })).toThrow()
    expect(() => decode({ ...valid, role: 'invented-role' })).toThrow()
    const tool = encodeSteps(sessions[0]!).flat().find(entry => entry.type === 'tool_result')!
    expect(() => decode({ ...tool, body: { ...tool.body, status: 'invented-result' } })).toThrow()
  })

  it('keeps every authored source inside the existing privacy gate', () => {
    for (const name of ['index.ts', 'script.ts', 'clock.ts']) {
      const path = fileURLToPath(new URL(`../src/sessions/${name}`, import.meta.url))
      const text = readFileSync(path, 'utf8')
      expect(scanContent(path, text)).toEqual([])
      expect(scanIdentities(path, text)).toEqual([])
    }
    // Negative control for the same privacy rules, assembled to avoid committing denied data.
    expect(scanContent('planted', '/' + 'home' + '/fictional-user')).toHaveLength(1)
    expect(scanIdentities('planted', ['agent', 'outside', 'namespace'].join('/'))).toHaveLength(1)
  })

  it('retains the failed hypothesis, repair, and successful rerun in order', () => {
    const steps = sessions[1]!.steps
    expect(steps.findIndex(step => step.kind === 'tool' && step.isError === true)).toBeLessThan(steps.findIndex(step => step.kind === 'tool' && step.id === 'repair-clock'))
    expect(steps.findIndex(step => step.kind === 'tool' && step.id === 'repair-clock')).toBeLessThan(steps.findIndex(step => step.kind === 'tool' && step.id === 'repeat-expiry'))
  })

  it('covers zero-output tools, many output lines, cancelled draft, and empty start', () => {
    const debug = sessions.find(session => session.id === 'long-debug')!
    expect(debug.steps.filter(step => step.kind === 'tool' && step.output === '')).toHaveLength(2)
    expect(debug.steps.some(step => step.kind === 'tool' && (step.output?.split('\n').length ?? 0) > 10)).toBe(true)
    expect(sessions.find(session => session.id === 'interrupted-draft')?.draft).toContain('saved inventory migration plan')
    expect(sessions.find(session => session.id === 'first-result')?.initialStepCount).toBe(0)
  })
})
