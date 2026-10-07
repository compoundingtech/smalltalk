// Agent conversations projected from the fixture world (`src/fixtures/world.ts`): every agent,
// session, repo, PR, CI run, message and timestamp below comes from (or is derived from) it.
//
// Each transcript is authored once as a step script and emitted in both source shapes: an st3
// conversation follow (`TimelineEntry` JSON frames) and agent-session-ingest `NormalizedRecord`s.
// Rendering both through their adapters is the parity check: whatever differs on screen is a real
// difference between the sources.

import { decodeUnknownSync, TimelineEntry, TimelineEntryId } from '@smalltalk/st3-client/schema'

import type { ConversationPage } from '../data/source.ts'
import {
  agentByRef,
  agents,
  ciRuns,
  decisionD12,
  missionByRef,
  operator,
  pullRequests,
  reviewer,
  worldNow,
  type WorldAgent,
} from '../fixtures/world.ts'
import type { IngestRecord } from './fromIngest.ts'
import {
  ProposedReasoningEntry,
  timelineItems,
  type FixtureConversationChunk,
} from './fromTimeline.ts'

type Step =
  | {
      readonly kind: 'user'
      readonly t: number
      readonly text: string
      readonly attachment?: string
    }
  | {
      readonly kind: 'think'
      readonly t: number
      readonly ms: number
      readonly text: string
      readonly final?: boolean
    }
  | { readonly kind: 'say'; readonly t: number; readonly text: string; readonly final?: boolean }
  | {
      readonly kind: 'tool'
      readonly t: number
      readonly end?: number
      readonly id: string
      readonly name: string
      readonly input: unknown
      readonly output?: string
      readonly isError?: boolean
    }
  | {
      readonly kind: 'mail'
      readonly t: number
      readonly id: string
      readonly from: string
      /** Defaults to the transcript's own agent. */
      readonly to?: string
      readonly replyTo?: string
      readonly title: string
    }
  | {
      readonly kind: 'status'
      readonly t: number
      readonly status: 'queued' | 'running' | 'waiting' | 'completed' | 'failed' | 'cancelled'
      readonly detail?: string
    }
  | {
      readonly kind: 'usage'
      readonly t: number
      readonly input: number
      readonly output: number
      readonly cached: number
      readonly cost: number
      readonly context: number
    }
  | {
      readonly kind: 'raw'
      readonly t: number
      readonly type: string
      readonly role: string
      readonly body: unknown
    }

/** One agent's latest turn: step `t` is seconds after the turn started. */
interface Script {
  readonly agent: WorldAgent
  /** Minutes before `worldNow` the turn started. */
  readonly startedMinutesAgo: number
  readonly steps: ReadonlyArray<Step>
}

const requireAgent = (ref: string): WorldAgent => {
  const agent = agentByRef(ref)
  if (agent === undefined) throw new Error(`conversation fixtures: ${ref} is not a world agent`)
  return agent
}

const workbenchShell = requireAgent('agent/build-host-a/workbench-shell')
const missionsUi = requireAgent('agent/build-host-a/missions-ui')
const gatewaySchema = requireAgent('agent/build-host-a/gateway-schema')

const prOf = (agent: WorldAgent) => {
  const pull = pullRequests.find((candidate) => candidate.agent === agent.ref)
  if (pull === undefined) throw new Error(`conversation fixtures: ${agent.ref} has no pull request`)
  return pull
}

const shellPr = prOf(workbenchShell)
const shellCi = ciRuns.find((run) => run.pullRequest === shellPr.ref)
const shellCwd = workbenchShell.session.cwd
const shellSha = shellPr.headSha.slice(0, 7)

// Message ids shared by both sides of the D12 exchange, so each transcript names the same message.
const d12Notice = `message/${decisionD12.ref.slice(10)}/notice`
const d12Reply = `message/${decisionD12.ref.slice(10)}/reply`
const d12NoticeTitle = `${decisionD12.id} escalated to ${operator.name.split(' ')[0]} — please hold SessionList virtualization`
const d12ReplyTitle = `re: ${decisionD12.id} — SessionList stays on GridList; +1 for option ${decisionD12.recommendation}`

// ── WorkbenchShell: the default rendered session ─────────────────────────────────────────────

const groupsPath = `${shellCwd}/src/shell/editorGroups.ts`
const focusTestPath = `${shellCwd}/src/shell/editorGroups.focus.unit.test.ts`

const ask = `${reviewer.name.split(' ')[0]} requested changes on #${shellPr.number}: closing the last editor in a group drops focus onto the activity bar instead of the neighbouring group. Fix it in \`src/shell\`, add a regression test, get \`pnpm typecheck\` green, push, and make sure storybook still builds. MissionsUi may ping you about ${decisionD12.id} — answer it, but don't virtualize anything until I decide.`

const grepOutput = `src/shell/editorGroups.ts:64:export const closeGroup = (state: GroupsState, groupId: GroupId): GroupsState => {
src/shell/editorGroups.ts:70:    focusedGroup: state.focusedGroup === groupId ? undefined : state.focusedGroup,
src/shell/editorGroups.ts:77:  if (group.editors.length === 1) return closeGroup(state, groupId)
src/shell/Workbench.tsx:212:  const focusedGroup = useAtomValue(focusedGroupAtom)
src/shell/editorGroups.unit.test.ts:9:describe('closeGroup', () => {`

const readOutput = `    64→export const closeGroup = (state: GroupsState, groupId: GroupId): GroupsState => {
    65→  const groups = state.groups.filter((group) => group.id !== groupId)
    66→  return {
    67→    ...state,
    68→    groups,
    69→    // The focused group is gone; let the shell pick a new one.
    70→    focusedGroup: state.focusedGroup === groupId ? undefined : state.focusedGroup,
    71→  }
    72→}
    73→
    74→export const closeEditor = (state: GroupsState, groupId: GroupId, editorId: EditorId): GroupsState => {
    75→  const group = state.groups.find((candidate) => candidate.id === groupId)
    76→  if (group === undefined) return state
    77→  if (group.editors.length === 1) return closeGroup(state, groupId)
    78→  return updateGroup(state, groupId, (current) => ({
    79→    ...current,
    80→    editors: current.editors.filter((editor) => editor.id !== editorId),
    81→    activeEditor: current.activeEditor === editorId ? neighbour(current.editors, editorId) : current.activeEditor,
    82→  }))
    83→}`

