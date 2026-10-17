// The app's tests run its TypeScript modules directly in Node. Metro and tsc resolve extensionless
// and directory imports; this hook resolves them the same way for Node.
import { registerHooks } from 'node:module';

// Native-only dependencies use test ports. They never initialize React Native in Node.
const nativeStubs = {
  'expo-constants': `
    export default { get expoConfig() { return globalThis.testPushExpoConfig; } };
  `,
  'expo-application': `
    export const applicationId = 'com.example.smalltalk';
    export const nativeApplicationVersion = '1.0';
    export const getIosPushNotificationServiceEnvironmentAsync = () => globalThis.testPushApplicationNative.environment();
  `,
  'expo-notifications': `
    export const IosAuthorizationStatus = { NOT_DETERMINED: 0, DENIED: 1, AUTHORIZED: 2, PROVISIONAL: 3 };
    export const getPermissionsAsync = () => globalThis.testPushNotificationsNative.permission(false);
    export const requestPermissionsAsync = options => globalThis.testPushNotificationsNative.permission(true, options);
    export const getDevicePushTokenAsync = () => globalThis.testPushNotificationsNative.token();
    export const addPushTokenListener = changed => globalThis.testPushNotificationsNative.listen(changed);
  `,
};
registerHooks({
  resolve(specifier, context, nextResolve) {
    if (Object.hasOwn(nativeStubs, specifier)) return {
      url: `data:text/javascript,${encodeURIComponent(nativeStubs[specifier])}`,
      shortCircuit: true,
    };
    try { return nextResolve(specifier, context); } catch (error) {
      if (!specifier.startsWith('.') || (error.code !== 'ERR_MODULE_NOT_FOUND' && error.code !== 'ERR_UNSUPPORTED_DIR_IMPORT')) throw error;
      for (const suffix of ['.ts', '.tsx', '/index.ts']) {
        try { return nextResolve(specifier + suffix, context); } catch { /* try the next form */ }
      }
      throw error;
    }
  },
});
