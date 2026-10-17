export const money = new Intl.NumberFormat('en-US', { style: 'currency', currency: 'USD', maximumFractionDigits: 2 })
export const compactNumber = new Intl.NumberFormat('en-US', { notation: 'compact', maximumFractionDigits: 1 })
export const clock = new Intl.DateTimeFormat('en-GB', { hour: '2-digit', minute: '2-digit', timeZone: 'UTC' })

export const CHART_WIDTH = 320
export const CHART_HEIGHT = 112
export const CHART_INSET = 8
export const baseline = CHART_HEIGHT - CHART_INSET

export interface Point { readonly x: number; readonly y: number }
export const linePath = (points: ReadonlyArray<Point>): string => points.map((point, index) => `${index === 0 ? 'M' : 'L'}${point.x.toFixed(1)},${point.y.toFixed(1)}`).join(' ')
export const areaPath = (upper: ReadonlyArray<Point>, lower: ReadonlyArray<Point>): string => upper.length === 0 ? '' : `${linePath(upper)} ${linePath(lower.toReversed()).replace(/^M/, 'L')} Z`
export const xFor = (ts: number, start: number, end: number): number => end === start ? CHART_WIDTH / 2 : CHART_INSET + ((ts - start) / (end - start)) * (CHART_WIDTH - CHART_INSET * 2)
export const yFor = (value: number, maximum: number): number => baseline - (value / Math.max(0.000001, maximum)) * (CHART_HEIGHT - CHART_INSET * 2)

