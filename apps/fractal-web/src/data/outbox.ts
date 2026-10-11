import type { ConversationItem, TextItem } from '../conversation/model.ts'

const timestampOf = (item: ConversationItem): number => item.at === undefined ? NaN : Date.parse(item.at)

/** Merge own sends without reordering the authoritative transcript or submission order. */
export const mergeOutboxItems = (
  projected: readonly ConversationItem[],
  pending: Iterable<{ readonly item: TextItem }>,
): readonly ConversationItem[] => {
  const sends: TextItem[] = []
  const thresholds: number[] = []
  let latest = -Infinity
  for (const send of pending) {
    sends.push(send.item)
    const time = timestampOf(send.item)
    // The original insertion rule leaves an unorderable send at the end.
    latest = Math.max(latest, Number.isNaN(time) ? Infinity : time)
    thresholds.push(latest)
  }
  if (sends.length === 0) return projected
  // Prefix-max submission times make insertion boundaries monotone, even if the
  // clock moves backward. One shared reverse cursor visits each source row once.
  const boundaries: number[] = new Array(sends.length)
  let to = projected.length
  let time = to > 0 ? timestampOf(projected[to - 1]!) : NaN
  for (let index = sends.length - 1; index >= 0; index -= 1) {
    while (to > 0 && time > thresholds[index]!) {
      to -= 1
      if (to > 0) time = timestampOf(projected[to - 1]!)
    }
    boundaries[index] = to
  }
  const merged: ConversationItem[] = []
  let from = 0
  for (let index = 0; index < sends.length; index += 1) {
    while (from < boundaries[index]!) merged.push(projected[from++]!)
    merged.push(sends[index]!)
  }
  while (from < projected.length) merged.push(projected[from++]!)
  return merged
}
