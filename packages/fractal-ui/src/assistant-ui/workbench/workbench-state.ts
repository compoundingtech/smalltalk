import * as Atom from 'effect/reactivity/Atom'
import * as Schema from 'effect/Schema'
import { geometryNumbers as geo } from '../composition-tokens.stylex'
import { walkSplits, type WorkbenchLayout } from './workbench-model'

const PaneSchema = Schema.Struct({ uri: Schema.String, form: Schema.optional(Schema.String), view: Schema.optional(Schema.String) })
const LayoutSchema: Schema.Codec<WorkbenchLayout> = Schema.suspend(() => Schema.Union([
  Schema.Struct({ kind: Schema.Literal('group'), tabs: Schema.Array(PaneSchema) }),
  Schema.Struct({ kind: Schema.Literal('split'), split: Schema.Literals(['right', 'below']), ratio: Schema.optional(Schema.Number), children: Schema.Tuple([LayoutSchema, LayoutSchema]) }),
]))
const StoredLayoutSchema = Schema.fromJsonString(LayoutSchema)

/** Remove pre-drawer terminal panes and collapse splits whose branch becomes empty. */
export function migrateTerminalLayout(layout: WorkbenchLayout, fallback: WorkbenchLayout) {
  const terminalRefs: string[] = []
  const visit = (node: WorkbenchLayout): WorkbenchLayout | undefined => {
    if (node.kind === 'group') {
      const tabs = node.tabs.filter(pane => {
        if (!pane.uri.startsWith('terminal:')) return true
        const id = pane.uri.slice('terminal:'.length)
        const ref = id.startsWith('terminal/') ? id : `terminal/${id}`
        if (!terminalRefs.includes(ref)) terminalRefs.push(ref)
        return false
      })
      return tabs.length === 0 ? undefined : tabs.length === node.tabs.length ? node : { ...node, tabs }
    }
    const left = visit(node.children[0])
    const right = visit(node.children[1])
    return left === undefined ? right : right === undefined ? left
      : left === node.children[0] && right === node.children[1] ? node
      : { ...node, children: [left, right] }
  }
  return { layout: visit(layout) ?? fallback, terminalRefs }
}

export function readStoredTerminalRefs(workspaceId: string): readonly string[] {
  try {
    const raw = window.localStorage.getItem(`workbench.${workspaceId}.terminals`)
    return raw === null ? [] : Schema.decodeUnknownSync(Schema.fromJsonString(Schema.Array(Schema.String)))(raw)
  } catch { return [] }
}

/** Layout and every split's committed ratio are one device-local, atomically written snapshot.
 * Legacy path ratios are meaningful only alongside the legacy layout, never alongside a new fixture.
 */
export function readStoredLayout(workspaceId: string, fallback: WorkbenchLayout): WorkbenchLayout {
  try {
    const snapshot = window.localStorage.getItem(`workbench.${workspaceId}.snapshot`)
    if (snapshot !== null) return Schema.decodeUnknownSync(StoredLayoutSchema)(snapshot)
    const stored = window.localStorage.getItem(`workbench.${workspaceId}.layout`)
    if (stored === null) {
      const published = window.localStorage.getItem(`workbench.${workspaceId}.snapshot`)
      return published === null ? fallback : Schema.decodeUnknownSync(StoredLayoutSchema)(published)
    }
    const restored = Schema.decodeUnknownSync(StoredLayoutSchema)(stored)
    const ratioReads = new Map<string, string | null>()
    const withRatios = (node: WorkbenchLayout, path: string): WorkbenchLayout => {
      if (node.kind === 'group') return node
      const key = `workbench.${workspaceId}.ratio.${path}`
      const raw = window.localStorage.getItem(key)
      ratioReads.set(key, raw)
      const value = raw === null ? NaN : Number(raw)
      return { ...node, ratio: Number.isFinite(value) ? clampRatio(value) : node.ratio ?? defaultRatio,
        children: [withRatios(node.children[0], `${path}.0`), withRatios(node.children[1], `${path}.1`)] }
    }
    // Attach legacy ratios before terminal migration changes the paths.
    const migrated = migrateTerminalLayout(withRatios(restored, '0'), fallback)
    const encoded = Schema.encodeSync(StoredLayoutSchema)(migrated.layout)
    // Another tab may have published while we decoded/read the legacy ratios. Validate every source
    // byte, then check the snapshot again immediately before writing. Never publish default ratios
    // merely because a competing migrator removed the legacy keys.
    const legacyUnchanged = () => {
      if (window.localStorage.getItem(`workbench.${workspaceId}.layout`) !== stored) return false
      for (const [key, value] of ratioReads) if (window.localStorage.getItem(key) !== value) return false
      return true
    }
    const unchanged = legacyUnchanged()
    const published = window.localStorage.getItem(`workbench.${workspaceId}.snapshot`)
    if (published !== null) return Schema.decodeUnknownSync(StoredLayoutSchema)(published)
    if (!unchanged) return fallback
    if (migrated.terminalRefs.length > 0) {
      // Companion data must succeed before publishing a snapshot that no longer contains these panes.
      // A quota failure propagates to the outer catch, leaving all legacy source bytes recoverable.
      window.localStorage.setItem(`workbench.${workspaceId}.terminals`, JSON.stringify(migrated.terminalRefs))
      const unchangedAfterTerminals = legacyUnchanged()
      const publishedAfterTerminals = window.localStorage.getItem(`workbench.${workspaceId}.snapshot`)
      if (publishedAfterTerminals !== null) return Schema.decodeUnknownSync(StoredLayoutSchema)(publishedAfterTerminals)
      if (!unchangedAfterTerminals) return fallback
    }
    window.localStorage.setItem(`workbench.${workspaceId}.snapshot`, encoded)
    removeLegacyLayout(workspaceId)
    return migrated.layout
  } catch { return fallback }
}

