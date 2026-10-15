import assert from 'node:assert/strict'
import { after, test } from 'node:test'
import * as React from 'react'
import { renderToStaticMarkup } from 'react-dom/server'
import { createServer } from 'vite'
import stylex from '@stylexjs/unplugin'

// Load the real TSX with the kit's StyleX transform, just as Storybook does.
const server = await createServer({
  configFile: false,
  root: new URL('../../../', import.meta.url).pathname,
  plugins: [stylex.vite({ useCSSLayers: false })],
  server: { middlewareMode: true },
})
after(() => server.close())
const { Transcript, LiveActivity } = await server.ssrLoadModule('/src/assistant-ui/composition/Transcript.tsx')
const { EmbraceRuntimeProvider } = await server.ssrLoadModule('/src/assistant-ui/EmbraceRuntime.tsx')
const { SendUnconfirmed } = await server.ssrLoadModule('/src/assistant-ui/composition/TranscriptFeedback.tsx')

const now = Date.parse('2026-01-15T12:30:00Z')
const prompt = { _tag: 'Text', id: 'prompt/example', role: 'user', text: 'Keep the visible rows grouped.', attachments: [], streaming: false, at: new Date(now).toISOString() }
const turn = { id: 'turn/example', prompt, items: [], work: { calls: [], durationMs: undefined, running: false, failed: false, interrupted: false } }

for (const turns of [[], [turn]]) test(`Transcript server-renders ${turns.length === 0 ? 'empty history' : 'an unpublished prompt'} without throwing`, () => {
  const element = React.createElement(EmbraceRuntimeProvider, {
    options: { messages: turns.flatMap(entry => [entry.prompt]), isRunning: false, onNew: async () => {} },
  }, React.createElement(Transcript, { turns, title: 'Row projection', sync: { _tag: 'Live', since: now }, now, observedAt: now }))
  let markup
  assert.doesNotThrow(() => { markup = renderToStaticMarkup(element) })
  assert.match(markup, /Row projection/)
})

test('live activity shares compact header durations and host-supplied waiting state', () => {
  for (const [elapsed, duration] of [[12000, '12s'], [60000, '1m'], [185000, '3m 05s'], [3720000, '1h 02m'], [14820000, '4h 07m'], [16027000, '4h 27m']]) {
    for (const tag of ['Working', 'WaitingForAnswer']) {
      const markup = renderToStaticMarkup(React.createElement(LiveActivity, { state: { _tag: tag }, startedAt: new Date(now - elapsed).toISOString(), now }))
      assert.ok(markup.includes(tag === 'Working' ? `Working · ${duration}` : `Waiting for your answer · ${duration}`))
    }
  }
  const noStart = renderToStaticMarkup(React.createElement(LiveActivity, { state: { _tag: 'WaitingForAnswer' }, now }))
  assert.ok(noStart.includes('Waiting for your answer'))
  assert.ok(!noStart.includes('Working'))
})

test('an unconfirmed send reads as not confirmed yet with Send again, never as a failure', () => {
  const unconfirmed = { ...prompt, sendState: { _tag: 'Unconfirmed', detail: 'Sending it again is safe.' } }
  const element = React.createElement(EmbraceRuntimeProvider, {
    options: { messages: [unconfirmed], isRunning: false, onNew: async () => {} },
  }, React.createElement(Transcript, { turns: [{ ...turn, prompt: unconfirmed }], title: 'Row projection', sync: { _tag: 'Live', since: now }, now, observedAt: now, onRetrySend: () => {} }))
  const markup = renderToStaticMarkup(element)
  assert.match(markup, /data-send-state="unconfirmed"/)
  assert.match(markup, /data-testid="send-unconfirmed"/)
  assert.match(markup, /role="status"/)
  assert.match(markup, /Not confirmed yet/)
  assert.match(markup, /Send again/)
  assert.doesNotMatch(markup, /send-failure|Couldn&#x27;t send|Couldn't send|role="alert"/)
  // Without a retry handler there is no action to offer.
  const bare = renderToStaticMarkup(React.createElement(SendUnconfirmed, { state: { _tag: 'Unconfirmed' } }))
  assert.match(bare, /Not confirmed yet/)
  assert.doesNotMatch(bare, /Send again/)
})
