import * as React from 'react'
import { composeStory, type Args, type Meta } from '@storybook/react'
import { renderToStaticMarkup } from 'react-dom/server'

import { ANCHOR_MS, catalog, DEFAULT_WORLD, foldSlice, manualClock, syncStatusAt, type SliceKind, type World } from '../index.ts'
import { createReadTracker, type ServedSlices } from '../react/index.ts'
import { resolveScenarioWorld, scenarioGlobalTypes, withScenario, type ScenarioParameter } from './index.ts'

export type ScenarioCheck = 1 | 2 | 3 | 4 | 5
export interface ScenarioCheckFailure {
  readonly _tag: 'Reads' | 'DataDependence' | 'WorldSwitch' | 'PinnedWorld' | 'UrlState'
  readonly check: ScenarioCheck
  readonly message: string
  readonly slice?: SliceKind
  readonly variants?: readonly [string, string]
}

export interface ScenarioStoryCheckResult {
  readonly _tag: 'Passed' | 'Failed'
  readonly failures: readonly ScenarioCheckFailure[]
  readonly checks: readonly { readonly check: ScenarioCheck; readonly _tag: 'Passed' | 'Failed' | 'Skipped'; readonly reason?: string }[]
  readonly reads: ReadonlySet<SliceKind>
}

export interface ScenarioStoryCheckOptions<TArgs extends Args = Args> {
  readonly scenario?: string
  readonly scenarioNow?: number | string | Date
  readonly args?: Partial<TArgs>
  readonly atMs?: number
  readonly tracker?: ServedSlices
  readonly contrastPairs?: Partial<Record<SliceKind, readonly (readonly [string, string])[]>>
  readonly assert?: boolean
}

export class ScenarioStoryCheckError extends Error {
  constructor(readonly result: ScenarioStoryCheckResult) {
    super(result.failures.map(({ check, message }) => `check ${check}: ${message}`).join('\n'))
    this.name = 'ScenarioStoryCheckError'
  }
}

const CONTRAST_PAIRS: Record<SliceKind, readonly (readonly [string, string])[]> = {
  roster: [['default', 'one-agent'], ['default', 'empty']],
  details: [['default', 'empty']],
  attention: [['default', 'none']],
  conversation: [['default', 'empty']],
  terminal: [['default', 'none']],
  sync: [['live', 'socket-dropped']],
}

