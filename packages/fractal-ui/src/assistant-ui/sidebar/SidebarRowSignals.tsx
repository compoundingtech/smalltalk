import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { spaceVars as s, textVars as ink, typeVars as t } from '../composition-tokens.stylex'

type Line1Row = {
  row: HTMLElement
  title: HTMLElement
  time: HTMLElement
  content: HTMLElement | null
  drops: HTMLElement[]
  count: HTMLElement | null
  trailing: HTMLElement | null
  floor: number
  gap: number
  nextDrop: number
  naturalTime: number
}
type Line1Cohort = { scope: HTMLElement; rows: Line1Row[]; widest: number; widestSignals: number }
const pendingLine1Rows = new Set<HTMLElement>()
const pendingSignalRows = new Set<HTMLElement>()
const line1Changes = new Map<HTMLElement, MutationObserver>()
let fitFrame: number | undefined

const writeStyle = (element: HTMLElement, property: string, value: string) => {
  if (element.style.getPropertyValue(property) !== value) {
    if (value === '') element.style.removeProperty(property)
    else element.style.setProperty(property, value)
  }
}
function flushFits() {
  fitFrame = undefined
  const scopes = new Set<HTMLElement>()
  for (const row of pendingLine1Rows) if (row.isConnected) scopes.add(row.closest<HTMLElement>('[data-testid="agent-sidebar"]') ?? row.closest<HTMLElement>('[data-frame-rows]') ?? row.closest<HTMLElement>('[data-explore-variant]') ?? row.parentElement!)
  pendingLine1Rows.clear()
  fitLine1(scopes)
  for (const scope of scopes) for (const signals of scope.querySelectorAll<HTMLElement>('[data-row-signals="measured"]')) pendingSignalRows.add(signals)
  const signals = [...pendingSignalRows].filter(element => element.isConnected)
  pendingSignalRows.clear()
  fitSignals(signals)
  // Our style writes must not enqueue a redundant line-1 pass through each row's observer.
  for (const changes of line1Changes.values()) changes.takeRecords()
}
function cancelEmptyFrame() {
  if (pendingLine1Rows.size === 0 && pendingSignalRows.size === 0 && fitFrame !== undefined) {
    cancelAnimationFrame(fitFrame)
    fitFrame = undefined
  }
}

/** Line 1 keeps a truncated title at least sixty percent of the row. Metric and trailing-signal tracks share their widest intrinsic content width per cohort, without measuring the already allocated grid track. Drop token and subagent counts before scope text; preserve the chevron and the distinct unread and needs marks with at least six pixels after the time. Restore, read every cohort, then write every cohort; each drop step has the same read/write partition. */
function fitLine1(scopes: Set<HTMLElement>) {
  const cohorts: Line1Cohort[] = []
  for (const scope of scopes) {
    const rows: Line1Row[] = []
    for (const row of scope.querySelectorAll<HTMLElement>('[data-testid="taste-agent-row"]')) {
      const title = row.querySelector<HTMLElement>('[data-row-column="title-text"]')
      const time = row.querySelector<HTMLElement>('[data-row-column="time"]')
      if (title === null || time === null) continue
      const drops = ['[data-row-column="time"] [data-line1-drop="tokens"]', '[data-row-column="children"] [data-line1-drop="subs"]', '[data-row-column="time"] [data-line1-drop="scope"]']
        .map(selector => row.querySelector<HTMLElement>(selector)).filter((drop): drop is HTMLElement => drop !== null)
      rows.push({ row, title, time, content: time.firstElementChild as HTMLElement | null, drops, count: row.querySelector<HTMLElement>('[data-row-column="children"] [data-line1-drop="subs"]'), trailing: row.querySelector<HTMLElement>('[data-row-trailing-signals]'), floor: 0, gap: 0, nextDrop: 0, naturalTime: 0 })
    }
    cohorts.push({ scope, rows, widest: 0, widestSignals: 0 })
  }
  for (const cohort of cohorts) for (const row of cohort.rows) for (const drop of row.drops) writeStyle(drop, 'display', '')
  // Row geometry is read after the restore writes, inside the first shared read phase.
  for (const cohort of cohorts) for (const row of cohort.rows) {
    row.floor = row.row.clientWidth * 0.6
    row.gap = Number.parseFloat(getComputedStyle(row.row).columnGap) || 0
  }
  measureAndApplyTracks(cohorts)
  // At most the existing token/subagent/scope fields can drop. Decisions are made before any row writes.
  for (let step = 0; step < 3; step++) {
    const drops: HTMLElement[] = []
    for (const cohort of cohorts) for (const row of cohort.rows) {
      if (row.nextDrop === row.drops.length) continue
      const bounds = row.content?.getBoundingClientRect()
      const count = row.count?.getBoundingClientRect()
      const overflow = bounds !== undefined && (bounds.width > row.time.clientWidth + 0.5 || (count !== undefined && count.width > 0 && count.right + row.gap > bounds.left + 0.5))
      if (row.title.clientWidth < row.floor || overflow) drops.push(row.drops[row.nextDrop++]!)
    }
    if (drops.length === 0) break
    for (const drop of drops) writeStyle(drop, 'display', 'none')
    measureAndApplyTracks(cohorts)
  }
  restoreLine1Fields(cohorts)
}

