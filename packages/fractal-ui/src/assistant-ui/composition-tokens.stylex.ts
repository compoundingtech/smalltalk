import * as stylex from '@stylexjs/stylex'

export const surfaceVars = stylex.defineVars({
  canvas: '#0a0a0a',
  sidebar: '#000000',
  raised: '#111111',
  message: '#141414',
  glassFill: 'rgba(17, 17, 17, 0.8)',
  rowHover: '#131313',
  rowActive: '#1a1b1b',
  codeBg: '#111111',
  washSubtle: 'rgba(255, 255, 255, 0.02)',
  controlFill: 'rgba(255, 255, 255, 0.025)',
  scrim: 'rgba(0, 0, 0, 0.65)', transparent: 'transparent',
})

export const textVars = stylex.defineVars({
  fg: '#f5f5f5',
  fgMuted: '#818181',
  fgSoft: 'rgba(245, 245, 245, 0.8)',
  fgFaint: 'rgba(245, 245, 245, 0.52)',
  sidebarFgMuted: '#a3a3a3',
})

export const borderVars = stylex.defineVars({
  border: 'rgba(255, 255, 255, 0.06)',
  borderStrong: 'rgba(255, 255, 255, 0.08)',
  glassBorder: 'rgba(255, 255, 255, 0.05)',
  controlBorder: 'rgba(255, 255, 255, 0.025)',
  focusBorder: 'rgba(255, 255, 255, 0.08)',
})

export const accentVars = stylex.defineVars({
  primary: '#346bf1',
  primaryHover: '#3061d9',
  onPrimary: '#ffffff',
})

export const statusVars = stylex.defineVars({
  running: '#3b82f6',
  runningFg: '#60a5fa',
  done: '#34d399',
  attention: '#fe9a00',
  danger: '#fb414a',
  dangerFg: '#ff6467',
  diffAdded: '#34d399',
  diffRemoved: '#f87171',
  diffAddedWash: 'rgba(52, 211, 153, 0.09)',
  diffRemovedWash: 'rgba(248, 113, 113, 0.09)',
  dangerMuted: 'rgba(251, 65, 74, 0.4)',
  scrollThumb: 'rgba(255, 255, 255, 0.08)',
  scrollThumbHover: 'rgba(255, 255, 255, 0.12)',
})

/** Type scale pairs and font stacks. UI 14/20, meta 12/16, dense 11, code 13, body 14/22.75. */
export const typeVars = stylex.defineVars({
  fontSans: `-apple-system, BlinkMacSystemFont, 'Segoe UI', system-ui, sans-serif`,
  fontMono: `ui-monospace, 'SF Mono', Menlo, Consolas, monospace`,
  uiSize: '14px', uiLeading: '20px',
  metaSize: '12px', metaLeading: '16px',
  denseSize: '11px',
  codeSize: '13px',
  bodySize: '14px', bodyLeading: '22.75px',
  weightMedium: '500',
  microSize: '10px', weightRegular: '400', weightSemibold: '600', weightBold: '700',
  trackingWide: '0.1em', trackingEyebrow: '0.2em',
  headingSize: '16px', headingLeading: '24px',
})

/** Radius scale; slab is the composer surface, control is buttons and inputs. */
export const radiusVars = stylex.defineVars({
  sm: '6px', md: '8px', base: '10px', lg: '14px', xl: '18px', slab: '22px', control: '8px',
  keyboard: '4px',
  full: '9999px', none: '0',
})

/** Spacing scale used across the shell. */
export const spaceVars = stylex.defineVars({
  xs2: '2px', xs: '4px', sm: '6px', md: '8px', lg: '12px', xl: '16px', xxl: '20px',
  zero: '0', hairline: '1px', xxxs: '1px', row: '10px', section: '24px', control: '28px', panel: '32px',
  proseGap: '10px',
})

