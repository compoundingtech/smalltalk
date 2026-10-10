// moves to smalltalk/clients/typescript
/**
 * Renderer registry, generic over the platform's view type (`View` = a React component in wf, a
 * ratatui widget fn in stui, a SwiftUI view builder on iOS). Construction validates once at
 * module load; `plan` turns an envelope into exactly one thing to draw.
 */
import {
  conflicts,
  type Density,
  type HostCapabilities,
  ObservationAddress,
  ObservationRendererManifest,
  type RendererManifest,
  type Representation,
  representations,
  resolveManifest,
  type Resolution,
  SubjectAddress,
  SubjectRendererManifest,
} from './contract.ts'
import { decodeWith, type SubjectEnvelope } from './envelope.ts'

/** A closed platform binding decodes its input before constructing its output. */
export type RendererBinding<View> = (
  request: RenderRequest,
) => { readonly ok: true; readonly view: View } | { readonly ok: false; readonly issue: string }

/** A manifest with schema-coupled bindings, never independent decoders and views. */
export interface RegisteredRenderer<View> {
  readonly manifest: RendererManifest
  readonly bindings: { readonly [R in Representation]?: RendererBinding<View> }
}

/** What to draw for one envelope at one representation. */
export type RenderPlan<View> =
  | {
      readonly _tag: 'Render'
      readonly renderer: RegisteredRenderer<View>
      readonly view: View
      readonly resolution: Resolution
    }
  | {
      /** The chosen renderer's schema rejected `data` (CAG.CLI.WEB.RES-R01): the generic view draws the bag plus the issue. */
      readonly _tag: 'Invalid'
      readonly renderer: RegisteredRenderer<View>
      readonly view: View
      readonly rejectedBy: string
      readonly issue: string
      readonly resolution: Resolution
    }

/** One envelope to draw, at one representation and density, on one host. */
export interface RenderRequest {
  readonly envelope: SubjectEnvelope
  readonly representation: Representation
  readonly density: Density
  readonly host: HostCapabilities
  readonly issue?: { readonly rejectedBy: string; readonly message: string } | undefined
}

/** Native views own authoritative observations; this route invents no observed envelope. */
export interface RegisteredSubjectRenderer<View> {
  readonly manifest: SubjectRendererManifest
  readonly view: View
}

/** Native routing yields a validated view, malformed input, or explicit unavailable state. */
export type SubjectPlan<View> =
  | {
      readonly _tag: 'Subject'
      readonly address: SubjectAddress
      readonly rendererId: string
      readonly view: View
    }
  | { readonly _tag: 'MalformedSubject'; readonly input: unknown; readonly issue: string }
  | {
      readonly _tag: 'UnavailableSubjectRenderer'
      readonly address: SubjectAddress
      readonly reason: string
    }

/** Schema-coupled platform callbacks retain their authoritative graph observation input. */
export interface RegisteredObservationRenderer<View> {
  readonly manifest: ObservationRendererManifest
  readonly view: View
}
/** Observation routing preserves kind and representation without inventing envelope metadata. */
export type ObservationPlan<View> =
  | {
      readonly _tag: 'Observation'
      readonly kind: string
      readonly representation: Representation
      readonly rendererId: string
      readonly view: View
    }
  | { readonly _tag: 'MalformedObservation'; readonly input: unknown; readonly issue: string }
  | {
      readonly _tag: 'UnavailableObservationRenderer'
      readonly kind: string
      readonly representation: Representation
      readonly reason: string
    }

/** A validated set of renderers and the plan function that picks one per request. */
export interface Registry<View, NativeView = unknown, ObservationView = unknown> {
  readonly manifests: readonly RendererManifest[]
  readonly plan: (request: RenderRequest) => RenderPlan<View>
  readonly subjectManifests: readonly SubjectRendererManifest[]
  readonly subjectPlan: (input: unknown) => SubjectPlan<NativeView>
  readonly observationManifests: readonly ObservationRendererManifest[]
  readonly observationPlan: (input: unknown) => ObservationPlan<ObservationView>
}

/** Registration error: the registry refuses to start rather than silently override (CAG.CLI.WEB.RES-R04). */
export class RegistryConflict extends Error {
  override readonly name = 'RegistryConflict'
}

/**
 * Validates and freezes a registry: `generic` must draw every representation without live needs;
 * `renderers` must not claim overlapping keys for a shared representation (CAG.CLI.WEB.RES-R04).
 */
