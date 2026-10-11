// Mirrors tmp-preview/fractal-ui/src/sidebar/folder-contract.ts (kit-owned).
// TODO(kit): replace this structural copy with the kit import when AgentFolders lands.
import type * as React from 'react'
import type { StyleXStyles } from '@stylexjs/stylex'

export type SidebarAgent = { readonly _tag: 'Agent'; readonly id: string; readonly subject: string; readonly label: string }
export type SidebarFolder = { readonly _tag: 'Folder'; readonly id: string; readonly label: string; readonly collapsed: boolean; readonly children: readonly SidebarNode[] }
export type SidebarGroup = { readonly _tag: 'Group'; readonly id: string; readonly label: string; readonly collapsed: boolean; readonly children: readonly SidebarNode[] }
export type SidebarNode = SidebarFolder | SidebarGroup | SidebarAgent
export type SidebarMoveTarget =
  /** Append to the mover's partition: subfolders or members. */
  | { readonly _tag: 'Into'; readonly folder: string }
  | { readonly _tag: 'Before' | 'After'; readonly sibling: string }
  | { readonly _tag: 'Unfiled' }
  /** Gap among current root folders, before removing the mover. Folders only. */
  | { readonly _tag: 'Root'; readonly index: number }
/** A single seat row id; the host derives shared membership from the row's subject. */
export type SidebarMove = { readonly items: readonly [string]; readonly target: SidebarMoveTarget }
export type SidebarDropVerdict = { readonly ok: true } | { readonly refused: string }
export type SidebarRowStatus = { readonly _tag: 'Pending' } | { readonly _tag: 'Refused'; readonly reason: string; readonly onRetry?: () => void }
export type SidebarCreateFolder = { readonly parent: string | null; readonly target?: SidebarMoveTarget; readonly name: string; readonly withAgent?: string }
export type AgentFoldersProps = {
  readonly tree: readonly SidebarNode[]
  /** Canonical, unfiltered projection for ordering; the kit defaults to tree when omitted. */
  readonly orderingTree?: readonly SidebarNode[]
  readonly canDrop: (items: readonly [string], target: SidebarMoveTarget) => SidebarDropVerdict
  readonly onMove: (intent: SidebarMove) => void
  readonly onCreateFolder: (intent: SidebarCreateFolder) => void
  readonly onRenameFolder: (intent: { readonly id: string; readonly name: string }) => void
  readonly onDeleteFolder: (intent: { readonly id: string }) => void
  readonly onToggleCollapsed: (intent: { readonly id: string; readonly collapsed: boolean }) => void
  readonly rowStatus?: ReadonlyMap<string, SidebarRowStatus>
  readonly treeStatus?: React.ReactNode
  readonly unavailable?: React.ReactNode
  readonly selectedId?: string
  readonly onSelect?: (id: string) => void
  readonly renderAgent?: (agent: SidebarAgent) => React.ReactNode
  readonly ariaLabel?: string
  readonly style?: StyleXStyles
}
