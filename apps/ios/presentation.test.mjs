import assert from 'node:assert/strict';
import { agentHeaderDetail, agentHealth, ago, attentionActionLabel, attentionHeadline, attentionKindLabel, deviceDetail, deviceTitle, missionLabels, pingPresentation, queuedWorkSummary } from './presentation.ts';

const now = Date.parse('2026-09-25T08:25:00Z');

// (1) An unreadable attention projection must never read as an empty inbox.
assert.deepEqual(attentionHeadline({ count: 0, loaded: true }), { text: 'Nothing needs your attention.', warning: false });
assert.deepEqual(attentionHeadline({ count: 7, loaded: true }), { text: '7 actionable items', warning: false });
assert.deepEqual(attentionHeadline({ count: 0, loaded: true, error: 'forbidden: device inventory requires an explicitly authenticated person' }), { text: 'Attention could not be loaded: forbidden: device inventory requires an explicitly authenticated person', warning: true });
assert.deepEqual(attentionHeadline({ count: 0, loaded: false }), { text: 'Attention has not loaded yet.', warning: true });
assert.deepEqual(attentionHeadline({ count: 3, loaded: true, error: 'offline' }), { text: '3 actionable items from the last load · refresh failed: offline', warning: true });

// (2) Queued work shows how long the next step has been waiting.
const pty = { id: 'agent/fleet/pty-rust/standing/hetz.pty-rust', name: 'fleet/pty-rust/standing/hetz.pty-rust', next_work_id: 'step-run/78cb/route', upcoming_work_ids: ['step-run/78cb/route'], queued_work_count: 1 };
const work = [{ id: 'step-run/78cb/route', path: 'route', state: 'ready', updated_at: '2026-09-24T11:56:00Z' }];
assert.equal(queuedWorkSummary(pty, work, now), 'Next: route · 1 queued · oldest ready 20h');
assert.equal(queuedWorkSummary({ ...pty, next_work_id: null, queued_work_count: 0 }, work, now), null);
assert.equal(queuedWorkSummary(pty, [], now), 'Next: route · 1 queued');
assert.equal(ago('2026-09-25T08:24:30Z', now), '30s');
assert.equal(ago('2026-09-25T07:25:00Z', now), '1h');
assert.equal(ago('2026-09-22T08:25:00Z', now), '3d');
assert.equal(ago('not a time', now), 'unknown');

// (3) A crash-looping seat must stand out rather than read as a normal row.
const failing = { id: 'agent/fleet/st3/standing/st3', name: 'fleet/st3/standing/st3', state: 'failed', driver: 'codex', harness_state: 'ended', updated_at: '2026-09-25T08:14:00Z', operational: { layer: 'current', actionable: false, reasons: ['unhealthy'] } };
assert.deepEqual(agentHealth(failing), { healthy: false, label: 'Failed · harness ended · unhealthy' });
assert.deepEqual(agentHealth({ ...failing, state: 'starting', harness_state: null }), { healthy: false, label: 'Starting · unhealthy' });
assert.deepEqual(agentHealth({ ...failing, state: 'running', harness_state: 'working', operational: { layer: 'current', actionable: true, reasons: [] } }), { healthy: true, label: 'Running · working' });

// (7) The agent header shows harness, state, and the observation's age.
assert.equal(agentHeaderDetail(failing, now), 'codex · Failed · harness ended · unhealthy · observed 11m ago');
assert.equal(agentHeaderDetail({ ...failing, driver: null, state: 'running', harness_state: 'ready', operational: undefined }, now), 'harness · Running · ready · observed 11m ago');

// (9) Devices are named, and the connected device is identified.
const mine = { id: 'device/925c2c443540582eb62871f8', person_id: 'person/nathan', session_actor: 'person/nathan/session/925c', state: 'active', updated_at: '2026-09-24T10:11:34Z', expires_at: '2026-10-24T10:11:27Z' };
const other = { ...mine, id: 'device/df90eb3d61ba882eb1ee9e14', session_actor: 'person/nathan/session/df90' };
assert.equal(deviceTitle({ ...mine, name: 'iPhone' }, 'person/nathan/session/925c'), 'iPhone · this device');
assert.equal(deviceTitle(mine, 'person/nathan/session/925c'), 'This device');
assert.equal(deviceTitle(other, 'person/nathan/session/925c'), 'Paired device df90eb3d');
assert.notEqual(deviceTitle(mine, 'x'), deviceTitle(other, 'x'));
assert.equal(deviceDetail(mine, now), 'active · paired 22h ago · expires 2026-10-24');

// (10) Missions with the same leaf name are told apart by their project.
const labels = missionLabels([
  { id: 'mission/fleet/st3/issue-triage', title: 'fleet/st3/issue-triage' },
  { id: 'mission/fleet/app-apple/issue-triage', title: 'fleet/app-apple/issue-triage' },
  { id: 'mission/fleet/app-apple/deploy', title: 'fleet/app-apple/deploy' },
  { id: 'mission/fleet/st3/tui-ios-fixes', title: 'fleet/st3/tui-ios-fixes' },
  { id: 'mission/fleet/st3', title: 'fleet/st3' },
  { id: 'mission/__st3/copilot-1468/loop/green/round', title: '__st3/copilot-1468/loop/green/round' },
  { id: 'mission/__st3/refresh-1445/loop/green/round', title: '__st3/refresh-1445/loop/green/round' },
]);
assert.equal(labels.get('mission/fleet/st3/issue-triage'), 'Issue Triage · ST3');
assert.equal(labels.get('mission/fleet/app-apple/issue-triage'), 'Issue Triage · App Apple');
assert.equal(labels.get('mission/fleet/app-apple/deploy'), 'Deploy');
assert.equal(labels.get('mission/fleet/st3/tui-ios-fixes'), 'TUI iOS Fixes');
assert.equal(labels.get('mission/fleet/st3'), 'ST3');
assert.equal(new Set(labels.values()).size, labels.size);

// (11) Actions and kinds are presented in words, never as raw action IDs.
assert.equal(attentionActionLabel('attention.resolve'), 'Resolve');
assert.equal(attentionActionLabel('mission.approve-revision'), 'Approve revision');
assert.equal(attentionKindLabel('human-gate'), 'Needs a decision');
for (const action of ['attention.resolve', 'review.approve', 'review.reject', 'launch.approve', 'launch.cancel', 'mission.approve-revision', 'mission.cancel-revision', 'message.read']) assert.doesNotMatch(attentionActionLabel(action), /\./);

// (6) Delivery envelopes read as a message, without the unknown marker or raw IDs.
assert.deepEqual(pingPresentation('[PING] ? agent/fleet/cos/standing/cos: Reclaim bounded reads; TUI and iOS now have their own seats [id:message/2d1268c28b3151be]'), { from: 'COS', text: 'Reclaim bounded reads; TUI and iOS now have their own seats' });
assert.deepEqual(pingPresentation('[PING] ← person/nathan: Ship it [id:message/abc]'), { from: 'nathan', text: 'Ship it' });
assert.deepEqual(pingPresentation('Plain text [id:message/abc]'), { from: null, text: 'Plain text' });
assert.deepEqual(pingPresentation('No envelope here'), { from: null, text: 'No envelope here' });
