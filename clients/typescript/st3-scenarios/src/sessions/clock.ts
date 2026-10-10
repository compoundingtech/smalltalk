/** Same pinned instant as fractal-web's fixtures/world.ts; no live clock enters the corpus. */
export const WORLD_NOW_ISO = '2026-10-01T15:30:00Z'
export const worldNow = Date.parse(WORLD_NOW_ISO)
export const minutesAgo = (minutes: number): string =>
  new Date(worldNow - Math.round(minutes * 60_000)).toISOString().replace('.000Z', 'Z')