/** A simultaneous drop can shrink the cohort track enough to make an earlier drop unnecessary. Try higher-priority fields
 * first against the settled track, measuring all candidates before writing. Collision pruning reaches a fixed point using
 * only those measurements: a rejected widest candidate must not lend its unpublished track width to another restoration. */
function restoreLine1Fields(cohorts: Line1Cohort[]) {
  for (const kind of ['scope', 'subs', 'tokens']) {
    const candidates = new Map<Line1Row, HTMLElement>()
    for (const cohort of cohorts) for (const row of cohort.rows) {
      const field = row.drops.find(drop => drop.dataset.line1Drop === kind && drop.style.display === 'none')
      if (field !== undefined) candidates.set(row, field)
    }
    if (candidates.size === 0) continue
    for (const field of candidates.values()) writeStyle(field, 'display', '')
    const decisions: { row: Line1Row; field: HTMLElement; width: number; keep: boolean }[] = []
    for (const cohort of cohorts) {
      const trial = []
      let ceiling = Number.POSITIVE_INFINITY
      for (const row of cohort.rows) {
        const field = candidates.get(row)
        const bounds = row.content?.getBoundingClientRect()
        const count = row.count?.getBoundingClientRect()
        const capacity = row.time.clientWidth + row.title.clientWidth - row.floor
        const overlap = bounds !== undefined && count !== undefined && count.width > 0 ? count.right + row.gap - bounds.left - 0.5 : Number.NEGATIVE_INFINITY
        const width = row.content === null ? 0 : Math.max(row.content.scrollWidth, bounds!.width)
        // Rows with no optional fields left cannot meet the title floor at every cohort width (e.g. a persistent chevron).
        // They must not prevent another row retaining a field that fits its own floor and does not collide.
        const hasRetainedField = row.drops.some(drop => drop !== field && drop.style.display !== 'none')
        if (hasRetainedField && field?.dataset.line1Drop !== 'subs') ceiling = Math.min(ceiling, capacity)
        if (field !== undefined) trial.push({ row, field, width, capacity, overlap, track: row.time.clientWidth, blocked: false })
      }
      let target: number
      let rejected: boolean
      do {
        target = Math.ceil(cohort.widest)
        for (const candidate of trial) {
          if (!candidate.blocked && Math.ceil(candidate.width) <= Math.min(ceiling, candidate.capacity)) target = Math.max(target, Math.ceil(candidate.width))
        }
        rejected = false
        for (const candidate of trial) if (!candidate.blocked && candidate.overlap > target - candidate.track) {
          candidate.blocked = true
          rejected = true
        }
      } while (rejected) // Each iteration rejects at least one candidate; no further DOM reads or layouts.
      for (const candidate of trial) decisions.push({
        row: candidate.row, field: candidate.field, width: candidate.width,
        keep: !candidate.blocked && Math.ceil(candidate.width) <= target && target <= Math.min(candidate.capacity, ceiling),
      })
    }
    for (const { row, field, width, keep } of decisions) {
      if (!keep) writeStyle(field, 'display', 'none')
      else row.naturalTime = width
    }
    // No geometry reads are needed to publish the accepted widths and track.
    for (const cohort of cohorts) {
      cohort.widest = 0
      for (const row of cohort.rows) {
        cohort.widest = Math.max(cohort.widest, row.naturalTime)
        const value = String(row.naturalTime)
        if (row.row.dataset.rowTimeNatural !== value) row.row.dataset.rowTimeNatural = value
      }
      if (cohort.widest > 0) writeStyle(cohort.scope, '--sidebar-metric-track', `${Math.ceil(cohort.widest)}px`)
    }
  }
}
function measureAndApplyTracks(cohorts: Line1Cohort[]) {
  for (const cohort of cohorts) {
    cohort.widest = 0
    cohort.widestSignals = 0
    for (const row of cohort.rows) {
      row.naturalTime = row.content === null ? 0 : Math.max(row.content.scrollWidth, row.content.getBoundingClientRect().width)
      cohort.widest = Math.max(cohort.widest, row.naturalTime)
      if (row.trailing !== null) {
        const gap = Number.parseFloat(getComputedStyle(row.trailing).columnGap) || 0
        let width = 0, count = 0
        for (const signal of row.trailing.children) {
          const naturalWidth = signal.getBoundingClientRect().width
          if (naturalWidth > 0) { width += naturalWidth + (count > 0 ? gap : 0); count++ }
        }
        cohort.widestSignals = Math.max(cohort.widestSignals, width)
      }
    }
  }
  for (const cohort of cohorts) {
    for (const row of cohort.rows) {
      const value = String(row.naturalTime)
      if (row.row.dataset.rowTimeNatural !== value) row.row.dataset.rowTimeNatural = value
    }
    if (cohort.widest > 0) writeStyle(cohort.scope, '--sidebar-metric-track', `${Math.ceil(cohort.widest)}px`)
    writeStyle(cohort.scope, '--sidebar-signal-track', cohort.widestSignals > 0 ? `${Math.ceil(cohort.widestSignals)}px` : '')
  }
}

