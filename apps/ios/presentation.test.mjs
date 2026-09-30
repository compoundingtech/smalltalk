import assert from 'node:assert/strict';
import { agentHeaderDetail, agentHealth, ago, attentionActionLabel, attentionHeadline, attentionKindLabel, currentWorkSummary, deviceDetail, deviceTitle, missionDetail, missionGroup, missionLabels, missionSteps, pingPresentation, queuedWorkSummary, smallTalkPresentation } from './presentation.ts';

const now = Date.parse('2026-09-25T08:25:00Z');

// (1) An unreadable attention projection must never read as an empty inbox.
assert.deepEqual(attentionHeadline({ count: 0, loaded: true }), { text: 'Nothing needs your attention.', warning: false });
assert.deepEqual(attentionHeadline({ count: 7, loaded: true }), { text: '7 actionable items', warning: false });
assert.deepEqual(attentionHeadline({ count: 0, loaded: true, error: 'forbidden: device inventory requires an explicitly authenticated person' }), { text: 'Attention could not be loaded: forbidden: device inventory requires an explicitly authenticated person', warning: true });
assert.deepEqual(attentionHeadline({ count: 0, loaded: false }), { text: 'Attention has not loaded yet.', warning: true });
assert.deepEqual(attentionHeadline({ count: 3, loaded: true, error: 'offline' }), { text: '3 actionable items from the last load · refresh failed: offline', warning: true });


// (2) Queued work shows how long the next step has been waiting, from the queue st joined into the
// agent's row: no work list is read.
const route = { id: 'step-run/78cb/route', mission_id: 'mission/fleet/pty-rust/route', mission_run_id: 'mission-run/78cb', path: 'route', state: 'ready', since: '2026-09-24T11:56:00Z', goal: 'Route output' };
const pty = { id: 'agent/example/pty-rust/standing/example-linux.pty-rust', name: 'fleet/pty-rust/standing/example-linux.pty-rust', next_work_id: route.id, next_work: route, upcoming_work: [route], queued_work_count: 1 };
assert.equal(queuedWorkSummary(pty, now), 'Next: route · 1 queued · oldest ready 20h');
assert.equal(queuedWorkSummary({ ...pty, next_work_id: null, next_work: null, upcoming_work: [], queued_work_count: 0 }, now), null);
assert.equal(queuedWorkSummary({ ...pty, next_work: { ...route, state: 'blocked' }, upcoming_work: [] }, now), 'Next: route · 1 queued');
// An older st without joined labels still names the next step by its ID.
assert.equal(queuedWorkSummary({ next_work_id: 'step-run/78cb/route', queued_work_count: 2 }, now), 'Next: route · 2 queued');
assert.equal(currentWorkSummary({ current_work: [{ ...route, path: 'review/diff', title: null, state: 'claimed' }] }), 'Current: diff (claimed)');
assert.equal(currentWorkSummary({ current_work: [{ ...route, title: 'Route output', state: 'claimed' }] }), 'Current: Route output (claimed)');
assert.equal(currentWorkSummary({}), null);

// Control groups and describes missions from the steps st joined into each run: no work list.
const step = (path, state) => ({ id: `step-run/9f/${path}`, path, state, attempt: 1, since: '2026-09-25T08:00:00Z' });
const release = { id: 'mission/fleet/app/release', title: 'fleet/app/release', state: 'running', runs: ['mission-run/9f'], visualization: { nodes: [{ kind: 'step' }, { kind: 'step' }, { kind: 'group' }], groups: [] }, run_details: [{ id: 'mission-run/9f', steps: [step('build', 'completed'), step('review', 'claimed')] }] };
assert.deepEqual(missionSteps(release).map(s => s.path), ['build', 'review']);
assert.equal(missionGroup(release), 'Running');
assert.equal(missionDetail(release), '1 runs · 2 planned steps · review (claimed)');
const stuck = { ...release, run_details: [{ id: 'mission-run/9f', steps: [step('build', 'claimed'), step('sign', 'blocked')] }] };
assert.equal(missionGroup(stuck), 'Blocked');
assert.equal(missionDetail(stuck), '1 runs · 2 planned steps · sign (blocked)');
assert.equal(missionGroup({ ...release, run_details: [{ id: 'mission-run/9f', steps: [step('approve', 'waiting')] }] }), 'Waiting');
assert.equal(missionGroup({ ...release, state: 'completed', run_details: [{ id: 'mission-run/9f', steps: null }] }), 'Archive');
assert.equal(missionGroup({ state: 'draft' }), 'Drafts');
assert.equal(missionDetail({ runs: [], run_details: [] }), '0 runs');
const shipped = { runs: ['mission-run/9f'], run_details: [{ id: 'mission-run/9f', steps: [step('gate', 'failed')], outcome: { status: 'completed', previous_status: 'failed', actor: 'person/avery', reason: 'it merged after the gate was fixed', at: '2026-09-25T08:20:00Z' } }] };
assert.equal(missionDetail(shipped), '1 runs · gate (failed) · set completed (was failed) by person/avery: it merged after the gate was fixed');

