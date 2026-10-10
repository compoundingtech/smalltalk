/**
 * The fixture world projected into the data seam's row types (`FixtureProjections`).
 *
 * `world.ts` owns the entities; the feature fixture modules shape them into wire/view models; this
 * module only assembles those into the one value `fixtureSource` serves.
 */
import {
  Agent,
  Attention,
  decodeUnknownSync,
  type AgentEncoded,
  type AttentionEncoded,
} from '@smalltalk/st3-client/schema'

import { agentConversations } from '../conversation/fixtures.ts'
import type { FixtureProjections } from '../data/fixtureSource.ts'
import { fixtureMissions, fixtureAttention, fixtureProposed } from '../missions/fixtures.ts'
import { worldUsageSource } from '../monitor/fixtures.ts'
import type { SubjectEnvelope } from '../resources/envelope.ts'
import {
  agentEnvelope,
  attentionEnvelope,
  ciRunEnvelopes,
  missionEnvelope,
  ptyEnvelope,
  pullRequestEnvelopes,
} from '../resources/fixtures.ts'
import { agentScreens } from '../terminal/fixtures.ts'
import { extensionFixtures } from '../extensions/build.ts'
import { agents, events, gatewayHost, hosts, operator, WORLD_NOW_ISO, worldNow } from './world.ts'

const envelopes: readonly SubjectEnvelope[] = [
  missionEnvelope,
  agentEnvelope,
  ptyEnvelope,
  attentionEnvelope,
  ...pullRequestEnvelopes,
  ...ciRunEnvelopes,
  ...extensionFixtures.envelopes,
]

const agentRows = agents.map((agent) =>
  decodeUnknownSync(
    Agent,
    'strict',
  )({
    kind: 'agent',
    id: agent.ref,
    name: agent.name,
    host_id: `host/${agent.host}`,
    driver: agent.session.harness,
    state:
      agent.activity === 'errored'
        ? 'failed'
        : agent.activity === 'working'
          ? 'running'
          : agent.activity === 'waiting'
            ? 'waiting'
            : 'stopped',
    blocked_on: agent.activity === 'waiting' ? 'human' : null,
    fault: agent.activity === 'errored' ? agent.status : null,
    reason: agent.status,
    runtime_ids: [],
    reachability: hosts.find((host) => host.id === agent.host)?.connected
      ? 'reachable'
      : 'unreachable',
    revision: '1',
    updated_at: WORLD_NOW_ISO,
  } satisfies AgentEncoded),
)
const agentAttention = agents
  .filter((agent) => agent.activity === 'waiting' || agent.activity === 'errored')
  .map((agent) =>
    decodeUnknownSync(
      Attention,
      'strict',
    )({
      kind: 'attention',
      id: `attention/${agent.slug}`,
      actions: [],
      attention_kind: agent.activity === 'errored' ? 'fault' : 'agent-request',
      detail: agent.status,
      person_id: operator.ref,
      priority: agent.activity === 'errored' ? 'high' : 'normal',
      requested_at: WORLD_NOW_ISO,
      revision: '1',
      updated_at: WORLD_NOW_ISO,
      source_id: agent.ref,
      state: 'open',
      title: agent.name,
    } satisfies AttentionEncoded),
  )

/** Shared fixture graph projected into the data source's fleet, resource and feature rows. */
export const fixtureProjections: FixtureProjections = {
  now: worldNow,
  gateway: gatewayHost,
  events,
  agents: agentRows,
  missions: fixtureMissions,
  attention: [...fixtureAttention, ...agentAttention],
  proposed: fixtureProposed,
  conversations: agentConversations,
  terminals: agentScreens,
  envelopes: Object.fromEntries(envelopes.map((envelope) => [envelope.ref, envelope])),
  usage: worldUsageSource,
}
