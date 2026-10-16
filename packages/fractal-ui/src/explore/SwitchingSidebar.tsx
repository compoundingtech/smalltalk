import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { Button, Tooltip, TooltipTrigger, VisuallyHidden } from 'react-aria-components'
import { SidebarStatus } from '../assistant-ui/sidebar/SidebarStatus'
import { Icon } from '../assistant-ui/composition/Icons'
import { activeCountName, activeSubagents, failedSubagents, sidebarFixtures, waitingSubagents, type Scenario, type WorkspaceFixture } from './switching-fixtures'
import { surfaceVars as surface, textVars as ink, statusVars as tone, borderVars as border, spaceVars as s, typeVars as t, radiusVars as r, geometryVars as g, accentVars as accent } from '../assistant-ui/composition-tokens.stylex'

/** P1 puts the count right after the name, where row metadata reads; P2 puts it at the trailing edge beside the status disc. */
export type CountPlacement = 'chip' | 'badge'
type Open = (state: Scenario, focusSwitcher: boolean) => void
export function SwitchingSidebar({ placement, selected, context, onOpen }: { placement: CountPlacement; selected: Scenario; context?: string; onOpen: Open }) {
  const samplesId = React.useId()
  const rows = (sample: boolean) => sidebarFixtures.filter(fixture => fixture.sample === sample).map(fixture => <li key={fixture.state}><CountRow fixture={fixture} placement={placement} selected={fixture.state === selected} onOpen={onOpen} /></li>)
  return <nav aria-label={context === undefined ? 'Agents' : `Agents, ${context}`} {...stylex.props(styles.nav)}>
    <ul {...stylex.props(styles.list)}>{rows(false)}</ul>
    <p id={samplesId} data-sample-group {...stylex.props(styles.group)}>Samples · child states need #2288</p>
    <ul aria-labelledby={samplesId} {...stylex.props(styles.list)}>{rows(true)}</ul>
  </nav>
}
function CountRow({ fixture, placement, selected, onOpen }: { fixture: WorkspaceFixture; placement: CountPlacement; selected: boolean; onOpen: Open }) {
  const waiting = waitingSubagents(fixture) > 0
  // The row's empty stretch and the disc extend the name's pointer target; keyboard users reach the same action on the name button. No spacer element, so no extra gap eats the name's width.
  const openRow = () => onOpen(fixture.state, false)
  const count = fixture.reporting === 'unreported' ? <UnknownCount fixture={fixture} placement={placement} selected={selected} onOpen={onOpen} /> : <ActiveCount fixture={fixture} placement={placement} selected={selected} onOpen={onOpen} />
  return <div data-count-row={fixture.state} data-placement={placement} data-sample={fixture.sample || undefined} onClick={event => { if (event.target === event.currentTarget) openRow() }} {...stylex.props(styles.row, selected && styles.selected)}>
    <Button aria-current={selected ? 'page' : undefined} onPress={openRow} {...stylex.props(styles.name)}><span data-row-name {...stylex.props(styles.nameText)}>{fixture.label}</span></Button>
    {placement === 'chip' ? count : null}
    <span onClick={openRow} {...stylex.props(styles.disc)}><SidebarStatus status={waiting ? 'waiting' : 'working'} statusLabel={waiting ? 'Needs you' : 'Running'} freshness="live" variant="SG-1" iconOnly /></span>
    {placement === 'badge' ? count : null}
  </div>
}
/** Zero shows nothing. The visible text is the start of the spoken name ("3 active" + " subagents"), so label-in-name holds without an aria-label override. */
function ActiveCount({ fixture, placement, selected, onOpen }: { fixture: WorkspaceFixture; placement: CountPlacement; selected: boolean; onOpen: Open }) {
  const active = activeSubagents(fixture)
  if (active === 0) return null
  const failed = failedSubagents(fixture) > 0
  const waiting = waitingSubagents(fixture) > 0
  const visible = placement === 'chip' ? `${active} active` : String(active)
  return <Button data-active-count={active} onPress={() => onOpen(fixture.state, true)} {...stylex.props(styles.countHit)}>
    <span {...stylex.props(styles.tag, placement === 'badge' && styles.badge, selected && styles.onSelected, failed ? styles.failed : waiting && styles.waiting)}>
      {failed ? <span aria-hidden="true" {...stylex.props(styles.cue, styles.failedCue)}><Icon name="x" size={12} /></span> : null}
      {waiting ? <span aria-hidden="true" {...stylex.props(styles.cue, styles.waitingCue)}><Icon name="alert" size={12} /></span> : null}
      <span data-count-text>{visible}</span>
    </span>
    <VisuallyHidden elementType="span">{activeCountName(fixture).slice(visible.length)}</VisuallyHidden>
  </Button>
}
/**
 * An unreported harness never shows 0: a neutral outlined "? subagents" (P1) or "?" with the subagent glyph (P2), with the reason spoken and shown on hover or focus.
 * P1 spells "subagents" rather than "sub" so its visible word is part of the spoken sentence (label in name); it is as wide as "14 active".
 */
