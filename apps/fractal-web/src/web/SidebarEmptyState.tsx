import * as Aria from 'react-aria-components'
import * as stylex from '@stylexjs/stylex'
import { colorVars as c, spaceVars as s, typeVars as t } from '../../../../packages/fractal-ui/src/assistant-ui/composition-tokens.stylex.ts'

export const SidebarEmptyState = ({ query, needsMe, onClear }: { readonly query: string; readonly needsMe: boolean; readonly onClear: () => void }) => <div {...stylex.props(styles.empty)}>
  <p role="status">{query.trim() !== '' ? `No agents match “${query}”.` : needsMe ? 'No agents need your attention with these filters.' : 'No agents match these filters.'}</p>
  <Aria.Button onPress={onClear} {...stylex.props(styles.button)}>Clear filters</Aria.Button>
</div>
const styles = stylex.create({
  empty: { padding: s.md, color: c.fgMuted, fontSize: t.metaSize },
  button: { border: `1px solid ${c.border}`, borderRadius: 6, backgroundColor: 'transparent', color: c.fg, padding: s.sm, cursor: 'pointer', ':focus-visible': { outline: `2px solid ${c.primary}` } },
})