/** Fixed geometry: band heights, lane widths, control sizes, gutters. */
export const geometryVars = stylex.defineVars({
  band: '52px',
  lane: '736px', laneMin: '640px', laneWide: '1152px',
  proseMax: 'none',
  sidebarMin: '208px', sidebarDefault: '256px',
  panelDefault: '540px', panelMin: '360px',
  controlSm: '24px', controlMd: '28px', controlLg: '32px',
  toolRow: '24px',
  menuRow: '32px',
  gutter: 'clamp(12px, 1.25vw, 20px)',
  drawerDefault: '220px', drawerMin: '120px',
  footer: '40px',
  heroSize: '30px', heroLeading: '36px',
  tabMax: '144px', crumbMax: '160px',
  editorMin: '78px', editorMax: '208px', userClamp: '176px', previewMax: '256px',
  toolIndent: '28px', gutter40: '40px', icon: '16px', status: '14px', statusPip: '6px', stop: '12px', glyphRing: '11px',
  sidebarRow: '48px', sidebarNested: '32px', resourceCard: '40px', hairline: '1px', focusRing: '2px', blur: '16px',
  sidebarMetric: '5ch',
  drawerMax: '600px', rowInset: '10px', scrollbar: '6px', caret: '1.5px',
  focusOffset: '1px', separatorHitSlop: '3px', specimenMax: '1400px', workshopMax: '1500px',
  specimenAside: '340px', specimenSidebar: '250px', commandMaxHeight: 'min(28rem, 60vh)',
  modalMax: '448px', tooltipMax: '320px', descriptionColumns: 'minmax(90px, auto) minmax(0, 1fr)',
  statusWord: '80px',
  switchWidth: '32px', switchHeight: '18px', iconSm: '12px', statusDot: '6px',
  emptyMark: '36px', avatar: '32px', fileMax: '192px', emptyHintMax: '288px',
  compactEditorMin: '48px', progressSm: '4px', progressMd: '6px',
  specimenInset: { default: '16px', '@media (min-width: 48rem)': '28px' },
  familyColumns: { default: 'minmax(0, 1fr)', '@media (min-width: 48rem)': 'repeat(2, minmax(0, 1fr))', '@media (min-width: 80rem)': 'repeat(3, minmax(0, 1fr))' },
  inventoryColumns: { default: 'minmax(0, 1fr)', '@media (min-width: 48rem)': 'repeat(2, minmax(0, 1fr))', '@media (min-width: 80rem)': 'repeat(4, minmax(0, 1fr))' },
  workshopColumns: { default: 'minmax(0, 1fr)', '@media (min-width: 80rem)': 'minmax(0, 1fr) 340px' },
  contextColumns: { default: 'minmax(0, 1fr)', '@media (min-width: 64rem)': '250px minmax(0, 1fr)' },
  menuMin: '208px', sectionTitle: '18px', dialogTitle: '20px', switchSelectedLeft: 'calc(100% - 16px)',
  sidebarThree: '68px', workExpandedMax: 'min(18rem, 50dvh)', timelineStub: '16px',
  transcriptFrameHeight: '640px',
  threadViewportMin: '96px',
  estimatedMessageHeight: '160px',

})

/** Numeric twins of the fixed-geometry tokens, for resize math in state. */
export const geometryNumbers = {
  laneMin: 640,
  sidebarMin: 208,
  sidebarDefault: 256,
  panelDefault: 540,
  panelMin: 360,
  drawerDefault: 220,
  footer: 40,
  drawerMin: 120, drawerMax: 600,
  resizeStep: 8, resizeStepLarge: 32, splitMinRatio: 0.1, splitMaxRatio: 0.9,
  tooltipOffset: 6, contextOffset: 8, scrollEndTolerance: 1,
  estimatedMessageHeight: 160,
  threadViewportMin: 96,
} as const

export const motionVars = stylex.defineVars({
  fast: '150ms', standard: '200ms', reveal: '600ms', spin: '1s', pulse: '2s',
  ease: 'ease', linear: 'linear',
})

export const elevationVars = stylex.defineVars({
  popover: '0 18px 44px -18px rgb(0 0 0 / 80%)',
  dialog: '0 24px 64px -24px rgb(0 0 0 / 65%)',
})



/** Compatibility palette for existing live consumers. */
export const colorVars = stylex.defineVars({
  canvas: '#0a0a0a',
  sidebar: '#000000',
  raised: '#111111',
  message: '#141414',
  fg: '#f5f5f5',
  fgMuted: '#818181',
  fgSoft: 'rgba(245, 245, 245, 0.8)',
  fgFaint: 'rgba(245, 245, 245, 0.52)',
  sidebarFgMuted: '#a3a3a3',
  border: 'rgba(255, 255, 255, 0.06)',
  borderStrong: 'rgba(255, 255, 255, 0.08)',
  glassFill: 'rgba(17, 17, 17, 0.8)',
  glassBorder: 'rgba(255, 255, 255, 0.05)',
  rowHover: '#131313',
  rowActive: '#1a1b1b',
  primary: '#346bf1',
  primaryHover: '#3061d9',
  onPrimary: '#ffffff',
  running: '#3b82f6',
  runningFg: '#60a5fa',
  done: '#34d399',
  attention: '#fe9a00',
  danger: '#fb414a',
  dangerFg: '#ff6467',
  diffAdded: '#34d399',
  diffRemoved: '#f87171',
  diffAddedWash: 'rgba(52, 211, 153, 0.09)',
  diffRemovedWash: 'rgba(248, 113, 113, 0.09)',
  codeBg: '#111111',
  washSubtle: 'rgba(255, 255, 255, 0.02)',
  controlFill: 'rgba(255, 255, 255, 0.025)',
  controlBorder: 'rgba(255, 255, 255, 0.025)',
  focusBorder: 'rgba(255, 255, 255, 0.08)',
  dangerMuted: 'rgba(251, 65, 74, 0.4)',
  scrollThumb: 'rgba(255, 255, 255, 0.08)',
  scrollThumbHover: 'rgba(255, 255, 255, 0.12)',
})

