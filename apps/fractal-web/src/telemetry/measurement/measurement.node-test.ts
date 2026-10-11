import assert from 'node:assert/strict'
import { test } from 'node:test'
import { createCounters } from './counters.ts'
import { createMeasurementEngine, type FrameClock } from './engine.ts'

const fakeClock = () => {
  let time = 0
  let id = 0
  const callbacks = new Map<number, (time: number) => void>()
  const clock: FrameClock = {
    now: () => time,
    request: (callback) => { callbacks.set(++id, callback); return id },
    cancel: (handle) => { callbacks.delete(handle) },
  }
  return {
    clock,
    tick: async (elapsed: number) => {
      time += elapsed
      const pending = [...callbacks.values()]
      callbacks.clear()
      for (const callback of pending) callback(time)
      await Promise.resolve()
    },
    pending: () => callbacks.size,
  }
}

test('counter snapshots isolate brackets and unchanged keys remain absent', async () => {
  const bag = createCounters()
  const fake = fakeClock()
  const engine = createMeasurementEngine({ bag, clock: fake.clock, framePeriodMs: 10 })
  assert.equal(bag.getDebug('missing'), 0)
  bag.incrDebug('Wf.frames', 100)
  bag.setDebug('Wf.httpInFlight', 2)
  const first = engine.beginMeasure()
  bag.incrDebug('Wf.frames', 24)
  const second = engine.beginMeasure()
  bag.incrDebug('Fn.conversationRow')
  bag.setDebug('Wf.httpInFlight', 1)
  await fake.tick(10)
  assert.deepEqual((await engine.endMeasure(first)).debugDelta, {
    'Wf.frames': 24, 'Fn.conversationRow': 1, 'Wf.httpInFlight': -1,
  })
  assert.deepEqual((await engine.endMeasure(second)).debugDelta, {
    'Fn.conversationRow': 1, 'Wf.httpInFlight': -1,
  })
  assert.equal(fake.pending(), 0)
  const snapshot = bag.snapshot()
  bag.reset()
  assert.equal(snapshot['Wf.frames'], 124)
  assert.equal(bag.getDebug('Wf.frames'), 0)
})

test('frame gaps count missed display slots, not slow-host FPS as a new baseline', async () => {
  const fake = fakeClock()
  const engine = createMeasurementEngine({ clock: fake.clock, framePeriodMs: 10, bag: createCounters() })
  const handle = engine.beginMeasure()
  await fake.tick(10)
  await fake.tick(30)
  await fake.tick(10)
  assert.deepEqual(await engine.endMeasure(handle), {
    frameDrops: 2, framesCaptured: 3, durationMs: 50, averageFps: 60, debugDelta: {},
  })
  await assert.rejects(engine.endMeasure(handle), /Unknown measurement handle/)
})

test('settle frames belong to bracket and real extra renders fail exact count gate', async () => {
  const bag = createCounters()
  const fake = fakeClock()
  const engine = createMeasurementEngine({ clock: fake.clock, bag, framePeriodMs: 10 })
  const handle = engine.beginMeasure()
  const ending = engine.endMeasure(handle, { settleFrames: 2 })
  await assert.rejects(engine.endMeasure(handle), /already ending/)
  bag.incrDebug('Fn.conversationRow')
  await fake.tick(10)
  bag.incrDebug('Fn.conversationRow')
  await fake.tick(10)
  const result = await ending
  assert.equal(result.framesCaptured, 2)
  assert.equal(result.debugDelta['Fn.conversationRow'], 2)
  assert.throws(() => assert.equal(result.debugDelta['Fn.conversationRow'], 1))
  assert.equal(fake.pending(), 0)
})

test('120Hz uses the supplied refresh period and disposal rejects pending brackets', async () => {
  const fake = fakeClock()
  const engine = createMeasurementEngine({ clock: fake.clock, framePeriodMs: 1000 / 120 })
  const handle = engine.beginMeasure()
  await fake.tick(1000 / 120)
  assert.equal((await engine.endMeasure(handle)).frameDrops, 0)
  const pending = engine.endMeasure(engine.beginMeasure(), { settleFrames: 1 })
  engine.dispose()
  await assert.rejects(pending, /Measurement disposed/)
  assert.equal(fake.pending(), 0)
})
