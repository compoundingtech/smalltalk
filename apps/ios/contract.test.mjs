import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { decodeWorld } from './clientView.ts';
import { conversation, fromHarness } from './harnessConversation.ts';
import { theme } from './theme.ts';
import { contractWords } from './words.ts';

// fixtures/clients is the contract stui writes; this app must agree with it exactly.
const fixture = name => JSON.parse(readFileSync(new URL(`../../fixtures/clients/${name}`, import.meta.url), 'utf8'));

// The demo world reads back field for field: nothing stui wrote is unknown here, nothing is lost.
{
  const raw = fixture('demo-world.json');
  const world = decodeWorld(raw);
  assert.deepStrictEqual(JSON.parse(JSON.stringify(world)), raw);
  assert.throws(() => decodeWorld({ ...raw, surprise: true }), /no such field/);
  // The contract names the person's devices and a terminal flag on each agent.
  assert.deepStrictEqual(world.devices.value.map(device => device.id), ['device/phone', 'device/tablet']);
  assert.ok(world.agents.value.every(agent => typeof agent.terminal === 'boolean'));
  assert.equal(world.agents.value.find(agent => agent.unmanaged).terminal, false);
}

// The words, glyphs and tabs match stui.
assert.deepStrictEqual(contractWords(), fixture('words.json'));

// The colour tokens and the one colour rule match stui.
assert.deepStrictEqual(JSON.parse(JSON.stringify(theme)), fixture('theme.json'));

// Each real transcript cleans to the conversation stui derives (display times left out).
for (const name of ['claude', 'codex']) {
  const derived = conversation(fixture(`transcripts/${name}.json`), [], {}).map(({ at: _, ...entry }) => entry);
  assert.deepStrictEqual(derived, fixture(`transcripts/${name}.expected.json`), name);
}

// An unclosed wrapper cannot leak.
assert.deepStrictEqual(fromHarness(true, 'hello\n<system-reminder>\nnever closed'), [{ kind: 'user', value: 'hello' }]);

// Tool calls take their results by call id; a failed result marks the box failed.
{
  const entries = conversation([
    { id: '1', sequence: 1, revision: 1, timestamp: '2026-09-28T09:00:00Z', role: 'assistant', final: true, type: 'tool_call', body: { call_id: 'c', name: 'Bash', arguments: { command: 'cargo test' } } },
    { id: '2', sequence: 2, revision: 1, timestamp: '2026-09-28T09:00:05Z', role: 'tool', final: true, type: 'tool_result', body: { call_id: 'c', status: 'error', media_type: 'text/plain', content: '1 failed' } },
  ], [], {});
  assert.equal(entries.length, 1);
  assert.deepStrictEqual(entries[0].body, { kind: 'tool', value: { title: '$ cargo test', state: 'failed', output: ['1 failed'] } });
}

// Messages merge by time with names, not ids; st's step-ready pings are one event line.
{
  const entries = conversation(
    [{ id: 't', sequence: 1, revision: 1, timestamp: '2026-09-28T09:00:02Z', role: 'assistant', final: true, type: 'content', body: { media_type: 'text/plain', text: 'On it.' } }],
    [
      { id: 'message/2', from: 'daemon/runtime', to: 'agent/fleet/x', title: 'Mission step ready: build', content: 'A mission step is ready', sent_at: '2026-09-28T09:00:03Z' },
      { id: 'message/1', from: 'person/robin', to: 'agent/fleet/x', title: null, content: 'Please build it', sent_at: '2026-09-28T09:00:01Z' },
    ],
    { 'person/robin': 'you', 'agent/fleet/x': 'X' },
  );
  assert.deepStrictEqual(entries.map(entry => entry.body), [
    { kind: 'mail', value: { from: 'you', to: 'X', subject: '', body: 'Please build it' } },
    { kind: 'assistant', value: 'On it.' },
    { kind: 'event', value: 'Mission step ready: build' },
  ]);
}
