import * as stylex from '@stylexjs/stylex'
import * as React from 'react'
import * as Aria from 'react-aria-components'

import { Markdown } from '../../../../packages/fractal-ui/src/assistant-ui/composition/Markdown.tsx'
import { colorVars as c, spaceVars as s, typeVars as t } from '../../../../packages/fractal-ui/src/assistant-ui/composition-tokens.stylex.ts'
import { useDataSource } from '../data/react.tsx'
import { DOCUMENT_REF, documentVersion, type DocumentRead } from './document-port.ts'
import { documentReferences } from './DocumentLink.tsx'

// An st document in a workspace tab: read once when opened, drawn as Markdown with its version,
// and every `doc/…` it links to opens in a tab of its own. "Read again" fetches the newest.

type State = { readonly _tag: 'Reading' } | DocumentRead

export const DocumentPane = ({
  name,
  onOpen,
}: {
  /** `doc/name` for the newest version, or an exact `doc/name@hash`. */
  readonly name: string
  readonly onOpen: (ref: string) => void
}): React.ReactNode => {
  const documents = useDataSource().documents
  const [state, setState] = React.useState<State>({ _tag: 'Reading' })
  const [attempt, setAttempt] = React.useState(0)
  React.useEffect(() => {
    if (documents === undefined) {
      setState({ _tag: 'Unavailable', reason: 'failed', detail: 'This connection cannot read documents.' })
      return
    }
    let live = true
    setState({ _tag: 'Reading' })
    void documents.read(name).then((read) => { if (live) setState(read) })
    return () => { live = false }
  }, [documents, name, attempt])
  const reference = React.useMemo(() => documentReferences(onOpen), [onOpen])
  // Markdown links to `doc/…` open here instead of leaving the app for a relative URL.
  const followLinks = (event: React.MouseEvent) => {
    const link = event.target instanceof Element ? event.target.closest('a[href]') : null
    const href = link?.getAttribute('href') ?? ''
    if (!DOCUMENT_REF.test(href)) return
    event.preventDefault()
    event.stopPropagation()
    onOpen(href)
  }
  const shown = state._tag === 'Text' || state._tag === 'Binary' ? documentVersion(state.reference) : documentVersion(name)
  return (
    <article aria-label={`Document ${shown.name}`} data-testid="document-pane" onClickCapture={followLinks} {...stylex.props(styles.pane)}>
      <header {...stylex.props(styles.header)}>
        <strong {...stylex.props(styles.name)}>{shown.name}</strong>
        {shown.version ? <span {...stylex.props(styles.meta)} title={state._tag === 'Text' || state._tag === 'Binary' ? state.reference : undefined}>version {shown.version}</span> : null}
        <Aria.Button onPress={() => setAttempt((value) => value + 1)} isDisabled={state._tag === 'Reading'} {...stylex.props(styles.button)}>
          {documentVersion(name).version ? 'Read again' : 'Read the newest'}
        </Aria.Button>
      </header>
      <div {...stylex.props(styles.body)}>
        {state._tag === 'Reading' ? (
          <p role="status" {...stylex.props(styles.notice)}>Reading {shown.name}…</p>
        ) : state._tag === 'Unavailable' ? (
          <p role="status" {...stylex.props(styles.notice)}>{state.detail}</p>
        ) : state._tag === 'Binary' ? (
          <p role="status" {...stylex.props(styles.notice)}>This document is not text ({state.size} bytes). Read it with st documents get {state.reference}.</p>
        ) : (
          <div {...stylex.props(styles.column)}><Markdown text={state.text} renderInlineReference={reference} /></div>
        )}
      </div>
    </article>
  )
}

const styles = stylex.create({
  // Over the retained conversation panes, which keep their state underneath.
  pane: { position: 'absolute', inset: 0, zIndex: 2, display: 'flex', flexDirection: 'column', flexGrow: 1, minHeight: 0, minWidth: 0, backgroundColor: c.canvas },
  header: { display: 'flex', alignItems: 'baseline', gap: s.md, paddingInline: s.lg, paddingBlock: s.md, borderBottomWidth: 1, borderBottomStyle: 'solid', borderBottomColor: c.border, flexShrink: 0, flexWrap: 'wrap' },
  name: { fontFamily: t.fontMono, fontSize: t.metaSize, overflowWrap: 'anywhere' },
  meta: { color: c.fgMuted, fontSize: t.denseSize, fontFamily: t.fontMono },
  button: { marginInlineStart: 'auto', padding: '4px 8px', borderWidth: 0, borderRadius: 6, backgroundColor: 'transparent', color: c.fgMuted, fontSize: t.metaSize, cursor: 'pointer', ':hover': { backgroundColor: c.rowHover }, ':disabled': { opacity: 0.5, cursor: 'default' } },
  body: { flexGrow: 1, minHeight: 0, overflowY: 'auto', padding: s.lg },
  column: { maxWidth: 820, marginInline: 'auto' },
  notice: { color: c.fgMuted, fontSize: t.metaSize },
})
