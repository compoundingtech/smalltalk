// The keys this phone signs with and verifies (modules/st-device-key/ios). Existing profiles
// remain usable without the module; new pairing requires proof verification support.
import { requireOptionalNativeModule } from 'expo';

export type DeviceKeyInfo = { key: string; storage: 'secure-enclave' | 'software'; handle: string };

type Native = {
  current(): Promise<Omit<DeviceKeyInfo, 'handle'> | null>;
  create(): Promise<DeviceKeyInfo>;
  sign(text: string, handle: string | null): Promise<string>;
  remove(handle: string | null): Promise<void>;
  verify(key: string, text: string, signature: string): Promise<boolean>;
};

const native = requireOptionalNativeModule<Native>('StDeviceKey');

/** A new key kept separately until its handle is committed with the paired credential. */
export async function createDeviceKey(): Promise<DeviceKeyInfo | null> {
  return native ? native.create() : null;
}

/** The key this phone signs with now, if any. */
export async function currentDeviceKey(): Promise<Omit<DeviceKeyInfo, 'handle'> | null> {
  return native ? native.current() : null;
}

/** The base64url r||s signature over `text`'s UTF-8 bytes. */
export async function signWithDeviceKey(text: string, handle?: string): Promise<string> {
  if (!native) throw new Error('This build cannot sign');
  return native.sign(text, handle ?? null);
}

export async function removeDeviceKey(handle?: string): Promise<void> {
  await native?.remove(handle ?? null);
}

export function canVerifyPairing(): boolean { return !!native; }
export async function verifyGrantSignature(key: string, text: string, signature: string): Promise<boolean> {
  if (!native) throw new Error('This build cannot verify pairing proofs; update the phone app.');
  return native.verify(key, text, signature);
}
