import * as Application from 'expo-application';
import Constants from 'expo-constants';
import * as Notifications from 'expo-notifications';
import type { PushPorts } from './pushRegistration';

export const PUSH_REQUEST_TIMEOUT_MS = 8_000;

/** Bound the complete push write (including response decoding), so Forget can delete credentials. */
export const pushWrite = async <T>(request: (signal: AbortSignal) => Promise<T>): Promise<T> => {
  const abort = new AbortController();
  let clearDeadline = () => {};
  const deadline = new Promise<never>((_, reject) => {
    const timer = setTimeout(() => { abort.abort(); reject(new Error('Push registration request timed out.')); }, PUSH_REQUEST_TIMEOUT_MS);
    clearDeadline = () => clearTimeout(timer);
  });
  try { return await Promise.race([request(abort.signal), deadline]); }
  finally { clearDeadline(); }
};

/** Direct APNs tokens only: no Expo push service and no notification delivery/tap handler. */
export const nativePushPorts = (storage: Pick<PushPorts, 'readEnabled' | 'writeEnabled'>): PushPorts => ({
  ...storage,
  bundleId: Application.applicationId,
  version: Application.nativeApplicationVersion,
  // SDK 57 reads aps-environment from the installed provisioning profile. Release is not production.
  environment: async () => {
    if (Constants.expoConfig?.extra?.stIosPush !== true) return undefined;
    try { return (await Application.getIosPushNotificationServiceEnvironmentAsync()) ?? undefined; }
    catch { return undefined; }
  },
  permission: async request => {
    const settings = request
      ? await Notifications.requestPermissionsAsync({ ios: { allowAlert: true, allowBadge: true, allowSound: true } })
      : await Notifications.getPermissionsAsync();
    return settings.granted || settings.ios?.status === Notifications.IosAuthorizationStatus.PROVISIONAL;
  },
  token: async () => {
    const token = await Notifications.getDevicePushTokenAsync();
    return token.type === 'ios' ? token.data : undefined;
  },
  listen: changed => {
    const subscription = Notifications.addPushTokenListener(token => {
      if (token.type === 'ios') changed(token.data);
    });
    return () => subscription.remove();
  },
});
