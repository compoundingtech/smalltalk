import { ContainerIcon } from '../shell/icons.tsx'
import { ChevronDownIcon, ChevronRightIcon } from '../workbench-kit/icons.tsx'

/** Semantic slots consumed by the sidebar; glyph ownership stays with production chrome. */
export type IconName =
  | 'folder'
  | 'folderOpen'
  | 'folderPlus'
  | 'chevronDown'
  | 'chevronRight'
  | 'plus'
  | 'edit'
  | 'close'
  | 'drag'
  | 'working'
  | 'waiting'
  | 'idle'
  | 'unobserved'
  | 'stale'
  | 'ended'
  | 'pending'
  | 'retired'
  | 'offline'
  | 'suspended'
  | 'check'
  | 'attention'
  | 'inbox'

/** Renders a workbench icon at its control or navigation-row size. */
export const WfIcon = ({
  name,
  role = 'control',
}: {
  readonly name: IconName
  readonly role?: 'control' | 'row'
}) =>
  name === 'chevronDown' ? (
    <ChevronDownIcon size={14} />
  ) : name === 'chevronRight' ? (
    <ChevronRightIcon size={14} />
  ) : (
    <ContainerIcon name={name} size={role === 'row' ? 16 : 14} />
  )
