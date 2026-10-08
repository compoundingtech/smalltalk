import type { ReactNode } from 'react'
import type { StoryContext } from '@storybook/react-vite'
import * as stylex from '@stylexjs/stylex'
import { lightTheme } from './composition-theme'
import { lightTheme as embraceLightTheme } from './embrace-theme'
import { surfaceVars, textVars, spaceVars, typeVars } from './composition-tokens.stylex'

export const scenarioTime = (context: StoryContext): { now: number; anchor: number } => {
  const pin = context.globals.scenarioNow
  const anchor = typeof pin === 'number' ? pin : typeof pin === 'string' ? Date.parse(pin) : Date.now()
  return { anchor, now: context.parameters.scenarioClock?.now() ?? anchor }
}

export function ScenarioPresentation({ scheme, title, children }: { readonly scheme: 'light' | 'dark'; readonly title: string; readonly children: ReactNode }) {
  return <main data-theme={scheme} {...stylex.props(scheme === 'light' && lightTheme, scheme === 'light' && embraceLightTheme, styles.page)}>
    <h1 {...stylex.props(styles.heading)}>{title}</h1>
    <section {...stylex.props(styles.content)}>{children}</section>
  </main>
}

const styles = stylex.create({
  page: { minHeight: '100vh', backgroundColor: surfaceVars.canvas, color: textVars.fg, padding: spaceVars.section, fontFamily: typeVars.fontSans },
  heading: { fontSize: typeVars.headingSize, margin: 0, marginBottom: spaceVars.section },
  content: { maxWidth: 1000, marginInline: 'auto' },
})