function removeLegacyLayout(workspaceId: string) {
  window.localStorage.removeItem(`workbench.${workspaceId}.layout`)
  const prefix = `workbench.${workspaceId}.ratio.`
  for (const key of Object.keys(window.localStorage)) if (key.startsWith(prefix)) window.localStorage.removeItem(key)
}

export function storeLayout(workspaceId: string, layout: WorkbenchLayout) {
  try {
    const legacyLayout = window.localStorage.getItem(`workbench.${workspaceId}.layout`)
    // One write publishes topology and ratios together; no separate ratio keys can diverge.
    window.localStorage.setItem(`workbench.${workspaceId}.snapshot`, Schema.encodeSync(StoredLayoutSchema)(layout))
    if (legacyLayout !== null) removeLegacyLayout(workspaceId)
    // Orphan legacy ratio keys have no known topology. Keep their bytes, but never apply them to a new tree.
  } catch { /* device storage can be unavailable; preserve legacy bytes if the snapshot write fails */ }
}

export const navigationSnapshot = () => typeof window === 'undefined' ? '' : window.location.search
export function subscribeNavigation(listener: () => void) {
  window.addEventListener('popstate', listener)
  return () => window.removeEventListener('popstate', listener)
}
export function navigatePane(workspaceId: string, pane: string) {
  const url = new URL(window.location.href)
  url.searchParams.set('workspace', workspaceId)
  url.searchParams.set('pane', pane)
  if (url.href === window.location.href) return
  window.history.pushState(null, '', url)
  window.dispatchEvent(new PopStateEvent('popstate'))
}

/**
 * Per-split committed ratios live in one Effect atom per split, keyed
 * `<workspaceId>:<indexPath>`. A drag never touches the atom: the preview is
 * a CSS variable written at most once per animation frame; the atom, storage
 * and the layout-change callback are each written exactly once on pointerup.
 */

export const clampRatio = (value: number): number =>
  Math.min(geo.splitMaxRatio, Math.max(geo.splitMinRatio, value))

/** A split no ratio was ever committed for: an even share. */
export const defaultRatio = 0.5

export const splitRatioFamily = Atom.family((key: string) => Atom.make(defaultRatio))

const activeTabKey = (workspaceId: string, path: string) => `workbench.${workspaceId}.tab.${path}`


export const readStoredActiveTab = (workspaceId: string, path: string): string | null => {
  try { return window.localStorage.getItem(activeTabKey(workspaceId, path)) } catch { return null }
}

export const storeActiveTab = (workspaceId: string, path: string, key: string): void => {
  try { window.localStorage.setItem(activeTabKey(workspaceId, path), key) } catch { /* best-effort */ }
}

/** Atoms seed only from the supplied layout: path-keyed legacy storage is consumed at readStoredLayout. */
export const initialRatioValues = (workspaceId: string, layout: WorkbenchLayout): readonly (readonly [Atom.Writable<number, number>, number])[] =>
  walkSplits(layout).map(({ path, node }) => [
    splitRatioFamily(`${workspaceId}:${path}`),
    node.ratio ?? defaultRatio,
  ] as const)
