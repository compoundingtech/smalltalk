import type { FabricPathMode } from '../../fabricProof';
import { requireOptionalNativeModule } from 'expo';

export type FabricDial = { url: string; node: string; fabricVersion: string; irohVersion: string };
type Native = {
  available(): Promise<boolean>;
  identity(): Promise<{ node: string }>;
  dial(node: string, service: string, address: string | null, mode: FabricPathMode): Promise<FabricDial>;
  stats(): Promise<Record<string, unknown>>;
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
