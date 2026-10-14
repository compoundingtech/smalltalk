import * as stylex from '@stylexjs/stylex'
import type { ReactNode } from 'react'
import type { TranscriptTurn } from '@smalltalk/fractal-ui/assistant-ui'
import { colorVars as c, typeVars as t } from '../../../../packages/fractal-ui/src/assistant-ui/composition-tokens.stylex.ts'
import { nativeTurnFacts, type NativeTurnFacts } from '../conversation/assistantFacts.ts'


export const NativeTurnFooter = ({ facts }: { readonly facts: NativeTurnFacts }): ReactNode => {
  const fields = [facts.model, facts.tokens === undefined ? undefined : `${facts.tokens.toLocaleString('en-US')} tokens`, facts.costUsd === undefined ? undefined : `$${facts.costUsd.toFixed(4)}`].filter(value => value !== undefined)
  return fields.length === 0 ? undefined : <footer data-testid="native-turn-footer" {...stylex.props(styles.footer)}>{fields.join(' · ')}</footer>
}
export const nativeTurnFooter = (turn: TranscriptTurn): ReactNode => {
  const facts = nativeTurnFacts(turn)
  return facts === undefined ? undefined : <NativeTurnFooter facts={facts} />
}
const styles = stylex.create({ footer: { color: c.fgMuted, fontSize: t.metaSize, paddingBlock: 4 } })
