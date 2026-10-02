import assert from 'node:assert/strict';
import { accounts, byOf, cost, groups, label, limitLine, money, nextBy, periodName, tokens, totalOf } from './usage.ts';

// The same day of invented spend stui's demo shows (crates/stui/src/ui/demo.rs).
const row = (agent, run, step, model, host, account, total, output, cost) => ({
  agent, mission_run: run, step, model, account, host, pricing: 'list',
  total_tokens: total, input_tokens: total / 50, output_tokens: output, cache_write_tokens: total / 20,
  cache_write_1h_tokens: 0, cached_tokens: total - total / 50 - output - total / 20,
  cost_microusd: cost, reported_cost_microusd: 0, unpriced_tokens: 0,
});
const atlas = 'mission-run/fleet/atlas/store-move/2026-10-01';
const rows = [
  row('agent/example/atlas/builder', atlas, 'step-run/atlas-1/compare', 'claude-opus-5-5', 'lark', 'anthropic', 182_000_000, 410_000, 41_800_000),
  row('agent/example/atlas/indexer', atlas, 'step-run/atlas-1/index', 'claude-sonnet-5-5', 'lark', 'anthropic', 96_000_000, 220_000, 9_300_000),
  row('agent/example/atlas/builder', atlas, 'step-run/atlas-1/cut-over', 'claude-opus-5-5', 'lark', 'anthropic', 31_000_000, 90_000, 7_100_000),
  row('agent/example/cos', undefined, undefined, 'claude-opus-5-5', 'lark', 'anthropic', 64_000_000, 150_000, 14_600_000),
  row('agent/example/release/captain', 'mission-run/fleet/release/weekly/2026-10-01', 'step-run/release-1/tag', 'gpt-5.5', 'wren', 'openai', 12_000_000, 60_000, 2_400_000),
  row('agent/example/harbor/reviewer', undefined, undefined, 'claude-sonnet-5-5', 'wren', undefined, 22_000_000, 70_000, 2_100_000),
];
const names = { agents: new Map([['agent/example/atlas/builder', 'Atlas Builder']]), missions: new Map([['mission/fleet/atlas/store-move', 'Move the atlas store']]) };

// Groups rank by cost and name what st does not know.
const byAgent = groups(rows, 'agent');
assert.equal(byAgent[0].id, 'agent/example/atlas/builder');
assert.equal(byAgent[0].total.costMicrousd, 48_900_000);
const byMission = groups(rows, 'mission');
assert.equal(byMission[0].id, 'mission/fleet/atlas/store-move');
assert.equal(cost(byMission[0].total), '$58.20');
assert.ok(byMission.some(group => group.id === 'usage/no-mission'));
assert.equal(label('usage/no-mission', names, rows), 'no mission (standing seats)');
assert.equal(label('step-run/atlas-1/compare', names, rows), 'Move the atlas store · compare');
assert.equal(label('account/claude/a1541a035f12ee1b', names, rows), 'claude · a1541a03');
assert.equal(byOf('usage/no-mission'), 'mission');
assert.equal(byOf('step-run/atlas-1/compare'), 'step');
assert.equal(nextBy('host'), 'agent');

// Money and tokens read at a glance; unpriced tokens mark the cost with a +.
assert.equal(money(0), '$0');
assert.equal(money(4_000), '<$0.01');
assert.equal(money(412_600_000), '$413');
assert.equal(tokens(1_240_000), '1.2M');
assert.equal(tokens(328_915_253), '329M');
assert.equal(cost({ ...totalOf([]), costMicrousd: 1_000_000, unpriced: 5 }), '$1.00+');
assert.equal(periodName(168), 'the last 7 days');

// Accounts: every account that spent or reported limits, spend with no account last.
const now = 1_800_000_000_000;
const limits = [{ account: 'anthropic', driver: 'claude', weekly_percent: 92, weekly_resets_at_unix_ms: now + 3 * 86_400_000, five_hour_percent: 41, measured_at_unix_ms: now - 180_000, measured_by: 'agent/example/atlas/builder', host: 'lark', seats: [] },
  { account: 'zeta', driver: 'codex', weekly_percent: 6, measured_at_unix_ms: now - 3_600_000, measured_by: 'agent/example/release/captain', host: 'wren', seats: [] }];
const listed = accounts(rows, limits);
assert.deepEqual(listed.map(entry => entry.id), ['account/anthropic', 'account/openai', 'account/zeta', 'usage/no-account']);
assert.equal(listed[0].limit?.account, 'anthropic');
const line = limitLine(limits[0], now);
assert.deepEqual(line.weekly, { text: '92%', tone: 'fault' });
assert.equal(line.resets, 'resets in 3d');
assert.equal(line.measured, 'measured 3m ago');
assert.deepEqual(limitLine(limits[1], now).fiveHour, { text: '?', tone: 'quiet' });
