import { child } from '../kit/context.ts'
import { agent } from '../kit/factories/agent.ts'
import { diff } from '../kit/factories/diff.ts'
import { terminalRecord } from '../kit/factories/terminalRecord.ts'
import { terminalRun } from '../kit/factories/terminalRun.ts'
import { toolCall } from '../kit/factories/toolCall.ts'
import { thread, turn } from '../kit/factories/turn.ts'
import * as resources from '../kit/resources.ts'
import type { Slice } from '../kit/slice.ts'
import { liveSync } from '../kit/variants.ts'
import type { WorldDefinition } from '../kit/world.ts'

const file = 'packages/cache/policy.ts'
const base = 'export const cachePolicy = { ttl: 60, stale: false };\n'
const ours = 'export const cachePolicy = { ttl: 300, stale: false };\n'
const theirs = 'export const cachePolicy = { ttl: 60, stale: true };\n'
const resolved = 'export const cachePolicy = { ttl: 300, stale: true };\n'
const conflict = `<<<<<<< ours\n${ours}||||||| base\n${base}=======\n${theirs}>>>>>>> theirs\n`
const decision = 'Keep the 300-second TTL from builder and stale reads from reviewer. Combine both changes, run the cache tests, then land the patch.'

/** Both branches share one base; review chooses the combined behavior rather than a winner. */
export const mergeConflictStandoff: WorldDefinition = {
  id: 'merge-conflict-standoff', title: 'Merge conflict standoff', seed: 5,
  narrative: 'Two agents change the cache policy on the same line. A person reviews the three-way conflict and approves a combined patch.',
  cast: { project: 'harbor', roles: ['builder', 'reviewer'], hosts: 1, people: 1, missions: [{ slug: 'cache-policy', title: 'Improve the cache policy', steps: ['Increase the cache TTL', 'Allow stale cache reads'] }] },
  slices: (ctx) => {
    const members = ctx.cast.agents
    const builder = members[0]!
    const reviewer = members[1]!
    const person = ctx.cast.people[0]!
    const mission = ctx.cast.missions[0]!
    const source = { _tag: 'synthetic' as const, seed: 5 }
    const agents = members.map((member, step) => agent(child(ctx, member.key), member, { state: 'waiting', sinceMs: -180_000, lastActivityMs: -5_000, harnessState: 'idle', ask: 'Adjudicate the cache policy conflict', mission, step, workState: 'waiting-person', workSinceMs: -5_000 }))
    const runtimes = agents.flatMap((value) => value.runtime ?? [])
    const machine = resources.machine(ctx, builder.host, runtimes.map((value) => value.id), mission.steps.map((step) => step.work))
    const roster: Slice<'roster'> = {
      kind: 'roster', variant: 'default', source, decode: 'strict', loading: false,
      state: { agents: agents.map((value) => value.agent), runtimes, machines: [machine], order: members.map((member) => member.id) },
      timeline: [
        { _tag: 'changes', at_ms: 8_000, store: 0, upserts: [{ ...machine, revision: 'mh-terminal', updated_at: ctx.t.at(8_000), runtime_ids: [...machine.runtime_ids, builder.terminalRuntime], occupancy: { running_runtimes: 3 } }], removes: [] },
        { _tag: 'changes', at_ms: 18_000, store: 0, upserts: members.flatMap((member, step) => {
          const done = agent(child(ctx, `${member.key}/done`), member, { state: 'waiting', sinceMs: -180_000, lastActivityMs: 18_000, harnessState: 'idle', mission, step, workState: 'completed', workSinceMs: 18_000 })
          return [done.agent, done.runtime!]
        }), removes: [] },
      ],
    }
    const details: Slice<'details'> = { kind: 'details', variant: 'default', source, decode: 'strict', loading: false, state: { missions: [resources.mission(ctx, mission, 'running', -5_000)], work: members.map((claimant, step) => resources.work(ctx, { mission, step, claimant, state: 'waiting-person', updatedMs: -5_000, goals: [step === 0 ? 'TTL is 300 seconds' : 'Stale reads are allowed'] })) }, timeline: [{ _tag: 'changes', at_ms: 18_000, store: 0, upserts: [resources.mission(ctx, mission, 'completed', 18_000), ...members.map((claimant, step) => resources.work(ctx, { mission, step, claimant, state: 'completed', updatedMs: 18_000, goals: [step === 0 ? 'TTL is 300 seconds' : 'Stale reads are allowed'] }))], removes: [] }] }
    const cards = members.map((requester) => resources.attention(ctx, { key: `review-${requester.key}`, kind: 'agent-request', person, requester, source: requester.id, mission, title: 'Review the cache policy three-way conflict', detail: `${file}\n${conflict}`, priority: 'high', requestedMs: -5_000, actions: ['review.approve', 'review.request-changes', 'custom.reply'] }))
    const attention: Slice<'attention'> = { kind: 'attention', variant: 'default', source, decode: 'strict', loading: false, state: { attention: cards, messages: [] }, timeline: [{ _tag: 'changes', at_ms: 2_000, store: 0, upserts: members.map((member) => resources.message(ctx, { key: `decision-${member.key}`, from: person.id, to: member.id, title: 'Combined cache policy approved', content: decision, sentMs: 2_000 })), removes: cards.map((card) => card.id) }] }
    const c = child(ctx, 'conversation')
    const cursors = members.map((member) => thread(member))
    const threads = members.map((member, index) => {
      const cursor = cursors[index]!
      const branch = index === 0 ? ours : theirs
      const items = turn(c, cursor, { atMs: -120_000, from: { _tag: 'person', person }, text: index === 0 ? 'Increase the cache TTL to 300 seconds.' : 'Allow stale reads without changing the TTL.', stepMs: 10_000, steps: [{ _tag: 'entries', build: (atMs) => diff(c, cursor, { atMs, file, before: base, after: branch }) }, { _tag: 'entries', build: (atMs) => toolCall(c, cursor, { atMs, name: 'shell', arguments: { command: 'git merge cache-policy', path: file, base, ours, theirs }, outcome: 'error', output: `CONFLICT (content): Merge conflict in ${file}\n${conflict}`, durationMs: 500 }) }, { _tag: 'say', text: 'Both edits change the same line. I sent the shared base/ours/theirs conflict for review; neither branch is landed.' }], status: 'waiting' })
      return { agent: member.id, session_id: member.session, items, page_size: 50, has_more: false }
    })
    const resolution = members.flatMap((member, index) => {
      const cursor = cursors[index]!
      const items = turn(c, cursor, { atMs: 2_000, from: { _tag: 'person', person }, text: decision, stepMs: 3_000, steps: index === 0 ? [{ _tag: 'entries', build: (atMs) => diff(c, cursor, { atMs, file, before: conflict, after: resolved }) }, { _tag: 'entries', build: (atMs) => toolCall(c, cursor, { atMs, name: 'shell', arguments: { command: 'pnpm test cache && git add packages/cache/policy.ts && git commit -m "Combine cache TTL and stale reads"' }, outcome: 'ok', output: '12 cache tests passed; 0 failures\n[cache-policy c0ffee1] Combine cache TTL and stale reads', durationMs: 2_000 }) }, { _tag: 'say', text: 'The combined patch landed as c0ffee1: ttl 300, stale true. All 12 cache tests passed.' }] : [{ _tag: 'say', text: 'Reviewed the combined policy: ttl 300 and stale true preserve both goals. Builder can land the approved patch.' }], status: 'completed' })
      return items.map((item) => ({ _tag: 'entries' as const, at_ms: Date.parse(item.timestamp) - ctx.t.now, store: 0, agent: member.id, items: [item] }))
    }).sort((a, b) => a.at_ms - b.at_ms)
    const conversation: Slice<'conversation'> = { kind: 'conversation', variant: 'default', source, decode: 'strict', loading: false, state: { threads }, timeline: resolution }
    const run = terminalRun(child(ctx, 'terminal'), builder, { startedAtMs: 8_000, command: 'pnpm test cache && git commit', lines: ['\u001b[32m12 cache tests passed; 0 failures\u001b[0m', '[cache-policy c0ffee1] Combine cache TTL and stale reads'], exit: 0 })
    const terminal: Slice<'terminal'> = { kind: 'terminal', variant: 'default', source, decode: 'strict', loading: false, state: { terminals: [] }, timeline: [{ _tag: 'terminal-create', at_ms: 8_000, store: 0, record: terminalRecord(ctx, builder, run, 8_000) }, ...run.screens.map(({ at_ms, screen }) => ({ _tag: 'screen' as const, at_ms, store: 0, terminal: builder.terminal, screen }))] }
    return { roster, details, attention, conversation, terminal, sync: liveSync(ctx, { conversation }) }
  },
}