const focusTest = `import { describe, expect, it } from 'vitest'

import { closeEditor } from './editorGroups.ts'
import { groupsFixture } from './testing.ts'

describe('closing the last editor in a group', () => {
  it('focuses the right-hand neighbour', () => {
    const state = groupsFixture(['left', 'middle', 'right'], { focused: 'middle' })
    expect(closeEditor(state, 'middle', 'middle/editor-0').focusedGroup).toBe('right')
  })

  it('falls back to the left neighbour when the rightmost group closes', () => {
    const state = groupsFixture(['left', 'right'], { focused: 'right' })
    expect(closeEditor(state, 'right', 'right/editor-0').focusedGroup).toBe('left')
  })
})`

const editOld = `  const groups = state.groups.filter((group) => group.id !== groupId)
  return {
    ...state,
    groups,
    // The focused group is gone; let the shell pick a new one.
    focusedGroup: state.focusedGroup === groupId ? undefined : state.focusedGroup,`

const editNew = `  const index = state.groups.findIndex((group) => group.id === groupId)
  const groups = state.groups.filter((group) => group.id !== groupId)
  // Focus moves to the right-hand neighbour, else the left one. Leaving it \`undefined\`
  // made FocusScope restore focus to the activity bar (review on #${shellPr.number}).
  const neighbour = groups[Math.min(index, groups.length - 1)]
  return {
    ...state,
    groups,
    focusedGroup: state.focusedGroup === groupId ? neighbour.id : state.focusedGroup,`

const editResult = `Updated src/shell/editorGroups.ts (+5 −2)
    64→export const closeGroup = (state: GroupsState, groupId: GroupId): GroupsState => {
    65→  const index = state.groups.findIndex((group) => group.id === groupId)
    66→  const groups = state.groups.filter((group) => group.id !== groupId)
    67→  // Focus moves to the right-hand neighbour, else the left one. Leaving it \`undefined\`
    68→  // made FocusScope restore focus to the activity bar (review on #${shellPr.number}).
    69→  const neighbour = groups[Math.min(index, groups.length - 1)]
    70→  return {
    71→    ...state,
    72→    groups,
    73→    focusedGroup: state.focusedGroup === groupId ? neighbour.id : state.focusedGroup,
    74→  }
    75→}`

const fixOld = `    focusedGroup: state.focusedGroup === groupId ? neighbour.id : state.focusedGroup,`
const fixNew = `    focusedGroup: state.focusedGroup === groupId ? neighbour?.id : state.focusedGroup,`
const fixResult = `Updated src/shell/editorGroups.ts (+1 −1)
    73→    focusedGroup: state.focusedGroup === groupId ? neighbour?.id : state.focusedGroup,`

const typecheckHeader = `> webfractal@0.0.0 typecheck ${shellCwd}
> tsc -p tsconfig.json --noEmit`

const typecheckRed = `${typecheckHeader}

src/shell/editorGroups.ts(73,52): error TS18048: 'neighbour' is possibly 'undefined'.

Found 1 error in src/shell/editorGroups.ts:73

 ELIFECYCLE  Command failed with exit code 2.`

const vitestGreen = ` RUN  v4.1.9 ${shellCwd}

 ✓ src/shell/editorGroups.focus.unit.test.ts (2 tests) 6ms
 ✓ src/shell/editorGroups.unit.test.ts (7 tests) 11ms

 Test Files  2 passed (2)
      Tests  9 passed (9)
   Duration  611ms`

const workbenchRead = `   206→export const Workbench = ({ variant }: WorkbenchProps) => {
   207→  const groups = useAtomValue(groupsAtom)
   208→  const dock = useAtomValue(dockAtom)
   209→  const quickOpen = useAtomValue(quickOpenAtom)
   210→  // FocusScope restores to the last focused element outside the scope when the
   211→  // focused group unmounts, so the group must hand focus over before it closes.
   212→  const focusedGroup = useAtomValue(focusedGroupAtom)
   213→  const focusGroup = useAtomSet(focusGroupAtom)
   214→  useFocusHandover(focusedGroup, focusGroup)`

const storybookBuild = `> webfractal@0.0.0 storybook:build ${shellCwd}
> storybook build --quiet

@storybook/core v10.1.4

info => Cleaning outputDir: storybook-static
info => Building manager..
info => Manager built (412 ms)
info => Building preview..
vite v7.1.9 building for production...
✓ 4127 modules transformed.
storybook-static/iframe.html                              1.92 kB │ gzip:   0.83 kB
storybook-static/assets/Workbench.stories-B3kQ1x9a.js    48.71 kB │ gzip:  13.02 kB
storybook-static/assets/iframe-Dk2Lw0pR.js            1,284.55 kB │ gzip: 361.40 kB
✓ built in 1m 58s
info => Preview built (2.0 min)
info => Output directory: ${shellCwd}/storybook-static`

const finalAnswer = `Fixed and pushed to #${shellPr.number} (\`${shellSha}\`).

- **\`src/shell/editorGroups.ts\`** — \`closeGroup\` now hands focus to the right-hand neighbour, else the left one. It used to clear \`focusedGroup\`, and with nothing focused React Aria's \`FocusScope\` restored focus to the activity bar — the jump ${reviewer.name.split(' ')[0]} saw.
- **\`src/shell/editorGroups.focus.unit.test.ts\`** — two regression tests: closing the middle group focuses the right neighbour; closing the rightmost focuses the left one.

\`pnpm typecheck\` was red once (\`neighbour\` is \`undefined\` when the last group closes); it is green now, as are the 9 shell tests and a local \`storybook build\`.${shellCi !== undefined ? ` CI run ${shellCi.id} is still going.` : ''}

${decisionD12.id}: MissionsUi escalated it to you; I told it SessionList stays on plain \`GridList\` until you pick. My vote is ${decisionD12.recommendation} — the Virtualizer keeps the focus restoration this fix relies on.`

