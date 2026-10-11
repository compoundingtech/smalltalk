import type { TimelineEntry } from '@smalltalk/st3-client'

import type { CastAgent, CastPerson } from '../kit/cast.ts'
import { child, type FactoryContext } from '../kit/context.ts'
import { agent } from '../kit/factories/agent.ts'
import { diff } from '../kit/factories/diff.ts'
import { terminalRun } from '../kit/factories/terminalRun.ts'
import { terminalRecord } from '../kit/factories/terminalRecord.ts'
import { toolCall } from '../kit/factories/toolCall.ts'
import { entry, revise, thread, turn } from '../kit/factories/turn.ts'
import * as resources from '../kit/resources.ts'
import type { ConversationEvent, Slice, TerminalRecord } from '../kit/slice.ts'
import { liveSync } from '../kit/variants.ts'
import * as vocabulary from '../kit/vocabulary/index.ts'
import type { Slices, WorldDefinition } from '../kit/world.ts'

const synthetic = (seed: number) => ({ _tag: 'synthetic' as const, seed })

export const SEED = 1

/** Four agents rename an API across a monorepo: blocked, waiting for review, streaming diffs, idle. */
export const fleetMidRefactor: WorldDefinition = {
  id: 'fleet-mid-refactor',
  title: 'Fleet mid-refactor',
  narrative:
    'Four agents rename an API across a monorepo. One is blocked on a typecheck, one waits for review, one streams diffs, one is idle.',
  seed: SEED,
  cast: {
    project: 'atlas',
    roles: ['builder', 'reviewer', 'migrator', 'docs'],
    hosts: 2,
    people: 2,
    missions: [
      {
        slug: 'load-user-rename',
        title: 'Rename fetchUser to loadUser',
        steps: [
          'Rename fetchUser in packages/api',
          'Update call sites in packages/web',
          'Fix the worker after the rename',
          'Review the rename',
          'Update the users API reference',
        ],
      },
    ],
  },
  slices: (ctx) => build(ctx),
}

