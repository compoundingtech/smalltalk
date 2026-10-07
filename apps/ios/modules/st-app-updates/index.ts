import { requireOptionalNativeModule } from 'expo';

export type AppUpdatesNative = { setPairedGateway(gateway: string): Promise<void> };
export const appUpdatesNative = requireOptionalNativeModule<AppUpdatesNative>('StAppUpdates');
