// The keys this phone signs with and verifies (modules/st-device-key/ios). Existing profiles
// remain usable without the module; new pairing requires proof verification support.
import { requireOptionalNativeModule } from 'expo';

export type DeviceKeyInfo = { key: string; storage: 'secure-enclave' | 'software'; handle: string };

type Native = {
  current(handle?: string): Promise<Omit<DeviceKeyInfo, 'handle'> | null>;
  create(): Promise<DeviceKeyInfo>;
  sign(text: string, handle?: string): Promise<string>;
  remove(handle?: string): Promise<void>;
  verify(key: string, text: string, signature: string): Promise<boolean>;
};

const native = requireOptionalNativeModule<Native>('StDeviceKey');

/** A new key kept separately until its handle is committed with the paired credential. */
export async function createDeviceKey(): Promise<DeviceKeyInfo | null> {
  return native ? native.create() : null;
}

/** Inspect a saved profile's key by handle, or the legacy default key. */
export async function currentDeviceKey(handle?: string): Promise<Omit<DeviceKeyInfo, 'handle'> | null> {
  return native ? (handle === undefined ? native.current() : native.current(handle)) : null;
}

/** The base64url r||s signature over `text`'s UTF-8 bytes. */
export async function signWithDeviceKey(text: string, handle?: string): Promise<string> {
  if (!native) throw new Error('This build cannot sign');
  return handle === undefined ? native.sign(text) : native.sign(text, handle);
}

export async function removeDeviceKey(handle?: string): Promise<void> {
  if (native) await (handle === undefined ? native.remove() : native.remove(handle));
}

export function canVerifyPairing(): boolean { return typeof native?.verify === 'function'; }
export async function verifyGrantSignature(key: string, text: string, signature: string): Promise<boolean> {
  if (!native) throw new Error('This build cannot verify pairing proofs; update the phone app.');
  return native.verify(key, text, signature);
}
