import * as stylex from '@stylexjs/stylex'
import { textVars, statusVars } from './composition-tokens.stylex'

// Neutral defaults retain V1; the full-app exploration opts into token themes.
// Group rule: one createTheme group per DOM path. Nested partial themes of the
// same group reset unspecified vars to these defaults, so direction themes are
// split across disjoint groups (palette/accent/transcript/composer/density/chrome).
export const tokens = stylex.defineVars({
  canvas: '#fafafa', panel: '#ffffff', recess: '#f3f3f3', ink: '#202020',
  muted: '#626262', line: '#d6d6d6', accent: '#303030', onAccent: '#ffffff',
  selection: '#e6e6e6', good: textVars.fgSoft, warning: '#805510', danger: '#a42b36',
  added: statusVars.diffAddTint, removed: '#f8e8e9',
})

/** Accent lives on its own group so a hue picker never re-resolves the palette. */
export const accentTokens = stylex.defineVars({ accent: '#303030', onAccent: '#ffffff' })

export const geometry = stylex.defineVars({
  bodyFont: 'system-ui, sans-serif',
  senderFont: 'inherit', senderWeight: '700', senderTracking: '0',
  senderTransform: 'none', avatarDisplay: 'grid', avatarRadius: '4px',
  sourceTagDisplay: 'inline', userBackground: 'transparent', userMarker: '0px',
  frameRadius: '6px', messageExtraInset: '0px', messageRule: '1px',
  toolFont: 'inherit', toolRadius: '4px', toolBackground: 'transparent',
  toolBorder: '0px', toolExtraHeight: '0px',
})

export const composerChrome = stylex.defineVars({
  composerRadius: '10px', composerMargin: '0px', composerOverlap: '0px', composerShadow: 'none',
  composerBackground: tokens.panel, composerDocsDisplay: 'block', composerHelpDisplay: 'block',
  historyDisplay: 'inline-block', disabledCancelDisplay: 'inline-flex',
  inputRadius: '6px', inputBackground: tokens.recess, inputBorder: '1px', controlRadius: '6px',
})

export const density = stylex.defineVars({
  bodySize: '13px', bodyLeading: '1.55', senderSize: '12px', gap: '6px',
  messageY: '8px', messageX: '10px', senderBottom: '6px', avatarSize: '24px',
  composerPad: '12px', composerGap: '10px',
  inputHeight: '76px', inputPad: '10px', inputSize: '14px',
  toolHeight: '0px', toolY: '6px', toolX: '8px', toolDetails: '10px',
  controlY: '6px', controlX: '10px',
})

export const appChrome = stylex.defineVars({
  sidebarWidth: '172px', sidebarBorder: '0px', sidebarBackground: tokens.canvas,
  contentInset: '10px', threadWidth: '720px', panelBorder: '0px',
  chromeSize: '12px', chromeRow: '30px',
})