/** The WorkbenchShell turn, ending in the final answer (streaming or finished). */
const workbenchShellSteps = (finalStreaming: boolean): ReadonlyArray<Step> => [
  { kind: 'status', t: 0, status: 'running' },
  { kind: 'user', t: 0, text: ask },
  {
    kind: 'think',
    t: 4.8,
    ms: 4_800,
    text: `Focus landing on the activity bar means nothing claims focus when the group unmounts, so FocusScope restores to the last element outside it. Either the close path clears \`focusedGroup\` or the shell ignores it.

Plan: grep for \`closeGroup\`, read the reducer, write a regression test that closes the middle of three groups and expects the right neighbour (the VS Code rule), then fix and typecheck.`,
  },
  { kind: 'say', t: 5.5, text: 'Let me find where groups close.' },
  {
    kind: 'tool',
    t: 6,
    end: 6.3,
    id: 'call_01grep',
    name: 'grep',
    input: { pattern: 'closeGroup|focusedGroup', path: `${shellCwd}/src/shell` },
    output: grepOutput,
  },
  {
    kind: 'tool',
    t: 7,
    end: 7.2,
    id: 'call_02read',
    name: 'read',
    input: { path: groupsPath, offset: 64, limit: 20 },
    output: readOutput,
  },
  {
    kind: 'tool',
    t: 58,
    end: 58.1,
    id: 'call_03write',
    name: 'write',
    input: { path: focusTestPath, content: focusTest },
    output: `Wrote src/shell/editorGroups.focus.unit.test.ts (${focusTest.split('\n').length} lines)`,
  },
  { kind: 'mail', t: 72, id: d12Notice, from: missionsUi.ref, title: d12NoticeTitle },
  {
    kind: 'tool',
    t: 96,
    end: 96.2,
    id: 'call_04edit',
    name: 'edit',
    input: { path: groupsPath, old_string: editOld, new_string: editNew },
    output: editResult,
  },
  {
    kind: 'tool',
    t: 101,
    end: 119,
    id: 'call_05bash',
    name: 'bash',
    input: { command: 'pnpm typecheck' },
    output: `${typecheckRed}\nExit code 2`,
    isError: true,
  },
  {
    kind: 'think',
    t: 123,
    ms: 3_100,
    text: '`groups` is empty when the last group closes, so `neighbour` really can be undefined — and then `focusedGroup` should be undefined too. Optional chaining is the honest fix, not a non-null assertion.',
  },
  {
    kind: 'tool',
    t: 124,
    end: 124.1,
    id: 'call_06edit',
    name: 'edit',
    input: { path: groupsPath, old_string: fixOld, new_string: fixNew },
    output: fixResult,
  },
  {
    kind: 'tool',
    t: 126,
    end: 143,
    id: 'call_07bash',
    name: 'bash',
    input: { command: 'pnpm typecheck' },
    output: typecheckHeader,
  },
  {
    kind: 'tool',
    t: 150,
    end: 152,
    id: 'call_08bash',
    name: 'bash',
    input: { command: 'pnpm vitest run src/shell/editorGroups' },
    output: vitestGreen,
  },
  {
    kind: 'think',
    t: 156,
    ms: 2_400,
    text: `MissionsUi wants SessionList left alone until ${decisionD12.id} is decided. Nothing in this fix touches virtualization; worth telling it the Virtualizer keeps FocusScope's restoration, which is exactly what this bug was about.`,
  },
  {
    kind: 'tool',
    t: 165,
    end: 165.6,
    id: 'call_09bash',
    name: 'bash',
    input: {
      command: `st message send --to ${missionsUi.ref} --reply-to ${d12Notice} --title "${d12ReplyTitle}" --body-file /tmp/d12-reply.md`,
    },
    output: `${d12Reply} accepted · delivered to ${missionsUi.ref}`,
  },
  {
    kind: 'tool',
    t: 210,
    end: 210.2,
    id: 'call_10read',
    name: 'read',
    input: { path: `${shellCwd}/src/shell/Workbench.tsx`, offset: 206, limit: 9 },
    output: workbenchRead,
  },
  {
    kind: 'tool',
    t: 400,
    end: 403,
    id: 'call_11bash',
    name: 'bash',
    input: {
      command: `git add src/shell/editorGroups.ts src/shell/editorGroups.focus.unit.test.ts && git commit -m "fix(shell): hand focus to the neighbouring group on close" && git push origin ${workbenchShell.session.branch}`,
    },
    output: `[${workbenchShell.session.branch} ${shellSha}] fix(shell): hand focus to the neighbouring group on close
 2 files changed, 21 insertions(+), 2 deletions(-)
 create mode 100644 src/shell/editorGroups.focus.unit.test.ts
To github.com:${shellPr.repo}.git
   3e81c0d..${shellSha}  ${workbenchShell.session.branch} -> ${workbenchShell.session.branch}`,
  },
  {
    kind: 'tool',
    t: 450,
    end: 576,
    id: 'call_12bash',
    name: 'bash',
    input: { command: 'pnpm storybook:build' },
    output: storybookBuild,
  },
  {
    kind: 'usage',
    t: 577,
    input: 61_480,
    output: 3_912,
    cached: 52_870,
    cost: 0.34,
    context: Math.round(workbenchShell.session.contextFill * 100),
  },
  {
    kind: 'say',
    t: 578,
    text: finalStreaming ? finalAnswer.slice(0, 420) : finalAnswer,
    final: !finalStreaming,
  },
  ...(finalStreaming ? [] : [{ kind: 'status' as const, t: 590, status: 'completed' as const }]),
]