function fitSignals(elements: HTMLElement[]) {
  const rows = elements.map(element => ({ element, gap: 0, available: 0, fields: [...element.children].map((child, index) => {
    const field = child as HTMLElement
    const value = field.firstElementChild?.getAttribute('data-row-retention')
    const rank = value === null || value === '' ? Number.NaN : Number(value)
    return { field, index, rank: Number.isFinite(rank) ? rank : 0, currentWork: field.querySelector('[data-row-field="current-work"]') !== null, width: 0, minimum: 0 }
  }) }))
  // One preparation write phase for all rows; no geometry read is interleaved with a field write.
  for (const row of rows) for (const { field } of row.fields) {
    writeStyle(field, 'position', 'absolute')
    writeStyle(field, 'visibility', 'hidden')
    writeStyle(field, 'max-width', 'none')
    writeStyle(field, 'width', 'max-content')
    writeStyle(field, 'flex-shrink', '0')
  }
  for (const row of rows) {
    row.gap = Number.parseFloat(getComputedStyle(row.element).columnGap) || 0
    row.available = row.element.clientWidth
    for (const measurement of row.fields) {
      const { field } = measurement
      measurement.width = field.getBoundingClientRect().width
      // Only free-text current work may shorten, with twelve measured characters retained.
      const walker = document.createTreeWalker(field, NodeFilter.SHOW_TEXT)
      const range = document.createRange()
      let remaining = 12, width = 0
      for (let node = walker.nextNode(); node && remaining > 0; node = walker.nextNode()) {
        const count = Math.min(remaining, node.textContent?.length ?? 0)
        if (!count) continue
        range.setStart(node, 0); range.setEnd(node, count)
        width += range.getBoundingClientRect().width
        remaining -= count
      }
      measurement.minimum = width + (field.textContent && field.textContent.length > 12 ? Number.parseFloat(getComputedStyle(field).fontSize) : 0)
    }
  }
  for (const row of rows) {
    const order = row.fields.sort((a, b) => b.rank - a.rank || b.index - a.index)
    let available = row.available, kept = 0, shortened = false
    for (const { field, width, minimum, currentWork } of order) {
      const space = available - (kept ? row.gap : 0)
      const complete = width <= space + 0.5
      const keep = !shortened && (complete || (currentWork && minimum <= space))
      field.dataset.rowDropped = String(!keep)
      field.dataset.rowShortened = String(keep && !complete)
      field.setAttribute('aria-hidden', String(!keep))
      if (!keep) continue
      writeStyle(field, 'position', 'static')
      writeStyle(field, 'visibility', 'visible')
      writeStyle(field, 'max-width', `${Math.max(0, space)}px`)
      available = space - width
      kept++
      if (!complete) shortened = true
    }
  }
}