function UnknownCount({ fixture, placement, selected, onOpen }: { fixture: WorkspaceFixture; placement: CountPlacement; selected: boolean; onOpen: Open }) {
  return <TooltipTrigger delay={300}>
    <Button data-count-unknown aria-label={activeCountName(fixture)} onPress={() => onOpen(fixture.state, true)} {...stylex.props(styles.countHit)}>
      <span {...stylex.props(styles.tag, placement === 'badge' && styles.badge, selected && styles.onSelected)}>{placement === 'badge' ? <Icon name="message" size={12} /> : null}<span data-count-text>{placement === 'chip' ? '? subagents' : '?'}</span></span>
    </Button>
    <Tooltip placement="right" offset={8} {...stylex.props(styles.tooltip)}>Subagents not reported by {fixture.harness}</Tooltip>
  </TooltipTrigger>
}
const styles = stylex.create({
  nav: { display: 'flex', flexDirection: 'column', gap: s.md },
  list: { display: 'flex', flexDirection: 'column', gap: s.md, margin: s.zero, padding: s.zero, listStyle: 'none' },
  group: { margin: s.zero, marginTop: s.md, paddingInline: s.md, color: ink.fgMuted, fontSize: t.denseSize, lineHeight: t.metaLeading },
  row: { display: 'flex', alignItems: 'center', gap: s.md, height: g.controlLg, minWidth: 0, boxSizing: 'border-box', paddingInline: s.md, borderRadius: r.sm, color: ink.fg, cursor: 'pointer', ':hover': { backgroundColor: surface.rowHover } },
  selected: { backgroundColor: surface.rowActive, ':hover': { backgroundColor: surface.rowActive } },
  name: { flex: '0 1 auto', minWidth: 0, display: 'flex', alignItems: 'center', height: g.controlLg, padding: s.zero, borderWidth: 0, backgroundColor: surface.transparent, color: ink.fg, fontFamily: t.fontSans, fontSize: t.uiSize, lineHeight: t.uiLeading, textAlign: 'start', cursor: 'pointer', ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary, outlineOffset: g.focusOffset } },
  nameText: { overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' },
  disc: { width: g.icon, height: g.icon, flexShrink: 0, marginInlineStart: 'auto', display: 'inline-flex', alignItems: 'center', justifyContent: 'center', cursor: 'pointer' },
  // The hit area stays 24px tall; the visible tag inside it is the 11/16 tag: 1px border, r6, no fill, tone on text and border only.
  countHit: { flexShrink: 0, display: 'inline-flex', alignItems: 'center', minHeight: g.controlSm, padding: s.zero, borderWidth: 0, borderRadius: r.sm, backgroundColor: surface.transparent, cursor: 'pointer', ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary, outlineOffset: g.focusOffset } },
  tag: { display: 'inline-flex', alignItems: 'center', gap: s.xs, paddingInline: s.sm, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.sm, color: ink.fgMuted, fontFamily: t.fontSans, fontSize: t.denseSize, lineHeight: t.metaLeading, fontVariantNumeric: 'tabular-nums', whiteSpace: 'nowrap' },
  badge: { justifyContent: 'center', minWidth: g.icon },
  // Muted ink falls below 4.5:1 on the selected row's fill at 11px; the selected row lifts its neutral tag to the soft ink.
  onSelected: { color: ink.fgSoft },
  failed: { color: tone.dangerFg, borderColor: tone.dangerMuted },
  waiting: { color: tone.attention, borderColor: tone.attention },
  cue: { display: 'inline-flex' }, failedCue: { color: tone.dangerFg }, waitingCue: { color: tone.attention },
  tooltip: { maxWidth: g.tooltipMax, paddingBlock: s.xs, paddingInline: s.md, backgroundColor: surface.raised, color: ink.fgSoft, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.sm, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading },
})
