import type { Schema } from 'effect'
import type { ComponentType, ReactNode } from 'react'

import type { Density, Representation } from './contract.ts'
import { decodeWith } from './envelope.ts'
import type { RegisteredObservationRenderer } from './registry.ts'

/** A host passes authoritative observations unchanged; the selected callback owns decoding. */
export interface ObservationInput {
  readonly value: unknown
  readonly representation: Representation
  readonly density: Density
  readonly open?: ((ref: string) => void) | undefined
}
/** Decoded observation data and the embedding requested by its host. */
export interface ObservationProps<A> {
  readonly data: A
  readonly representation: Representation
  readonly density: Density
  readonly open?: ((ref: string) => void) | undefined
}
/** Rejected observation input and decoder context for the registered fallback component. */
export interface InvalidObservationProps extends ObservationInput {
  readonly issue: { readonly rejectedBy: string; readonly message: string }
}
/** Closed decoder/view callback selected by the observation registry. */
export type ObservationReactView = (props: ObservationInput) => ReactNode

/** Decoder and typed view remain in one closure; no erased data or synthetic envelope crosses it. */
export const defineObservationRenderer = <A,>(spec: {
  readonly id: string
  readonly kind: string
  readonly reason: string
  readonly schema: Schema.Decoder<A>
  readonly view: ComponentType<ObservationProps<A>>
  readonly fallback: ComponentType<InvalidObservationProps>
  readonly representations?: readonly Representation[]
}): RegisteredObservationRenderer<ObservationReactView> => {
  const View = spec.view
  const Fallback = spec.fallback
  return {
    manifest: {
      id: spec.id,
      kind: spec.kind,
      reason: spec.reason,
      representations: spec.representations ?? ['inline', 'row', 'card', 'detail'],
    },
    view: ({ value, representation, density, open }) => {
      const decoded = decodeWith({ schema: spec.schema, input: value })
      return decoded.ok ? (
        <View data={decoded.value} representation={representation} density={density} open={open} />
      ) : (
        <Fallback
          value={value}
          representation={representation}
          density={density}
          open={open}
          issue={{ rejectedBy: spec.id, message: decoded.issue }}
        />
      )
    },
  }
}
