let latestStamp = 0
/** A wall-clock position stamp remains strictly increasing within this browser context. */
export function nextViewportStamp() {
  latestStamp = Math.max(Date.now(), latestStamp + 1)
  return latestStamp
}
/** Hydrated clocks advance subsequent local changes without touching any mounted viewport. */
export function observeViewportStamp(stamp: number) { latestStamp = Math.max(latestStamp, stamp) }
