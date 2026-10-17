import { Predicate } from 'effect'
import type { AgentDeclaration, PublicationDefinition } from '@smalltalk/st3-client/schema'
import type { DeclarationPanelState } from '@smalltalk/fractal-ui/assistant-ui/shell'
import type { Feed } from '../data/source.ts'

/** st's placeholder for an environment value it did not disclose. */
export const REDACTED = '<redacted>'
/** Embedded declaration text is KDL st does not redact; the web client never shows it. */
export const WITHHELD_DECLARATIONS = '<withheld: embedded declarations can carry environment values>'

/**
 * Which canonical source serves a subject. Agents: `GET /v1/client/agent-declarations/{id}` with
 * `show_env_values=false`, KDL rendered and redacted by st. Missions: st keeps no canonical
 * mission KDL, so `GET /v1/client/publication-definition` serves the structured definition,
 * shown as JSON and redacted here because that read has no `show_env_values` switch.
 */
export type DeclarationSubject =
  | { readonly _tag: 'Agent'; readonly ref: string }
  | { readonly _tag: 'Mission'; readonly ref: string }
  | { readonly _tag: 'Unsupported'; readonly ref: string }

export const declarationSubject = (ref: string): DeclarationSubject =>
  ref.startsWith('agent/') ? { _tag: 'Agent', ref }
    : ref.startsWith('mission/') ? { _tag: 'Mission', ref }
    : { _tag: 'Unsupported', ref }

type Fields = Record<string, (child: unknown) => unknown>
/** Rewrite only the named fields of one spec object; every other key, including user-chosen ids, is kept as is. */
const patch = (value: unknown, fields: Fields): unknown =>
  Predicate.isObject(value) ? Object.fromEntries(Object.entries(value).map(([key, child]) => [key, Object.hasOwn(fields, key) ? fields[key]!(child) : child])) : value
const each = (redact: (item: unknown) => unknown) => (list: unknown): unknown => Array.isArray(list) ? list.map(redact) : list

/** An environment map keeps its names, as st's agent redaction does; a malformed one is hidden whole. */
const hideEnvironment = (environment: unknown): unknown =>
  Predicate.isObject(environment) ? Object.fromEntries(Object.keys(environment).map(name => [name, REDACTED])) : REDACTED
const withhold = (kdl: unknown): unknown => typeof kdl === 'string' ? WITHHELD_DECLARATIONS : kdl

// The positions below mirror st's MissionSpec serde shape (crates/st3/src/model.rs): environment
// values live only in mechanical/llm `GateSpec.environment` and exec `MetricSource.environment`;
// embedded declaration text in `MissionSpec`/`StepSpec.declarations_kdl`.
const gate = (value: unknown): unknown => patch(value, { environment: hideEnvironment })
const gates = each(gate)
const dependencies = each(dependency => patch(dependency, { gate }))
const baselines = each(baseline => patch(baseline, { gates }))
const loop = (value: unknown): unknown => patch(value, {
  metrics: each(metric => patch(metric, { source: source => patch(source, { environment: hideEnvironment }) })),
  until: gates,
  candidates: candidates => patch(candidates, { select: select => patch(select, { gate }) }),
  on_exhausted: outcome => patch(outcome, { gate }),
  round: redactMissionDefinition,
  on_keep: redactMissionDefinition,
  on_discard: redactMissionDefinition,
})
const step = (value: unknown): unknown => patch(value, { declarations_kdl: withhold, dependencies, baselines, gates, nested_mission: redactMissionDefinition, loop_spec: loop })

/**
 * Hide environment values and embedded declaration text in a mission definition, leaving everything
 * else verbatim. Step ids are user-chosen keys (a step may be named `env`), so each step value is
 * walked as a StepSpec and no key name outside these positions is matched. Idempotent.
 */
export const redactMissionDefinition = (value: unknown): unknown => patch(value, {
  declarations_kdl: withhold,
  completion: completion => patch(completion, { dependencies }),
  baselines,
  gates,
  steps: steps => Predicate.isObject(steps) ? Object.fromEntries(Object.entries(steps).map(([id, child]) => [id, step(child)])) : steps,
})

/** Applied where the definition is decoded, so an undisclosed value never reaches a feed or cache. */
export const redactPublicationDefinition = (definition: PublicationDefinition): PublicationDefinition => ({
  ...definition,
  declaration: redactMissionDefinition(definition.declaration) as PublicationDefinition['declaration'],
})

type Unread = Extract<Feed<unknown>, { readonly _tag: 'Unavailable' }>
const unread = (feed: Unread): DeclarationPanelState =>
  feed.reason === 'ungranted' ? { _tag: 'Unavailable', message: 'Read access to declarations has not been granted to this connection.' }
    : feed.reason === 'unsupported' ? { _tag: 'Unavailable', message: `This connection cannot read declarations. ${feed.detail}` }
    : { _tag: 'Failed', message: `The declaration could not be loaded: ${feed.detail.replace(/\.$/, '')}. Nothing was changed; close and try again.` }

/** The agent's canonical KDL exactly as st rendered it. */
export const agentDeclarationState = (feed: Feed<AgentDeclaration>): DeclarationPanelState =>
  feed._tag === 'Waiting' ? { _tag: 'Loading' }
    : feed._tag === 'Unavailable' ? unread(feed)
    : { _tag: 'Ready', text: feed.value.kdl, language: 'kdl', source: `Canonical KDL from st · revision ${feed.value.revision} · environment values redacted${feed.freshness === 'stale' ? ' · last verified copy, reconnecting' : ''}` }

/** The mission's structured definition, labelled as a fallback; never presented as KDL. */
export const missionDeclarationState = (feed: Feed<PublicationDefinition>): DeclarationPanelState =>
  feed._tag === 'Waiting' ? { _tag: 'Loading' }
    : feed._tag === 'Unavailable' ? unread(feed)
    : { _tag: 'Ready', text: `${JSON.stringify(redactMissionDefinition(feed.value.declaration), null, 2)}\n`, language: 'json', source: `Structured definition · revision ${feed.value.revision} · st serves no mission KDL; environment values redacted${feed.freshness === 'stale' ? ' · last verified copy, reconnecting' : ''}` }
