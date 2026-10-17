import type { SidebarAgentRowData } from '@smalltalk/fractal-ui/assistant-ui'
import { sidebarRowAccessibleName, sidebarRowDescription } from '@smalltalk/fractal-ui/assistant-ui/shell'

/** RAC TreeItem owns the focusable row and drops aria-describedby, so its name carries the kit's reported facts. */
export const sidebarTreeRowName = (row: SidebarAgentRowData): string => `${sidebarRowAccessibleName(row)}. ${sidebarRowDescription(row)}`
