const { createRunOncePlugin } = require('expo/config-plugins');

// Expo auto-applies expo-notifications when it is installed. Reserve its run-once identity without
// changing any native config, so ordinary builds retain their existing entitlements/signing.
module.exports = createRunOncePlugin(config => config, 'expo-notifications');