const build = (ctx: FactoryContext): Slices => {
  const [builder, reviewer, migrator, docs] = ctx.cast.agents as [CastAgent, CastAgent, CastAgent, CastAgent]
  const [ada, robin] = ctx.cast.people as [CastPerson, CastPerson]
  const mission = ctx.cast.missions[0]!
  const source = synthetic(SEED)

  // roster
  const agents = [
    agent(child(ctx, 'agent/builder'), builder, {
      state: 'running',
      sinceMs: -38 * 60_000,
      lastActivityMs: -95_000,
      harnessState: 'blocked',
      blockedOn: 'pnpm -r typecheck fails in packages/worker',
      mission,
      step: 2,
      workState: 'blocked',
    }),
    agent(child(ctx, 'agent/reviewer'), reviewer, {
      state: 'waiting',
      sinceMs: -12 * 60_000,
      harnessState: 'idle',
      ask: 'Review the loadUser rename before the CLI changes',
      mission,
      step: 3,
      workState: 'waiting-person',
    }),
    agent(child(ctx, 'agent/migrator'), migrator, {
      state: 'running',
      sinceMs: -21 * 60_000,
      lastActivityMs: -4_000,
      harnessState: 'busy',
      mission,
      step: 1,
      workState: 'claimed',
      upcoming: [4],
    }),
    agent(child(ctx, 'agent/docs'), docs, { state: 'waiting', sinceMs: -55 * 60_000, harnessState: 'idle' }),
  ]
  const runtimes = agents.flatMap((value) => value.runtime ?? [])
  const machines = ctx.cast.hosts.map((host) =>
    resources.machine(
      ctx,
      host,
      runtimes.filter((runtime) => runtime.owner_host_id === host.id).map((runtime) => runtime.id),
      [],
    ),
  )
  const roster: Slice<'roster'> = {
    kind: 'roster',
    variant: 'default',
    source,
    decode: 'strict',
    loading: false,
    state: { agents: agents.map((value) => value.agent), runtimes, machines, order: agents.map((value) => value.agent.id!) },
    timeline: [
      {
        _tag: 'changes',
        at_ms: 6_000,
        store: 0,
        upserts: [
          agent(child(ctx, 'agent/docs/start'), docs, {
            state: 'running',
            sinceMs: 6_000,
            harnessState: 'busy',
            mission,
            step: 4,
            workState: 'claimed',
          }).agent,
        ],
        removes: [],
      },
    ],
  }

  // details
  const details: Slice<'details'> = {
    kind: 'details',
    variant: 'default',
    source,
    decode: 'strict',
    loading: false,
    state: {
      missions: [resources.mission(ctx, mission, 'running', -95_000)],
      work: [
        resources.work(ctx, { mission, step: 0, state: 'completed', updatedMs: -30 * 60_000, claimant: builder, goals: ['Rename the export and its tests'] }),
        resources.work(ctx, { mission, step: 1, state: 'claimed', updatedMs: -21 * 60_000, claimant: migrator, goals: ['Every web call site uses loadUser'] }),
        resources.work(ctx, {
          mission,
          step: 2,
          state: 'blocked',
          updatedMs: -95_000,
          claimant: builder,
          blockedReason: 'syncAvatars calls loadUser without the session',
          goals: ['pnpm -r typecheck passes'],
        }),
        resources.work(ctx, { mission, step: 3, state: 'waiting-person', updatedMs: -12 * 60_000, claimant: reviewer, goals: ['A person approves the rename'] }),
        resources.work(ctx, { mission, step: 4, state: 'ready', updatedMs: -12 * 60_000, goals: ['docs/api/users.md describes loadUser'] }),
      ],
    },
    timeline: [],
  }

  // attention
  const reviewCard = resources.attention(ctx, {
    key: 'review-rename',
    kind: 'agent-request',
    person: ada,
    source: reviewer.id,
    requester: reviewer,
    mission,
    title: 'Review the loadUser rename',
    detail: vocabulary.reviewComments[0],
    priority: 'high',
    requestedMs: -12 * 60_000,
    actions: ['custom.reply', 'review.approve', 'review.request-changes'],
  })
  const blockedCard = resources.attention(ctx, {
    key: 'worker-typecheck',
    kind: 'agent-request',
    person: robin,
    source: builder.id,
    requester: builder,
    mission,
    title: 'Worker typecheck fails after the rename',
    detail: 'syncAvatars calls loadUser without the session. Thread the session through, or keep a one-argument overload?',
    priority: 'normal',
    requestedMs: -95_000,
    actions: ['custom.reply'],
  })
  const attention: Slice<'attention'> = {
    kind: 'attention',
    variant: 'default',
    source,
    decode: 'strict',
    loading: false,
    state: {
      attention: [reviewCard, blockedCard],
      messages: [
        resources.message(ctx, {
          key: 'migrator-progress',
          from: migrator.id,
          to: ada.id,
          title: 'Web call sites',
          content: 'packages/web uses loadUser now; the typecheck is clean there.',
          sentMs: -6 * 60_000,
        }),
      ],
    },
    timeline: [],
  }

  // conversation
  const c = child(ctx, 'conversation')
  const builderThread = thread(builder)
  const builderItems: TimelineEntry[] = [
    ...turn(c, builderThread, {
      atMs: -9 * 60_000,
      from: { _tag: 'person', person: robin },
      text: vocabulary.personRequests[1],
      steps: [
        { _tag: 'say', text: vocabulary.assistantNotes[1] },
        {
          _tag: 'entries',
          build: (atMs) =>
            toolCall(c, builderThread, {
              atMs,
              name: 'shell',
              arguments: { command: vocabulary.shellRuns[0].command },
              outcome: 'error',
              output: vocabulary.shellRuns[0].lines.join('\n').replace(/\u001b\[[0-9;]*m/g, ''),
              durationMs: 41_000,
            }),
        },
        { _tag: 'say', text: 'The typecheck still fails in packages/worker: syncAvatars calls loadUser with one argument. I asked Robin whether to thread the session through.' },
      ],
      status: 'waiting',
      stepMs: 50_000,
    }),
  ]
  const migratorThread = thread(migrator)
  const migratorItems: TimelineEntry[] = turn(c, migratorThread, {
    atMs: -21 * 60_000,
    from: { _tag: 'person', person: ada },
    text: vocabulary.personRequests[0],
    steps: [
      { _tag: 'say', text: vocabulary.assistantNotes[0] },
      {
        _tag: 'entries',
        build: (atMs) => diff(c, migratorThread, { atMs, file: 'packages/web/src/profile/useProfile.ts', ...vocabulary.edits['packages/web/src/profile/useProfile.ts'] }),
      },
    ],
    stepMs: 60_000,
  })
  // Streaming after now: a revised content entry, then a diff.
  const streamingFirst = entry(c, migratorThread, {
    atMs: 2_000,
    role: 'assistant',
    type: 'content',
    final: false,
    body: { media_type: 'text/markdown', text: 'Next: packages/api/src/users/index.ts still' },
  })
  const migratorEvents: ConversationEvent[] = [
    { _tag: 'entries', at_ms: 2_000, store: 0, agent: migrator.id, items: [streamingFirst] },
    {
      _tag: 'entries',
      at_ms: 3_000,
      store: 0,
      agent: migrator.id,
      items: [
        revise(c, streamingFirst, 3_000, { media_type: 'text/markdown', text: 'Next: packages/api/src/users/index.ts still re-exports fetchUser. Replacing the export.' }, true),
      ],
    },
    {
      _tag: 'entries',
      at_ms: 5_000,
      store: 0,
      agent: migrator.id,
      items: diff(c, migratorThread, { atMs: 5_000, file: 'packages/api/src/users/index.ts', ...vocabulary.edits['packages/api/src/users/index.ts'] }),
    },
  ]
  const conversation: Slice<'conversation'> = {
    kind: 'conversation',
    variant: 'default',
    source,
    decode: 'strict',
    loading: false,
    state: {
      threads: [
        { agent: builder.id, session_id: builder.session, items: builderItems, page_size: 50, has_more: false },
        { agent: migrator.id, session_id: migrator.session, items: migratorItems, page_size: 50, has_more: false },
      ],
    },
    timeline: migratorEvents,
  }

  // terminal
  const run = terminalRun(child(ctx, 'terminal/builder'), builder, {
    startedAtMs: -150_000,
    command: vocabulary.shellRuns[0].command,
    lines: vocabulary.shellRuns[0].lines,
  })
  const record: TerminalRecord = terminalRecord(ctx, builder, run, -150_000)
  const terminal: Slice<'terminal'> = {
    kind: 'terminal',
    variant: 'default',
    source,
    decode: 'strict',
    loading: false,
    state: { terminals: [record] },
    timeline: [],
  }

  return { roster, details, attention, conversation, terminal, sync: { ...liveSync(ctx, { conversation }), variant: 'default', source } }
}

