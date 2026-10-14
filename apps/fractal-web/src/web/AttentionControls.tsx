import * as stylex from '@stylexjs/stylex'
import { colorVars as c, typeVars as t } from '../../../../packages/fractal-ui/src/assistant-ui/composition-tokens.stylex.ts'
import type { SidebarFolder, SidebarGroup } from '../folders/sidebarContract.ts'
import { folderAttentionCount } from './sidebarAttention.ts'

export const FolderAttentionSignal = ({ folder, refs }: { readonly folder: SidebarFolder | SidebarGroup; readonly refs: ReadonlySet<string> }) => {
  const count = folderAttentionCount(folder.children, refs)
  return count === 0 ? null : <span data-folder-attention={count} aria-label={`${count} ${count === 1 ? 'agent needs' : 'agents need'} your attention`} {...stylex.props(styles.signal)}>{count} need you</span>
}

const styles = stylex.create({
  signal: { marginInlineStart: 'auto', color: c.attention, fontSize: t.denseSize, whiteSpace: 'nowrap' },
})
