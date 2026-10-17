import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { Button, Disclosure, DisclosurePanel } from 'react-aria-components'
import type { EventItem, HarnessNotice as HarnessNoticeData, HarnessWeight } from '../embrace-data/model.ts'
import { surfaceVars as surface, textVars as ink, borderVars as border, accentVars as accent, statusVars as status, typeVars as t, radiusVars as r, spaceVars as s, geometryVars as g } from '../composition-tokens.stylex'
import { Icon, type IconName } from './Icons'
import { Markdown } from './Markdown'

const presentation: { readonly [Tag in HarnessNoticeData['_tag']]: { readonly icon: IconName; readonly kind: string; readonly noun: string } } = {
  SubagentResult: { icon: 'check', kind: 'Subagent', noun: 'result' },
  Irc: { icon: 'message', kind: 'IRC', noun: 'message' },
  BackgroundJob: { icon: 'clock', kind: 'Background job', noun: 'result' },
  Compaction: { icon: 'panel', kind: 'Summary', noun: 'summary' },
  Interjection: { icon: 'pencil', kind: 'Interjection', noun: 'message' },
  SkillInvocation: { icon: 'gear', kind: 'Skill', noun: 'details' },
  Delivery: { icon: 'send', kind: 'Delivery', noun: 'message' },
}

/**
 * Neutral presentation of typed harness facts; the app alone owns q101 transport recognition.
 * `weight` is the q106 pairing choice: 'row' is a quiet inline row, 'card' a compact bordered card.
 */
