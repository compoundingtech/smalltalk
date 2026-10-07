import type { ActionCommon, Attention, Mission } from '@smalltalk/st3-client/schema'

/** Mission actions exposed by the fixture-only proposal overlay. */
export type MissionActionType = Extract<ActionCommon['type'], `mission.${string}`>

// NOT IN THE PROTOCOL. Fields the missions views need that client-v0 does not serve today. They are
// kept apart from the resource so every view can render with and without them, which is exactly
// the gap list handed back to smalltalk (see the story docs).
// ---------------------------------------------------------------------------------------------

/** A recurring schedule as declared in the mission KDL (`schedule "…" { every; anchor; … }`). */
export interface ProposedSchedule {
  readonly name: string
  readonly every: string
  readonly anchor: string
  readonly timezone?: string
  readonly catchUp: 'latest' | 'all' | 'skip'
  readonly nextRunAt: string
  /** The finite mission each occurrence runs. */
  readonly worksMissionId: string
}

/** An axe decision filed per CAG.MIS-R03 and its StewardDecision alert. */
export interface ProposedEscalation {
  readonly handle: string
  readonly question: string
  readonly options: readonly string[]
  readonly state: 'open' | 'answered'
  readonly alert: 'firing' | 'resolved' | 'expired'
  readonly askedAt: string
  readonly stepPath: string
}

/** Mission fields the views need that the protocol does not serve yet (gap list for smalltalk). */
export interface ProposedMissionFields {
  readonly schedule?: ProposedSchedule
  /** Set on a finite cycle mission started by a schedule parent. */
  readonly scheduledBy?: string
  readonly escalation?: ProposedEscalation
  /** Mission actions currently meaningful, each with the fence it must be sent with. */
  readonly actions?: ReadonlyArray<{ readonly id: MissionActionType; readonly fence: string }>
  /** Executor seat and goal from the declaration (today only inside `visualization`/step assignees). */
  readonly executor?: string
}

/** Everything one mission row or detail needs: the resource, its open attention, the proposals. */
export interface MissionView {
  readonly mission: Mission
  readonly attention: readonly Attention[]
  readonly proposed?: ProposedMissionFields | undefined
}