const workbenchShellScript = (finalStreaming: boolean): Script => ({
  agent: workbenchShell,
  startedMinutesAgo: 10,
  steps: workbenchShellSteps(finalStreaming),
})

// ── The rest of the fleet: one short turn each ────────────────────────────────────────────────

/** `t` (seconds after `start`) of a moment `minutes` before `worldNow`. */
const sinceStart = ({ start, minutes }: { readonly start: number; readonly minutes: number }) =>
  (start - minutes) * 60

const missionsUiScript: Script = {
  agent: missionsUi,
  startedMinutesAgo: 16,
  steps: [
    { kind: 'status', t: 0, status: 'running' },
    {
      kind: 'user',
      t: 0,
      text: 'Wire the missions collection to the gateway follow and keep it usable past 5k missions. If virtualization needs a design call, escalate it as a decision instead of picking.',
    },
    {
      kind: 'think',
      t: 6,
      ms: 5_200,
      text: 'Before escalating I need numbers: bench React Aria’s Virtualizer against hand-rolled windowing on the same 10k-row collection.',
    },
    {
      kind: 'tool',
      t: 20,
      end: sinceStart({ start: 16, minutes: 10 }),
      id: 'call_01bash',
      name: 'bash',
      input: { command: 'pnpm bench:collections --rows 10000 --frames 600' },
      output: `collection      rows    p50 ms/frame  p95 ms/frame  a11y
virtualizer     10000   1.4           2.1           ListBox/GridList semantics kept
windowing       10000   0.9           1.3           focus, typeahead, aria-setsize re-implemented
paginate(200)   10000   0.3           0.5           keyboard paging breaks at page edges`,
    },
    {
      kind: 'tool',
      t: sinceStart({ start: 16, minutes: 9.05 }),
      end: sinceStart({ start: 16, minutes: 9 }),
      id: 'call_02bash',
      name: 'bash',
      input: {
        command: `st attention raise --mission ${decisionD12.mission} --decision ${decisionD12.id} --owner ${decisionD12.owner} --body-file /tmp/d12.md`,
      },
      output: `${decisionD12.ref} opened · owner ${decisionD12.owner}`,
    },
    {
      kind: 'tool',
      t: sinceStart({ start: 16, minutes: 8.85 }),
      end: sinceStart({ start: 16, minutes: 8.8 }),
      id: 'call_03bash',
      name: 'bash',
      input: {
        command: `st message send --to ${workbenchShell.ref} --title "${d12NoticeTitle}" --body-file /tmp/d12-notice.md`,
      },
      output: `${d12Notice} accepted · delivered to ${workbenchShell.ref}`,
    },
    {
      kind: 'say',
      t: sinceStart({ start: 16, minutes: 8.8 }),
      text: `Escalated **${decisionD12.title}** to ${operator.name}. Bench: Virtualizer 1.4 ms/frame vs 0.9 ms/frame hand-rolled at 10k rows; I recommend ${decisionD12.recommendation}. Parked until it is decided.`,
    },
    {
      kind: 'status',
      t: sinceStart({ start: 16, minutes: 8.8 }),
      status: 'waiting',
      detail: missionsUi.status,
    },
    {
      kind: 'mail',
      t: sinceStart({ start: 16, minutes: 10 - 165.6 / 60 }),
      id: d12Reply,
      from: workbenchShell.ref,
      replyTo: d12Notice,
      title: d12ReplyTitle,
    },
  ],
}

const ciDoctor = requireAgent('agent/build-host-b/ci-doctor')
const flakyRun = ciRuns.find((run) => run.conclusion === 'failure')
const ciDoctorScript: Script = {
  agent: ciDoctor,
  startedMinutesAgo: 6,
  steps: [
    { kind: 'status', t: 0, status: 'running' },
    {
      kind: 'user',
      t: 0,
      text: `darwin-activation failed on run ${flakyRun?.id ?? ''} but passed on retry yesterday. Find out whether it is flaky or real, and quarantine it if it is flaky.`,
    },
    {
      kind: 'tool',
      t: 4,
      end: 9,
      id: 'toolu_01Bash',
      name: 'Bash',
      input: {
        command: `gh run view ${flakyRun?.id ?? ''} --log-failed | tail -6`,
        description: 'Failed step log',
      },
      output: `darwin-activation  activate  ${flakyRun?.startedAt ?? ciDoctor.session.startedAt}  waiting for launchd job org.nixos.activate-system
darwin-activation  activate  ${flakyRun?.updatedAt ?? ciDoctor.session.lastActivityAt}  error: timed out after 1200s waiting for org.nixos.activate-system
darwin-activation  activate  ${flakyRun?.updatedAt ?? ciDoctor.session.lastActivityAt}  ##[error]Process completed with exit code 1.`,
    },
    {
      kind: 'think',
      t: 14,
      ms: 4_100,
      text: 'A 1200 s timeout on launchd activation with no evaluation change smells like a slow runner, not a regression. Reproduce locally on the same derivation before quarantining.',
    },
    {
      kind: 'tool',
      t: 20,
      end: sinceStart({ start: 6, minutes: 0.2 }),
      id: 'toolu_02Bash',
      name: 'Bash',
      input: { command: 'nix build .#checks.darwin-activation -L', timeout: 600000 },
      output: `darwin-activation> waiting for launchd job org.nixos.activate-system
darwin-activation> error: timed out after 300s waiting for org.nixos.activate-system
error: builder for '/nix/store/4qk1…-darwin-activation.drv' failed with exit code 1
Exit code 1`,
      isError: true,
    },
    {
      kind: 'think',
      t: sinceStart({ start: 6, minutes: 0.1 }),
      ms: 0,
      final: false,
      text: 'Fails locally too, but at the 300 s local timeout — so the job is slow everywhere, not only on the runner. Next: bisect the last three flake bumps in',
    },
  ],
}

