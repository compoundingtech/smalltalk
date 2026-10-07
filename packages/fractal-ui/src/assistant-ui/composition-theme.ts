import * as stylex from '@stylexjs/stylex'
import { surfaceVars, textVars, borderVars, accentVars, statusVars, colorVars } from './composition-tokens.stylex'

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
  washSubtle: 'rgba(0, 0, 0, 0.02)',
  controlFill: 'rgba(0, 0, 0, 0.025)',
  scrim: 'rgba(0, 0, 0, 0.3)', transparent: 'transparent',
})

const textVarsTheme = stylex.createTheme(textVars, {
  fg: '#27272a',
  fgMuted: '#66666e',
  fgSoft: 'rgba(39, 39, 42, 0.82)',
  fgFaint: 'rgba(39, 39, 42, 0.55)',
  sidebarFgMuted: '#626b77',
})

const borderVarsTheme = stylex.createTheme(borderVars, {
  border: '#e4e4e7',
  borderStrong: '#d4d4d8',
  glassBorder: 'rgba(0, 0, 0, 0.06)',
  controlBorder: 'rgba(0, 0, 0, 0.1)',
  focusBorder: 'rgba(0, 0, 0, 0.12)',
})

const accentVarsTheme = stylex.createTheme(accentVars, {
  primary: '#1b4ed8',
  primaryHover: '#1a46c2',
  onPrimary: '#ffffff',
})

const statusVarsTheme = stylex.createTheme(statusVars, {
  running: '#2563eb',
  runningFg: '#1d4ed8',
  done: '#059669',
  attention: '#b45309',
  danger: '#dc2626',
  dangerFg: '#b91c1c',
  diffAdded: '#059669',
  diffRemoved: '#dc2626',
  diffAddedWash: 'rgba(5, 150, 105, 0.08)',
  diffRemovedWash: 'rgba(220, 38, 38, 0.08)',
  dangerMuted: 'rgba(220, 38, 38, 0.4)',
  scrollThumb: 'rgba(0, 0, 0, 0.15)',
  scrollThumbHover: 'rgba(0, 0, 0, 0.25)',
})

const colorTheme = stylex.createTheme(colorVars, {
  canvas: '#fcfcfc', sidebar: '#ffffff', raised: '#ffffff', message: '#f4f4f5',
  fg: '#27272a', fgMuted: '#66666e', fgSoft: 'rgba(39, 39, 42, 0.82)', fgFaint: 'rgba(39, 39, 42, 0.55)',
  sidebarFgMuted: '#626b77', border: '#e4e4e7', borderStrong: '#d4d4d8',
  glassFill: 'rgba(255, 255, 255, 0.85)', glassBorder: 'rgba(0, 0, 0, 0.06)',
  primary: '#1b4ed8', primaryHover: '#1a46c2', onPrimary: '#ffffff',
  running: '#2563eb', runningFg: '#1d4ed8', done: '#059669', attention: '#b45309',
  danger: '#dc2626', dangerFg: '#b91c1c',
  diffAdded: '#059669', diffRemoved: '#dc2626',
  diffAddedWash: 'rgba(5, 150, 105, 0.08)', diffRemovedWash: 'rgba(220, 38, 38, 0.08)',
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

export const lightTheme = [surfaceVarsTheme, textVarsTheme, borderVarsTheme, accentVarsTheme, statusVarsTheme, colorTheme] as const
export const lightThemes = { color: colorTheme }
