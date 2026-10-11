import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import { Schema } from 'effect'
import { assistantDarkTheme, liveComposerDarkTheme, liveAccentTheme, compositionLightTheme } from '@smalltalk/fractal-ui/assistant-ui/shell'
import { colorVars, typeVars } from '../../../../packages/fractal-ui/src/assistant-ui/composition-tokens.stylex.ts'
import { DataSourceProvider } from '../data/react.tsx'
import { persistedAtom } from '../state/persistence.ts'
import { setTheme } from '../ui-compat/foundations.ts'
import { liveLegacyTheme } from '../ui-compat/live-theme.stylex.ts'
import { appStoryStates, createAppStoryFixture, type AppStoryFixture } from './storybookScenarios.ts'

export interface AppStoryArgs { readonly scheme: 'light' | 'dark'; readonly width: 600 | 1440 }

/** Reset the production persistence boundary without importing App/liveRuntime.
 * Each specimen has its own source and registry; no session adoption leaks. */
export const prepareAppStory = (scheme: AppStoryArgs['scheme']): void => {
  setTheme(scheme)
  // Preserve Storybook's iframe pathname and id/refId/args/globals query.
  // Only `open` belongs to the app router; dropping it selects the thread.
  const url = new URL(window.location.href)
  url.searchParams.delete('open')
  window.history.replaceState(window.history.state, '', url)
  const registry = AtomRegistry.make()
  registry.set(persistedAtom({ key: 'round2.scheme', schema: Schema.Literals(['dark', 'light']), defaultValue: 'dark' }), scheme)
  registry.set(persistedAtom({ key: 'round2.agent', schema: Schema.String, defaultValue: '' }), '')
  registry.set(persistedAtom({ key: 'round2.sidebarClosed', schema: Schema.Boolean, defaultValue: false }), false)
  registry.dispose()
}

const Specimen = ({ state, scheme, width, children }: AppStoryArgs & {
  readonly state: typeof appStoryStates[number]
  readonly children: (fixture: AppStoryFixture) => React.ReactNode
}) => {
  const [fixture] = React.useState(() => createAppStoryFixture(state))
  const [registry] = React.useState(() => AtomRegistry.make())
  React.useLayoutEffect(() => () => registry.dispose(), [registry])
  return <section data-app-state={state} aria-label={`${state} · ${scheme} · ${width}`} style={{ width, flexShrink: 0 }}>
    <h2>{state} · {scheme} · {width}px</h2>
    {/* Same theme roots as LiveAgentWorkspace, for the standalone pane too. */}
    <div data-scheme={scheme} {...stylex.props(styles.frame, liveLegacyTheme, scheme === 'dark' && assistantDarkTheme, scheme === 'dark' && liveComposerDarkTheme, liveAccentTheme, scheme === 'light' && compositionLightTheme)}>
      <DataSourceProvider source={fixture.source} registry={registry}>{children(fixture)}</DataSourceProvider>
    </div>
  </section>
}

export const AppStoryStates = ({ renderSurface, ...args }: AppStoryArgs & {
  readonly renderSurface: (fixture: AppStoryFixture) => React.ReactNode
}) => <main aria-label="Production app states" style={{ display: 'flex', flexDirection: 'column', gap: 32, padding: 16 }}>
  {appStoryStates.map(state => <Specimen key={`${state}.${args.scheme}.${args.width}`} state={state} {...args}>{renderSurface}</Specimen>)}
</main>

const styles = stylex.create({
  frame: { display: 'flex', flexDirection: 'column', height: '100dvh', minHeight: 600, minWidth: 0, overflow: 'hidden', backgroundColor: colorVars.canvas, color: colorVars.fg, fontFamily: typeVars.fontSans, fontSize: typeVars.uiSize },
})
