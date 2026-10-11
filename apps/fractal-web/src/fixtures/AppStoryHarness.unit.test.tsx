// @vitest-environment jsdom
import { afterEach, expect, it, vi } from 'vitest'
import { prepareAppStory } from './AppStoryHarness.tsx'

// The compiler is absent in node tests; retain the production harness behavior.
vi.mock('@stylexjs/stylex', () => ({ create: (styles: unknown) => styles, defineVars: (variables: unknown) => variables, createTheme: () => ({}), keyframes: () => 'animation', props: () => ({}) }))

afterEach(() => window.history.replaceState(null, '', '/'))

it('preserves the composed iframe and Storybook identity while resetting only the app view', () => {
  const pathname = '/apps/fractal-web/storybook-static/iframe.html'
  const query = 'id=fractal-app-liveagentworkspace--all-states&viewMode=story&refId=fractal-app&args=scheme%3Alight&globals=theme%3Adark&open=terminal'
  window.history.replaceState({ frame: 'app-ref' }, '', `${pathname}?${query}`)
  prepareAppStory('dark')
  expect(window.location.pathname).toBe(pathname)
  const params = new URLSearchParams(window.location.search)
  expect(Object.fromEntries(params)).toEqual({
    id: 'fractal-app-liveagentworkspace--all-states', viewMode: 'story', refId: 'fractal-app', args: 'scheme:light', globals: 'theme:dark',
  })
  expect(window.history.state).toEqual({ frame: 'app-ref' })
  // Planted old behavior proves both identity assertions detect the regression.
  window.history.replaceState(null, '', '/iframe.html')
  expect(window.location.pathname).not.toBe(pathname)
  expect(new URLSearchParams(window.location.search).has('refId')).toBe(false)
})