const escapeHtml = (text: string): string => text.replace(/[&<>"']/g, (char) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#x27;' })[char]!)
// Decorator diagnostics are not evidence that the component consumed different data.
const contentMarkup = (markup: string): string => markup.replace(/ data-scenario(?:-slices)?="[^"]*"/g, '')
const texts = (value: unknown): string[] => {
  if (typeof value !== 'object' || value === null) return []
  if (Array.isArray(value)) return value.flatMap(texts)
  return Object.entries(value).flatMap(([key, item]) => key === 'text' && typeof item === 'string' ? [item.slice(0, 24)] : texts(item))
}

const markers = (world: World, kind: SliceKind, atMs: number): string[] => {
  if (world.slices[kind].loading) return []
  let values: (string | null | undefined)[]
  switch (kind) {
    case 'roster': values = foldSlice(world.slices.roster, atMs).state.agents.map(({ name }) => name); break
    case 'details': {
      const state = foldSlice(world.slices.details, atMs).state
      values = [...state.missions, ...state.work].map(({ title }) => title)
      break
    }
    case 'attention': {
      const state = foldSlice(world.slices.attention, atMs).state
      values = [...state.attention, ...state.messages].map(({ title }) => title)
      break
    }
    case 'conversation': values = foldSlice(world.slices.conversation, atMs).state.threads.flatMap(({ items }) => items.flatMap((entry) => entry.type === 'content' ? texts(entry.body) : [])); break
    case 'terminal': values = foldSlice(world.slices.terminal, atMs).state.terminals.flatMap(({ screens }) => screens.at(-1)?.screen.lines.map(({ text }) => text) ?? []); break
    case 'sync': values = []; break
  }
  return values.filter((value): value is string => typeof value === 'string' && value.trim().length > 0)
}

const wireState = (world: World, kind: SliceKind, atMs: number): string => JSON.stringify({
  loading: world.slices[kind].loading,
  state: foldSlice(world.slices[kind], atMs).state,
  ...(kind === 'sync' ? { status: syncStatusAt(world.slices.sync, atMs, world.now) } : {}),
})

// Pair selection follows check 2's visible expectations, not incidental wire metadata.
// Notice-only changes do not distinguish roster rows; sync scripts distinguish renders
// through their projected status, not capabilities or unexecuted future operations.
const visibleState = (world: World, kind: SliceKind, atMs: number): string => JSON.stringify({
  loading: world.slices[kind].loading,
  content: world.slices[kind].loading ? [] : kind === 'sync'
    ? syncStatusAt(world.slices.sync, atMs, world.now)
    : markers(world, kind, atMs).sort(),
})

/** Finds a visible contrast, including sync transitions that occur after the initial snapshot. */
const contrastTime = (left: World, right: World, kind: SliceKind, atMs: number): number => {
  if (kind !== 'sync') return atMs
  const times = [atMs, ...left.slices.sync.state.expected.map(({ at_ms }) => at_ms), ...right.slices.sync.state.expected.map(({ at_ms }) => at_ms)]
  return times.find((at) => at >= atMs && JSON.stringify(syncStatusAt(left.slices.sync, at, left.now)) !== JSON.stringify(syncStatusAt(right.slices.sync, at, right.now))) ?? atMs
}

const contrastFailures = (left: World, right: World, kind: SliceKind, atMs: number, leftMarkup: string, rightMarkup: string): string[] => {
  const leftMarkers = markers(left, kind, atMs)
  const rightMarkers = markers(right, kind, atMs)
  const sides = [
    { label: left.slices[kind].variant, own: leftMarkers, other: rightMarkers, markup: contentMarkup(leftMarkup) },
    { label: right.slices[kind].variant, own: rightMarkers, other: leftMarkers, markup: contentMarkup(rightMarkup) },
  ]
  const failures = sides.flatMap(({ label, own, other, markup }) => {
    const ownTexts = [...new Set(own)]
    const otherTexts = [...new Set(other)]
    const textMarkup = markup.replace(/<[^>]*>/g, '')
    const countFailures = ownTexts.flatMap((marker) => {
      const ownCount = own.filter((value) => value === marker).length
      const otherCount = other.filter((value) => value === marker).length
      if (otherCount === 0 || ownCount === otherCount) return []
      const renderedCount = textMarkup.split(escapeHtml(marker)).length - 1
      return renderedCount === ownCount ? [] : [`${label} renders ${renderedCount} occurrences of ${JSON.stringify(marker)}, expected ${ownCount}`]
    })
    return [
      ...ownTexts.filter((marker) => !textMarkup.includes(escapeHtml(marker))).map((marker) => `${label} does not render expected marker ${JSON.stringify(marker)}`),
      ...otherTexts.filter((marker) => !ownTexts.includes(marker) && textMarkup.includes(escapeHtml(marker))).map((marker) => `${label} still renders the other side's marker ${JSON.stringify(marker)}`),
      ...countFailures,
    ]
  })
  if (contentMarkup(leftMarkup) === contentMarkup(rightMarkup)) failures.push('different wire states render identical content')
  return failures
}

export const scenarioStoryCheck = <TArgs extends Args>(
  story: Parameters<typeof composeStory<TArgs>>[0],
  meta: Meta<TArgs>,
  options: ScenarioStoryCheckOptions<TArgs> = {},
): ScenarioStoryCheckResult => {
  const parameter: ScenarioParameter | undefined = story.parameters?.scenario === undefined
    ? meta.parameters?.scenario
    : { ...meta.parameters?.scenario, ...story.parameters.scenario }
  if (parameter === undefined) throw new Error('scenarioStoryCheck requires parameters.scenario.slices')
  for (const [kind, reason] of Object.entries(parameter.invariant ?? {})) {
    if (typeof reason !== 'string' || reason.trim().length === 0) throw new Error(`scenario invariant ${kind} requires a reason`)
  }
  const now = options.scenarioNow ?? ANCHOR_MS
  const scenario = options.scenario ?? DEFAULT_WORLD
  const baselineArgs = Object.assign({}, meta.args, story.args, options.args)
  const failures: ScenarioCheckFailure[] = []
  const readKinds = new Set<SliceKind>()
  const skipped: Partial<Record<ScenarioCheck, string>> = {}
  const declared = new Set(parameter.slices)
  const source = options.tracker ?? (parameter.tracker === undefined ? undefined : 'served' in parameter.tracker
    ? parameter.tracker
    : { served: parameter.tracker.reads })

  const render = (selected: string, overrides: Args = {}, atMs = options.atMs ?? 0): string => {
    const args = Object.assign({}, baselineArgs, overrides)
    const tracker = createReadTracker(source)
    const globals = { scenario: selected, scenarioNow: now }
    const world = resolveScenarioWorld(parameter, globals, args)
    const clock = manualClock(world.now + atMs)
    const Composed = composeStory(story, meta, {
      decorators: [withScenario],
      globalTypes: scenarioGlobalTypes,
      initialGlobals: globals,
      parameters: { scenarioReadTracker: tracker, scenarioClock: clock },
    })
    const markup = renderToStaticMarkup(React.createElement(Composed, args))
    const reads = tracker.reads()
    reads.forEach((kind) => readKinds.add(kind))
    const missing = parameter.slices.filter((kind) => !reads.has(kind))
    const undeclared = [...reads].filter((kind) => !declared.has(kind))
    if (missing.length > 0 || undeclared.length > 0) {
      const message = `declared/read mismatch: missing [${missing.join(', ')}], undeclared [${undeclared.join(', ')}]`
      if (!failures.some((failure) => failure.check === 1 && failure.message === message)) failures.push({ _tag: 'Reads', check: 1, message })
    }
    return markup
  }

  const initialMarkup = render(scenario)
  const activeKinds = parameter.slices.filter((kind) => parameter.invariant?.[kind] === undefined)
  const baseWorld = resolveScenarioWorld(parameter, { scenario, scenarioNow: now }, baselineArgs)
  let variantPairs = 0
  for (const kind of activeKinds) {
    for (const pair of options.contrastPairs?.[kind] ?? CONTRAST_PAIRS[kind]) {
      if (!pair.every((variant) => baseWorld.available[kind].includes(variant))) continue
      const left = resolveScenarioWorld(parameter, { scenario, scenarioNow: now }, { ...baselineArgs, [kind]: pair[0] })
      const right = resolveScenarioWorld(parameter, { scenario, scenarioNow: now }, { ...baselineArgs, [kind]: pair[1] })
      const atMs = contrastTime(left, right, kind, options.atMs ?? 0)
      if (wireState(left, kind, atMs) === wireState(right, kind, atMs)) continue
      variantPairs++
      const messages = contrastFailures(left, right, kind, atMs, render(scenario, { [kind]: pair[0] }, atMs), render(scenario, { [kind]: pair[1] }, atMs))
      failures.push(...messages.map((message): ScenarioCheckFailure => ({ _tag: 'DataDependence', check: 2, slice: kind, variants: pair, message })))
    }
  }
  if (variantPairs === 0) skipped[2] = 'No non-invariant slice has an available, distinct contrast pair'

  if (parameter.world === undefined) {
    skipped[4] = 'Story does not pin a world'
    const pendingKinds = new Set(activeKinds)
    const leftMarkupByTime = new Map([[options.atMs ?? 0, initialMarkup]])
    // One representative pair per declared slice is enough. Resolve each candidate
    // once, and render only candidates with a contrast under the declared contract.
    for (const entry of catalog) {
      if (pendingKinds.size === 0) break
      if (entry.id === baseWorld.id) continue
      const right = resolveScenarioWorld(parameter, { scenario: entry.id, scenarioNow: now }, baselineArgs)
      const rightMarkupByTime = new Map<number, string>()
      for (const kind of pendingKinds) {
        const atMs = contrastTime(baseWorld, right, kind, options.atMs ?? 0)
        if (wireState(baseWorld, kind, atMs) === wireState(right, kind, atMs)
          || visibleState(baseWorld, kind, atMs) === visibleState(right, kind, atMs)) continue
        pendingKinds.delete(kind)
        const leftMarkup = leftMarkupByTime.get(atMs) ?? render(scenario, {}, atMs)
        const rightMarkup = rightMarkupByTime.get(atMs) ?? render(entry.id, {}, atMs)
        leftMarkupByTime.set(atMs, leftMarkup)
        rightMarkupByTime.set(atMs, rightMarkup)
        const messages = contrastFailures(baseWorld, right, kind, atMs, leftMarkup, rightMarkup)
        failures.push(...messages.map((message): ScenarioCheckFailure => ({ _tag: 'WorldSwitch', check: 3, slice: kind, message: `${baseWorld.id} / ${entry.id}: ${message}` })))
      }
    }
    if (pendingKinds.size === activeKinds.length) skipped[3] = 'Catalog worlds have no visible contrast on the non-invariant declared slices'
  } else {
    skipped[3] = 'Story pins a world'
    const baseline = initialMarkup
    for (const entry of catalog) {
      if (render(entry.id) !== baseline) failures.push({ _tag: 'PinnedWorld', check: 4, message: `toolbar world ${entry.id} changes a pinned story` })
    }
  }

  const urlArgs: Args = Object.fromEntries(parameter.slices.map((kind) => [kind, baseWorld.available[kind].find((variant) => variant !== 'default') ?? 'default']))
  const query = new URLSearchParams({ globals: `scenario:${scenario}`, args: Object.entries(urlArgs).map(([kind, variant]) => `${kind}:${variant}`).join(';') })
  const parsed = new URLSearchParams(`?${query}`)
  const parsedWorld = parsed.get('globals')?.split(':')[1] ?? DEFAULT_WORLD
  const parsedArgs: Args = Object.fromEntries((parsed.get('args') ?? '').split(';').filter(Boolean).map((part) => {
    const separator = part.indexOf(':')
    return [part.slice(0, separator), part.slice(separator + 1)]
  }))
  if (render(parsedWorld, parsedArgs) !== render(scenario, urlArgs)) failures.push({ _tag: 'UrlState', check: 5, message: 'URL globals/args differ from directly supplied state' })

  const result: ScenarioStoryCheckResult = {
    _tag: failures.length === 0 ? 'Passed' : 'Failed',
    failures,
    checks: ([1, 2, 3, 4, 5] as const).map((check) => failures.some((failure) => failure.check === check)
      ? { check, _tag: 'Failed' }
      : skipped[check] === undefined ? { check, _tag: 'Passed' } : { check, _tag: 'Skipped', reason: skipped[check] }),
    reads: readKinds,
  }
  if (options.assert === true && result._tag === 'Failed') throw new ScenarioStoryCheckError(result)
  return result
}
