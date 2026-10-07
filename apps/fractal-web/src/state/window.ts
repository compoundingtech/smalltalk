import { Effect, Schema } from 'effect'
import * as Atom from 'effect/reactivity/Atom'

import {
  defaultResourcePanel,
  LayoutJson,
  monitorDetailBounds,
  ResourcePanelState,
} from '../shell/state.ts'
import type { WindowState } from '../shell/workspaces.ts'
import { persistedAtom } from './persistence.ts'
import { migrateWindowState } from './window-migration.ts'

const { docks: _docks, ...layoutFields } = LayoutJson.fields
// Older layouts keep their existing panes and docks while seeding the new inspector.
const resources = ResourcePanelState.pipe(
  Schema.withDecodingDefaultKey(Effect.succeed(defaultResourcePanel)),
)
const monitorDetailSize = Schema.Finite.pipe(
  Schema.withDecodingDefaultKey(Effect.succeed(monitorDetailBounds.default)),
)
const LayoutStateSchema = Schema.toCodecJson(
  Schema.Struct({
    selected: Schema.String,
    layouts: Schema.Record(Schema.String, Schema.Struct(layoutFields)),
    docks: LayoutJson.fields.docks,
    resources,
    monitorDetailSize,
  }),
)
/** Persisted window layout and per-window read markers. */
export const WindowStateSchema = Schema.toCodecJson(
  Schema.Struct({
    selected: Schema.String,
    layouts: Schema.Record(Schema.String, Schema.Struct(layoutFields)),
    docks: LayoutJson.fields.docks,
    resources,
    monitorDetailSize,
    read: Schema.ReadonlySet(Schema.String),
  }),
)
/** sessionStorage survives reload, but does not synchronize navigation between windows. */
export const getWindowId = () => {
  if (typeof window === 'undefined') return 'server'
  const key = 'wf.window-id'
  const existing = window.sessionStorage.getItem(key)
  if (existing !== null) return existing
  const id = crypto.randomUUID()
  window.sessionStorage.setItem(key, id)
  return id
}
/** Shared read markers; window-local navigation seeded from the most recently used layout. */
export const windowStateAtom = ({
  gateway,
  seed,
  windowId = getWindowId(),
  persist = persistedAtom,
}: {
  readonly gateway: string
  readonly seed: WindowState
  readonly windowId?: string
  readonly persist?: typeof persistedAtom
}) => {
  const { read: _read, ...seedLayout } = seed
  const migration = { version: 2, migrate: migrateWindowState } as const
  const legacy = persist({
    key: `${gateway}:window`,
    schema: WindowStateSchema,
    defaultValue: seed,
    ...migration,
  })
  const latest = persist({
    key: `${gateway}:layout:last-used`,
    schema: LayoutStateSchema,
    defaultValue: seedLayout,
    ...migration,
  })
  let initialLayout: Omit<WindowState, 'read'> | undefined
  const local = persist({
    key: `${gateway}:window:${windowId}`,
    schema: Schema.NullOr(LayoutStateSchema),
    defaultValue: null,
    ...migration,
  })
  const reads = persist({
    key: `${gateway}:read`,
    schema: Schema.NullOr(Schema.toCodecJson(Schema.ReadonlySet(Schema.String))),
    defaultValue: null,
  })
  return Atom.writable(
    (get) => {
      if (initialLayout === undefined) {
        const previous = get(legacy)
        const last = get(latest)
        initialLayout =
          last === seedLayout
            ? {
                selected: previous.selected,
                layouts: previous.layouts,
                docks: previous.docks,
                resources: previous.resources,
                monitorDetailSize: previous.monitorDetailSize,
              }
            : last
      }
      return { ...(get(local) ?? initialLayout), read: get(reads) ?? get(legacy).read }
    },
    (ctx, value: WindowState) => {
      const { read, ...layout } = value
      Atom.batch(() => {
        ctx.set(local, layout)
        ctx.set(latest, layout)
        ctx.set(reads, read)
      })
    },
  ).pipe(Atom.keepAlive)
}
