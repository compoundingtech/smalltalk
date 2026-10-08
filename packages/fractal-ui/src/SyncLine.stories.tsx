import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import type { Meta, StoryObj } from '@storybook/react-vite'
import { useArgs } from 'storybook/preview-api'
import { Button } from 'react-aria-components'
import { SyncLine } from './assistant-ui/st3-views/SyncLine'
import { syncLine, observeSyncStatus } from './assistant-ui/st3-views/sync-line'
import { syncObservations, syncNow } from './assistant-ui/st3-views/sync-fixtures'
import { baselineTheme } from './assistant-ui/neutral-theme'
import { lightTheme, type Scheme } from './assistant-ui/composition-theme'
import { ThemePortal } from './assistant-ui/taste/ThemePortal'
import { accentVars as accent, borderVars as border, geometryVars as g, radiusVars as r, spaceVars as s, surfaceVars as surface, textVars as ink, typeVars as t } from './assistant-ui/composition-tokens.stylex'
interface SyncArgs { scheme: Scheme; sequence: number }
const surfaces = ['agents', 'conversation', 'terminal', 'missions', 'usage'] as const
function TransitionPreview({ scheme, sequence, next, retry }: { scheme: Scheme; sequence: number; next: () => void; retry: () => void }) {
  const current = syncObservations[sequence % syncObservations.length]!
  return <main data-testid="sync-transition" data-sequence={sequence} {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}><ThemePortal>
    <header {...stylex.props(styles.heading)}><h1 {...stylex.props(styles.title)}>Sync transitions</h1><span {...stylex.props(styles.stateName)}>{current.id}</span><Button onPress={next} {...stylex.props(styles.button)}>Next observation</Button></header>
    {surfaces.map(label => <section key={label} aria-label={`${label} sync surface`} data-testid={`sync-surface-${label}`} {...stylex.props(styles.surface)}><header data-testid={`sync-header-${label}`} {...stylex.props(styles.band)}><strong {...stylex.props(styles.surfaceTitle)}>{label}</strong><div {...stylex.props(styles.slot)}><SyncLine status={current.status} label={label} now={syncNow} observedAt={current.observedAt} onRetry={retry} /></div></header><div data-testid={`sync-retained-${label}`} {...stylex.props(styles.retained)}>Last-known {label} content stays in place.</div></section>)}
    <div {...stylex.props(styles.statusBar)}><SyncLine status={current.status} label="socket" socket gateway="gateway-1" now={syncNow} observedAt={current.observedAt} onRetry={retry} /></div>
  </ThemePortal></main>
}
function TransitionStory({ args, updateArgs }: { args: SyncArgs; updateArgs: (patch: Partial<SyncArgs>) => void }) {
  const next = React.useCallback(() => updateArgs({ sequence: (args.sequence + 1) % syncObservations.length }), [args.sequence, updateArgs])
  const retry = React.useCallback(() => updateArgs({ sequence: 1 }), [updateArgs])
  return <TransitionPreview scheme={args.scheme} sequence={args.sequence} next={next} retry={retry} />
}
function StoryRender(args: SyncArgs) {
  const [, updateArgs] = useArgs<SyncArgs>()
  return <TransitionStory args={args} updateArgs={updateArgs} />
}
function ObservationRow({ observation, label }: { observation: typeof syncObservations[number]; label?: string }) {
  const [retried, setRetried] = React.useState(false)
  const retry = React.useCallback(() => setRetried(true), [])
  const status = retried ? requestedAfterRetry : observation.status
  return <div data-testid="sync-observation" data-sync-surface={label ?? 'conversation'} {...stylex.props(styles.observation)}><span {...stylex.props(styles.stateLabel)}>{label ?? observation.id}</span><SyncLine status={status} label={label ?? 'conversation'} now={syncNow} observedAt={retried ? syncNow - 1000 : observation.observedAt} onRetry={retry} /></div>
}
const requestedAfterRetry = { _tag: 'Requested', since: syncNow - 1000 } as const
function AllStatesStory() {
  return <main data-testid="sync-all-states" {...stylex.props(styles.allRoot, ...baselineTheme)}><h1 {...stylex.props(styles.title)}>Sync line · all observations</h1><div {...stylex.props(styles.schemes)}>{(['dark', 'light'] as const).map(scheme => <section key={scheme} aria-label={`${scheme} sync observations`} {...stylex.props(styles.themeColumn, ...baselineTheme, scheme === 'light' && lightTheme)}><ThemePortal><h2 {...stylex.props(styles.title)}>{scheme}</h2>{syncObservations.map(observation => <ObservationRow key={observation.id} observation={observation} />)}</ThemePortal></section>)}</div></main>
}
const subscriptionLimitObservations = syncObservations.filter(({ status }) => status._tag === 'Failed' && ((status.cause._tag === 'Server' && status.cause.code === 'subscription-limit') || (status.cause._tag === 'Local' && status.cause.kind === 'subscription-limit')))
function SubscriptionLimitsStory({ scheme }: SyncArgs) {
  return <main data-testid="sync-subscription-limits" {...stylex.props(styles.root, ...baselineTheme, scheme === 'light' && lightTheme)}><ThemePortal>
    <h1 {...stylex.props(styles.title)}>Subscription limits</h1>
    <p>Local and Server limits use the same plain message; only a reported cap is shown. Usage is an HTTP read and does not consume subscription slots.</p>
    {subscriptionLimitObservations.map(observation => <section key={observation.id} data-testid="sync-limit-case" {...stylex.props(styles.surface)}>
      <header {...stylex.props(styles.band)}><h2 {...stylex.props(styles.title)}>{observation.id}</h2></header>
      <ObservationRow observation={observation} label="conversation" />
      <ObservationRow observation={observation} label="usage" />
      <div {...stylex.props(styles.retained)}>Last-known content stays in place.</div>
    </section>)}
  </ThemePortal></main>
}
const subscriptionLimitPlay: NonNullable<StoryObj<SyncArgs>['play']> = async ({ canvasElement }) => {
  const cases = canvasElement.querySelectorAll('[data-testid="sync-limit-case"]')
  if (cases.length !== 4) throw new Error('Subscription limit stories must include Server and Local failures with absent, zero, and positive caps')
  for (const element of cases) {
    const conversation = element.querySelector('[data-sync-surface="conversation"] [data-testid="sync-line"]')
    const usage = element.querySelector('[data-sync-surface="usage"] [data-testid="sync-line"]')
    if (conversation?.getAttribute('data-sync-visible') !== 'true') throw new Error('Subscription resource failures must remain visible')
    if (usage === null || usage.getAttribute('data-sync-visible') !== 'false') throw new Error('Usage must exclude subscription limit failures')
    if (usage.querySelector('button:not(:disabled)') !== null) throw new Error('Excluded Usage failures must not expose retry or details actions')
  }
}
const meta = {
  title: 'Fractal UI/Sync Line',
  render: StoryRender,
  parameters: { layout: 'fullscreen', docs: { description: { component: 'Portable SyncStatus tagged union with epoch-ms timestamps and the syncLine vocabulary. Every status/stage, derived stalled observations, both themes, and fixed-height existing header/status slots. Last-known content remains present. Local and Server subscription-limit failures share plain vocabulary; only reported caps appear, including zero. HTTP Usage reads exclude subscription-limit failures and their actions. Dedicated dark/light stories cover those exclusions and the full transition sequence. No byte-progress or invented stage is shown.' } } },
  args: { scheme: 'dark', sequence: 0 }, argTypes: { scheme: { options: ['dark', 'light'], control: 'radio' }, sequence: { control: { type: 'number', min: 0, max: syncObservations.length - 1 } } },
} satisfies Meta<SyncArgs>
export default meta
type Story = StoryObj<SyncArgs>
export const AllStates: Story = { render: AllStatesStory }
export const UnknownReason: Story = { args: { sequence: syncObservations.findIndex(observation => observation.id === 'Stale · Unknown') } }
export const UnknownReasonLastLive: Story = { args: { sequence: syncObservations.findIndex(observation => observation.id === 'Stale · Unknown · last live') } }
export const LocalFailure: Story = { args: { sequence: syncObservations.findIndex(observation => observation.id === 'Failed · Local · cap') } }
export const LocalFailureWithoutCap: Story = { args: { sequence: syncObservations.findIndex(observation => observation.id === 'Failed · Local · no cap') } }
export const LocalFailureZeroCap: Story = { args: { sequence: syncObservations.findIndex(observation => observation.id === 'Failed · Local · zero cap') } }
export const UnknownFailure: Story = { args: { sequence: syncObservations.findIndex(observation => observation.id === 'Failed · Unknown') } }
export const ServerFailure: Story = { args: { sequence: syncObservations.findIndex(observation => observation.id === 'Failed') } }
export const ServerFailureSubscriptionLimit: Story = { args: { sequence: syncObservations.findIndex(observation => observation.id === 'Failed · Server · subscription limit') } }
export const SubscriptionLimitsDark: Story = { render: SubscriptionLimitsStory, args: { scheme: 'dark' }, argTypes: { sequence: { control: false } }, play: subscriptionLimitPlay }
export const SubscriptionLimitsLight: Story = { render: SubscriptionLimitsStory, args: { scheme: 'light' }, argTypes: { sequence: { control: false } }, play: subscriptionLimitPlay }
export const TransitionSequence: Story = {
  render: StoryRender,
  play: async ({ canvasElement }) => {
    const next = canvasElement.querySelector<HTMLButtonElement>('button')!
    const root = canvasElement.querySelector<HTMLElement>('[data-testid="sync-transition"]')!
    await document.fonts.ready
    const geometry = () => [...canvasElement.querySelectorAll<HTMLElement>('[data-testid^="sync-header-"], [data-testid^="sync-retained-"], [data-testid="sync-line"]')].map(element => { const box = element.getBoundingClientRect(); return [box.x, box.y, box.width, box.height] })
    const before = geometry()
    let cls = 0
    const observer = new PerformanceObserver(list => { for (const entry of list.getEntries()) cls += (entry as PerformanceEntry & { value: number }).value })
    observer.observe({ type: 'layout-shift' })
    try {
      for (let index = 0; index < syncObservations.length; index++) {
        const expected = (Number(root.dataset.sequence) + 1) % syncObservations.length
        next.click()
        for (let frame = 0; Number(root.dataset.sequence) !== expected && frame < 120; frame++) await new Promise<void>(resolve => requestAnimationFrame(() => resolve()))
        if (Number(root.dataset.sequence) !== expected) throw new Error(`Sync transition ${index} did not advance to ${expected}`)
        await new Promise<void>(resolve => requestAnimationFrame(() => requestAnimationFrame(() => resolve())))
        const after = geometry()
        if (JSON.stringify(before) !== JSON.stringify(after)) throw new Error(`Sync transition ${index} changed final geometry: ${JSON.stringify({ before, after })}`)
      }
      await new Promise<void>(resolve => requestAnimationFrame(() => resolve()))
      if (cls !== 0) throw new Error(`Sync transitions require CLS 0; measured ${cls}`)
      root.dataset.cls = String(cls)
      root.dataset.transitions = String(syncObservations.length)
      const requested = { _tag: 'Requested', since: syncNow - 1000 } as const
      const first = syncLine({ status: requested, label: 'agents', now: syncNow, observedAt: syncNow - 1000 })
      const tick = syncLine({ status: requested, label: 'agents', now: syncNow + 1000, observedAt: syncNow - 1000 })
      if (first === undefined || tick === undefined) throw new Error('Requested observations must be visible after the delay')
      if (first.announce !== tick.announce || first.text === tick.text) throw new Error('Elapsed tick must change only visual text, not announcements')
      const resync = syncObservations.find(observation => observation.id === 'Stale · Resync')!.status
      const reconnecting = syncObservations.find(observation => observation.id === 'Stale · Reconnecting')!.status
      if (syncLine({ status: resync, label: 'agents', now: syncNow, observedAt: syncNow - 399 }) !== undefined) throw new Error('Resync surfaced before client-observed 400ms delay')
      if (syncLine({ status: reconnecting, label: 'agents', now: syncNow, observedAt: syncNow - 1999 }) !== undefined) throw new Error('Reconnecting surfaced before client-observed 2s delay')
      const unknownReason = { _tag: 'Stale', reason: { _tag: 'Unknown' } } as const
      const immediate = syncLine({ status: unknownReason, label: 'agents', now: syncNow, observedAt: syncNow })
      if (immediate?.text !== 'Stale · observed 0s ago' || immediate.announce !== 'Stale') throw new Error('Unknown stale reason must show immediately without an invented cause')
      const knownLastLive = syncLine({ status: { ...unknownReason, lastLiveAt: syncNow - 3000 }, label: 'agents', now: syncNow, observedAt: syncNow })
      if (knownLastLive?.text !== 'Stale · 3s since last live') throw new Error('Unknown cause must preserve an observed last-live age')
      const local = { _tag: 'Failed', cause: { _tag: 'Local', kind: 'subscription-limit' } } as const
      if (syncLine({ status: local, label: 'agents', now: syncNow, observedAt: syncNow })?.text !== "Couldn't load agents: too many active subscriptions; close an unused pane and retry") throw new Error('Absent cap must not be invented')
      if (syncLine({ status: { ...local, cause: { ...local.cause, detail: { cap: 0 } } }, label: 'agents', now: syncNow, observedAt: syncNow })?.text !== "Couldn't load agents: too many active subscriptions (cap 0); close an unused pane and retry") throw new Error('A reported zero cap is a known diagnostic fact')
      const serverLimit = { _tag: 'Failed', cause: { _tag: 'Server', code: 'subscription-limit', message: 'Raw server diagnostic' } } as const
      if (syncLine({ status: serverLimit, label: 'agents', now: syncNow, observedAt: syncNow })?.text !== syncLine({ status: local, label: 'agents', now: syncNow, observedAt: syncNow })?.text) throw new Error('Server subscription limit must use the same plain vocabulary as Local')
      for (const status of [local, serverLimit]) {
        if (syncLine({ status, label: 'usage', now: syncNow, observedAt: syncNow }) !== undefined) throw new Error('HTTP Usage reads do not consume subscription slots')
      }
      const live = { _tag: 'Live', since: syncNow } as const
      if (syncLine({ status: live, label: 'agents', now: syncNow, observedAt: syncNow }) !== undefined) throw new Error('A missing socket fact must not turn a resource into a socket status')
      if (syncLine({ status: live, label: 'socket', socket: true, now: syncNow, observedAt: syncNow })?.text !== 'Connected') throw new Error('Absent gateway must not be fabricated')
      const observed = observeSyncStatus(undefined, requested, syncNow)
      const same = observeSyncStatus(observed, requested, syncNow + 1000)
      if (same.observedAt !== observed.observedAt) throw new Error('Same status reset client transition clock')
      const changed = observeSyncStatus(same, resync, syncNow + 2000)
      if (changed.observedAt !== syncNow + 2000) throw new Error('Status transition failed to reset client transition clock')
    } finally { observer.disconnect() }
  },
}
export const TransitionSequenceLight: Story = { args: { scheme: 'light' }, play: TransitionSequence.play }
const styles = stylex.create({
  root: { height: '100vh', overflow: 'auto', boxSizing: 'border-box', display: 'flex', flexDirection: 'column', gap: s.md, padding: s.md, backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading }, allRoot: { height: '100vh', overflow: 'auto', boxSizing: 'border-box', padding: s.md, backgroundColor: surface.canvas, color: ink.fg, fontFamily: t.fontSans, fontSize: t.metaSize, lineHeight: t.metaLeading }, title: { margin: 0, fontSize: t.headingSize, lineHeight: t.headingLeading }, heading: { display: 'flex', alignItems: 'center', gap: s.lg, minHeight: g.band, flexShrink: 0 },
  stateName: { width: g.tooltipMax, minWidth: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' },
  surface: { borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.border, borderRadius: r.sm, minWidth: 0, flexShrink: 0 }, band: { height: g.band, display: 'flex', alignItems: 'center', gap: s.lg, paddingInline: s.lg, boxSizing: 'border-box', minWidth: 0 }, surfaceTitle: { width: g.statusWord, flexShrink: 0, textTransform: 'capitalize' }, slot: { flex: '1 1 0', minWidth: 0 }, retained: { height: g.footer, display: 'flex', alignItems: 'center', paddingInline: s.lg, color: ink.fgMuted, backgroundColor: surface.washSubtle }, statusBar: { height: g.footer, display: 'flex', alignItems: 'center', paddingInline: s.lg, borderTopWidth: g.hairline, borderTopStyle: 'solid', borderTopColor: border.border, flexShrink: 0 },
  button: { minHeight: g.controlMd, paddingInline: s.md, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.borderStrong, backgroundColor: surface.controlFill, color: ink.fg, borderRadius: r.sm, fontFamily: t.fontSans, fontSize: t.metaSize, cursor: 'pointer', ':focus-visible': { outlineWidth: g.focusRing, outlineStyle: 'solid', outlineColor: accent.primary } }, schemes: { display: 'grid', gridTemplateColumns: 'repeat(2, minmax(0, 1fr))', gap: s.md, marginTop: s.md }, themeColumn: { minWidth: 0, backgroundColor: surface.canvas, color: ink.fg, padding: s.md, borderWidth: g.hairline, borderStyle: 'solid', borderColor: border.border, borderRadius: r.sm }, observation: { height: g.controlLg, display: 'flex', alignItems: 'center', gap: s.md, minWidth: 0 },
  stateLabel: { width: g.fileMax, flexShrink: 0, overflow: 'hidden', whiteSpace: 'nowrap', textOverflow: 'ellipsis' },
})
