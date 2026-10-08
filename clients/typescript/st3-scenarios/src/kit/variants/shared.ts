import type { Slice, SliceKind } from '../slice.ts'

export const cleared = <K extends SliceKind>(slice: Slice<K>, state: Slice<K>['state']): Slice<K> => ({ ...slice, state, timeline: [] })
