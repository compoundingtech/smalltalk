import type * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { QuestionFormKitProps } from '@smalltalk/fractal-ui/assistant-ui'
import { QuestionForm } from '../../../../../packages/fractal-ui/src/assistant-ui/questions/QuestionForm.tsx'
import { geometryVars, spaceVars } from '../../../../../packages/fractal-ui/src/assistant-ui/composition-tokens.stylex.ts'

/** The kit owns expansion and settlement; the containing group owns scrolling. */
export function QuestionDock(props: Omit<QuestionFormKitProps, 'variant' | 'autoFocus'>) {
  return <div data-testid="question-dock" {...stylex.props(styles.dock)}>
    <QuestionForm {...props} variant="pinned" autoFocus={false} />
  </div>
}

/** One budget for every pending ask, relative to the conversation pane (including drawer splits). */
export function QuestionDockGroup({ children }: { readonly children: React.ReactNode }) {
  return <div data-testid="question-dock-group" style={{ flex: '0 1 auto', minHeight: 0, maxHeight: '45%', overflowY: 'auto' }}>{children}</div>
}
const styles = stylex.create({
  dock: { minWidth: 0, width: '100%', boxSizing: 'border-box', paddingInline: spaceVars.lg, paddingBlock: spaceVars.sm, marginInline: 'auto', maxWidth: geometryVars.lane },
})
