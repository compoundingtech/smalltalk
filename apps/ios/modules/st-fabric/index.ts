import { requireOptionalNativeModule } from 'expo';

export type FabricDial = { url: string; node: string; fabricVersion: string; irohVersion: string };
type Native = {
  identity(): Promise<{ node: string }>;
  dial(node: string, service: string, address: string | null): Promise<FabricDial>;
  stop(): Promise<void>;
};
const native = requireOptionalNativeModule<Native>('StFabric');

function available(): Native {
  if (!__DEV__ || !native) throw new Error('This build has no development fabric bridge');
  return native;
}

export async function fabricIdentity(): Promise<string> { return (await available().identity()).node; }

/** Only this live native return value can select loopback; it is never saved as a gateway URL. */
export async function dialFabric(node: string, service: string, address?: string): Promise<FabricDial> {
  const result = await available().dial(node, service, address ?? null);
  if (!/^http:\/\/127\.0\.0\.1:[1-9]\d{0,4}$/.test(result.url) || Number(new URL(result.url).port) > 65535) {
    await native?.stop();
    throw new Error('Fabric returned an invalid loopback listener');
  }
  return result;
}

export async function stopFabric(): Promise<void> { if (__DEV__) await native?.stop(); }
