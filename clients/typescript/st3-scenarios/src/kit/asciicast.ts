/** asciicast v2 embedded as JSON; `started_at_ms` places it in world time (an offset, never rebased). */
export interface Asciicast {
  readonly header: { readonly version: 2; readonly width: number; readonly height: number; readonly title?: string }
  readonly started_at_ms: number
  /** `[seconds from start, "o", data]` */
  readonly events: [number, 'o', string][]
}

/** NDJSON `.cast` text for asciinema-compatible players. */
export const toCastText = (cast: Asciicast): string =>
  [JSON.stringify(cast.header), ...cast.events.map((event) => JSON.stringify(event))].join('\n') + '\n'
