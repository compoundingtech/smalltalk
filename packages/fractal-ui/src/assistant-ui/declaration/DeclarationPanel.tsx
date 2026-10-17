import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { Button, Heading, VisuallyHidden } from 'react-aria-components'
import { accentVars as accent, borderVars as border, geometryVars as g, radiusVars as r, spaceVars as s, statusVars as status, surfaceVars as surface, textVars as ink, typeVars as t } from '../composition-tokens.stylex'
import { tokenizeDeclaration, type DeclarationLanguage, type DeclarationTokenKind } from './declaration-tint'

export type { DeclarationLanguage } from './declaration-tint'

/** What the host knows about one subject's declaration. Messages are host-written display copy. */
export type DeclarationPanelState =
  | { readonly _tag: 'Loading' }
  | { readonly _tag: 'Unavailable'; readonly message: string }
  | { readonly _tag: 'Failed'; readonly message: string }
  | {
      readonly _tag: 'Ready'
      /** Shown and copied verbatim. */
      readonly text: string
      /** `json` marks a structured fallback where the server serves no KDL text. */
      readonly language: DeclarationLanguage
      /** Optional provenance line, e.g. the revision and where the text came from. */
      readonly source?: string
    }

export interface DeclarationPanelProps {
  /** The subject id, e.g. `agent/dotfiles/ui-design`. Inside a React Aria Dialog it names the dialog. */
  readonly title: string
  readonly state: DeclarationPanelState
  /** Host controls placed after Copy all, such as a modal's close button. */
  readonly actions?: React.ReactNode
}

type CopyState = { readonly tag: 'ready' } | { readonly tag: 'copying' } | { readonly tag: 'copied'; readonly text: string } | { readonly tag: 'failed' }

/** Read-only declaration source with tinting, line numbers and copy-all. Presentational; no modal coupling. */
export function DeclarationPanel({ title, state, actions }: DeclarationPanelProps) {
  const [copy, setCopy] = React.useState<CopyState>({ tag: 'ready' })
  const text = state._tag === 'Ready' ? state.text : undefined
  const copyAll = async () => {
    if (text === undefined) return
    setCopy({ tag: 'copying' })
    try {
      await navigator.clipboard.writeText(text)
      setCopy({ tag: 'copied', text })
    } catch {
      setCopy({ tag: 'failed' })
    }
  }
  const copiedCurrent = copy.tag === 'copied' && copy.text === text
  const copyLabel = copy.tag === 'copying' ? 'Copying…' : copiedCurrent ? 'Copied' : 'Copy all'
  return <section data-testid="declaration-panel" data-declaration-state={state._tag} aria-busy={state._tag === 'Loading'} {...stylex.props(styles.panel)}>
    <header {...stylex.props(styles.header)}>
      <Heading slot="title" level={2} {...stylex.props(styles.title)}>{title}</Heading>
      {state._tag === 'Ready' ? <span data-testid="declaration-language" {...stylex.props(styles.language)}>{state.language === 'kdl' ? 'KDL' : 'JSON'}</span> : null}
      {text === undefined ? null : <Button aria-label={copyLabel === 'Copy all' ? 'Copy all declaration source' : copyLabel} isDisabled={copy.tag === 'copying'} onPress={copyAll} {...stylex.props(styles.button)}>{copyLabel}</Button>}
      {actions}
    </header>
    {state._tag === 'Ready' && state.source !== undefined ? <p data-testid="declaration-source" {...stylex.props(styles.source)}>{state.source}</p> : null}
    <div {...stylex.props(styles.body)}>
      {state._tag === 'Ready' ? <DeclarationCode title={title} text={state.text} language={state.language} />
        : state._tag === 'Loading' ? <p role="status" {...stylex.props(styles.notice)}>Loading declaration…</p>
        : state._tag === 'Unavailable' ? <p role="status" {...stylex.props(styles.notice)}>{state.message}</p>
        : <p role="alert" {...stylex.props(styles.notice, styles.failed)}>{state.message}</p>}
    </div>
    <span role="status" {...stylex.props(copy.tag === 'failed' && styles.copyFailed)}>{copy.tag === 'failed' ? 'Could not copy the declaration. Check clipboard permissions and try again.' : <VisuallyHidden>{copiedCurrent ? 'Declaration copied.' : ''}</VisuallyHidden>}</span>
  </section>
}

