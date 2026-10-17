import * as stylex from '@stylexjs/stylex'
import { surfaceVars, textVars, borderVars, accentVars, interactionVars, focusRingBridgeVars, elevationVars, statusVars, colorVars } from './composition-tokens.stylex'

export type Scheme = 'dark' | 'light'

const surfaceVarsTheme = stylex.createTheme(surfaceVars, {
  canvas: '#fcfcfc',
  sidebar: '#ffffff',
  raised: '#ffffff',
  message: '#f4f4f5',
  glassFill: 'rgba(255, 255, 255, 0.85)',
  rowHover: '#f4f4f5',
  rowActive: '#ebebeb',
  codeBg: '#f4f4f5',
  terminal: '#ffffff',
  washSubtle: 'rgba(0, 0, 0, 0.02)',
  controlFill: 'rgba(0, 0, 0, 0.025)',
  scrim: 'rgba(0, 0, 0, 0.3)', transparent: 'transparent',
})

const textVarsTheme = stylex.createTheme(textVars, {
  fg: '#27272a',
  fgMuted: '#66666e',
  fgSoft: 'rgba(39, 39, 42, 0.82)',
  sidebarFgMuted: '#626b77',
})

const borderVarsTheme = stylex.createTheme(borderVars, {
  border: '#e4e4e7',
  borderStrong: '#e4e4e7',
  glassBorder: 'rgba(0, 0, 0, 0.06)',
  controlBorder: 'rgba(0, 0, 0, 0.1)',
  focusBorder: 'rgba(0, 0, 0, 0.12)',
  rail: 'rgba(0, 0, 0, 0.1)',
  railStrong: 'rgba(0, 0, 0, 0.12)',
})

const accentVarsTheme = stylex.createTheme(accentVars, {
  primary: '#1b4ed8',
  primaryHover: '#1a46c2',
  onPrimary: '#ffffff',
})

const interactionTheme = stylex.createTheme(interactionVars, {
  focusRing: accentVars.primary,
  composerFocusBorder: 'rgba(27, 78, 216, 0.4)',
  wash: 'rgba(39, 39, 42, 0.06)',
})
// LIVE-MIGRATION BRIDGE arn:lmig:smalltalk:2026-10-12-fractal-focus-ring-fallback — DELETE at contraction — https://www.notion.so/3f7e3d41f4a381839fa5f7b257ba8f3c
// Re-resolve the global fallback ring's alias at the themed element.
const focusRingBridgeTheme = stylex.createTheme(focusRingBridgeVars, {
  '--fractal-focus-ring': interactionVars.focusRing,
})
// LIVE-MIGRATION END arn:lmig:smalltalk:2026-10-12-fractal-focus-ring-fallback
const elevationTheme = stylex.createTheme(elevationVars, {
  popover: '0 18px 44px -18px rgb(0 0 0 / 20%)',
  dialog: '0 24px 64px -24px rgb(0 0 0 / 20%)',
})

export const lightStatusTheme = stylex.createTheme(statusVars, {
  running: '#2563eb',
  runningFg: '#1d4ed8',
  done: textVars.fgSoft,
  attention: '#b45309',
  danger: '#dc2626',
  dangerFg: '#b91c1c',
  diffAdded: textVars.fgMuted,
  diffRemoved: '#dc2626',
  diffAddTint: '#4269df',
  diffRemovedWash: 'rgba(220, 38, 38, 0.08)',
  dangerMuted: 'rgba(220, 38, 38, 0.4)',
  scrollThumb: 'rgba(0, 0, 0, 0.15)',
  scrollThumbHover: 'rgba(0, 0, 0, 0.25)',
})

const colorTheme = stylex.createTheme(colorVars, {
  canvas: '#fcfcfc', sidebar: '#ffffff', raised: '#ffffff', message: '#f4f4f5',
  fg: '#27272a', fgMuted: '#66666e', fgSoft: 'rgba(39, 39, 42, 0.82)',
  sidebarFgMuted: '#626b77', border: '#e4e4e7', borderStrong: '#e4e4e7',
  glassFill: 'rgba(255, 255, 255, 0.85)', glassBorder: 'rgba(0, 0, 0, 0.06)',
  primary: '#1b4ed8', primaryHover: '#1a46c2', onPrimary: '#ffffff',
  running: '#2563eb', runningFg: '#1d4ed8', done: textVars.fgSoft, attention: '#b45309',
  danger: '#dc2626', dangerFg: '#b91c1c',
  diffAdded: textVars.fgMuted, diffRemoved: '#dc2626',
  diffRemovedWash: 'rgba(220, 38, 38, 0.08)',
  codeBg: '#f4f4f5',
  rowHover: '#f4f4f5', rowActive: '#ebebeb',
  washSubtle: 'rgba(0, 0, 0, 0.02)',
  controlFill: 'rgba(0, 0, 0, 0.025)',
  controlBorder: 'rgba(0, 0, 0, 0.1)',
  focusBorder: 'rgba(0, 0, 0, 0.12)',
  dangerMuted: 'rgba(220, 38, 38, 0.4)',
  scrollThumb: 'rgba(0, 0, 0, 0.15)',
  scrollThumbHover: 'rgba(0, 0, 0, 0.25)',
})

export const lightThemeWithoutStatus = [surfaceVarsTheme, textVarsTheme, borderVarsTheme, accentVarsTheme, interactionTheme, focusRingBridgeTheme, elevationTheme, colorTheme] as const
export const lightTheme = [...lightThemeWithoutStatus, lightStatusTheme] as const
export const lightThemes = { color: colorTheme }
