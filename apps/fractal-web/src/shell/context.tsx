// Hooks contributed views and native subjects use instead of importing the shell.

import * as React from 'react'

import type { PresentationId } from '../resources/contract.ts'
import type { Platform } from '../workbench-kit/keys.tsx'
import type { ResourcePanelState } from './state.ts'

/** What tabs, quick-open and the workspace sidebar know about one subject before its editor loads. */
export interface SubjectSummary {
  readonly ref: string
  readonly title: string
  /** Host or path: disambiguates equal titles in tabs and quick-open. */
  readonly detail?: string
  /** Gateway-projected host; refs identify subjects, not their execution host. */
  readonly host?: string
  /** Structured harness/provider id; rendered as its mark, never parsed out of `detail`. */
  readonly harness?: string
  /** Icon name (see `ContainerIcon`). */
  readonly icon: string
  readonly status?: 'live' | 'unavailable'
  /** The subject needs the user (attention open, agent waiting or faulted): the notification ring. */
  readonly attention?: boolean
}

/** Opens a subject's detail presentation; explicit `side` chooses tab (false) or side (true). */
export interface OpenRequest {
  readonly ref: string
  readonly presentation?: PresentationId
  readonly side?: boolean
}

/** What the shell provides to contributed views and editors. */
export interface WorkbenchContextValue {
  readonly open: (request: OpenRequest) => void
  readonly focusedRef: string | null
  readonly subjects: ReadonlyMap<string, SubjectSummary>
  readonly platform: Platform
}

const WorkbenchActionsContext = React.createContext<Pick<
  WorkbenchContextValue,
  'open' | 'platform'
> | null>(null)
const SubjectsContext = React.createContext<WorkbenchContextValue['subjects'] | null>(null)
const FocusedSubjectContext = React.createContext<string | null | undefined>(undefined)

/** Focus changes notify focus consumers, not every open/subject lookup in the sidebar. */
export const WorkbenchContextProvider = ({
  value,
  children,
}: {
  readonly value: WorkbenchContextValue
  readonly children: React.ReactNode
}) => {
  const actions = React.useMemo(
    () => ({
      open: value.open,
      platform: value.platform,
    }),
    [value.open, value.platform],
  )
  return (
    <WorkbenchActionsContext value={actions}>
      <SubjectsContext value={value.subjects}>
        <FocusedSubjectContext value={value.focusedRef}>{children}</FocusedSubjectContext>
      </SubjectsContext>
    </WorkbenchActionsContext>
  )
}

/** Opens a subject in the workbench. */
export const useOpen = () => useWorkbenchActions().open
/** Ref of the subject in the focused editor, or null. */
export const useFocusedSubject = () => {
  const value = React.useContext(FocusedSubjectContext)
  if (value === undefined) throw new Error('Workbench hooks are only available inside <Workbench>')
  return value
}
/** Every subject the workbench knows, by ref. */
export const useSubjects = () => {
  const value = React.useContext(SubjectsContext)
  if (value === null) throw new Error('Workbench hooks are only available inside <Workbench>')
  return value
}

const useWorkbenchActions = (): Pick<WorkbenchContextValue, 'open' | 'platform'> => {
  const value = React.useContext(WorkbenchActionsContext)
  if (value === null) throw new Error('Workbench hooks are only available inside <Workbench>')
  return value
}

/** Resource inspector layout is supplied by the window owner, not by an agent editor. */
const ResourcePanelContext = React.createContext<{
  readonly state: ResourcePanelState
  readonly onChange: (change: Partial<ResourcePanelState>) => void
} | null>(null)
/** Supplies window-owned resource inspector layout. */
export const ResourcePanelProvider = ResourcePanelContext.Provider
/** Reads and updates the resource inspector layout in the owning workbench. */
export const useResourcePanel = () => {
  const value = React.useContext(ResourcePanelContext)
  if (value === null) throw new Error('Resource panel layout requires <Workbench>')
  return value
}

/** Account detail width belongs to the window, independent of the selected account. */
const MonitorDetailContext = React.createContext<{
  readonly size: number
  readonly onSizeChange: (size: number) => void
} | null>(null)
/** Supplies the window-owned account detail width. */
export const MonitorDetailProvider = MonitorDetailContext.Provider
/** Reads and updates account detail width in the owning workbench. */
export const useMonitorDetailLayout = () => {
  const value = React.useContext(MonitorDetailContext)
  if (value === null) throw new Error('Monitor detail layout requires <Workbench>')
  return value
}
