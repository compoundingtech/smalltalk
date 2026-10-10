import { Schema } from 'effect'
import type { ReactNode } from 'react'
import { Button } from 'react-aria-components'
import { extensionViews } from '../extensions/build.ts'
import type { AgentRowProps } from '../extensions/contract.ts'
import { SubjectAddress } from '../resources/contract.ts'
import { AgentAttentionSignals } from './AgentAttention.tsx'
import type { WindowAction, Workspace } from './workspaces.ts'

/** Public attention survives without private spend attribution. */
export const AgentUsageSignals = extensionViews.AgentSignals ?? AgentAttentionSignals
export const AgentUsageRow = extensionViews.AgentRow ?? (({ onOpen, children }: AgentRowProps): ReactNode =>
  <Button onPress={onOpen} style={{ display: 'flex', width: '100%', textAlign: 'left' }}>{children}</Button>)

/** Uses the window-owned transition, never a second navigation state. */
export const openAgentUsage = ({ workspace, dispatch }: {
  readonly workspace: Pick<Workspace, 'id'>
  readonly dispatch: (action: WindowAction) => void
}): void => dispatch({
  _tag: 'Layout', workspace: workspace.id,
  action: { _tag: 'Open', input: Schema.decodeSync(SubjectAddress)({
    ref: workspace.id, presentation: extensionViews.AgentRow === undefined ? 'detail' : 'overview',
  }) },
})
