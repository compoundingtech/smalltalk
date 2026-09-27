import assert from 'node:assert/strict';
import { projectionEventsRequireRefresh, tabsChangedByProjectionEvents } from './projectionRefresh.ts';

assert.equal(projectionEventsRequireRefresh([]), false);
assert.equal(projectionEventsRequireRefresh([{ resource_ids: ['session/current'], body: {} }]), false);
assert.equal(projectionEventsRequireRefresh([{ resource_ids: ['session/current', 'message/1'], body: {} }]), true);
assert.equal(projectionEventsRequireRefresh([{ resource_ids: ['agent/one'], body: {} }]), true);
assert.equal(projectionEventsRequireRefresh([{ resource_ids: [], body: {} }]), true);
assert.equal(projectionEventsRequireRefresh([{ resource_ids: ['step-run/one'], body: { change: 'work.renewed' } }], 'Control'), false);
assert.equal(projectionEventsRequireRefresh([{ resource_ids: ['step-run/one'], body: { change: 'work.claimed' } }], 'Control'), true);
assert.equal(projectionEventsRequireRefresh([{ resource_ids: ['step-run/one'], body: { change: 'work.claimed' } }], 'Chat'), true);
assert.equal(projectionEventsRequireRefresh([{ resource_ids: ['step-run/one'], body: { change: 'work.claimed' } }], 'Fleet'), true);
assert.equal(projectionEventsRequireRefresh([{ resource_ids: ['agent/one'], body: { change: 'harness.observed', state: 'idle' } }], 'Chat'), false);
assert.equal(projectionEventsRequireRefresh([{ resource_ids: ['agent/one'], body: { change: 'harness.observed', state: 'failed' } }], 'Chat'), true);
assert.deepEqual(tabsChangedByProjectionEvents([]), []);
assert.deepEqual(tabsChangedByProjectionEvents([{ resource_ids: ['session/current'], body: {} }]), []);
assert.deepEqual(tabsChangedByProjectionEvents([{ resource_ids: ['mission/current'], body: {} }]), ['Control']);
assert.deepEqual(tabsChangedByProjectionEvents([{ resource_ids: ['message/new'], body: {} }]), ['Now', 'Chat']);
assert.deepEqual(tabsChangedByProjectionEvents([{ resource_ids: ['step-run/current'], body: { change: 'work.claimed' } }]), ['Chat', 'Control', 'Fleet']);
