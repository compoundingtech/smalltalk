import { RegistryContext, useAtom, useAtomValue } from '@effect/atom-react'
import * as Atom from 'effect/reactivity/Atom'
import * as React from 'react'
import { arrangementSidebarDoc, folders, type FolderState } from './client.ts'
import { emptyDoc, type FolderDoc } from './core.mts'
import { arrangementUuid, refusalText, type RestageVerdict } from './edit.ts'
import { boundRows, canDrop, filterSidebarTree, restageBoundMove, restageCreate, restageDelete, restageRename, rowStatus, sidebarProjection, sidebarTree, type SidebarDocument, type SidebarSeat } from './sidebarAdapter.ts'
import type { AgentFoldersProps } from './sidebarContract.ts'

export interface SidebarFoldersOptions {
  /** The host supplies actual seat row keys and their shared subject; never infer a subject from a label. */
  readonly roster: readonly SidebarSeat[]
  readonly source?: Atom.Atom<FolderState>
  readonly query?: string
}
export type SidebarFoldersBinding = Pick<AgentFoldersProps, 'tree' | 'canDrop' | 'onMove' | 'onCreateFolder' | 'onRenameFolder' | 'onDeleteFolder' | 'onToggleCollapsed' | 'unavailable' | 'treeStatus'> & {
  readonly orderingTree: AgentFoldersProps['tree']
  readonly status: NonNullable<AgentFoldersProps['rowStatus']>
  readonly rowStatus: NonNullable<AgentFoldersProps['rowStatus']>
}

/**
 * Subscription owns the existing sidebarFolders atom's editor, inventory follow and abort finalizer.
 * No second reader/editor or persistence of st domain data. Source injection exercises this without the kit.
 */
export const useSidebarFolders = ({ roster, source = folders, query = '' }: SidebarFoldersOptions): SidebarFoldersBinding => {
  const registry = React.useContext(RegistryContext)
  const snapshot = useAtomValue(source)
  const local = React.useMemo(() => ({
    collapse: Atom.make<ReadonlyMap<string, boolean>>(new Map()),
    /** Fixed-copy refusal of a write planned without a request, such as the filing stage of create-with-agent. */
    refused: Atom.make<string | undefined>(undefined),
  }), [source])
  const [collapse, setCollapse] = useAtom(local.collapse)
  const refused = useAtomValue(local.refused)
  const currentRoster = React.useRef(roster)
  currentRoster.current = roster
  const doc = { ...snapshot.doc, roster }
  // Filtering belongs to this hook; kit ordering always uses the complete canonical projection.
  const orderingTree = sidebarTree(sidebarProjection(doc), roster, collapse)
  const currentDoc = (): FolderDoc => registry.get(source).doc
  const ready = (): boolean => {
    const state = registry.get(source)
    return state.readOnly !== true && state.restoreUnavailable !== true && state.phase !== 'connecting'
  }
  /** Plans against fresh state at gesture time and again on every restage; the old fence is never resent. */
  const submit = (plan: (fresh: SidebarDocument) => RestageVerdict, onApplied?: () => void): void => {
    if (!ready()) return
    registry.set(local.refused, undefined)
    const fresh = (doc: FolderDoc): SidebarDocument => ({ ...doc, roster: currentRoster.current })
    const verdict = plan(fresh(currentDoc()))
    if (verdict._tag === 'Refused') { registry.set(local.refused, verdict.sentence); return }
    if (verdict._tag === 'Satisfied') { onApplied?.(); return }
    void registry.get(source).edit(verdict.operations, {
      restage: (arrangement) => plan(fresh(arrangement === undefined ? emptyDoc() : arrangementSidebarDoc(arrangement))),
      ...(onApplied === undefined ? {} : { onApplied }),
    })
  }
  // Only a definitive typed refusal can be restaged. Unknown delivery is never a fresh user edit.
  // edit.ts prepareRetry drops the old request and drain recomputes intent after refresh.
  const retry = snapshot.refusal?.reason._tag === 'Known' && snapshot.retryEdit !== undefined && snapshot.restoreUnavailable !== true
    ? () => { void snapshot.retryEdit?.() } : undefined
  const targets = new Map<string, string>()
  const attempted = new Set([...(snapshot.pendingTargets ?? []), ...(snapshot.refusal?.targets ?? [])])
  for (const target of attempted) targets.set(target, target)
  for (const seat of roster) if (attempted.has(seat.subject)) targets.set(seat.id, seat.subject)
  const status = rowStatus({
    phase: snapshot.phase === 'pending' ? 'pending' : snapshot.refusal === undefined ? 'synced' : 'refused',
    retryReady: snapshot.retryReady === true,
    ...(snapshot.refusal === undefined ? {} : { refusal: snapshot.refusal }),
    ...(retry === undefined ? {} : { onRetry: retry }),
  }, targets)
  const treeStatus = snapshot.refusal !== undefined
    ? React.createElement('div', { role: 'status' }, refusalText(snapshot.refusal), retry === undefined ? null : React.createElement('button', { type: 'button', onClick: retry }, snapshot.retryReady ? 'Try again' : 'Refresh for retry'))
    : refused !== undefined ? React.createElement('div', { role: 'status' }, refused)
    : snapshot.phase === 'pending' ? 'Saving folder layout…' : undefined
  const unavailable = snapshot.restoreUnavailable ? 'Restore unavailable.'
    : snapshot.phase === 'connecting' ? 'Loading folder layout…'
    : snapshot.phase === 'unavailable' && snapshot.refusal === undefined ? 'Folder layout is unavailable. Reconnect to try again.'
    : snapshot.readOnly ? 'Folder editing is unavailable for this connection.'
    : (snapshot.sidebarCandidates?.length ?? 0) > 1 ? 'Several sidebar layouts are available. Using the earliest layout; the others are unchanged.' : undefined
  return {
    tree: filterSidebarTree(orderingTree, query), orderingTree, status, rowStatus: status, treeStatus, unavailable,
    canDrop: (items, target) => ready() ? canDrop({ ...currentDoc(), roster: currentRoster.current }, { items, target }) : { refused: 'Folder editing is unavailable for this connection.' },
    onMove: (intent) => {
      // Agent rows are bound to the subject they showed now; a rebound row key refuses instead of moving another agent.
      const bound = boundRows(currentRoster.current, intent)
      submit((fresh) => restageBoundMove(fresh, intent, bound))
    },
    onCreateFolder: (intent) => {
      const id = arrangementUuid()
      const { withAgent, ...named } = intent
      // Create-with-agent always lands at root, as in the TUI; the filing is a separate stage.
      const folder = withAgent === undefined ? named : { ...named, parent: null }
      const seat = withAgent === undefined ? undefined
        : currentRoster.current.find((row) => row.id === withAgent) ?? currentRoster.current.find((row) => row.subject === withAgent)
      // TUI parity: the folder survives a refused filing; filing runs only after creation lands, including via Try again.
      submit((fresh) => restageCreate(fresh, folder, id), seat === undefined ? undefined : () => {
        const move = { items: [seat.id], target: { _tag: 'Into', folder: id } } as const
        const bound = new Map([[seat.id, seat.subject]])
        submit((fresh) => restageBoundMove(fresh, move, bound))
      })
    },
    onRenameFolder: (intent) => { submit((fresh) => restageRename(fresh, intent)) },
    onDeleteFolder: (intent) => { submit((fresh) => restageDelete(fresh, intent)) },
    onToggleCollapsed: ({ id, collapsed }) => {
      const folds = new Map(registry.get(local.collapse))
      folds.set(id, collapsed)
      setCollapse(folds)
    },
  }
}