const gatewayPatch = `*** Begin Patch
*** Update File: crates/st3-client/src/envelope.rs
@@
-pub mod unstable {
-    pub use crate::wire::SubjectEnvelope;
-}
+/// Frozen in st3.client v1: additive changes only.
+pub use crate::wire::SubjectEnvelope;
+
+#[deprecated(note = "SubjectEnvelope is stable; import it from the crate root")]
+pub mod unstable {
+    pub use crate::wire::SubjectEnvelope;
+}
*** End Patch`

const gatewaySchemaScript: Script = {
  agent: gatewaySchema,
  startedMinutesAgo: 5,
  steps: [
    { kind: 'status', t: 0, status: 'running' },
    {
      kind: 'user',
      t: 0,
      text: 'Freeze the st3.client v1 envelopes: move `SubjectEnvelope` out of `unstable`, then regenerate the TypeScript and Rust clients.',
    },
    {
      kind: 'tool',
      t: 8,
      end: 8.4,
      id: 'call_01shell',
      name: 'shell',
      input: {
        command: ['bash', '-lc', 'rg -n "unstable::SubjectEnvelope" crates'],
        workdir: gatewaySchema.session.cwd,
      },
      output: `crates/st3-gateway/src/collections.rs:14:use st3_client::unstable::SubjectEnvelope;
crates/st3-gateway/src/actions.rs:9:use st3_client::unstable::SubjectEnvelope;
crates/st3-codegen/src/ts.rs:31:    emit::<st3_client::unstable::SubjectEnvelope>(&mut out);`,
    },
    {
      kind: 'tool',
      t: 41,
      end: 41.3,
      id: 'call_02apply_patch',
      name: 'apply_patch',
      input: { input: gatewayPatch },
      output: 'Success. Updated the following files:\nM crates/st3-client/src/envelope.rs',
    },
    {
      kind: 'say',
      t: 60,
      text: '`SubjectEnvelope` is re-exported from the crate root; `unstable` stays one release as a deprecated alias so the gateway keeps compiling. Regenerating the clients now.',
    },
    {
      kind: 'tool',
      t: sinceStart({ start: 5, minutes: 0.8 }),
      id: 'call_03shell',
      name: 'shell',
      input: {
        command: ['bash', '-lc', 'cargo run -p st3-codegen -- --target ts,rust --out clients/'],
        workdir: gatewaySchema.session.cwd,
      },
    },
  ],
}

const casJanitor = requireAgent('agent/build-host-b/cas-janitor')
const casJanitorScript: Script = {
  agent: casJanitor,
  startedMinutesAgo: 12,
  steps: [
    { kind: 'status', t: 0, status: 'running' },
    {
      kind: 'user',
      t: 0,
      text: 'Run the next CAS migration batch (`/srv/artefacts/2026-09`) and verify every blob before deleting anything.',
    },
    {
      kind: 'tool',
      t: 6,
      end: sinceStart({ start: 12, minutes: 4 }),
      id: 'call_01shell',
      name: 'shell',
      input: {
        command: [
          'bash',
          '-lc',
          'cas-migrate copy --from /srv/artefacts/2026-09 --to /srv/cas-staging --verify',
        ],
        workdir: casJanitor.session.cwd,
      },
      output: `copied 18,412 / 26,930 blobs (41.7 GiB), 18,412 verified
error: write /srv/cas-staging/blobs/sha256/7f/7f3e…: No space left on device (os error 28)
Exit code 1`,
      isError: true,
    },
    {
      kind: 'raw',
      t: sinceStart({ start: 12, minutes: 4 }),
      role: 'system',
      type: 'error',
      body: {
        code: 'harness_fault',
        message: 'codex exited: ENOSPC on /srv/cas-staging',
        retryable: false,
        details: { exit_code: 1 },
      },
    },
    {
      kind: 'status',
      t: sinceStart({ start: 12, minutes: 4 }),
      status: 'failed',
      detail: casJanitor.status,
    },
  ],
}

/** Tool call in the agent's own harness vocabulary. */
const shellCall = ({
  agent,
  id,
  t,
  command,
  end,
  output,
}: {
  readonly agent: WorldAgent
  readonly id: string
  readonly t: number
  readonly command: string
  readonly end?: number | undefined
  readonly output?: string | undefined
}): Step => {
  const call = {
    kind: 'tool' as const,
    t,
    id,
    ...(end !== undefined ? { end } : {}),
    ...(output !== undefined ? { output } : {}),
  }
  switch (agent.session.harness) {
    case 'omp':
      return { ...call, name: 'bash', input: { command } }
    case 'claude':
      return { ...call, name: 'Bash', input: { command } }
    case 'codex':
      return {
        ...call,
        name: 'shell',
        input: { command: ['bash', '-lc', command], workdir: agent.session.cwd },
      }
  }
}

/** The agent's turn closes the way its world activity says: still running, waiting, done or failed. */
const closing = ({
  agent,
  t,
}: {
  readonly agent: WorldAgent
  readonly t: number
}): ReadonlyArray<Step> => {
  switch (agent.activity) {
    case 'working':
      return []
    case 'waiting':
      return [{ kind: 'status', t, status: 'waiting', detail: agent.status }]
    case 'idle':
      return [{ kind: 'status', t, status: 'completed' }]
    case 'errored':
      return [{ kind: 'status', t, status: 'failed', detail: agent.status }]
  }
}

interface Brief {
  readonly ref: string
  readonly startedMinutesAgo: number
  readonly prompt: string
  readonly command: string
  /** Absent: the command is still running (the agent is working). */
  readonly output?: string
  readonly answer?: string
}

