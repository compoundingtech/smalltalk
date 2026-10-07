import * as stylex from '@stylexjs/stylex'
import { tokens, composerChrome, accentTokens } from './embrace-tokens.stylex'

export const darkTheme = stylex.createTheme(tokens, {
  canvas: '#151515', panel: '#202020', recess: '#292929', ink: '#ededed',
  muted: '#b3b3b3', line: '#4c4c4c', accent: '#ededed', onAccent: '#202020',
  selection: '#383838', good: '#a3d8b8', warning: '#e4c286', danger: '#f0a6ad',
  added: '#24382b', removed: '#442b30',
})

/** Live round-2 composer chrome from the reviewed composition palette. */
export const liveComposerDarkTheme = stylex.createTheme(composerChrome, {
  composerBackground: '#111111', inputBackground: '#141414', composerRadius: '22px',
  inputRadius: '8px', inputBorder: '0px', composerDocsDisplay: 'none', composerHelpDisplay: 'none',
  historyDisplay: 'none', disabledCancelDisplay: 'none',
})
export const liveAccentTheme = stylex.createTheme(accentTokens, { accent: '#346bf1', onAccent: '#ffffff' })
