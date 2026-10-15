/** Inferred activity state for the interval between consecutive request timestamps. */
export type ActivitySegmentState = 'working' | 'waiting' | 'idle'
/** Epoch-millisecond interval of one session's inferred working, waiting, or idle activity. */
export interface ActivitySegment {
  readonly from: number
  readonly to: number
  readonly state: ActivitySegmentState
}