const DeclarationCode = React.memo(function DeclarationCode({ title, text, language }: { readonly title: string; readonly text: string; readonly language: DeclarationLanguage }) {
  // One trailing newline ends the last line; it is not an extra numbered line. Copy keeps it.
  const lines = React.useMemo(() => tokenizeDeclaration(text.endsWith('\n') ? text.slice(0, -1) : text, language), [text, language])
  // Source is a named focusable group within its panel, not a page landmark:
  // the same subject can appear in multiple panels (for example, scheme comparisons).
  return <div role="group" aria-label={`${title} declaration source`} tabIndex={0} data-testid="declaration-code" data-language={language} {...stylex.props(styles.scroller)}>
    <pre {...stylex.props(styles.pre)}>{lines.map((line, index) => <span key={index} data-line={index + 1} {...stylex.props(styles.line)}>
      <span aria-hidden="true" data-line-number="" {...stylex.props(styles.lineNumber)}>{index + 1}</span>
      <code {...stylex.props(styles.code)}>{line.map((token, position) => token.kind === 'text' ? token.text : <span key={position} data-token={token.kind} {...stylex.props(tint[token.kind])}>{token.text}</span>)}</code>
    </span>)}</pre>
  </div>
})

const styles = stylex.create({
  panel: { display: 'flex', flexDirection: 'column', minWidth: 0, minHeight: 0, height: '100%', color: ink.fg, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading },
  header: { display: 'flex', alignItems: 'center', gap: s.md, minHeight: g.band, paddingInline: s.lg, borderBottomWidth: g.hairline, borderBottomStyle: 'solid', borderBottomColor: border.border },
  title: { flexGrow: 1, minWidth: 0, margin: 0, overflowWrap: 'anywhere', fontFamily: t.fontMono, fontSize: t.uiSize, lineHeight: t.uiLeading, fontWeight: t.weightSemibold },
  language: { flexShrink: 0, fontFamily: t.fontMono, fontSize: t.metaSize, color: ink.fgMuted },
  button: { flexShrink: 0, minHeight: g.controlSm, paddingInline: s.sm, borderWidth: g.hairline, borderStyle: 'solid', borderColor: surface.transparent, borderRadius: r.sm, backgroundColor: surface.transparent, color: ink.fgSoft, fontFamily: t.fontSans, fontSize: t.metaSize, cursor: 'pointer', ':hover': { backgroundColor: surface.rowHover }, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary, outlineOffset: g.focusOffset }, ':disabled': { cursor: 'default', color: ink.fgMuted } },
  source: { margin: 0, paddingInline: s.lg, paddingBlockStart: s.md, color: ink.fgMuted, fontVariantNumeric: 'tabular-nums', overflowWrap: 'anywhere' },
  body: { display: 'flex', flexDirection: 'column', flexGrow: 1, minHeight: 0, padding: s.lg },
  notice: { margin: 0, color: ink.fgMuted },
  failed: { color: status.dangerFg },
  copyFailed: { display: 'block', paddingInline: s.lg, paddingBlockEnd: s.md, color: status.dangerFg },
  scroller: { flexGrow: 1, minHeight: 0, overflow: 'auto', backgroundColor: surface.codeBg, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.border, borderRadius: r.md, ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary, outlineOffset: `calc(-1 * ${g.focusRing})` } },
  pre: { display: 'grid', gridTemplateColumns: 'max-content max-content', width: 'max-content', minWidth: '100%', margin: 0, paddingBlock: s.md, fontFamily: t.fontMono, fontSize: t.codeSize, lineHeight: t.uiLeading, color: ink.fgMuted },
  line: { display: 'contents' },
  lineNumber: { position: 'sticky', left: 0, paddingInlineStart: s.lg, paddingInlineEnd: s.lg, textAlign: 'right', color: ink.fgMuted, backgroundColor: surface.codeBg, fontVariantNumeric: 'tabular-nums', userSelect: 'none' },
  code: { paddingInlineEnd: s.lg, fontFamily: 'inherit', whiteSpace: 'pre' },
  node: { color: ink.fg, fontWeight: t.weightMedium },
  property: { color: status.runningFg },
  string: { color: ink.fgSoft },
  literal: { color: status.attention },
  comment: { color: ink.fgMuted, fontStyle: 'italic' },
  redacted: { color: ink.fgMuted, fontStyle: 'italic' },
})

const tint: Record<Exclude<DeclarationTokenKind, 'text'>, stylex.StyleXStyles | null> = {
  node: styles.node,
  property: styles.property,
  string: styles.string,
  number: styles.literal,
  keyword: styles.literal,
  comment: styles.comment,
  punctuation: null,
  redacted: styles.redacted,
}
