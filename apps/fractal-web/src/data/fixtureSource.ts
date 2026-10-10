/** Constant generated rows over the deterministic fixture world. */
import type { Agent, Attention, Mission, TerminalScreen } from '@smalltalk/st3-client/schema'
import type { ConnectionState } from '@st3/sdk/effect'
import * as Atom from 'effect/reactivity/Atom'

import type { ProposedMissionFields } from '../missions/model.ts'
import type { MonitorSource } from '../monitor/source.ts'
import type { SubjectEnvelope } from '../resources/envelope.ts'
import {
  type ConversationPage,
  type DataSource,
  type Feed,
  type GatewayEvent,
  type Grants,
  observed,
  unavailable,
} from './source.ts'
import { fixtureSubjectReads, type SubjectReadFixtures } from './subjectReadPort.ts'

/** Strict-decoded fixture rows and renderer resources for a deterministic world. */
export interface FixtureProjections {
  readonly now: number
  readonly gateway?: string
  readonly events: readonly GatewayEvent[]
  readonly agents: readonly Agent[]
  readonly missions: readonly Mission[]
  readonly attention: readonly Attention[]
  readonly proposed?: Readonly<Record<string, ProposedMissionFields>>
  readonly conversations: Readonly<Record<string, ConversationPage>>
  readonly terminals: Readonly<Record<string, TerminalScreen>>
  readonly envelopes: Readonly<Record<string, SubjectEnvelope>>
  readonly usage: MonitorSource
  readonly subjectReads?: SubjectReadFixtures
}
/** Feed overrides for explicit connection, freshness and failure scenarios. */
export interface FixtureOverrides {
  readonly connection?: ConnectionState
  readonly grants?: Grants
  readonly agents?: Feed<readonly Agent[]>
  readonly missions?: Feed<readonly Mission[]>
  readonly attention?: Feed<readonly Attention[]>
  readonly events?: Feed<FixtureProjections['events']>
  readonly conversation?: Readonly<Record<string, Feed<ConversationPage>>>
  readonly terminal?: Readonly<Record<string, Feed<TerminalScreen>>>
  readonly envelope?: Readonly<Record<string, Feed<SubjectEnvelope>>>
  readonly subjectReads?: SubjectReadFixtures
}
/** Construct a source from immutable fixtures with optional scenario overrides. */
export const fixtureSource = ({
  world,
  overrides = {},
}: {
  readonly world: FixtureProjections
  readonly overrides?: FixtureOverrides
}): DataSource => ({
  mode: 'fixtures',
  label: 'fixtures',
  subjectReads: fixtureSubjectReads({
    fixtures: {
      agent: Object.fromEntries(world.agents.map((value) => [value.id, observed({ value })])),
      mission: Object.fromEntries(world.missions.map((value) => [value.id, observed({ value })])),
      missionRun: Object.fromEntries(
        world.missions.flatMap((mission) =>
          (mission.run_details ?? []).map((value) => [value.id, observed({ value })]),
        ),
      ),
      attention: Object.fromEntries(
        world.attention.map((value) => [value.id, observed({ value })]),
      ),
      terminalScreen: Object.fromEntries(
        Object.entries(world.terminals).map(([ref, value]) => [ref, observed({ value })]),
      ),
      ...world.subjectReads,
    },
    ...(overrides.subjectReads === undefined ? {} : { overrides: overrides.subjectReads }),
  }),
  ...(world.gateway === undefined ? {} : { gateway: world.gateway }),
  ...(world.proposed === undefined ? {} : { proposed: world.proposed }),
  now: Atom.make(world.now),
  grants: Atom.make(
    overrides.grants ?? { actions: 'granted', messageSend: 'ungranted', terminalInput: 'granted' },
  ),
  connection: Atom.make<ConnectionState>(overrides.connection ?? { _tag: 'Live' }),
  agents: Atom.make(overrides.agents ?? observed({ value: world.agents })),
  missions: Atom.make(overrides.missions ?? observed({ value: world.missions })),
  attention: Atom.make(overrides.attention ?? observed({ value: world.attention })),
  events: Atom.make(overrides.events ?? observed({ value: world.events })),
  conversation: keyed({ table: world.conversations, pinned: overrides.conversation }),
  terminal: keyed({ table: world.terminals, pinned: overrides.terminal }),
  envelope: keyed({ table: world.envelopes, pinned: overrides.envelope }),
  usage: world.usage,
})
const keyed = <A>({
  table,
  pinned,
}: {
  readonly table: Readonly<Record<string, A>>
  readonly pinned: Readonly<Record<string, Feed<A>>> | undefined
}) =>
  Atom.family((ref: string) =>
    Atom.make<Feed<A>>(
      pinned?.[ref] ??
        (ref in table
          ? observed({ value: table[ref] as A })
          : unavailable({ reason: 'failed', detail: `fixture world has no ${ref}` })),
    ),
  )
