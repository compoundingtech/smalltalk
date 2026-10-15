import assert from 'node:assert/strict'
import { after, test } from 'node:test'
import { createServer } from 'vite'
import stylex from '@stylexjs/unplugin'

const server = await createServer({ configFile: false, root: new URL('../../', import.meta.url).pathname, plugins: [stylex.vite({ useCSSLayers: false })], optimizeDeps: { noDiscovery: true, include: [] }, server: { middlewareMode: true } })
after(() => server.close())
const { createSystemClock } = await server.ssrLoadModule('/src/time/clock.tsx')
const { formatClock, formatDay, formatDuration, formatRelative, nextCivilChange, nextCountdownChange, nextDurationChange, nextRelativeChange, earliestChange } = await server.ssrLoadModule('/src/time/human-time.ts')
const { syncLine, syncLineNextChange } = await server.ssrLoadModule('/src/assistant-ui/st3-views/sync-line.ts')

/** A host whose timers and visibility the test drives. */
const pageHost = (start = 0) => {
  let time = start, hidden = false, nextId = 0
  const timers = new Map()
  const listeners = new Set()
  return {
    host: {
      now: () => time,
      setTimer: (run, ms) => { timers.set(++nextId, { at: time + ms, run }); return nextId },
      clearTimer: id => { timers.delete(id) },
      hidden: () => hidden,
      onVisibilityChange: listener => { listeners.add(listener); return () => listeners.delete(listener) },
    },
    armedAt: () => [...timers.values()].map(timer => timer.at).sort((a, b) => a - b),
    listeners: () => listeners.size,
    advance: to => {
      for (;;) {
        const [id, timer] = [...timers].sort(([, a], [, b]) => a.at - b.at)[0] ?? []
        if (id === undefined || timer.at > to) break
        timers.delete(id)
        time = timer.at
        timer.run()
      }
      time = to
    },
    setHidden: value => { hidden = value; for (const listener of [...listeners]) listener() },
  }
}

test('one shared timer wakes only the subscribers that are due', () => {
  const page = pageHost()
  const clock = createSystemClock(page.host)
  const wakes = []
  let minuteDue = 60_000
  clock.subscribe(() => { wakes.push(['minute', clock.now()]); minuteDue += 60_000 }, () => minuteDue)
  clock.subscribe(() => wakes.push(['once', clock.now()]), () => wakes.some(([name]) => name === 'once') ? undefined : 90_000)
  assert.deepEqual(page.armedAt(), [60_000])
  page.advance(10 * 60_000)
  assert.deepEqual(wakes, [['minute', 60_000], ['once', 90_000], ...Array.from({ length: 9 }, (_, index) => ['minute', (index + 2) * 60_000])])
  assert.deepEqual(page.armedAt(), [11 * 60_000])
})

test('a subscriber with no deadline arms nothing; the last unsubscribe releases timer and visibility listener', () => {
  const page = pageHost()
  const clock = createSystemClock(page.host)
  const stopIdle = clock.subscribe(() => assert.fail('an idle subscriber must not wake on a timer'), () => undefined)
  assert.deepEqual(page.armedAt(), [])
  const stop = clock.subscribe(() => {}, () => 5_000)
  assert.deepEqual(page.armedAt(), [5_000])
  stop()
  assert.deepEqual(page.armedAt(), [])
  stopIdle()
  assert.equal(page.listeners(), 0)
})

test('hidden pages arm nothing and becoming visible wakes every subscriber once', () => {
  const page = pageHost()
  const clock = createSystemClock(page.host)
  const wakes = []
  let due = 60_000
  clock.subscribe(() => { wakes.push(['a', clock.now()]); due = (Math.floor(clock.now() / 60_000) + 1) * 60_000 }, () => due)
  clock.subscribe(() => wakes.push(['b', clock.now()]), () => undefined)
  page.setHidden(true)
  assert.deepEqual(page.armedAt(), [])
  page.advance(30 * 60_000 + 5_000)
  assert.deepEqual(wakes, [])
  page.setHidden(false)
  assert.deepEqual(wakes, [['a', 30 * 60_000 + 5_000], ['b', 30 * 60_000 + 5_000]])
  assert.deepEqual(page.armedAt(), [31 * 60_000])
})

const zone = { locale: 'en-GB', timeZone: 'Europe/Berlin' }
const base = Date.parse('2026-10-14T21:47:13.250Z')
// Deterministic pseudo-random offsets so failures reproduce.
let seed = 7
const random = () => { seed = (seed * 1_103_515_245 + 12_345) % 2_147_483_648; return seed / 2_147_483_648 }

