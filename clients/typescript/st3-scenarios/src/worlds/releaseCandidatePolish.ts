import { child } from '../kit/context.ts'
import { agent } from '../kit/factories/agent.ts'
import { diff } from '../kit/factories/diff.ts'
import { terminalRun } from '../kit/factories/terminalRun.ts'
import { terminalRecord } from '../kit/factories/terminalRecord.ts'
import { thread, turn } from '../kit/factories/turn.ts'
import * as resources from '../kit/resources.ts'
import type { Slice } from '../kit/slice.ts'
import { liveSync } from '../kit/variants.ts'
import type { WorldDefinition } from '../kit/world.ts'

/** A finished release candidate: no blocked work and no manufactured urgent review cards. */
export const releaseCandidatePolish: WorldDefinition = {
  id: 'release-candidate-polish', title: 'Release candidate polish', seed: 7,
  narrative: 'The release candidate is ready: a patch version and changelog are committed, the verification matrix is green, and the team has gone idle.',
  cast: { project: 'meadow', roles: ['builder', 'reviewer', 'docs'], hosts: 2, people: 1,
    missions: [{ slug: 'release-candidate', title: 'Polish the 0.8.4 release candidate', steps: ['Bump the patch version', 'Write the changelog', 'Verify the release matrix'] }] },
  slices: (ctx) => {
    const source = { _tag: 'synthetic' as const, seed: 7 }
    const builder = ctx.cast.agents[0]!
    const mission = ctx.cast.missions[0]!
    const generated = ctx.cast.agents.map((member) => agent(child(ctx, member.key), member,
      { state: 'waiting', sinceMs: -120_000, lastActivityMs: -120_000, harnessState: 'idle' }))
    const runtimes = generated.flatMap((value) => value.runtime ?? [])
    const c = child(ctx, 'conversation')
    const cursor = thread(builder)
    const items = turn(c, cursor, { atMs: -12 * 60_000, from: { _tag: 'person', person: ctx.cast.people[0]! },
      text: 'Prepare the 0.8.4 candidate. Limit changes to the patch version and release notes, then verify the matrix.',
      steps: [
        { _tag: 'entries', build: (atMs) => diff(c, cursor, { atMs, file: 'package.json', before: '{ "version": "0.8.3" }\n', after: '{ "version": "0.8.4" }\n' }) },
        { _tag: 'entries', build: (atMs) => diff(c, cursor, { atMs, file: 'CHANGELOG.md', before: '# Changelog\n', after: '# Changelog\n\n## 0.8.4\n- Preserve selected tabs when reopening the settings panel.\n- Clarify the offline recovery message.\n' }) },
        { _tag: 'say', text: 'The 0.8.4 candidate is ready. Linux and macOS unit tests, typecheck, lint, package smoke tests and release-note checks all passed. No API changes; no remaining release gates. I am idle until the release is tagged.' },
      ], stepMs: 60_000, status: 'completed' })
    const conversation: Slice<'conversation'> = { kind: 'conversation', variant: 'default', source, decode: 'strict', loading: false,
      state: { threads: [{ agent: builder.id, session_id: builder.session, items, page_size: 50, has_more: false }] }, timeline: [] }
    const startedMs = -7 * 60_000
    const run = terminalRun(child(ctx, 'release-matrix'), builder, { startedAtMs: startedMs, command: 'pnpm release:verify --version 0.8.4',
      lines: ['Release candidate 0.8.4', 'linux   unit tests       PASS (184 tests)', 'macos   unit tests       PASS (184 tests)', 'all     typecheck        PASS', 'all     lint             PASS', 'all     package smoke    PASS', 'all     release notes    PASS', 'Matrix: 6/6 checks passed', 'Exit code: 0'] })
    const record = terminalRecord(ctx, builder, run, startedMs)
    return {
      roster: { kind: 'roster', variant: 'default', source, decode: 'strict', loading: false,
        state: { agents: generated.map((value) => value.agent), runtimes, order: ctx.cast.agents.map((member) => member.id),
          machines: ctx.cast.hosts.map((host) => resources.machine(ctx, host, runtimes.filter((runtime) => runtime.owner_host_id === host.id).map((runtime) => runtime.id), [])) }, timeline: [] },
      details: { kind: 'details', variant: 'default', source, decode: 'strict', loading: false,
        state: { missions: [resources.mission(ctx, mission, 'completed', -120_000)],
          work: mission.steps.map((step, index) => resources.work(ctx, { mission, step: index, state: 'completed', updatedMs: -120_000, claimant: ctx.cast.agents[index]!, goals: [step.title] })) }, timeline: [] },
      attention: { kind: 'attention', variant: 'default', source, decode: 'strict', loading: false, state: { attention: [], messages: [] }, timeline: [] },
      conversation,
      terminal: { kind: 'terminal', variant: 'default', source, decode: 'strict', loading: false,
        state: { terminals: [{ ...record, runtime: { ...record.runtime, state: 'exited' } }] }, timeline: [] },
      sync: { ...liveSync(ctx, { conversation }), variant: 'default', source },
    }
  },
}
