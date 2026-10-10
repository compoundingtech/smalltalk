import type { ReactNode } from 'react'
import type { Decorator, StoryContext } from '@storybook/react-vite'
import { manualClock } from '@smalltalk/st3-scenarios'
import { withScenario } from '@smalltalk/st3-scenarios/storybook'
import * as stylex from '@stylexjs/stylex'
import { lightTheme } from './composition-theme'
import { darkTheme as embraceDarkTheme } from './embrace-theme'
import { surfaceVars, textVars, spaceVars, typeVars } from './composition-tokens.stylex'

export const scenarioTime = (context: StoryContext): { now: number; anchor: number } => {
  const pin = context.globals.scenarioNow
  const anchor = typeof pin === 'number' ? pin : typeof pin === 'string' ? Date.parse(pin) : Date.now()
  const elapsed = typeof context.args.scenarioAt === 'number' ? context.args.scenarioAt : 0
  return { anchor, now: context.parameters.scenarioClock?.now() ?? anchor + elapsed }
}

/** A deterministic elapsed-time control lets the actual sync timeline reach its named state. */
export const withScenarioTime: Decorator = (Story, context) => {
  if (context.parameters.scenario === undefined || context.parameters.scenarioClock !== undefined) return withScenario(Story, context)
  const { now } = scenarioTime(context)
  return withScenario(Story, { ...context, parameters: { ...context.parameters, scenarioClock: manualClock(now) } })
}

export function ScenarioPresentation({ scheme, title, children }: { readonly scheme: 'light' | 'dark'; readonly title: string; readonly children: ReactNode }) {
  return <main data-theme={scheme} {...stylex.props(scheme === 'light' && lightTheme, scheme === 'dark' && embraceDarkTheme, styles.page)}>
    <h1 {...stylex.props(styles.heading)}>{title}</h1>
    <section {...stylex.props(styles.content)}>{children}</section>
  </main>
}

const styles = stylex.create({
  page: { minHeight: '100vh', backgroundColor: surfaceVars.canvas, color: textVars.fg, padding: spaceVars.section, fontFamily: typeVars.fontSans },
  heading: { fontSize: typeVars.headingSize, margin: 0, marginBottom: spaceVars.section },
  content: { maxWidth: 1000, marginInline: 'auto' },
})
