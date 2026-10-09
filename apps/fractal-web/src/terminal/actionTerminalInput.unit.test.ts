import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import { afterEach, beforeEach, describe, expect, it } from 'vitest'

import { TerminalInputUnsendable } from './actionTerminalInput.ts'
import { makeTerminalInputFixture } from './terminalInputFixture.ts'
import { pasteMaxBytes } from './terminalInputWire.ts'

const text = new TextEncoder()
const raw = (value: string) => ({ mode: 'raw', value: btoa(value) })
// The session settles answers through a short promise chain; drain it without wall-clock time.
const settled = async () => {
  for (let hop = 0; hop < 5; hop += 1) await Promise.resolve()
}

let registry: AtomRegistry.AtomRegistry
beforeEach(() => {
  registry = AtomRegistry.make()
})
afterEach(() => registry.dispose())

describe('action terminal input session', () => {
  it('refuses input before it is armed and after it is closed', async () => {
    const fixture = makeTerminalInputFixture({ registry })
    await expect(fixture.port.send(text.encode('early'))).rejects.toThrow('not open')
    fixture.port.open()
    await fixture.port.send(text.encode('a'))
    fixture.port.close()
    await expect(fixture.port.send(text.encode('late'))).rejects.toThrow('not open')
    expect(registry.get(fixture.writes)).toEqual([raw('a')])
    expect(registry.get(fixture.port.state)).toMatchObject({ _tag: 'Closed', uncertain: false })
  })

  it('keeps one action in flight and submits queued chunks in order once it is delivered', async () => {
    const fixture = makeTerminalInputFixture({ registry, autoDeliver: false })
    fixture.port.open()
    const first = fixture.port.send(text.encode('a'))
    const escape = fixture.port.send(text.encode('\x1b'))
    const second = fixture.port.send(text.encode('b'))
    const third = fixture.port.send(text.encode('c'))
    expect(registry.get(fixture.writes)).toEqual([raw('a')])
    expect(registry.get(fixture.port.state)).toEqual({ _tag: 'Ready', nextSeq: 1, pending: 3 })
    fixture.answer({ _tag: 'Delivered' })
    await first
    expect(registry.get(fixture.writes)).toEqual([raw('a'), { mode: 'key', value: 'escape' }])
    fixture.answer({ _tag: 'Delivered' })
    await escape
    // Raw chunks that waited together become one write with the same bytes in the same order.
    expect(registry.get(fixture.writes)).toEqual([raw('a'), { mode: 'key', value: 'escape' }, raw('bc')])
    fixture.answer({ _tag: 'Delivered' })
    await Promise.all([second, third])
    expect(registry.get(fixture.port.state)).toEqual({ _tag: 'Ready', nextSeq: 3, pending: 0 })
  })

  it('stops on a stale fence, drops the queue and never resends against the old fence', async () => {
    const fixture = makeTerminalInputFixture({ registry, autoDeliver: false })
    fixture.port.open()
    const inFlight = fixture.port.send(text.encode('a'))
    const queued = fixture.port.send(text.encode('\r'))
    const reason = 'The terminal changed before this input arrived. Queued keys were dropped.'
    fixture.answer({ _tag: 'Refused', reason })
    await expect(inFlight).rejects.toThrow(reason)
    await expect(queued).rejects.toThrow(reason)
    await settled()
    expect(registry.get(fixture.writes)).toEqual([raw('a')])
    expect(registry.get(fixture.port.state)).toEqual({ _tag: 'Closed', reason, uncertain: false })
    fixture.port.open()
    await expect(fixture.port.send(text.encode('again'))).rejects.toThrow('not open')
  })

  it('marks delivery uncertain when no answer arrives and replays nothing', async () => {
    const fixture = makeTerminalInputFixture({ registry, autoDeliver: false })
    fixture.port.open()
    const once = fixture.port.send(text.encode('once'))
    fixture.answer({ _tag: 'Uncertain', reason: 'Input delivery could not be confirmed. Queued keys were dropped.' })
    await expect(once).rejects.toThrow('could not be confirmed')
    expect(registry.get(fixture.port.state)).toMatchObject({ _tag: 'Closed', uncertain: true })
    expect(registry.get(fixture.writes)).toEqual([raw('once')])
  })

  it('drops queued chunks on close and ignores a late answer for the closed session', async () => {
    const fixture = makeTerminalInputFixture({ registry, autoDeliver: false })
    fixture.port.open()
    const inFlight = fixture.port.send(text.encode('a'))
    const queued = fixture.port.send(text.encode('b'))
    fixture.port.close()
    await expect(inFlight).rejects.toThrow('Input is off')
    await expect(queued).rejects.toThrow('Input is off')
    fixture.answer({ _tag: 'Delivered' })
    await settled()
    expect(registry.get(fixture.writes)).toEqual([raw('a')])
    expect(registry.get(fixture.port.state)).toMatchObject({ _tag: 'Closed', uncertain: false })
  })

  it('refuses a NUL byte or an oversized chunk locally without ending the session', async () => {
    const fixture = makeTerminalInputFixture({ registry })
    fixture.port.open()
    await expect(fixture.port.send(new Uint8Array([0]))).rejects.toBeInstanceOf(TerminalInputUnsendable)
    await expect(fixture.port.send(new Uint8Array(pasteMaxBytes + 1).fill(97))).rejects.toBeInstanceOf(TerminalInputUnsendable)
    await fixture.port.send(text.encode('ok'))
    expect(registry.get(fixture.writes)).toEqual([raw('ok')])
    expect(registry.get(fixture.port.state)._tag).toBe('Ready')
  })
})
