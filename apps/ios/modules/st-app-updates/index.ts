import { requireOptionalNativeModule } from 'expo';

export type AppUpdatesNative = {
  setPairedGateway(gateway: string): Promise<void>;
  setUpdateToken(token: string | null, expiresAtUnixMs: number): void;
};
export const appUpdatesNative = requireOptionalNativeModule<AppUpdatesNative>('StAppUpdates');
