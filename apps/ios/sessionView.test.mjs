import assert from 'node:assert/strict';
import { isSnapshotChurn, isUnmanaged, isUnresolved, listSessionPages, recentTimeline, sessionLabel, timelineText } from './sessionView.ts';

const managed = { id: 'session/managed', kind: 'session', owner_id: 'agent/one', state: 'running' };
const exact = { id: 'session/exact', kind: 'session', owner_id: 'external-session/codex/one', state: 'running', managed: false, driver: 'codex', native_session_id: 'one' };
const unresolved = { id: 'session/process', kind: 'session', owner_id: 'external-process/claude/42', state: 'running', managed: false, driver: 'claude', native_session_id: null, process: { pid: 42, exact_session: false } };
assert.equal(isUnmanaged(managed), false);
assert.equal(isUnmanaged(exact), true);
assert.equal(isUnresolved(exact), false);
assert.equal(isUnresolved(unresolved), true);
assert.match(sessionLabel(exact, 'gateway-host'), /Undeclared.*codex session.*gateway-host/);
assert.match(sessionLabel(unresolved, 'gateway-host'), /Undeclared.*unresolved claude process.*gateway-host/);

const calls = [];
const sessions = await listSessionPages(async options => {
  calls.push(options);
  return { value: { items: options.cursor ? [unresolved] : [managed, exact], page: { has_more: !options.cursor, next_cursor: options.cursor ? null : 'next' } } };
}, 2);
assert.deepEqual(sessions.map(session => session.id), ['session/managed', 'session/exact', 'session/process']);
assert.deepEqual(calls, [{ limit: 2, cursor: undefined, history: false }, { limit: 2, cursor: 'next', history: false }]);
await assert.rejects(listSessionPages(async () => ({ value: { items: [], page: { has_more: true, next_cursor: 'same' } } }), 2), /did not advance/);
let boundedCalls = 0;
await listSessionPages(async () => {
  boundedCalls++;
  return { value: { items: [exact], page: { has_more: true, next_cursor: `page-${boundedCalls}` } } };
}, 2);
assert.equal(boundedCalls, 5);
const expired = { response: { code: 'page-cursor-expired' } };
assert.equal(isSnapshotChurn(expired), true);
assert.equal(isSnapshotChurn(new Error('offline')), false);
let firstPages = 0, secondPages = 0;
const restarted = await listSessionPages(async options => {
  if (!options.cursor) {
    firstPages++;
    return { value: { items: [managed], page: { has_more: true, next_cursor: 'next' } } };
  }
  secondPages++;
  if (secondPages === 1) throw expired;
  return { value: { items: [exact], page: { has_more: false, next_cursor: null } } };
}, 2);
assert.equal(firstPages, 2);
assert.equal(secondPages, 2);
assert.deepEqual(restarted.map(session => session.id), ['session/managed', 'session/exact']);
assert.deepEqual(recentTimeline([{ sequence: 9 }, { sequence: 10 }, { sequence: 1 }], 2).map(entry => entry.sequence), [9, 10]);
assert.equal(timelineText({ media_type: 'text/plain', text: 'Hello' }), 'Hello');
assert.equal(timelineText({ message_id: 'metadata-only' }), null);
