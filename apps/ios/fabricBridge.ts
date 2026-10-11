// The carrier's ports on this build's native fabric bridge. Nothing here runs unless the person
// chose fabric and the build links the bridge.
import type { CarrierPorts, FabricPath } from './carrier';
import { nativeRefusedAfter } from './fabricSession';
import { dialFabric, fabricAvailable, fabricIdentity, fabricStats, stopFabric } from './modules/st-fabric';

export { fabricAvailable, fabricIdentity };

function newestAttempt(stats: Record<string, unknown>): number {
  const attempts = Array.isArray(stats.attempts) ? stats.attempts : [];
  return Math.max(0, ...attempts.map(attempt => (attempt && typeof attempt.id === 'number' ? attempt.id : 0)));
}

export function nativeCarrierPorts(): CarrierPorts {
  // Attempts before this dial are old news: only a refusal after it concerns this connection.
  let floor = 0;
  return {
    dial: async target => {
      try { floor = newestAttempt(await fabricStats()); } catch { /* the first dial has no earlier attempts */ }
      const result = await dialFabric(target.node, target.service, target.address, 'auto');
      return { url: result.url };
    },
    stop: stopFabric,
    refused: async () => nativeRefusedAfter(await fabricStats(), floor),
    now: () => Date.now(),
    wait: ms => new Promise(resolve => setTimeout(resolve, ms)),
  };
}

/** The path the newest successful connection took, read on demand for the connection screen. */
export async function fabricPathInUse(): Promise<FabricPath> {
  try {
    const stats = await fabricStats();
    const attempts = Array.isArray(stats.attempts) ? stats.attempts : [];
    for (let index = attempts.length - 1; index >= 0; index--) {
      const attempt = attempts[index];
      if (attempt?.result === 'connected') {
        return attempt.selectedPath === 'direct' ? 'direct' : attempt.selectedPath === 'relay' ? 'relay' : 'unknown';
      }
    }
  } catch { /* no measurement is not a failure */ }
  return 'unknown';
}
