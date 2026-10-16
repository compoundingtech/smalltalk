import type * as St3 from '@smalltalk/st3-client/schema'
import { DateTime, Option } from 'effect'

import words from '../../../../fixtures/clients/words.json' with { type: 'json' }

// The mission presentation of docs/clients/ui-contract.md ("Presentation projections",
// "Mission"), derived in the app until st serves it on the missions collection (#2646). The
// fields and words are the contract's, so reading st's projection replaces only this file. The
// precedence is `st3-ui-model::missions::adapt`'s, first match wins.

export type MissionWord = (typeof words.mission)[number]['id']
export type StepWord = 'needs_you' | 'done' | 'working' | 'ready' | 'waiting' | 'cancelled' | 'failed' | 'pending'

/** Who must act, or who holds a step: `{kind, agent, label}` as the contract names it. */
export type Who = { readonly kind: 'you' | 'agent' | 'st' | 'nobody'; readonly agent?: string; readonly label: string }

export type PresentedStep = {
  readonly id: string
  readonly path: string
  readonly title: string
  readonly word: StepWord
  readonly owner: Who
  readonly note: string | null
  readonly attempts: number
  readonly blockers: readonly string[]
  readonly keep_open: boolean
}

export type MissionPresentation = {
  readonly word: MissionWord
  readonly system: boolean
  readonly who: Who
  readonly steps: readonly PresentedStep[]
  readonly stuck: { readonly step: string; readonly word: StepWord; readonly attempts: number; readonly blockers: readonly string[]; readonly agent: { readonly id: string; readonly label: string; readonly state: string } | null } | null
  readonly progress: { readonly done: number; readonly total: number }
  readonly last_progress: string | null
  readonly settled_at: number | null
}

export const MISSION_WORDS: readonly MissionWord[] = words.mission.map((word) => word.id)
export const missionWord = (id: MissionWord) => words.mission.find((word) => word.id === id)!

type Step = St3.MissionStep & { readonly keep_open?: boolean }
type Row = St3.Mission & { readonly system?: boolean }

const ACTIVE = new Set(['claimed', 'working', 'running', 'verifying'])
const FINISHED = new Set(['completed', 'failed', 'cancelled'])

/** The steps of the open runs, or of the latest run when none is open. */
const missionSteps = (mission: Row): readonly Step[] => {
  const runs = mission.run_details ?? []
  const open = runs.filter((run) => !FINISHED.has(run.status))
  const shown = open.length > 0 ? open : runs.slice(-1)
  return shown.flatMap((run) => Option.getOrElse(run.steps, () => [] as readonly St3.MissionStep[]))
}

const queuedFor = (agents: readonly St3.Agent[], work: string) =>
  agents.find((agent) => Option.getOrUndefined(agent.next_work_id) === work || (agent.upcoming_work_ids ?? []).some((id) => id === work))

const agentState = (agent: St3.Agent): string => (typeof agent.state === 'string' ? agent.state : agent.state.raw)
const broken = (agent: St3.Agent) => ['failed', 'stopped'].includes(agentState(agent)) || Option.isSome(agent.fault)
const seatOf = (step: Step): string | undefined =>
  step.agentless === true ? undefined : Option.getOrUndefined(step.claimant) ?? Option.getOrUndefined(step.assignee)

const stepWord = (step: Step, gated: ReadonlySet<string>): StepWord =>
  gated.has(step.id) ? 'needs_you'
    : step.state === 'completed' ? 'done'
      : ACTIVE.has(step.state) ? 'working'
        : step.state === 'ready' ? 'ready'
          : ['waiting', 'pending', 'blocked'].includes(step.state) ? 'waiting'
            : step.state === 'cancelled' ? 'cancelled'
              : step.state === 'failed' ? 'failed'
                : 'pending'

// Read a mission like a pipeline: what finished, what is happening, what is next.
const STEP_ORDER: Record<StepWord, number> = { done: 0, cancelled: 0, needs_you: 1, failed: 1, working: 2, ready: 3, waiting: 4, pending: 5 }

