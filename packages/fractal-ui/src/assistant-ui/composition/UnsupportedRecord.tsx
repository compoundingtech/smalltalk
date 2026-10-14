import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { Button, Disclosure, DisclosurePanel } from 'react-aria-components'
import type { UnsupportedItem } from '../embrace-data/model.ts'
import { surfaceVars as surface, textVars as ink, borderVars as border, accentVars as accent, typeVars as t, radiusVars as r, spaceVars as s, geometryVars as g } from '../composition-tokens.stylex'
import { Icon } from './Icons'

/** F2 marker: diagnostic previews are already redacted and capped by the app, never raw records. */
export function UnsupportedRecord({ item }: { readonly item: UnsupportedItem }) {
  const time = item.at === undefined ? NaN : Date.parse(item.at)
  return <Disclosure data-testid="unsupported-record" data-record-kind={item.recordKind} data-harness={item.harness} data-item-id={item.id} {...stylex.props(styles.base)}>
    <div data-testid="unsupported-record-row" {...stylex.props(styles.row)}>
      <Icon name="dot" size={14} />
      <span {...stylex.props(styles.label)}>Unsupported {item.harness} record · {item.recordKind}</span>
      <span aria-hidden="true">·</span>
      <Button slot="trigger" {...stylex.props(styles.trigger)}>Inspect</Button>
      {Number.isFinite(time) && <time dateTime={item.at} {...stylex.props(styles.time)}>{new Date(time).toLocaleTimeString([], { hour: 'numeric', minute: '2-digit' })}</time>}
    </div>
    {/* Keep the frame inside the panel so collapsed hidden=until-found panels occupy no space. */}
    <DisclosurePanel data-testid="unsupported-record-panel"><div {...stylex.props(styles.panel)}>
      <pre {...stylex.props(styles.preview)}>{item.inspect.text}</pre>
      <p {...stylex.props(styles.note)}>{item.inspect.bytes.toLocaleString('en-US')} bytes{item.inspect.truncated ? ' · truncated' : ''}</p>
    </div></DisclosurePanel>
  </Disclosure>
}

const styles = stylex.create({
  base: { minWidth: 0, color: ink.fgMuted, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading },
  row: { display: 'flex', alignItems: 'center', gap: s.sm, minHeight: t.uiLeading },
  label: { minWidth: 0, overflowWrap: 'anywhere' },
  trigger: { flexShrink: 0, minHeight: t.uiLeading, padding: 0, borderWidth: 0, backgroundColor: 'transparent', color: ink.fgSoft, fontFamily: t.fontSans, fontSize: t.metaSize, cursor: 'pointer', ':hover': { color: ink.fg }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
  time: { marginInlineStart: 'auto', flexShrink: 0, color: ink.fgMuted, fontVariantNumeric: 'tabular-nums' },
  panel: { marginTop: s.sm, paddingBlock: s.sm, paddingInline: s.md, maxHeight: 'min(32rem, 60vh)', overflowY: 'auto', borderRadius: r.sm, backgroundColor: surface.washSubtle, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.border },
  preview: { margin: 0, whiteSpace: 'pre-wrap', overflowWrap: 'anywhere', fontFamily: t.fontMono, fontSize: t.metaSize, color: ink.fgSoft },
  note: { marginBlockEnd: 0, marginBlockStart: s.sm, color: ink.fgMuted },
})
