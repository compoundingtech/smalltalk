import { Alert } from 'react-native';
import * as Updates from 'expo-updates';
import { appUpdatesNative } from './modules/st-app-updates/index.ts';
import { AppUpdateSession, mintAppUpdateToken } from './appUpdates.ts';
import type { ForegroundGate } from './foreground.ts';

export const createAppUpdates = (foreground: ForegroundGate): AppUpdateSession => new AppUpdateSession({
  enabled: !__DEV__ && Updates.isEnabled && appUpdatesNative !== null,
  mint: mintAppUpdateToken,
  setGateway: async gateway => {
    if (!appUpdatesNative) throw new Error('Paired app updates are unavailable on this build');
    await appUpdatesNative.setPairedGateway(gateway);
  },
  setToken: token => {
    if (!appUpdatesNative) throw new Error('Paired app updates are unavailable on this build');
    appUpdatesNative.setUpdateToken(token?.token ?? null, token?.expiresAtUnixMs ?? 0);
  },
  check: Updates.checkForUpdateAsync,
  fetch: Updates.fetchUpdateAsync,
  reload: Updates.reloadAsync,
  consent: apply => Alert.alert('Smalltalk update ready', 'Restart Smalltalk now? Unsaved work will be interrupted. Otherwise the verified update starts on your next app launch.', [
    { text: 'Next launch', style: 'cancel' },
    { text: 'Restart now', onPress: apply },
  ]),
  onFailure: () => console.warn('App update check failed; keeping the current verified bundle.'),
  now: Date.now,
}, foreground);
