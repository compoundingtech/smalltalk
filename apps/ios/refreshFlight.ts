// A credential or gateway change starts a new projection generation. A slow
// refresh from the prior generation must neither block nor clear the new one.
export class RefreshFlight {
  private activeGeneration: number | null = null;

  start(generation: number): boolean {
    if (this.activeGeneration === generation) return false;
    this.activeGeneration = generation;
    return true;
  }

  finish(generation: number): void {
    if (this.activeGeneration === generation) this.activeGeneration = null;
  }

  isCurrent(generation: number): boolean {
    return this.activeGeneration === generation;
  }
}

// A busy fleet emits projection events continuously, and every full reload runs status scans on
// the daemon. Reload for events at most once per interval; returns how long to wait first.
export function coalescedRefreshDelay(lastRefreshAt: number, now: number, minIntervalMs: number): number {
  return lastRefreshAt ? Math.max(0, lastRefreshAt + minIntervalMs - now) : 0;
}