/** One frame batches every mounted row rather than forcing layout from each ref callback. Returns the ref cleanup. */
export function attachSidebarLine1Fit(row: HTMLElement | null) {
  if (row === null) return
  let active = true
  const schedule = () => { if (active) { pendingLine1Rows.add(row); fitFrame ??= requestAnimationFrame(flushFits) } }
  const resize = new ResizeObserver(schedule)
  const changes = new MutationObserver(schedule)
  line1Changes.set(row, changes)
  resize.observe(row)
  for (const element of row.querySelectorAll('[data-row-column="title-text"], [data-row-column="time"]')) resize.observe(element)
  changes.observe(row, { childList: true, characterData: true, subtree: true, attributes: true, attributeFilter: ['style'] })
  schedule()
  document.fonts?.ready.then(schedule)
  return () => { active = false; resize.disconnect(); changes.disconnect(); line1Changes.delete(row); pendingLine1Rows.delete(row); cancelEmptyFrame() }
}

export function SidebarRowSignals({ children }: { readonly children: React.ReactNode }) {
  const attach = React.useCallback((element: HTMLSpanElement | null) => {
    if (!element) return
    const schedule = () => { pendingSignalRows.add(element); fitFrame ??= requestAnimationFrame(flushFits) }
    const resize = new ResizeObserver(schedule)
    const changes = new MutationObserver(schedule)
    resize.observe(element)
    changes.observe(element, { childList: true, characterData: true, subtree: true })
    schedule()
    return () => { resize.disconnect(); changes.disconnect(); pendingSignalRows.delete(element); cancelEmptyFrame() }
  }, [])
  return <span ref={attach} data-row-column="subtitle" data-row-signals="measured" {...stylex.props(styles.signals)}>{React.Children.toArray(children).map((child, index) => <span key={index} data-row-priority={index} {...stylex.props(styles.field)}>{child}</span>)}</span>
}
const styles = stylex.create({
  signals: { position: 'relative', gridColumn: '1 / -1', gridRow: '2', display: 'flex', alignItems: 'center', gap: s.sm, minWidth: 0, overflow: 'hidden', color: ink.sidebarFgMuted, fontSize: t.denseSize, lineHeight: t.metaLeading, fontVariantNumeric: 'tabular-nums', whiteSpace: 'nowrap' },
  field: { display: 'block', flexShrink: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' },
})