export function HarnessNotice({ item, weight = 'card' }: { readonly item: EventItem & { readonly notice: HarnessNoticeData }; readonly weight?: HarnessWeight }) {
  const notice = item.notice
  const compacted = notice._tag === 'Compaction'
  const time = item.at === undefined ? NaN : Date.parse(item.at)
  const [expanded, setExpanded] = React.useState(false)
  const shown = presentation[notice._tag]
  const failed = notice._tag === 'SubagentResult' && notice.status !== 'completed'
  const noun = shown.noun
  const payload = /^[\s]*[\[{]/.test(notice.payload) ? `\`\`\`json\n${notice.payload}\n\`\`\`` : notice.payload
  const row = weight === 'row'
  return <Disclosure isExpanded={expanded} onExpandedChange={setExpanded} data-testid="harness-notice" data-harness-kind={item.kind} data-harness-weight={weight} data-item-id={item.id} {...stylex.props(styles.base, row ? styles.row : styles.card)}>
    <div {...stylex.props(styles.header)}><Icon name={failed ? 'alert' : shown.icon} size={14} /><span {...stylex.props(styles.label)}>{item.title}</span><span data-testid="harness-kind" {...stylex.props(styles.pill, failed && styles.pillFailed)}>{notice._tag === 'SubagentResult' ? notice.status : shown.kind}</span>{Number.isFinite(time) && <time dateTime={item.at} title={new Date(time).toLocaleString()} {...stylex.props(styles.time)}>{new Date(time).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' })}</time>}</div>
    <p data-testid="harness-excerpt" {...stylex.props(styles.excerpt, row && styles.rowIndent)}>{item.text}</p>
    <Button slot="trigger" data-testid="harness-trigger" {...stylex.props(styles.trigger, row && styles.rowIndent)}><Icon name={expanded ? 'chevron-down' : 'chevron-right'} size={14} />{expanded ? `Hide ${noun}` : `Read ${noun}`}</Button>
    {/* The frame lives inside the panel: a collapsed hidden=until-found panel still keeps its own box in Chromium. */}
    <DisclosurePanel data-testid="harness-panel"><div data-testid="harness-panel-frame" {...stylex.props(styles.payload, row ? styles.rowPayload : styles.cardPayload)}>
      <div data-testid={compacted ? 'compaction-summary' : 'harness-payload'}><Markdown text={payload} /></div>
      {notice._tag === 'SubagentResult' && <dl {...stylex.props(styles.facts)}><dt>Agent</dt><dd>{notice.agent}</dd>{notice.lines !== undefined && <><dt>Output</dt><dd>{notice.lines} lines · {notice.size}</dd></>}{notice.fullOutput !== undefined && <><dt>Full output</dt><dd>{notice.fullOutput}</dd></>}{notice.abortReason !== undefined && <><dt>Stop reason</dt><dd>{notice.abortReason}</dd></>}{notice.error !== undefined && <><dt>Error</dt><dd>{notice.error}</dd></>}{notice.mergeSummary !== undefined && <><dt>Merge</dt><dd>{notice.mergeSummary}</dd></>}{notice.followUp !== undefined && <><dt>Follow-up</dt><dd>{notice.followUp}</dd></>}</dl>}
      {notice._tag === 'Irc' && (notice.to !== undefined || notice.replyTo !== undefined) && <dl {...stylex.props(styles.facts)}>{notice.to !== undefined && <><dt>To</dt><dd>{notice.to}</dd></>}{notice.replyTo !== undefined && <><dt>Reply to</dt><dd>{notice.replyTo}</dd></>}</dl>}
      {notice._tag === 'Interjection' && <dl {...stylex.props(styles.facts)}><dt>From</dt><dd>{notice.from}</dd>{notice.to !== undefined && <><dt>To</dt><dd>{notice.to}</dd></>}</dl>}
      {notice._tag === 'SkillInvocation' && <dl {...stylex.props(styles.facts)}><dt>Skill</dt><dd>{notice.skill}</dd><dt>Requested by</dt><dd>{notice.by}</dd></dl>}
      {notice._tag === 'Delivery' && <dl {...stylex.props(styles.facts)}><dt>From</dt><dd>{notice.from}</dd><dt>To</dt><dd>{notice.to}</dd>{notice.subject !== undefined && <><dt>Subject</dt><dd>{notice.subject}</dd></>}</dl>}
    </div></DisclosurePanel>
  </Disclosure>
}

const styles = stylex.create({
  base: { minWidth: 0, color: ink.fgSoft, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading },
  card: { padding: s.md, borderWidth: 1, borderStyle: 'solid', borderColor: border.borderStrong, borderRadius: r.md, backgroundColor: surface.raised },
  row: { paddingBlock: s.xs, paddingInline: s.zero },
  rowIndent: { marginInlineStart: s.xl },
  pill: { flexShrink: 0, paddingInline: s.sm, borderRadius: r.full, borderWidth: 1, borderStyle: 'solid', borderColor: border.borderStrong, color: ink.fgMuted, fontSize: t.denseSize },
  pillFailed: { borderColor: status.dangerMuted, color: status.dangerFg },
  header: { display: 'flex', alignItems: 'center', gap: s.sm },
  label: { minWidth: 0, flex: 1, overflowWrap: 'anywhere', fontWeight: 500 },
  time: { flexShrink: 0, color: ink.fgMuted, fontVariantNumeric: 'tabular-nums' },
  excerpt: { marginBlock: s.xs, color: ink.fgMuted, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' },
  trigger: { display: 'flex', alignItems: 'center', gap: s.xs, padding: 0, minHeight: g.toolRow, borderWidth: 0, backgroundColor: 'transparent', color: ink.fgSoft, fontFamily: t.fontSans, fontSize: t.metaSize, cursor: 'pointer', ':hover': { color: ink.fg }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
  payload: { marginTop: s.sm, paddingBlock: s.sm, paddingInline: s.md, maxHeight: 'min(32rem, 60vh)', overflowY: 'auto', overflowWrap: 'anywhere' },
  cardPayload: { borderRadius: r.sm, backgroundColor: surface.washSubtle, borderWidth: 1, borderStyle: 'solid', borderColor: border.border },
  rowPayload: { marginInlineStart: s.xl, borderInlineStartWidth: 2, borderInlineStartStyle: 'solid', borderInlineStartColor: border.borderStrong },
  facts: { display: 'grid', gridTemplateColumns: 'auto 1fr', gap: s.sm, color: ink.fgMuted, marginBottom: 0, marginInline: 0 },
})
