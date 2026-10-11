import * as React from 'react'
import type { AgentPlacement } from './agent-drag'
import type { WorkbenchPane } from './workbench-model'

export interface WorkbenchAppearance {
  readonly width: 'W1' | 'W2' | 'W3'
  readonly header: 'H1' | 'H2' | 'H3'
  readonly chrome: 'P1' | 'P2' | 'P3'
  readonly dropZones: 'D1' | 'D2' | 'D3'
}
export interface WorkbenchPaneDetails {
  readonly title: string
  readonly breadcrumb?: string
  readonly status?: string
  readonly statusTone?: 'running' | 'done' | 'attention' | 'danger' | 'neutral'
  readonly pullRequestUrl?: string
  readonly onStop?: () => void
}
export interface WorkbenchPresentation {
  readonly appearance?: WorkbenchAppearance
  readonly landmarkContext?: string
  readonly isSplit: boolean
  /** Story state: the same overlay used by an actual drag, without starting a native gesture. */
  readonly previewPlacement?: AgentPlacement
  readonly previewPath?: string
  readonly describePane?: (pane: WorkbenchPane) => WorkbenchPaneDetails
  readonly onOpenTerminal?: (ref?: string) => void
  readonly onOpenDiff?: (path?: string) => void
  readonly diffReveal?: { readonly path: string; readonly sequence: number }
}
export const WorkbenchPresentationContext = React.createContext<WorkbenchPresentation>({ isSplit: false })
