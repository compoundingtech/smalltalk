import assert from 'node:assert/strict'
import { after, test } from 'node:test'
import { createServer } from 'vite'

const server = await createServer({ configFile: false, root: new URL('../../../', import.meta.url).pathname, server: { middlewareMode: true } })
after(() => server.close())
const { workLogTurnFromItems } = await server.ssrLoadModule('/src/assistant-ui/taste/work-log.ts')
const at = '2026-01-15T12:30:00Z'
const project = content => workLogTurnFromItems([{ _tag: 'ToolCall', id: 'tool/read', callId: 'call/read', name: 'read', input: {}, status: 'success', callSeen: true, at, result: { content, isError: false, at } }], { kindFor: () => 'read', running: false, failed: false, interrupted: false }).calls[0]

test('string output remains the work-log detail', () => assert.equal(project('Native output').detail, 'Native output'))
test('native content arrays retain every text part in order', () => assert.equal(project([{ type: 'text', text: 'First' }, { type: 'text', text: 'Second' }]).detail, 'First\nSecond'))
test('empty output has no detail', () => {
  for (const content of ['', [], undefined, [{ type: 'text', text: '' }]]) assert.equal(project(content).detail, undefined)
})
test('non-text parts are ignored, not serialized into invented output', () => {
  assert.equal(project([{ type: 'image', data: 'synthetic-bytes', mimeType: 'image/png' }, { type: 'text', text: 'Visible output' }, { type: 'audio', data: 'synthetic-audio' }]).detail, 'Visible output')
  assert.equal(project([{ type: 'image', data: 'synthetic-bytes' }]).detail, undefined)
})
