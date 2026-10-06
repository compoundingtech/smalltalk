// The key this phone signs its messages with (modules/st-device-key/ios). The native module is
// absent in a build without it; then the phone pairs and sends unsigned, as before.
import { requireOptionalNativeModule } from 'expo';

export type DeviceKeyInfo = { key: string; storage: 'secure-enclave' | 'software' };

type Native = {
  current(): Promise<DeviceKeyInfo | null>;
  create(): Promise<DeviceKeyInfo>;
  sign(text: string): Promise<string>;
  remove(): Promise<void>;
};

const native = requireOptionalNativeModule<Native>('StDeviceKey');

/** A new signing key, replacing any earlier one; null in a build without the module. */
export async function createDeviceKey(): Promise<DeviceKeyInfo | null> {
  return native ? native.create() : null;
}

/** The key this phone signs with now, if any. */
export async function currentDeviceKey(): Promise<DeviceKeyInfo | null> {
  return native ? native.current() : null;
}

/** The base64url r||s signature over `text`'s UTF-8 bytes. */
export async function signWithDeviceKey(text: string): Promise<string> {
  if (!native) throw new Error('This build cannot sign');
  return native.sign(text);
}

export async function removeDeviceKey(): Promise<void> {
  await native?.remove();
}