const briefScript = (brief: Brief): Script => {
  const agent = requireAgent(brief.ref)
  const start = brief.startedMinutesAgo
  const last = (Date.parse(agent.session.lastActivityAt) - (worldNow - start * 60_000)) / 1000
  return {
    agent,
    startedMinutesAgo: start,
    steps: [
      { kind: 'status', t: 0, status: 'running' },
      { kind: 'user', t: 0, text: brief.prompt },
      shellCall({
        agent,
        id: 'call_01',
        t: 5,
        command: brief.command,
        end: brief.output !== undefined ? last - 2 : undefined,
        output: brief.output,
      }),
      ...(brief.answer !== undefined
        ? [{ kind: 'say' as const, t: last, text: brief.answer }]
        : []),
      ...closing({ agent, t: last }),
    ],
  }
}

const briefs: readonly Brief[] = [
  {
    ref: 'agent/build-host-a/terminal-renderer',
    startedMinutesAgo: 8,
    prompt: 'Benchmark ghostty-web frame decoding against the recorded terminal trace.',
    command: 'pnpm bench:terminal --renderer ghostty-web --frames 600',
  },
  {
    ref: 'agent/build-host-a/palette-review',
    startedMinutesAgo: 30,
    prompt: `Review the command palette's fenced dispatch before merging.`,
    command: `gh pr diff ${prOf(requireAgent('agent/build-host-a/palette-review')).number}`,
    output:
      'src/command-palette/dispatch.ts: confirms against the selected subject snapshot before dispatch.',
    answer: 'Review posted: the fence is correct; two keyboard-navigation nits remain.',
  },
  {
    ref: 'agent/build-host-b/deps-steward',
    startedMinutesAgo: 9,
    prompt: 'Prove the weekly external flake bumps against the host closures.',
    command: 'nix build .#nixosConfigurations.build-host-b.config.system.build.toplevel -L',
  },
  {
    ref: 'agent/build-host-b/health-probe',
    startedMinutesAgo: 325,
    prompt: 'Run the daily fleet health probes and report failed invariants.',
    command: 'fleet-health probe --all',
    output: 'build-host-a: 41/41 passed\nbuild-host-b: 41/41 passed\nbuild-host-c: 41/41 passed',
    answer: 'Daily run completed. All three hosts passed; no new findings.',
  },
  {
    ref: 'agent/build-host-b/docs-writer',
    startedMinutesAgo: 23,
    prompt: 'Update the gateway restart and builder disk-pressure runbooks, then request review.',
    command: `gh pr view ${prOf(requireAgent('agent/build-host-b/docs-writer')).number} --json reviewRequests`,
    output: `{"reviewRequests":[{"login":"${reviewer.handle}"}]}`,
    answer: `Runbooks updated. Review requested from ${reviewer.name}.`,
  },
  {
    ref: 'agent/build-host-c/sdk-interop',
    startedMinutesAgo: 55,
    prompt: 'Prove TypeScript SDK interop against the Rust gateway on Darwin.',
    command: 'pnpm test:interop --gateway rust --platform darwin',
  },
  {
    ref: 'agent/build-host-c/web-scout',
    startedMinutesAgo: 410,
    prompt: 'Compare virtualization libraries that preserve React Aria collection semantics.',
    command: 'pnpm bench:collections --rows 5000',
    output:
      'React Aria Virtualizer: collection semantics retained\nCustom windowing: focus and typeahead require a separate implementation',
    answer:
      'Comparison ready: React Aria Virtualizer preserves keyboard navigation and collection semantics.',
  },
]

// ── Emitters ────────────────────────────────────────────────────────────────────────────────

const toTimeline = ({
  script,
  reasoning,
}: {
  readonly script: Script
  readonly reasoning: boolean
}): FixtureConversationChunk['entries'] => {
  let sequence = 0
  const { agent, startedMinutesAgo } = script
  const entry = ({
    step,
    role,
    type,
    body,
    final = true,
    t = step.t,
  }: {
    readonly step: Step
    readonly role: string
    readonly type: string
    readonly body: unknown
    readonly final?: boolean
    readonly t?: number
  }) => {
    const raw = {
      id: `timeline-entry/${agent.session.id.slice(8)}/${++sequence}`,
      sequence,
      revision: final ? 1 : 3,
      timestamp: new Date(worldNow - startedMinutesAgo * 60_000 + t * 1000).toISOString(),
      role,
      type,
      final,
      body,
    }
    // Intentionally rejected entry for the unknown-entry story; live rejection belongs to the SDK.
    if (type === 'citation') {
      return {
        type: 'unrecognized' as const,
        id: decodeUnknownSync(TimelineEntryId, 'strict')(raw.id),
        sequence,
        revision: raw.revision,
        timestamp: raw.timestamp,
        rawType: type,
        raw,
      }
    }
    return type === 'reasoning'
      ? decodeUnknownSync(ProposedReasoningEntry, 'strict')(raw)
      : decodeUnknownSync(TimelineEntry, 'strict')(raw)
  }
  return script.steps.flatMap((step): FixtureConversationChunk['entries'] => {
    switch (step.kind) {
      case 'user':
        return [
          entry({
            step,
            role: 'user',
            type: 'message',
            body: {
              message_id: `message/${agent.session.id.slice(8)}/operator`,
              from: operator.ref,
              to: agent.ref,
            },
          }),
          entry({
            step,
            role: 'user',
            type: 'content',
            body: { media_type: 'text/markdown', text: step.text },
          }),
          ...(step.attachment !== undefined
            ? [
                entry({
                  step,
                  role: 'user',
                  type: 'content',
                  body: { media_type: 'image/png', attachment_id: step.attachment },
                }),
              ]
            : []),
        ]
      case 'think':
        return reasoning
          ? [
              entry({
                step,
                role: 'assistant',
                type: 'reasoning',
                body:
                  step.final === false
                    ? { text: step.text }
                    : { text: step.text, duration_ms: step.ms },
                final: step.final ?? true,
              }),
            ]
          : []
      case 'say':
        return [
          entry({
            step,
            role: 'assistant',
            type: 'content',
            body: { media_type: 'text/markdown', text: step.text },
            final: step.final ?? true,
          }),
        ]
      case 'tool':
        return [
          entry({
            step,
            role: 'assistant',
            type: 'tool_call',
            body: { call_id: step.id, name: step.name, arguments: step.input },
          }),
          ...(step.output !== undefined
            ? [
                entry({
                  step,
                  role: 'tool',
                  type: 'tool_result',
                  body: {
                    call_id: step.id,
                    status: step.isError === true ? 'error' : 'success',
                    media_type: 'text/plain',
                    content: step.output,
                  },
                  final: true,
                  t: step.end ?? step.t,
                }),
              ]
            : []),
        ]
      case 'mail':
        return [
          entry({
            step,
            role: 'system',
            type: 'message',
            body: {
              message_id: step.id,
              from: step.from,
              to: step.to ?? agent.ref,
              title: step.title,
              ...(step.replyTo !== undefined ? { reply_to: step.replyTo } : {}),
            },
          }),
        ]
      case 'status':
        return [
          entry({
            step,
            role: 'system',
            type: 'status',
            body: {
              status: step.status,
              ...(step.detail !== undefined ? { detail: step.detail } : {}),
            },
          }),
        ]
      case 'usage':
        return [
          entry({
            step,
            role: 'system',
            type: 'usage',
            body: {
              semantics: 'response',
              driver: agent.session.harness,
              model: agent.session.model,
              input_tokens: step.input,
              output_tokens: step.output,
              total_tokens: step.input + step.output,
              cached_tokens: step.cached,
              cost: step.cost,
              currency: 'USD',
              context_used_percent: step.context,
              attribution: {
                agent_id: agent.ref,
                ...(agent.mission !== undefined
                  ? {
                      mission_run_id: `mission-run/${agent.mission.slice(8)}/${missionByRef(agent.mission)?.runs}`,
                    }
                  : {}),
              },
            },
          }),
        ]
      case 'raw':
        return [entry({ step, role: step.role, type: step.type, body: step.body })]
    }
  })
}