export const presentMission = ({
  mission,
  agents,
  attention,
}: {
  readonly mission: St3.Mission
  /** The agents collection: names, and who queues each ready step. */
  readonly agents: readonly St3.Agent[]
  /** Open attention, already the reader's. */
  readonly attention: readonly St3.Attention[]
}): MissionPresentation => {
  const row = mission as Row
  const work = missionSteps(row)
  const label = (id: string) => agents.find((agent) => agent.id === id)?.name ?? id.replace(/^agent\//, '')
  const gates = attention.filter((item) => item.state === 'open' && item.attention_kind === 'human-gate')
  const gated = new Set(gates.map((item) => item.source_id as string))
  const decision = gates.some((item) => item.mission_id === mission.id)
  const states = work.map((step) => step.state as string)
  const missionState = mission.state as string
  const ready = work.find((step) => step.state === 'ready' && Option.isNone(step.claimant))
  const readyAgent = ready === undefined ? undefined : queuedFor(agents, ready.id)
  const word: MissionWord =
    decision ? 'decision'
      : mission.runs.length === 0 && (mission.run_details ?? []).length === 0 && work.length === 0 ? 'not_started'
        : missionState === 'blocked' || states.includes('blocked') ? 'stalled'
          : missionState === 'completed' ? 'done'
            : missionState === 'failed' || states.includes('failed') ? 'failed'
              : missionState === 'cancelled' ? 'cancelled'
                : work.length > 0 && work.some((step) => step.state !== 'completed')
                    && work.every((step) => step.state === 'completed' || (ACTIVE.has(step.state) && step.agentless === true && step.keep_open === true)) ? 'watching'
                  : states.some((state) => ACTIVE.has(state)) ? 'working'
                    : ready !== undefined ? readyAgent === undefined ? 'unclaimed' : broken(readyAgent) ? 'unstaffed' : 'queued'
                      : states.some((state) => state === 'waiting' || state === 'pending') ? 'held'
                        : ['standing', 'running', 'ready', 'draft'].includes(missionState) ? 'idle'
                          : 'done'
  const owner = (step: Step, stepWordValue: StepWord): Who => {
    if (stepWordValue === 'needs_you') return { kind: 'you', label: 'you' }
    if (step.agentless === true) return { kind: 'st', label: 'st' }
    const seat = seatOf(step)
    return seat === undefined ? { kind: 'nobody', label: 'nobody yet' } : { kind: 'agent', agent: seat, label: label(seat) }
  }
  const steps = work
    .map((step): PresentedStep => {
      const presented = stepWord(step, gated)
      const queuing = step.state === 'ready' && Option.isNone(step.claimant) ? queuedFor(agents, step.id) : undefined
      const busy = queuing?.current_work?.[0]
      const note = queuing !== undefined
        ? busy !== undefined && !broken(queuing)
          ? `queued for ${queuing.name}, which is busy with ${Option.getOrUndefined(busy.title) ?? busy.path}`
          : `queued for ${queuing.name}, which is ${agentState(queuing)}`
        : ['waiting', 'blocked'].includes(step.state) ? Option.getOrNull(step.blocked_reason) : null
      return {
        id: step.id,
        path: step.path,
        title: Option.getOrUndefined(step.title) ?? step.path,
        word: presented,
        owner: owner(step, presented),
        note,
        attempts: step.attempt,
        blockers: step.blockers ?? [],
        keep_open: step.keep_open === true,
      }
    })
    .map((step, index) => ({ step, index }))
    .sort((a, b) => STEP_ORDER[a.step.word] - STEP_ORDER[b.step.word] || a.index - b.index)
    .map(({ step }) => step)
  // The stuck step: the first failed, ready or waiting one, for a mission that is not moving.
  const stuckStep = ['stalled', 'failed', 'unstaffed', 'queued'].includes(word)
    ? work.find((step) => ['failed', 'ready', 'waiting', 'blocked'].includes(step.state))
    : undefined
  const stuckSeat = stuckStep === undefined ? undefined : seatOf(stuckStep) ?? queuedFor(agents, stuckStep.id)?.id
  const stuckAgent = stuckSeat === undefined ? undefined : agents.find((agent) => agent.id === stuckSeat)
  const workingStep = work.find((step) => ACTIVE.has(step.state) && step.agentless !== true)
  const open = work.filter((step) => !FINISHED.has(step.state))
  const whoSeat = word === 'working' ? (workingStep === undefined ? undefined : seatOf(workingStep)) : stuckSeat
  const who: Who =
    word === 'decision' ? { kind: 'you', label: 'you' }
      : ['working', 'queued', 'unstaffed', 'stalled', 'failed'].includes(word) && whoSeat !== undefined ? { kind: 'agent', agent: whoSeat, label: label(whoSeat) }
        : open.length > 0 && open.every((step) => step.agentless === true) ? { kind: 'st', label: 'st' }
          : { kind: 'nobody', label: 'nobody' }
  const latest = (mission.run_details ?? []).at(-1)
  const settled = latest === undefined ? undefined : Option.getOrUndefined(latest.outcome)
  const inHand = work.find((step) => ACTIVE.has(step.state) && Option.isSome(step.last_progress))
  return {
    word,
    system: row.system === true,
    who,
    steps,
    stuck: stuckStep === undefined ? null : {
      step: stuckStep.id,
      word: stepWord(stuckStep, gated),
      attempts: stuckStep.attempt,
      blockers: stuckStep.blockers ?? [],
      agent: stuckAgent === undefined ? stuckSeat === undefined ? null : { id: stuckSeat, label: label(stuckSeat), state: 'unknown' } : { id: stuckAgent.id, label: stuckAgent.name, state: agentState(stuckAgent) },
    },
    progress: { done: work.filter((step) => step.state === 'completed').length, total: work.length },
    last_progress: inHand === undefined ? null : Option.getOrNull(inHand.last_progress),
    settled_at: ['done', 'failed', 'cancelled'].includes(word) && settled !== undefined ? DateTime.toEpochMillis(settled.at) : ['done', 'failed', 'cancelled'].includes(word) ? DateTime.toEpochMillis(mission.updated_at) : null,
  }
}

/** Missions in `words.json` order, st's order within each word. */
export const sortMissions = <T extends { readonly presentation: MissionPresentation }>(rows: readonly T[]): readonly T[] =>
  rows.map((row, index) => ({ row, index }))
    .sort((a, b) => MISSION_WORDS.indexOf(a.row.presentation.word) - MISSION_WORDS.indexOf(b.row.presentation.word) || a.index - b.index)
    .map(({ row }) => row)

/** Hidden by default, in the client as the contract says: st's own, never started, or settled before today. */
export const hiddenByDefault = (presentation: MissionPresentation, startOfToday: number): boolean =>
  presentation.system || presentation.word === 'not_started' || (presentation.settled_at !== null && presentation.settled_at < startOfToday)
