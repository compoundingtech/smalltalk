// moves to smalltalk/clients/typescript
/**
 * Renderer contract: UI-kit-free, JSON-serializable, so stui (Rust/ratatui), iOS (Swift/RN)
 * and wf (React) implement the same manifests and the same resolution algorithm, each binding
 * its own views. A manifest says *what* a renderer claims; a platform binding says *how* it
 * draws. Resolution is a pure function of (manifests, envelope header, representation, host);
 * its trace is the cross-client conformance fixture.
 *
 * Fallback chain, most specific first:
 *   exact   `st3.mission` with major in [min, max]
 *   family  `resource`, optionally kind glob `vcs.*`
 *   generic fact-bag / typed reference — always present, renders anything
 */
import { Id } from '@smalltalk/st3-client/schema'
import { Schema } from 'effect'

import { Family, parseSchemaId } from './envelope.ts'

/** How much room a subject gets. */
export const Representation = Schema.Literals(['inline', 'row', 'card', 'detail'])
export type Representation = typeof Representation.Type
/** Every representation, smallest first; the generic renderer must draw all of them. */
export const representations: readonly Representation[] = ['inline', 'row', 'card', 'detail']

/** Information density inside a representation; hosts pick one per surface. */
export const Density = Schema.Literals(['compact', 'regular', 'rich'])
export type Density = typeof Density.Type

/** Local presentation identity is independent of the component chosen to render it. */
export const PresentationId = Schema.Literals(['detail', 'overview', 'resources']).annotate({
  identifier: 'Wf.Subject.PresentationId',
})
export type PresentationId = typeof PresentationId.Type

/** Native subjects can be addressed before their first authoritative observation arrives. */
export const SubjectAddress = Schema.Struct({
  ref: Id,
  presentation: PresentationId,
}).annotate({ identifier: 'Wf.Subject.Address' })
export type SubjectAddress = typeof SubjectAddress.Type

/** Native view routing is local presentation policy, never fabricated gateway metadata. */
export const SubjectRendererManifest = Schema.Struct({
  id: Schema.NonEmptyString,
  family: Schema.Union([Family, Schema.Literal('*')]),
  presentations: Schema.Array(PresentationId).check(Schema.isMinLength(1)),
  /** Why a bespoke native surface is needed instead of the observed fact-bag fallback. */
  reason: Schema.NonEmptyString,
}).annotate({ identifier: 'Wf.Subject.RendererManifest' })
export type SubjectRendererManifest = typeof SubjectRendererManifest.Type

/** Semantic bodies for generated graph observations, without a provisional envelope adapter. */
export const ObservationAddress = Schema.Struct({
  kind: Schema.NonEmptyString,
  representation: Representation,
}).annotate({ identifier: 'Wf.Subject.ObservationAddress' })
export type ObservationAddress = typeof ObservationAddress.Type

/** Declares the observed kind and representations supplied by one semantic renderer. */
export const ObservationRendererManifest = Schema.Struct({
  id: Schema.NonEmptyString,
  kind: Schema.NonEmptyString,
  representations: Schema.Array(Representation).check(Schema.isMinLength(1)),
  reason: Schema.NonEmptyString,
}).annotate({ identifier: 'Wf.Subject.ObservationRendererManifest' })
export type ObservationRendererManifest = typeof ObservationRendererManifest.Type

/** Which envelopes a renderer claims: one schema's majors, a family (optionally a kind glob), or anything. */
export const RendererMatch = Schema.Union(
  [
    Schema.Struct({
      level: Schema.Literal('exact'),
      schema: Schema.String,
      majors: Schema.Struct({ min: Schema.Int, max: Schema.Int }),
    }),
    Schema.Struct({
      level: Schema.Literal('family'),
      family: Schema.String,
      /** Glob over the schema kind segment; `*` only as a trailing wildcard, e.g. `vcs.*`. */
      kind: Schema.optionalKey(Schema.String),
    }),
    Schema.Struct({ level: Schema.Literal('generic') }),
  ],
  { mode: 'oneOf' },
).annotate({ identifier: 'RendererMatch' })
export type RendererMatch = typeof RendererMatch.Type
/** Resolution tier, most specific first: `exact` → `family` → `generic`. */
export type MatchLevel = RendererMatch['level']

/** What a renderer needs from its host before it may be chosen. */
export const RendererNeeds = Schema.Struct({
  /** `required`: unusable without the live channel (skipped on static hosts). */
  live: Schema.Literals(['none', 'optional', 'required']),
  /** Action ids the renderer can surface; hosts without dispatch render them disabled. */
  actions: Schema.Array(Schema.String),
}).annotate({ identifier: 'RendererNeeds' })
export type RendererNeeds = typeof RendererNeeds.Type

/** A renderer's platform-independent claim: what it matches, which representations it draws, what it needs. */
export const RendererManifest = Schema.Struct({
  /** Stable renderer id, e.g. `wf.mission`; unique per registry. */
  id: Schema.String,
  match: RendererMatch,
  representations: Schema.Array(Representation),
  needs: RendererNeeds,
}).annotate({ identifier: 'RendererManifest' })
export type RendererManifest = typeof RendererManifest.Type

