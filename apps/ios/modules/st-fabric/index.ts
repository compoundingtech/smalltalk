import type { FabricPathMode } from '../../fabricProof';
import { requireOptionalNativeModule } from 'expo';

export type FabricDial = { handle?: string; url: string; node: string; fabricVersion: string; irohVersion: string };
type Native = {
  available(): Promise<boolean>;
  identity(): Promise<{ node: string }>;
  dial(node: string, service: string, address: string | null, mode: FabricPathMode): Promise<FabricDial>;
  stats(): Promise<Record<string, unknown>>;
  openTarget?(node: string, service: string, address: string | null, mode: FabricPathMode): Promise<FabricDial & { handle: string }>;
  closeTarget?(handle: string): Promise<void>;
  targetStats?(handle: string): Promise<Record<string, unknown>>;
  stop(): Promise<void>;
};
const native = requireOptionalNativeModule<Native>('StFabric');

function available(): Native {
  if (!native) throw new Error('This build has no fabric bridge');
  return native;
}

/**
 * Whether this build links the fabric bridge (a build made with `ST3_FABRIC=1`). A build without it
 * has a disabled stand-in that refuses every call, and the app offers no fabric carrier.
 */
export async function fabricAvailable(): Promise<boolean> {
  if (!native) return false;
  try { return (await native.available()) === true; } catch { return false; }
}

export async function fabricIdentity(): Promise<string> { return (await available().identity()).node; }

/** Only this live native return value can select loopback; it is never saved as a gateway URL. */
export async function dialFabric(node: string, service: string, address?: string, mode: FabricPathMode = 'auto'): Promise<FabricDial> {
  const result = await available().dial(node, service, address ?? null, mode);
  if (!/^http:\/\/127\.0\.0\.1:[1-9]\d{0,4}$/.test(result.url) || Number(new URL(result.url).port) > 65535) {
    await native?.stop();
    throw new Error('Fabric returned an invalid loopback listener');
  }
  return result;
}

export async function stopFabric(): Promise<void> { await native?.stop(); }

/** On-demand measurements; no polling timer or private network addresses. */
export async function fabricStats(): Promise<Record<string, unknown>> { return available().stats(); }

/** A second target never replaces the gateway listener. Older native modules fail visibly. */
export async function openFabricTarget(node: string, service: string): Promise<FabricDial & { handle: string }> {
  const bridge = available();
  if (!bridge.openTarget || !bridge.closeTarget) throw new Error('This fabric build cannot open an owner target');
  const result = await bridge.openTarget(node, service, null, 'auto');
  if (!result.handle || !/^http:\/\/127\.0\.0\.1:[1-9]\d{0,4}$/.test(result.url) || Number(new URL(result.url).port) > 65535) {
    if (result.handle) await bridge.closeTarget(result.handle);
    throw new Error('Fabric returned an invalid owner listener');
  }
  return result;
}
export async function closeFabricTarget(handle: string): Promise<void> { await native?.closeTarget?.(handle); }
export async function fabricTargetStats(handle: string): Promise<Record<string, unknown>> {
  if (!native?.targetStats) throw new Error('This fabric build has no target diagnostics');
  return native.targetStats(handle);
}
