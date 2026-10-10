import * as stylex from '@stylexjs/stylex'
import { geometryVars as g } from './composition-tokens.stylex'

/** A shared fluid lane for transcript rows, history boundaries and the live composer. */
export const readingColumnStyles = stylex.create({
  column: { width: `calc(100% - (2 * ${g.gutter}))`, maxWidth: g.lane, minWidth: 0, marginInline: 'auto', boxSizing: 'border-box' },
  composerGutter: { paddingInline: g.gutter },
})
