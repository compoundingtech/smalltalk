import type { Slice, SliceKind } from '../slice.ts'

/** Fully regenerated wire data has no inherited contamination declarations. */
export const cleared = <K extends SliceKind>(slice: Slice<K>, state: Slice<K>['state']): Slice<K> => {
  const { unknown: _unknown, ...clean } = slice
  return { ...clean, decode: 'strict', loading: false, state, timeline: [] }
}
