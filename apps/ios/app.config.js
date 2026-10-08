const base = require('./app.json').expo;
const fs = require('node:fs');
const path = require('node:path');

module.exports = () => {
  const variant = process.env.APP_VARIANT ?? 'dev';
  if (variant !== 'dev' && variant !== 'daily') throw new Error('APP_VARIANT must be dev or daily');
  const daily = variant === 'daily';
  const configuredCertificate = process.env.ST_IOS_UPDATES_CERT;
  if (daily && !configuredCertificate) {
    throw new Error('Daily OTA builds require ST_IOS_UPDATES_CERT pointing to your public code-signing certificate; generate your own pair with npx expo-updates codesigning:generate');
  }
  const certificate = configuredCertificate ? path.resolve(__dirname, configuredCertificate) : undefined;
  if (daily && (!fs.existsSync(certificate) || !fs.statSync(certificate).isFile())) {
    throw new Error('ST_IOS_UPDATES_CERT must name an existing public code-signing certificate file');
  }
  return {
    ...base,
    name: daily ? 'Smalltalk' : 'Smalltalk Dev',
    icon: daily ? './assets/icon.png' : './assets/icon-dev.png',
    ios: {
      ...base.ios,
      bundleIdentifier: daily ? 'com.compoundingtech.smalltalk' : 'com.compoundingtech.smalltalk.starter',
      // Increase for every native dependency/configuration/certificate change.
      buildNumber: '2',
    },
    plugins: [...base.plugins, './plugins/with-paired-app-updates.js'],
    ...(daily ? {
      runtimeVersion: { policy: 'nativeVersion' },
      updates: {
        enabled: true,
        // Reserved non-routable origin. No request runs before pairing/Keychain hydration.
        url: 'https://app-updates.invalid/v1/client/app-updates/manifest?app=com.compoundingtech.smalltalk&channel=daily',
        checkAutomatically: 'NEVER',
        fallbackToCacheTimeout: 0,
        useEmbeddedUpdate: true,
        disableAntiBrickingMeasures: false,
        requestHeaders: { Authorization: 'Bearer unavailable' },
        // Expo's native config plugin joins this path onto projectRoot, even for absolute input.
        codeSigningCertificate: path.relative(__dirname, certificate),
        codeSigningMetadata: { keyid: 'main', alg: 'rsa-v1_5-sha256' },
      },
    } : { updates: { enabled: false } }),
  };
};