const toIngest = (script: Script): ReadonlyArray<IngestRecord> => {
  const { agent } = script
  const tool = agent.session.harness === 'claude' ? 'claude-code' : agent.session.harness
  const sourceId = `${tool}:${agent.session.id.slice(8)}`
  const sessionId = agent.session.id
  return [
    {
      _tag: 'SessionMeta',
      sourceId,
      sessionId,
      cwd: agent.session.cwd,
      model: agent.session.model,
      gitBranch: agent.session.branch,
      tool,
      timestamp: agent.session.startedAt,
    },
    ...script.steps.flatMap((step): ReadonlyArray<IngestRecord> => {
      const timestamp = new Date(
        worldNow - script.startedMinutesAgo * 60_000 + step.t * 1000,
      ).toISOString()
      const common = { sourceId, sessionId, timestamp }
      switch (step.kind) {
        case 'user':
          return [{ ...common, _tag: 'UserMessage', content: step.text }]
        case 'think':
          return [{ ...common, _tag: 'Thinking', content: step.text }]
        case 'say':
          return [
            { ...common, _tag: 'AssistantText', content: step.text, model: agent.session.model },
          ]
        case 'tool':
          return [
            {
              ...common,
              _tag: 'ToolCallStart',
              toolCallId: step.id,
              toolName: step.name,
              input: step.input,
            },
            ...(step.output !== undefined
              ? [
                  {
                    ...common,
                    _tag: 'ToolCallEnd' as const,
                    timestamp: new Date(
                      worldNow - script.startedMinutesAgo * 60_000 + (step.end ?? step.t) * 1000,
                    ).toISOString(),
                    toolCallId: step.id,
                    toolName: step.name,
                    output: step.output,
                    ...(step.isError === true ? { isError: true } : {}),
                  },
                ]
              : []),
          ]
        // Offline normalized history carries no Small Talk deliveries, statuses or wire usage.
        case 'mail':
        case 'status':
        case 'usage':
        case 'raw':
          return []
      }
    }),
  ]
}

const single = ({
  items,
  hasMore = false,
}: {
  readonly items: FixtureConversationChunk['entries']
  readonly hasMore?: boolean
}): ReadonlyArray<FixtureConversationChunk> => [{ replace: true, entries: items, hasMore }]

/** Workbench-shell follow as st emits it today: final answer still streaming, no reasoning entries. */
export const stLiveFrames = single({
  items: toTimeline({ script: workbenchShellScript(true), reasoning: false }),
  hasMore: true,
})
/** Finished workbench-shell session as offline agent-session-ingest history. */
export const ingestHistory = toIngest(workbenchShellScript(false))
/** Streaming workbench-shell follow including proposed D08 reasoning entries. */
export const stProposedFrames = single({
  items: toTimeline({ script: workbenchShellScript(true), reasoning: true }),
  hasMore: true,
})
/** Completed workbench-shell follow including proposed D08 reasoning entries. */
export const stProposedFinishedFrames = single({
  items: toTimeline({ script: workbenchShellScript(false), reasoning: true }),
  hasMore: true,
})

const scripts = [
  workbenchShellScript(true),
  missionsUiScript,
  ciDoctorScript,
  gatewaySchemaScript,
  casJanitorScript,
  ...briefs.map(briefScript),
]
/** Decoded fixture chunks for harnesses that drive the production timeline store. */
export const agentConversationFrames: Readonly<
  Record<string, ReadonlyArray<FixtureConversationChunk>>
> = Object.fromEntries(
  scripts.map((script) => [
    script.agent.ref,
    single({ items: toTimeline({ script, reasoning: true }), hasMore: true }),
  ]),
)
/** Data-source projection: one conversation page per world agent. */
export const agentConversations: Readonly<Record<string, ConversationPage>> = Object.fromEntries(
  agents.map((agent) => [
    agent.ref,
    { items: timelineItems(agentConversationFrames[agent.ref] ?? []), hasOlder: true },
  ]),
)
/** Returns a distinct world-derived transcript, or undefined for an unknown agent. */
export const conversationFor = (agentRef: string): ConversationPage | undefined =>
  agentConversations[agentRef]

