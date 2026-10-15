import * as stylex from '@stylexjs/stylex'
import { borderVars as border, radiusVars as radius, spaceVars as space, surfaceVars as surface } from '../composition-tokens.stylex'

/**
 * Loading placeholders mirror the populated section structure exactly — same
 * wrappers, gaps and chart aspect ratios — so data arrival cannot shift the
 * pane. Ink reserves a formatted value's width (`ch`, matching tabular digits)
 * without exposing a value; plots reserve the chart box. Nothing here is
 * announced: the pane's single SyncLine connection status speaks for every
 * section while loading.
 */
export const DetailsInk = ({ ch }: { readonly ch: number }) => <span aria-hidden="true" {...stylex.props(styles.ink(ch))} />

export const DetailsSkeletonPlot = ({ compact = false, spend = false }: { readonly compact?: boolean; readonly spend?: boolean }) => <div aria-hidden="true" {...stylex.props(styles.plot, compact && styles.compact, spend && styles.spend)} />

const styles = stylex.create({
  ink: (ch: number) => ({ display: 'inline-block', width: `${ch}ch`, height: space.md, borderRadius: radius.full, backgroundColor: border.borderStrong }),
  plot: { display: 'block', width: '100%', aspectRatio: '320 / 112', margin: space.zero, backgroundColor: surface.rowActive, borderRadius: radius.sm },
  compact: { aspectRatio: '320 / 64' },
  spend: { aspectRatio: '4 / 1' },
})
