import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import { describe, expect, it } from 'vitest'

import { makeTerminalInputFixture } from './terminalInputFixture.ts'

const text = new TextEncoder()

describe('ordered terminal input lifetime', () => {
  it('sends ordered batches from the opened sequence and refuses before-open/after-close writes', async () => {
    const registry = AtomRegistry.make()
    const fixture = makeTerminalInputFixture({ registry })
    try {
      await expect(fixture.port.send(text.encode('before-open'))).rejects.toThrow('not open')
      fixture.port.open()
      await Promise.all([fixture.port.send(text.encode('a')), fixture.port.send(text.encode('b'))])
      fixture.port.acknowledged(7)
      expect(registry.get(fixture.writes)).toEqual([
        { seq: 7, bytes: [97] },
        { seq: 8, bytes: [98] },
      ])
      fixture.port.close()
      await expect(fixture.port.send(text.encode('after-close'))).rejects.toThrow('not open')
    } finally {
      fixture.dispose()
      registry.dispose()
    }
  })

  it('rejects uncertain sends on disconnect and never rearms or replays', async () => {
    const registry = AtomRegistry.make()
    const fixture = makeTerminalInputFixture({ registry, acknowledge: false })
    try {
      fixture.port.open()
      const rejected = expect(fixture.port.send(text.encode('once'))).rejects.toThrow(
        'will not be replayed',
      )
      fixture.disconnect()
      await rejected
      fixture.port.open()
      await expect(fixture.port.send(text.encode('again'))).rejects.toThrow('not open')
      expect(registry.get(fixture.writes)).toEqual([{ seq: 7, bytes: [111, 110, 99, 101] }])
      expect(registry.get(fixture.port.state)).toMatchObject({ _tag: 'Closed', uncertain: true })
    } finally {
      fixture.dispose()
      registry.dispose()
    }
  })

  it.each([{ localOwner: false }, { controlGranted: false }])(
    'never opens input across a denied ownership or grant boundary: %j',
    async (gate) => {
      const registry = AtomRegistry.make()
      const fixture = makeTerminalInputFixture({ registry, ...gate })
      try {
        fixture.port.open()
        await expect(fixture.port.send(text.encode('forbidden'))).rejects.toThrow('not open')
        expect(registry.get(fixture.port.state)).toMatchObject({ _tag: 'Closed', uncertain: false })
        expect(registry.get(fixture.writes)).toEqual([])
      } finally {
        fixture.dispose()
        registry.dispose()
      }
    },
  )

  it('closes on out-of-order acknowledgement without accepting pending delivery', async () => {
    const registry = AtomRegistry.make()
    const fixture = makeTerminalInputFixture({ registry, acknowledge: false })
    try {
      fixture.port.open()
      const first = expect(fixture.port.send(text.encode('a'))).rejects.toThrow('out of order')
      const second = expect(fixture.port.send(text.encode('b'))).rejects.toThrow('out of order')
      fixture.port.acknowledged(8)
      await Promise.all([first, second])
      expect(registry.get(fixture.port.state)).toMatchObject({ _tag: 'Closed', uncertain: true })
    } finally {
      fixture.dispose()
      registry.dispose()
    }
  })
})
