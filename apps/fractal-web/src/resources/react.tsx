import type { Schema } from 'effect'
/**
 * React bindings close over each schema decoder and its typed components.
 * The registry selects already-constructed React nodes, never erased data/view pairs.
 * Stays in wf; the contract and registry above it are UI-kit-free.
 */
import { createContext, type ComponentType, type ReactNode, useContext } from 'react'

import type {
  Density,
  HostCapabilities,
  RendererMatch,
  RendererNeeds,
  Representation,
  SubjectAddress,
} from './contract.ts'
import { representations } from './contract.ts'
import {
  type ActionOffer,
  decodeEnvelope,
  decodeWith,
  parseSchemaId,
  type SubjectEnvelope,
} from './envelope.ts'
import { type DataOf, dataSchemas, type KnownSchemaId } from './families.ts'
import type { RegisteredRenderer, Registry, RendererBinding } from './registry.ts'

/** Native subject surfaces read their own authoritative feeds, without fabricating an envelope. */
export interface NativeSubjectProps {
  readonly address: SubjectAddress
  /** Hidden retained tabs must not start visible-only readers or resize observations. */
  readonly visibility: 'visible' | 'hidden'
}
/** Native subject component selected by the registry for an authoritative subject address. */
export type NativeSubjectView = ComponentType<NativeSubjectProps>

/** What every renderer view receives: the envelope, its decoded `data`, and where it is drawn. */
export interface RenderProps<A> {
  readonly envelope: SubjectEnvelope
  readonly data: A
  readonly representation: Representation
  readonly density: Density
  readonly host: HostCapabilities
  /** Set when the generic view stands in for a renderer whose schema rejected `data`. */
  readonly issue?: { readonly rejectedBy: string; readonly message: string } | undefined
}

type Views<A> = { readonly [R in Representation]?: ComponentType<RenderProps<A>> }

const bindViews = <A,>({
  decoder,
  views,
}: {
  readonly decoder: Schema.Decoder<A>
  readonly views: Views<NoInfer<A>>
}) => {
  const bindings: { [R in Representation]?: RendererBinding<ReactNode> } = {}
  for (const representation of representations) {
    const View = views[representation]
    if (View === undefined) continue
    bindings[representation] = (request) => {
      const decoded = decodeWith({ schema: decoder, input: request.envelope.data })
      if (!decoded.ok) return decoded
      return { ok: true, view: <View {...request} data={decoded.value} /> }
    }
  }
  return bindings
}

/** Any-level renderer with its own decoder (schema, family, generic levels). */
export const defineRenderer = <A,>(spec: {
  readonly id: string
  readonly match: RendererMatch
  readonly needs: RendererNeeds
  readonly data: Schema.Decoder<A>
  readonly views: Views<NoInfer<A>>
}): RegisteredRenderer<ReactNode> => ({
  manifest: {
    id: spec.id,
    match: spec.match,
    needs: spec.needs,
    representations: representations.filter(
      (representation) => spec.views[representation] !== undefined,
    ),
  },
  bindings: bindViews({ decoder: spec.data, views: spec.views }),
})

/** Exact-level renderer for one known schema id; data type and decoder come from the catalog. */
export const defineSchemaRenderer = <Id extends KnownSchemaId>(spec: {
  readonly schema: Id
  readonly id: string
  readonly needs: RendererNeeds
  readonly views: Views<NoInfer<DataOf<Id>>>
}): RegisteredRenderer<ReactNode> => {
  const { schema } = spec
  const parsed = parseSchemaId(schema)
  if (parsed === undefined) throw new Error(`not a schema id: ${schema}`)
  const decoder: Schema.Decoder<DataOf<Id>> = dataSchemas[schema]
  return defineRenderer({
    id: spec.id,
    match: {
      level: 'exact',
      schema: parsed.name,
      majors: { min: parsed.major, max: parsed.major },
    },
    needs: spec.needs,
    data: decoder,
    views: spec.views,
  })
}

/** Host-level context renderers read without prop drilling: clock and action dispatch. */
export interface RenderEnvironment {
  /** Epoch ms used for relative times; stories pin it for stable screenshots. */
  readonly now: number
  readonly dispatch?: (action: {
    readonly envelope: SubjectEnvelope
    readonly offer: ActionOffer
  }) => void
}

const RenderEnvironmentContext = createContext<RenderEnvironment>({
  now: Date.now(),
})
/** Supplies clock and action dispatch to every renderer below. */
export const RenderEnvironmentProvider = RenderEnvironmentContext.Provider
/** The nearest render environment; outside a provider actions remain unavailable. */
export const useRenderEnvironment = () => useContext(RenderEnvironmentContext)

/** Inputs for drawing one raw envelope through a registry. */
export interface SubjectViewProps {
  readonly registry: Registry<ReactNode>
  /** Raw wire value; decoded here so malformed envelopes still draw something. */
  readonly value: unknown
  readonly representation: Representation
  readonly density: Density
  readonly host: HostCapabilities
  /** Drawn when the envelope header itself fails to decode. */
  readonly malformed: ComponentType<{
    readonly input: unknown
    readonly issue: string
    readonly representation: Representation
  }>
}

/** Decodes `value` and draws the registry's plan for it, or `malformed` when the header is invalid. */
export const SubjectView = ({
  registry,
  value,
  representation,
  density,
  host,
  malformed: Malformed,
}: SubjectViewProps): ReactNode => {
  const decoded = decodeEnvelope(value)
  if (decoded._tag === 'Malformed') {
    return <Malformed input={decoded.input} issue={decoded.issue} representation={representation} />
  }
  const plan = registry.plan({ envelope: decoded.envelope, representation, density, host })
  return plan.view
}
