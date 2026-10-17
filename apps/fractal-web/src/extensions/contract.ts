/** A composition supplies bindings; the application retains its routing and claim vocabulary. */
export interface ExtensionSet<TPane, THost, TClaim> {
  readonly panes: readonly TPane[]
  readonly hosts: readonly THost[]
  readonly claims: readonly TClaim[]
}

export interface ExtensionRegistry<TPane, THost, TClaim> {
  readonly panes: readonly TPane[]
  readonly hosts: readonly THost[]
  readonly publicClaims: readonly TClaim[]
  readonly extensionClaims: readonly TClaim[]
  readonly claims: readonly TClaim[]
}

/** Hosts decide overlap semantics once, including overlaps between both ownership groups. */
export interface RegistryInput<TPane, THost, TClaim> {
  readonly publicClaims: readonly TClaim[]
  readonly extensions: ExtensionSet<TPane, THost, TClaim>
  readonly validateClaims: (claims: readonly TClaim[]) => void
}

import type { ComponentType, ReactNode } from 'react'
import type { ObservationReactView } from '../resources/observation.tsx'
import type { NativeSubjectView } from '../resources/react.tsx'
import type { RegisteredRenderer, RegisteredSubjectRenderer, Registry } from '../resources/registry.ts'
import type { SubjectEnvelope } from '../resources/envelope.ts'
import type { SubjectSummary } from '../shell/context.tsx'
import type { Feed } from '../data/source.ts'
import type { DetailsDisplayObservation, DetailsSection, TraceAccumulationWindow, TraceCoverage, TraceFact, TraceSubagent } from '@smalltalk/fractal-ui/assistant-ui'

export interface AgentRowProps {
  readonly agentRef: string
  readonly onOpen: () => void
  readonly onDetails?: () => void
  readonly children: ReactNode
}
export interface DiffProps {
  readonly id: string
  readonly source:
    | { readonly _tag: 'patch'; readonly patch: string }
    | { readonly _tag: 'files'; readonly path: string; readonly oldContents: string; readonly newContents: string }
  readonly view: 'unified'
  readonly caption?: string
}
export interface ExtensionViews {
  readonly AgentSignals?: ComponentType<{ readonly agentRef: string }>
  readonly AgentRow?: ComponentType<AgentRowProps>
  readonly Diff?: ComponentType<DiffProps>
  readonly Markdown?: ComponentType<{ readonly source: string }>
}
/** Claims retain the existing conflict validation; extensions cannot override public claims. */
export interface FractalExtensions {
  readonly renderers: readonly RegisteredRenderer<ReactNode>[]
  readonly native: readonly RegisteredSubjectRenderer<NativeSubjectView>[]
}
export interface ExtensionHost {
  readonly getRegistry: () => Registry<ReactNode, NativeSubjectView, ObservationReactView>
}
export type CreateExtensions = (host: ExtensionHost) => FractalExtensions
export interface ExtensionFixtures {
  readonly envelopes: readonly SubjectEnvelope[]
  readonly subjects: readonly SubjectSummary[]
}

/** A host contribution supplements independently scoped facts, never native session accounting. */
export interface DetailsSupplementQuery {
  readonly agentRef: string
  readonly nativeSessionId?: string
  /** False permits matching retained observations only, never another provider read. */
  readonly canRefresh?: boolean
  /** Native identity/root prerequisites are still acquiring; missing fields are not settled absence. */
  readonly nativeRootPending?: boolean
  readonly incarnationId?: string
  /** Canonical native host reference used for identity, not a formatted display name. */
  readonly host?: string
  readonly harness?: string
}
export interface DetailsSupplement {
  readonly nativeSessionId: string
  readonly totalUsd: TraceFact<number>
  readonly usdWindow: TraceFact<TraceAccumulationWindow>
  readonly coverage: TraceCoverage
  readonly subagents: TraceFact<readonly TraceSubagent[]>
  readonly subagentsWindow: TraceFact<TraceAccumulationWindow>
  readonly subagentCount: TraceFact<number>
  readonly durations?: readonly DetailsDisplayObservation[]
  /** Client time of the last successful read, not the producer's observation time. */
  readonly receivedAtMs?: number
}
export type UseDetailsSupplement = (query: DetailsSupplementQuery) => Feed<DetailsSupplement>
/** Protected host metadata sections are independent of any trace/root binding. */
export type UseDetailsSections = (query: { readonly agentRef: string }) => readonly DetailsSection[]