/** What the drawing surface can do right now. */
export const HostCapabilities = Schema.Struct({
  live: Schema.Boolean,
  actions: Schema.Boolean,
}).annotate({ identifier: 'HostCapabilities' })
export type HostCapabilities = typeof HostCapabilities.Type

/** The envelope fields resolution reads; every client can decode these for any schema. */
export interface RoutingHeader {
  readonly schema: string
  readonly family: string
}

/** Why one manifest was or was not chosen at one resolution step. */
export type StepOutcome =
  | 'chosen'
  | 'no-renderer'
  | 'version-mismatch'
  | 'missing-representation'
  | 'needs-live'

/** One line of the resolution trace. */
export interface ResolutionStep {
  readonly level: MatchLevel
  readonly rendererId: string | undefined
  readonly outcome: StepOutcome
}

/** The chosen manifest plus the trace that led to it (the cross-client conformance fixture). */
export interface Resolution {
  readonly manifest: RendererManifest
  readonly trace: readonly ResolutionStep[]
}

const levels: readonly MatchLevel[] = ['exact', 'family', 'generic']

const kindGlobMatches = ({
  glob,
  kind,
}: {
  readonly glob: string | undefined
  readonly kind: string | undefined
}): boolean => {
  if (glob === undefined) return true
  if (kind === undefined) return false
  return glob.endsWith('*') ? kind.startsWith(glob.slice(0, -1)) : kind === glob
}

/** Does `manifest` claim this header at its own level, ignoring version? */
const claims = ({ manifest, header }: ManifestHeader): boolean => {
  const parsed = parseSchemaId(header.schema)
  const match = manifest.match
  switch (match.level) {
    case 'exact':
      return parsed?.name === match.schema
    case 'family':
      return (
        header.family === match.family && kindGlobMatches({ glob: match.kind, kind: parsed?.kind })
      )
    case 'generic':
      return true
  }
}

const versionOk = ({ manifest, header }: ManifestHeader): boolean => {
  if (manifest.match.level !== 'exact') return true
  const major = parseSchemaId(header.schema)?.major
  return (
    major !== undefined && major >= manifest.match.majors.min && major <= manifest.match.majors.max
  )
}

/**
 * Resolves the renderer for one envelope at one representation. Walks the levels in order and,
 * within a level, manifests in registration order; the first manifest that claims the header,
 * accepts its major, offers the representation and whose live need the host meets wins. The
 * generic manifest must claim everything, so resolution is total.
 */
export const resolveManifest = ({
  manifests,
  header,
  representation,
  host,
}: {
  readonly manifests: readonly RendererManifest[]
  readonly header: RoutingHeader
  readonly representation: Representation
  readonly host: HostCapabilities
}): Resolution => {
  const trace: ResolutionStep[] = []
  for (const level of levels) {
    const atLevel = manifests.filter(
      (manifest) => manifest.match.level === level && claims({ manifest, header }),
    )
    if (atLevel.length === 0) {
      trace.push({ level, rendererId: undefined, outcome: 'no-renderer' })
      continue
    }
    for (const manifest of atLevel) {
      const outcome: StepOutcome = !versionOk({ manifest, header })
        ? 'version-mismatch'
        : !manifest.representations.includes(representation)
          ? 'missing-representation'
          : manifest.needs.live === 'required' && !host.live
            ? 'needs-live'
            : 'chosen'
      trace.push({ level, rendererId: manifest.id, outcome })
      if (outcome === 'chosen') return { manifest, trace }
    }
  }
  throw new Error(`No generic renderer claims ${header.schema} at ${representation}`)
}

const overlaps = ({ a, b }: { readonly a: RendererMatch; readonly b: RendererMatch }): boolean => {
  if (a.level !== b.level) return false
  switch (a.level) {
    case 'exact':
      return (
        b.level === 'exact' &&
        a.schema === b.schema &&
        a.majors.min <= b.majors.max &&
        b.majors.min <= a.majors.max
      )
    case 'family': {
      if (b.level !== 'family' || a.family !== b.family) return false
      if (a.kind === undefined || b.kind === undefined) return true
      const aWildcard = a.kind.endsWith('*')
      const bWildcard = b.kind.endsWith('*')
      const aPrefix = aWildcard ? a.kind.slice(0, -1) : a.kind
      const bPrefix = bWildcard ? b.kind.slice(0, -1) : b.kind
      return aWildcard && bWildcard
        ? aPrefix.startsWith(bPrefix) || bPrefix.startsWith(aPrefix)
        : aWildcard
          ? b.kind.startsWith(aPrefix)
          : bWildcard
            ? a.kind.startsWith(bPrefix)
            : a.kind === b.kind
    }
    case 'generic':
      return true
  }
}

/**
 * The registration conflict rule (CAG.CLI.WEB.RES-R04): two manifests that claim an overlapping key and
 * share a representation are a startup error, never a silent override. Returns the conflicting
 * representations.
 */
export const conflicts = ({
  a,
  b,
}: {
  readonly a: RendererManifest
  readonly b: RendererManifest
}): readonly Representation[] =>
  overlaps({ a: a.match, b: b.match })
    ? a.representations.filter((rep) => b.representations.includes(rep))
    : []

interface ManifestHeader {
  readonly manifest: RendererManifest
  readonly header: RoutingHeader
}