export const makeRegistry = <View, NativeView = unknown, ObservationView = unknown>({
  generic,
  renderers,
  genericNative,
  nativeRenderers = [],
  genericObservation,
  observationRenderers = [],
}: {
  readonly generic: RegisteredRenderer<View>
  readonly renderers: readonly RegisteredRenderer<View>[]
  readonly genericNative?: RegisteredSubjectRenderer<NativeView>
  readonly nativeRenderers?: readonly RegisteredSubjectRenderer<NativeView>[]
  readonly genericObservation?: RegisteredObservationRenderer<ObservationView>
  readonly observationRenderers?: readonly RegisteredObservationRenderer<ObservationView>[]
}): Registry<View, NativeView, ObservationView> => {
  if (generic.manifest.match.level !== 'generic' || generic.manifest.needs.live === 'required') {
    throw new RegistryConflict(
      `${generic.manifest.id}: the fallback must be a generic manifest that needs no live channel`,
    )
  }
  const missing = representations.filter((rep) => generic.bindings[rep] === undefined)
  if (missing.length > 0) {
    throw new RegistryConflict(
      `${generic.manifest.id}: the generic renderer must draw every representation; missing ${missing.join(', ')}`,
    )
  }
  const unclaimed = representations.filter((rep) => !generic.manifest.representations.includes(rep))
  if (unclaimed.length > 0) {
    throw new RegistryConflict(
      `${generic.manifest.id}: the generic manifest must claim every representation; missing ${unclaimed.join(', ')}`,
    )
  }
  const all = [...renderers, generic]
  const byId = new Map<string, RegisteredRenderer<View>>()
  for (const renderer of all) {
    const { id } = renderer.manifest
    if (byId.has(id)) throw new RegistryConflict(`duplicate renderer id ${id}`)
    const undrawn = renderer.manifest.representations.filter(
      (rep) => renderer.bindings[rep] === undefined,
    )
    if (undrawn.length > 0)
      throw new RegistryConflict(`${id}: manifest lists ${undrawn.join(', ')} without a binding`)
    for (const other of byId.values()) {
      const shared = conflicts({ a: renderer.manifest, b: other.manifest })
      if (shared.length > 0) {
        throw new RegistryConflict(
          `${id} and ${other.manifest.id} both claim ${shared.join(', ')} for the same key`,
        )
      }
    }
    byId.set(id, renderer)
  }
  const manifests = all.map((renderer) => renderer.manifest)

  const plan = (request: RenderRequest): RenderPlan<View> => {
    const { envelope, representation, host } = request
    const resolution = resolveManifest({ manifests, header: envelope, representation, host })
    const renderer = byId.get(resolution.manifest.id) ?? generic
    const binding = renderer.bindings[representation] ?? generic.bindings[representation]
    const genericBinding = generic.bindings[representation]
    if (binding === undefined || genericBinding === undefined)
      throw new RegistryConflict('unreachable: validated above')
    const rendered = binding(request)
    if (rendered.ok) return { _tag: 'Render', renderer, view: rendered.view, resolution }
    const fallback =
      renderer === generic
        ? rendered
        : genericBinding({
            ...request,
            issue: { rejectedBy: renderer.manifest.id, message: rendered.issue },
          })
    if (!fallback.ok)
      throw new RegistryConflict(
        `${generic.manifest.id}: generic binding rejected the envelope: ${fallback.issue}`,
      )
    return {
      _tag: 'Invalid',
      renderer: generic,
      view: fallback.view,
      rejectedBy: renderer.manifest.id,
      issue: rendered.issue,
      resolution,
    }
  }

  const subjectRenderers =
    genericNative === undefined ? [...nativeRenderers] : [...nativeRenderers, genericNative]
  if (nativeRenderers.length > 0 && genericNative === undefined) {
    throw new RegistryConflict('native subject registrations require a generic native renderer')
  }
  if (nativeRenderers.some((renderer) => renderer.manifest.family === '*')) {
    throw new RegistryConflict('wildcard native subjects must use the generic native registration')
  }
  const subjectManifests: SubjectRendererManifest[] = []
  for (const renderer of subjectRenderers) {
    const decoded = decodeWith({ schema: SubjectRendererManifest, input: renderer.manifest })
    if (!decoded.ok) throw new RegistryConflict(decoded.issue)
    const manifest = decoded.value
    if (renderer.view === undefined)
      throw new RegistryConflict(`${manifest.id}: native subject renderer has no view`)
    if (byId.has(manifest.id) || subjectManifests.some((other) => other.id === manifest.id)) {
      throw new RegistryConflict(`duplicate renderer id ${manifest.id}`)
    }
    if (new Set(manifest.presentations).size !== manifest.presentations.length) {
      throw new RegistryConflict(`${manifest.id}: duplicate subject presentations`)
    }
    for (const other of subjectManifests) {
      if (
        manifest.family === other.family &&
        manifest.presentations.some((presentation) => other.presentations.includes(presentation))
      ) {
        throw new RegistryConflict(
          `${manifest.id} and ${other.id} claim the same native subject presentation`,
        )
      }
    }
    subjectManifests.push(manifest)
  }
  if (
    genericNative !== undefined &&
    (genericNative.manifest.family !== '*' ||
      !(['detail', 'overview', 'resources'] as const).every((presentation) =>
        genericNative.manifest.presentations.includes(presentation),
      ))
  ) {
    throw new RegistryConflict(
      `${genericNative.manifest.id}: generic native renderer must cover every subject presentation`,
    )
  }
  const subjectPlan = (input: unknown): SubjectPlan<NativeView> => {
    const decoded = decodeWith({ schema: SubjectAddress, input })
    if (!decoded.ok) return { _tag: 'MalformedSubject', input, issue: decoded.issue }
    const address = decoded.value
    const family = address.ref.slice(0, address.ref.indexOf('/'))
    const renderer =
      nativeRenderers.find(
        (candidate) =>
          candidate.manifest.family === family &&
          candidate.manifest.presentations.includes(address.presentation),
      ) ?? genericNative
    return renderer === undefined
      ? {
          _tag: 'UnavailableSubjectRenderer',
          address,
          reason: 'This registry has no native subject renderer.',
        }
      : { _tag: 'Subject', address, rendererId: renderer.manifest.id, view: renderer.view }
  }
  const observationBindings =
    genericObservation === undefined
      ? [...observationRenderers]
      : [...observationRenderers, genericObservation]
  if (observationRenderers.length > 0 && genericObservation === undefined) {
    throw new RegistryConflict(
      'graph observation registrations require a generic observation renderer',
    )
  }
  if (observationRenderers.some((renderer) => renderer.manifest.kind === '*')) {
    throw new RegistryConflict(
      'wildcard observations must use the generic observation registration',
    )
  }
  const observationManifests: ObservationRendererManifest[] = []
  for (const renderer of observationBindings) {
    const decoded = decodeWith({ schema: ObservationRendererManifest, input: renderer.manifest })
    if (!decoded.ok) throw new RegistryConflict(decoded.issue)
    const manifest = decoded.value
    if (renderer.view === undefined)
      throw new RegistryConflict(`${manifest.id}: observation renderer has no view`)
    if (
      byId.has(manifest.id) ||
      subjectManifests.some((other) => other.id === manifest.id) ||
      observationManifests.some((other) => other.id === manifest.id)
    ) {
      throw new RegistryConflict(`duplicate renderer id ${manifest.id}`)
    }
    if (new Set(manifest.representations).size !== manifest.representations.length) {
      throw new RegistryConflict(`${manifest.id}: duplicate observation representations`)
    }
    for (const other of observationManifests) {
      if (
        manifest.kind === other.kind &&
        manifest.representations.some((representation) =>
          other.representations.includes(representation),
        )
      ) {
        throw new RegistryConflict(
          `${manifest.id} and ${other.id} claim the same observation representation`,
        )
      }
    }
    observationManifests.push(manifest)
  }
  if (
    genericObservation !== undefined &&
    (genericObservation.manifest.kind !== '*' ||
      !representations.every((representation) =>
        genericObservation.manifest.representations.includes(representation),
      ))
  ) {
    throw new RegistryConflict(
      `${genericObservation.manifest.id}: generic observation renderer must cover every representation`,
    )
  }
  const observationPlan = (input: unknown): ObservationPlan<ObservationView> => {
    const decoded = decodeWith({ schema: ObservationAddress, input })
    if (!decoded.ok) return { _tag: 'MalformedObservation', input, issue: decoded.issue }
    const { kind, representation } = decoded.value
    const renderer =
      observationRenderers.find(
        (candidate) =>
          candidate.manifest.kind === kind &&
          candidate.manifest.representations.includes(representation),
      ) ?? genericObservation
    return renderer === undefined
      ? {
          _tag: 'UnavailableObservationRenderer',
          kind,
          representation,
          reason: 'This registry has no graph observation renderer.',
        }
      : {
          _tag: 'Observation',
          kind,
          representation,
          rendererId: renderer.manifest.id,
          view: renderer.view,
        }
  }
  return { manifests, plan, subjectManifests, subjectPlan, observationManifests, observationPlan }
}
