import * as stylex from '@stylexjs/stylex'
import * as React from 'react'

import { colorVars as c, typeVars as t } from '../../../../packages/fractal-ui/src/assistant-ui/composition-tokens.stylex.ts'
import { DOCUMENT_REF } from './document-port.ts'

/** A `doc/…` name in Markdown that opens the document in a workspace tab. */
export const DocumentLink = ({ name, onOpen }: { readonly name: string; readonly onOpen: (ref: string) => void }): React.ReactNode => (
  <a
    href={name}
    onClick={(event) => {
      event.preventDefault()
      event.stopPropagation()
      onOpen(name)
    }}
    {...stylex.props(styles.link)}
  >
    {name}
  </a>
)

/** The inline-reference renderer a transcript or document uses for `doc/…` names. */
export const documentReferences = (onOpen: (ref: string) => void) => (text: string): React.ReactNode | undefined =>
  DOCUMENT_REF.test(text) ? <DocumentLink name={text} onOpen={onOpen} /> : undefined

const styles = stylex.create({
  link: { fontFamily: t.fontMono, fontSize: '0.92em', color: c.primary, textDecoration: 'underline', textUnderlineOffset: '2px', cursor: 'pointer', overflowWrap: 'anywhere' },
})
