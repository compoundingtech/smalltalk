import * as React from 'react'
import * as stylex from '@stylexjs/stylex'
import { spaceVars as s, textVars as ink, typeVars as t } from '../composition-tokens.stylex'

/** Line 1 keeps a truncated title at least sixty percent of the row. Metric and trailing-signal tracks share their widest intrinsic content width per cohort, without measuring the already allocated grid track. Drop token and subagent counts before scope text; preserve the chevron and the distinct unread and needs marks with at least six pixels after the time. */
export function attachSidebarLine1Fit(row: HTMLElement | null) {
  if (row === null) return
  let frame: number | undefined
  const applyTrack = () => {
    const scope = row.closest<HTMLElement>('[data-testid="agent-sidebar"]') ?? row.closest<HTMLElement>('[data-frame-rows]') ?? row.closest<HTMLElement>('[data-explore-variant]') ?? row.parentElement!
    let widest = 0
    let widestSignals = 0
    for (const peer of scope.querySelectorAll<HTMLElement>('[data-testid="taste-agent-row"]')) {
      const natural = Number.parseFloat(peer.dataset.rowTimeNatural ?? '')
      if (Number.isFinite(natural)) widest = Math.max(widest, natural)
      const trailing = peer.querySelector<HTMLElement>('[data-row-trailing-signals]')
      if (trailing !== null) {
        const gap = Number.parseFloat(getComputedStyle(trailing).columnGap) || 0
        let width = 0, count = 0
        for (const signal of trailing.children) {
          const naturalWidth = signal.getBoundingClientRect().width
          if (naturalWidth > 0) { width += naturalWidth + (count > 0 ? gap : 0); count++ }
        }
        widestSignals = Math.max(widestSignals, width)
      }
    }
    if (widest > 0) scope.style.setProperty('--sidebar-metric-track', `${Math.ceil(widest)}px`)
    if (widestSignals > 0) scope.style.setProperty('--sidebar-signal-track', `${Math.ceil(widestSignals)}px`)
    else scope.style.removeProperty('--sidebar-signal-track')
  }
  const fit = () => {
    frame = undefined
    const title = row.querySelector<HTMLElement>('[data-row-column="title-text"]')
    const time = row.querySelector<HTMLElement>('[data-row-column="time"]')
    if (title === null || time === null) return
    const measureTime = () => {
      const content = time.firstElementChild as HTMLElement | null
      row.dataset.rowTimeNatural = String(content === null ? 0 : Math.max(content.scrollWidth, content.getBoundingClientRect().width))
    }
    const drops = ['[data-row-column="time"] [data-line1-drop="tokens"]', '[data-row-column="children"] [data-line1-drop="subs"]', '[data-row-column="time"] [data-line1-drop="scope"]']
      .map(selector => row.querySelector<HTMLElement>(selector)).filter((drop): drop is HTMLElement => drop !== null)
    for (const drop of drops) drop.style.display = ''
    measureTime()
    applyTrack()
    const floor = row.clientWidth * 0.6
    const overflow = () => {
      const content = time.firstElementChild
      if (content === null) return false
      const bounds = content.getBoundingClientRect()
      if (bounds.width > time.clientWidth + 0.5) return true
      const count = row.querySelector<HTMLElement>('[data-row-column="children"] [data-line1-drop="subs"]')
      if (count === null || count.getBoundingClientRect().width === 0) return false
      const gap = Number.parseFloat(getComputedStyle(row).columnGap) || 0
      return count.getBoundingClientRect().right + gap > bounds.left + 0.5
    }
    for (const drop of drops) {
      if (title.clientWidth >= floor && !overflow()) break
      drop.style.display = 'none'
      measureTime()
      applyTrack()
    }
    // Restoring and dropping our own fields must not schedule another fit.
    changes.takeRecords()
  }
  const schedule = () => { if (frame === undefined) frame = requestAnimationFrame(fit) }
  const resize = new ResizeObserver(schedule)
  const changes = new MutationObserver(schedule)
  resize.observe(row)
  for (const element of row.querySelectorAll('[data-row-column="title-text"], [data-row-column="time"]')) resize.observe(element)
  changes.observe(row, { childList: true, characterData: true, subtree: true, attributes: true, attributeFilter: ['style'] })
  fit()
  document.fonts?.ready.then(schedule)
  requestAnimationFrame(() => requestAnimationFrame(schedule))
  return () => { resize.disconnect(); changes.disconnect(); if (frame !== undefined) cancelAnimationFrame(frame) }
}

 export function SidebarRowSignals({ children }: { readonly children: React.ReactNode }) {
  const attach = React.useCallback((element: HTMLSpanElement | null) => {
    if (!element) return
    let frame: number | undefined
    const fit = () => {
      frame = undefined
      const fields = [...element.children] as HTMLElement[]
      const gap = Number.parseFloat(getComputedStyle(element).columnGap) || 0
      const widths = new Map<HTMLElement, number>()
      const minimum = new Map<HTMLElement, number>()
      for (const field of fields) {
        field.style.position = 'absolute'
        field.style.visibility = 'hidden'
        field.style.maxWidth = 'none'
        field.style.width = 'max-content'
        field.style.flexShrink = '0'
        widths.set(field, field.getBoundingClientRect().width)
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
        minimum.set(field, width + (field.textContent && field.textContent.length > 12 ? Number.parseFloat(getComputedStyle(field).fontSize) : 0))
      }
      const retention = (field: HTMLElement) => {
        const value = field.firstElementChild?.getAttribute('data-row-retention')
        const rank = value === null || value === '' ? Number.NaN : Number(value)
        return Number.isFinite(rank) ? rank : 0
      }
      const order = fields.map((field, index) => ({ field, index })).sort((a, b) => retention(b.field) - retention(a.field) || b.index - a.index)
      let available = element.clientWidth, kept = 0, shortened = false
      for (const { field } of order) {
        const space = available - (kept ? gap : 0)
        const complete = widths.get(field)! <= space + 0.5
        const keep = !shortened && (complete || (field.querySelector('[data-row-field="current-work"]') !== null && minimum.get(field)! <= space))
        field.dataset.rowDropped = String(!keep)
        field.dataset.rowShortened = String(keep && !complete)
        field.setAttribute('aria-hidden', String(!keep))
        if (!keep) continue
        field.style.position = 'static'
        field.style.visibility = 'visible'
        field.style.maxWidth = `${Math.max(0, space)}px`
        available = space - widths.get(field)!
        kept++
        if (!complete) shortened = true
      }
    }
    const schedule = () => { if (frame === undefined) frame = requestAnimationFrame(fit) }
    const resize = new ResizeObserver(schedule)
    const changes = new MutationObserver(schedule)
    resize.observe(element)
    changes.observe(element, { childList: true, characterData: true, subtree: true })
    fit()
    return () => { resize.disconnect(); changes.disconnect(); if (frame !== undefined) cancelAnimationFrame(frame) }
  }, [])
  return <span ref={attach} data-row-column="subtitle" data-row-signals="measured" {...stylex.props(styles.signals)}>{React.Children.toArray(children).map((child, index) => <span key={index} data-row-priority={index} {...stylex.props(styles.field)}>{child}</span>)}</span>
}
const styles = stylex.create({
  signals: { position: 'relative', gridColumn: '1 / -1', gridRow: '2', display: 'flex', alignItems: 'center', gap: s.sm, minWidth: 0, overflow: 'hidden', color: ink.sidebarFgMuted, fontSize: t.denseSize, lineHeight: t.metaLeading, fontVariantNumeric: 'tabular-nums', whiteSpace: 'nowrap' },
  field: { display: 'block', flexShrink: 0, overflow: 'hidden', textOverflow: 'ellipsis', whiteSpace: 'nowrap' },
})