/** No label may change strictly before its computed next instant, sampled at every second and just before `next`. */
const assertStableUntilNext = (name, label, next, at, now) => {
  const until = next(now)
  if (until === undefined) return
  assert.ok(until > now, `${name}: next ${until} must follow now ${now}`)
  const text = label(now)
  const end = Math.min(until, now + 3_600_000)
  for (let time = now; time < end; time += 1_000) assert.equal(label(time), text, `${name} at=${at} now=${now} t=${time} next=${until}`)
  if (until <= now + 3_600_000) assert.equal(label(until - 1), text, `${name} just before next at=${at} now=${now}`)
}

test('relative, elapsed, countdown and civil labels never change before their next instant', () => {
  const ages = [0, 999, 29_999, 30_000, 44_999, 59_999, 60_000, 61_000, 119_999, 3_599_999, 3_600_000, 5_400_000, 86_399_000, 90_000_000, 6 * 86_400_000]
  for (let index = 0; index < 24; index++) ages.push(Math.floor(random() * 3 * 86_400_000))
  for (const age of ages) {
    const now = base + Math.floor(random() * 1_000)
    const at = now - age
    assertStableUntilNext('relative-past', time => formatRelative(at, time, zone), time => nextRelativeChange(at, time), at, now)
    assertStableUntilNext('relative-future', time => formatRelative(now + age + 31_000, time, zone), time => nextRelativeChange(now + age + 31_000, time), now + age + 31_000, now)
    assertStableUntilNext('elapsed-2', time => formatDuration(time - at), time => nextDurationChange(at, time), at, now)
    assertStableUntilNext('elapsed-1', time => formatDuration(time - at, { maxUnits: 1 }), time => nextDurationChange(at, time, { maxUnits: 1 }), at, now)
    assertStableUntilNext('countdown-1', time => formatDuration(now + age - time, { maxUnits: 1 }), time => nextCountdownChange(now + age, time, { maxUnits: 1 }), now + age, now)
    assertStableUntilNext('clock', time => formatClock(at, time, zone), time => nextCivilChange(time), at, now)
    assertStableUntilNext('day', time => formatDay(at, time, zone), time => nextCivilChange(time), at, now)
  }
})

test('minute-granular labels wake exactly at their minute boundary, not every second', () => {
  const at = base - 5 * 60_000 - 12_345
  const now = base
  assert.equal(formatRelative(at, now, zone), '5m ago')
  const next = nextRelativeChange(at, now)
  assert.equal(next, at + 6 * 60_000)
  assert.equal(formatRelative(at, next - 1, zone), '5m ago')
  assert.equal(formatRelative(at, next, zone), '6m ago')
  assert.equal(nextDurationChange(at, now, { maxUnits: 1 }), at + 6 * 60_000)
  assert.equal(nextDurationChange(base - 12_345, now), base - 12_345 + 13_000)
  assert.equal(earliestChange(now, [undefined, now, now - 1, NaN, now + 5, now + 2]), now + 2)
})

test('sync lines never change before syncLineNextChange', () => {
  const observedAt = base
  const statuses = [
    { _tag: 'Connecting', attempt: 1, since: observedAt },
    { _tag: 'Requested', since: observedAt },
    { _tag: 'Progress', stage: 'queued', elapsedMs: 200, reportedAt: observedAt },
    { _tag: 'Progress', stage: 'reading', elapsedMs: 0, reportedAt: observedAt, done: 3, total: 9 },
    { _tag: 'Stale', reason: { _tag: 'Unknown' } },
    { _tag: 'Stale', reason: { _tag: 'Unknown' }, lastLiveAt: observedAt - 50_000 },
    { _tag: 'Stale', reason: { _tag: 'Quiet', lastFrameAt: observedAt - 3_000 } },
    { _tag: 'Stale', reason: { _tag: 'Reconnecting', attempt: 2, nextAt: observedAt + 9_500, issue: 'transport lost' } },
    { _tag: 'Stale', reason: { _tag: 'Resync', code: 'internal' } },
  ]
  for (const status of statuses) for (const socket of [false, true]) {
    const text = time => JSON.stringify(syncLine({ status, label: 'conversation', now: time, observedAt, socket, gateway: 'gw' }) ?? null)
    for (const now of [observedAt, observedAt + 399, observedAt + 1_500, observedAt + 4_999, observedAt + 9_999, observedAt + 61_000]) {
      const until = syncLineNextChange({ status, label: 'conversation', now, observedAt, socket })
      const end = Math.min(until ?? now + 120_000, now + 120_000)
      for (let time = now; time < end; time += 50) assert.equal(text(time), text(now), `${status._tag}/${status.reason?._tag ?? status.stage ?? ''} socket=${socket} now=${now} t=${time} next=${until}`)
    }
  }
})