/** Named conversation state rendered by state-matrix stories. */
export interface StateFixture {
  readonly id: string
  readonly label: string
  readonly agentName: string
  readonly frames: ReadonlyArray<FixtureConversationChunk>
}

const longLog = Array.from({ length: 640 }, (_, index) =>
  (index + 1) % 97 === 0
    ? `warning: unused variable \`frame\` --> crates/st3-client/src/collections.rs:${index + 1}:9`
    : `   Compiling st3-client v0.1.0 (${gatewaySchema.session.cwd}/crates/st3-client)`,
).join('\n')
const stateFrames = ({
  steps,
  hasMore = false,
  agent = workbenchShell,
}: {
  readonly steps: ReadonlyArray<Step>
  readonly hasMore?: boolean
  readonly agent?: WorldAgent
}) =>
  single({
    items: toTimeline({ script: { agent, startedMinutesAgo: 10, steps }, reasoning: true }),
    hasMore,
  })

/** Conversation edge and lifecycle states covered by the state-matrix stories. */
export const allStates: ReadonlyArray<StateFixture> = [
  {
    id: 'empty',
    label: 'Empty — follow open, no entries yet',
    agentName: workbenchShell.name,
    frames: single({ items: [] }),
  },
  {
    id: 'thinking',
    label: 'Streaming — reasoning in progress',
    agentName: workbenchShell.name,
    frames: stateFrames({
      steps: [
        { kind: 'status', t: 0, status: 'running' },
        { kind: 'user', t: 0, text: ask },
        {
          kind: 'think',
          t: 2,
          ms: 0,
          final: false,
          text: 'FocusScope restores outside the editor when nothing claims focus. I need to trace closeGroup and determine which neighbour should receive',
        },
      ],
    }),
  },
  {
    id: 'tool-running',
    label: 'Streaming — tool call without result',
    agentName: gatewaySchema.name,
    frames: agentConversationFrames[gatewaySchema.ref] ?? [],
  },
  {
    id: 'answer-streaming',
    label: 'Streaming — final answer (st today)',
    agentName: workbenchShell.name,
    frames: stLiveFrames,
  },
  {
    id: 'finished',
    label: 'Finished — typecheck fixed, tests and Storybook green',
    agentName: workbenchShell.name,
    frames: stProposedFinishedFrames,
  },
  {
    id: 'error',
    label: 'Failed — ENOSPC, harness fault, failed run',
    agentName: casJanitor.name,
    frames: agentConversationFrames[casJanitor.ref] ?? [],
  },
  {
    id: 'waiting',
    label: 'Waiting — D12 escalation and peer reply',
    agentName: missionsUi.name,
    frames: agentConversationFrames[missionsUi.ref] ?? [],
  },
  {
    id: 'truncated',
    label: 'Truncated + redacted — earlier history omitted, credential withheld',
    agentName: workbenchShell.name,
    frames: stateFrames({
      steps: [
        {
          kind: 'raw',
          t: 0,
          role: 'system',
          type: 'truncation',
          body: {
            reason: 'response-limit',
            omitted_from_sequence: 1,
            omitted_to_sequence: 184,
            continuation_cursor: `timeline-cursor/${workbenchShell.session.id.slice(8)}/1`,
          },
        },
        {
          kind: 'user',
          t: 1,
          text: 'Publish the workbench preview with the CI deploy credential.',
        },
        {
          kind: 'tool',
          t: 3,
          end: 4,
          id: 'call_redacted',
          name: 'bash',
          input: { command: 'systemd-creds decrypt /run/credentials/deploy-token -' },
          output: '[redacted]',
        },
        {
          kind: 'raw',
          t: 4,
          role: 'system',
          type: 'redaction',
          body: { reason: 'credential', withheld_bytes: 48 },
        },
        {
          kind: 'say',
          t: 6,
          text: `Preview published at https://preview.acme.dev/webfractal/${shellPr.number}/.`,
        },
        { kind: 'status', t: 7, status: 'completed' },
      ],
      hasMore: true,
    }),
  },
  {
    id: 'long-output',
    label: 'Very long tool output — 640-line build log',
    agentName: gatewaySchema.name,
    frames: stateFrames({
      steps: [
        { kind: 'user', t: 0, text: 'Build the gateway workspace and summarize the warnings.' },
        {
          kind: 'tool',
          t: 1,
          end: 74,
          id: 'call_build',
          name: 'shell',
          input: { command: 'cargo build --workspace', workdir: gatewaySchema.session.cwd },
          output: `${longLog}\nFinished dev profile in 1m 12s`,
        },
        {
          kind: 'say',
          t: 76,
          text: 'Build is green. Six unused-variable warnings in crates/st3-client/src/collections.rs.',
        },
        { kind: 'status', t: 77, status: 'completed' },
      ],
      hasMore: false,
      agent: gatewaySchema,
    }),
  },
  {
    id: 'edges',
    label: 'Edges — attachment, unknown entry type, orphan result',
    agentName: workbenchShell.name,
    frames: stateFrames({
      steps: [
        {
          kind: 'user',
          t: 0,
          text: 'Here is the screenshot of the broken sidebar.',
          attachment: `attachment/${workbenchShell.session.id.slice(8)}/sidebar`,
        },
        {
          kind: 'raw',
          t: 2,
          role: 'assistant',
          type: 'citation',
          body: { url: 'https://example.com/design-review' },
        },
        {
          kind: 'raw',
          t: 3,
          role: 'tool',
          type: 'tool_result',
          body: {
            call_id: 'call_lost',
            status: 'success',
            media_type: 'text/plain',
            content: 'ok',
          },
        },
        { kind: 'status', t: 4, status: 'waiting', detail: 'waiting for approval' },
      ],
    }),
  },
]
