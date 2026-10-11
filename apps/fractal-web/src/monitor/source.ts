/**
 * Where the monitor's bytes come from (CAG.CLI.WEB.MON-R03): a declared source or none.
 *
 * The active monitor reads the authenticated same-origin usage hub v3 relay. Native st
 * usage.period remains available as a separate, explicitly projected source for consumers that
 * need those observations; it is not a substitute for producer quota coverage or ledger identity.
 */
import type { Duration } from 'effect'
import { Effect, Schema } from 'effect'

import type { QuotaFailure } from './model.ts'
import { projectNativeQuota } from './nativeQuota.ts'

/** The usage hub transport: two bounded raw-JSON reads plus an optional cadence override. */
export interface UsageSource {
  /** Shown in the dependency note, e.g. `build-host-a gateway`. */
  readonly label: string
  readonly quota: Effect.Effect<unknown, QuotaFailure>
  /** Set only by the native adapter, never inferred from untrusted response metadata. */
  readonly quotaFormat?: 'native-projection'
  /** `ledgerAccountId` verbatim, as the producer published it (CAG.CLI.TUI.TUI.MON-R12). */
  readonly history: (ledgerAccountId: string) => Effect.Effect<unknown, QuotaFailure>
  /** Refresh cadence; defaults to Fractal's 30 s quota / 5 min history. */
  readonly cadence?: { readonly quota: Duration.Input; readonly history: Duration.Input }
}

/** A declared usage source, or none (the monitor then requests nothing). */
export type MonitorSource =
  | { readonly _tag: 'undeclared' }
  | { readonly _tag: 'declared'; readonly source: UsageSource }

/** No usage hub source declared: the monitor says so and requests nothing (CAG.CLI.WEB.MON-R03). */
export const undeclared: MonitorSource = { _tag: 'undeclared' }

/** The relay prefix the gateway serves; same origin as `/v1/client`, no CORS. */
export const RELAY_PREFIX = '/v1/client/usage'

const MAX_RESPONSE_BYTES = 2 * 1024 * 1024

const getJson = ({
  url,
  what,
}: {
  readonly url: string
  readonly what: 'quota' | 'history'
}): Effect.Effect<unknown, QuotaFailure> =>
  Effect.tryPromise({
    try: (signal) =>
      fetch(url, { headers: { accept: 'application/json' }, credentials: 'same-origin', signal }),
    catch: (error): QuotaFailure => ({
      kind: 'unreachable',
      reason: `${what} request failed: ${String(error)}`,
    }),
  }).pipe(
    Effect.flatMap((response) =>
      response.ok
        ? Effect.tryPromise({
            try: () => response.text(),
            catch: (error): QuotaFailure => ({
              kind: 'unreachable',
              reason: `cannot read ${what}: ${String(error)}`,
            }),
          })
        : Effect.fail<QuotaFailure>({
            kind: what === 'quota' && response.status === 404 ? 'unavailable' : 'http',
            reason: `${what} returned HTTP ${response.status}`,
          }),
    ),
    Effect.flatMap((text) =>
      text.length > MAX_RESPONSE_BYTES
        ? Effect.fail<QuotaFailure>({
            kind: 'incompatible',
            reason: `${what} exceeds ${MAX_RESPONSE_BYTES} bytes`,
          })
        : Schema.decodeEffect(Schema.fromJsonString(Schema.Unknown))(text).pipe(
            Effect.mapError(
              (): QuotaFailure => ({ kind: 'incompatible', reason: `${what} is not JSON` }),
            ),
          ),
    ),
  )

/** Usage relay; quota 404 is unsupported/unconfigured, separate from live-source connectivity. */
export const relayUsageSource = ({
  label,
  prefix = RELAY_PREFIX,
}: {
  readonly label: string
  readonly prefix?: string
}): UsageSource => ({
  label,
  quota: getJson({ url: `${prefix}/quota`, what: 'quota' }),
  history: (ledgerAccountId) =>
    getJson({
      url: `${prefix}/history?${new URLSearchParams({ account: ledgerAccountId }).toString()}`,
      what: 'history',
    }),
})

/** Canonical st usage.period; account limits are observations, not the relay contract. */
export const nativeUsageSource = ({
  label,
  prefix = RELAY_PREFIX,
}: {
  readonly label: string
  readonly prefix?: string
}): UsageSource => ({
  label,
  quotaFormat: 'native-projection',
  quota: getJson({ url: prefix, what: 'quota' }).pipe(
    Effect.flatMap((json) => {
      const projected = projectNativeQuota(json, Date.now())
      return projected._tag === 'ok'
        ? Effect.succeed(projected.value)
        : Effect.fail<QuotaFailure>(projected.failure)
    }),
  ),
  history: () => Effect.fail({
    kind: 'unavailable',
    reason: 'Native st usage does not publish ledger account history.',
  }),
})
