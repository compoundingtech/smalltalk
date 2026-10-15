import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { Button as AriaButton } from 'react-aria-components'
import { accentVars as accent, textVars as text, borderVars as border, surfaceVars as surface, spaceVars as s, geometryVars as g, typeVars as t, radiusVars as r, motionVars as m } from '../composition-tokens.stylex'
import { Markdown } from './Markdown'
import { Icon } from './Icons'

const reasoningType = stylex.createTheme(t, { bodySize: t.codeSize, bodyLeading: t.uiLeading })

/** Muted reasoning as a work row: glyph slot, label, and the shared pinned disclosure; the opened body starts on the text column. */
export function ThinkingEntry({ text: value, streaming = false }: { text: string; streaming?: boolean }) {
  const [open, setOpen] = React.useState(false)
  const contentId = React.useId()
  return <div data-testid="thinking-entry" data-streaming={streaming} {...stylex.props(styles.thinking)}>
    <AriaButton aria-expanded={open} aria-controls={contentId} onPress={() => setOpen(value => !value)} {...stylex.props(stylex.defaultMarker(), styles.thinkingControl)}>
      <span aria-hidden="true" {...stylex.props(styles.slot)}><Icon name={streaming ? 'spinner' : 'thought'} spinning={streaming} size={14} /></span>
      <span data-call-title {...stylex.props(styles.label)}>Thinking</span>
      <span data-call-word />
      <span aria-hidden="true" data-row-disclosure={open ? 'open' : 'closed'} {...stylex.props(styles.slot, styles.disclosure)}><span {...stylex.props(styles.chevron, open && styles.disclosureOpen)}><Icon name="chevron-right" size={12} /></span></span>
      <span data-call-duration />
    </AriaButton>
    {open ? <div id={contentId} data-testid="thinking-body" {...stylex.props(styles.body, reasoningType)}><Markdown text={value} streaming={streaming} tone="muted" /></div> : null}
  </div>
}

const styles = stylex.create({
  thinking: { color: text.fgMuted, fontSize: t.uiSize, lineHeight: t.uiLeading },
  thinkingControl: { display: 'grid', gridTemplateColumns: g.workRowColumns, alignItems: 'center', gap: s.sm, width: '100%', padding: 0, minHeight: g.toolRow, borderWidth: 0, backgroundColor: surface.transparent, color: text.fgMuted, fontFamily: t.fontSans, fontSize: t.uiSize, lineHeight: t.uiLeading, textAlign: 'left', cursor: 'pointer', transitionProperty: 'background-color, color', transitionDuration: m.fast, transitionTimingFunction: m.ease, '@media (prefers-reduced-motion: reduce)': { transitionDuration: '0s' }, ':hover': { color: text.fgSoft, backgroundColor: surface.rowHover }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } },
  slot: { display: 'inline-flex', alignItems: 'center', justifyContent: 'center', width: g.icon },
  label: { minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' },
  disclosure: { opacity: { default: 0.5, [stylex.when.ancestor(':hover')]: 1, [stylex.when.ancestor(':focus-visible')]: 1 }, transitionProperty: 'opacity', transitionDuration: m.fast, transitionTimingFunction: m.ease, '@media (prefers-reduced-motion: reduce)': { transitionDuration: '0s' } },
  chevron: { display: 'inline-flex', transitionProperty: 'transform', transitionDuration: m.fast, transitionTimingFunction: m.ease, '@media (prefers-reduced-motion: reduce)': { transitionDuration: '0s' } },
  disclosureOpen: { transform: 'rotate(90deg)' },
  body: { marginBlockStart: s.xs2, marginInlineStart: g.textColumn, paddingBlock: s.md, paddingInline: s.lg, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.border, borderRadius: r.sm, color: text.fgMuted, fontSize: t.codeSize, lineHeight: t.uiLeading },
})