// Small Talk st joined into a conversation shows who wrote to whom and its title.
assert.deepEqual(smallTalkPresentation({ message_id: 'message/1', from: 'agent/example/cos/standing/cos', to: 'person/alex', title: 'Release is ready' }), { from: 'COS → alex', text: 'Release is ready' });
assert.deepEqual(smallTalkPresentation({ message_id: 'message/2', from: 'agent/example/app' }), { from: 'App → someone', text: 'Small Talk' });

assert.equal(ago('2026-09-25T08:24:30Z', now), '30s');
assert.equal(ago('2026-09-25T07:25:00Z', now), '1h');
assert.equal(ago('2026-09-22T08:25:00Z', now), '3d');
assert.equal(ago('not a time', now), 'unknown');

// (3) A crash-looping seat must stand out rather than read as a normal row.
const failing = { id: 'agent/example/st3/standing/st3', name: 'fleet/st3/standing/st3', state: 'failed', driver: 'codex', harness_state: 'ended', updated_at: '2026-09-25T08:14:00Z', operational: { layer: 'current', actionable: false, reasons: ['unhealthy'] } };
assert.deepEqual(agentHealth(failing), { healthy: false, label: 'Failed · harness ended · unhealthy' });
assert.deepEqual(agentHealth({ ...failing, state: 'starting', harness_state: null }), { healthy: false, label: 'Starting · unhealthy' });
assert.deepEqual(agentHealth({ ...failing, state: 'running', harness_state: 'working', operational: { layer: 'current', actionable: true, reasons: [] } }), { healthy: true, label: 'Running · working' });

// (7) The agent header shows harness, state, and the observation's age.
assert.equal(agentHeaderDetail(failing, now), 'codex · Failed · harness ended · unhealthy · observed 11m ago');
assert.equal(agentHeaderDetail({ ...failing, driver: null, state: 'running', harness_state: 'ready', operational: undefined }, now), 'harness · Running · ready · observed 11m ago');

// (9) Devices are named, and the connected device is identified.
const mine = { id: 'device/925c2c443540582eb62871f8', person_id: 'person/alex', session_actor: 'person/alex/session/925c', state: 'active', updated_at: '2026-09-24T10:11:34Z', expires_at: '2026-10-24T10:11:27Z' };
const other = { ...mine, id: 'device/df90eb3d61ba882eb1ee9e14', session_actor: 'person/alex/session/df90' };
assert.equal(deviceTitle({ ...mine, name: 'iPhone' }, 'person/alex/session/925c'), 'iPhone · this device');
assert.equal(deviceTitle(mine, 'person/alex/session/925c'), 'This device');
assert.equal(deviceTitle(other, 'person/alex/session/925c'), 'Paired device df90eb3d');
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
assert.equal(labels.get('mission/fleet/st3/issue-triage'), 'Issue Triage · ST');
assert.equal(labels.get('mission/fleet/app-apple/issue-triage'), 'Issue Triage · App Apple');
assert.equal(labels.get('mission/fleet/app-apple/deploy'), 'Deploy');
assert.equal(labels.get('mission/fleet/st3/tui-ios-fixes'), 'TUI iOS Fixes');
assert.equal(labels.get('mission/fleet/st3'), 'ST');
assert.equal(new Set(labels.values()).size, labels.size);

// (11) Actions and kinds are presented in words, never as raw action IDs.
assert.equal(attentionActionLabel('attention.resolve'), 'Resolve');
assert.equal(attentionActionLabel('mission.approve-revision'), 'Approve revision');
assert.equal(attentionActionLabel('review.request-changes'), 'Request changes');
assert.equal(attentionKindLabel('human-gate'), 'Needs a decision');
assert.equal(attentionKindLabel('agent-request'), 'Agent request');
for (const action of ['attention.resolve', 'review.approve', 'review.reject', 'review.request-changes', 'launch.approve', 'launch.cancel', 'mission.approve-revision', 'mission.cancel-revision', 'message.read']) assert.doesNotMatch(attentionActionLabel(action), /\./);

// (6) Delivery envelopes read as a message, without the unknown marker or raw IDs.
assert.deepEqual(pingPresentation('[PING] ? agent/example/cos/standing/cos: Reclaim bounded reads; TUI and iOS now have their own seats [id:message/2d1268c28b3151be]'), { from: 'COS', text: 'Reclaim bounded reads; TUI and iOS now have their own seats' });
assert.deepEqual(pingPresentation('[PING] ← person/alex: Ship it [id:message/abc]'), { from: 'alex', text: 'Ship it' });
assert.deepEqual(pingPresentation('Plain text [id:message/abc]'), { from: null, text: 'Plain text' });
assert.deepEqual(pingPresentation('No envelope here'), { from: null, text: 'No envelope here' });
