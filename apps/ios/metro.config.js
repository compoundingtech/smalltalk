const path = require('path');
const { getDefaultConfig } = require('expo/metro-config');
const config = getDefaultConfig(__dirname);
config.watchFolders = [path.resolve(__dirname, '../..')];
// An embedded Debug proof has no Metro server. Expo 57's development message socket
// throws for file bundles before the app starts; omit only that devtools module in
// explicitly prepared offline iOS builds. The carrier and __DEV__ guards stay intact.
const resolve = config.resolver.resolveRequest;
const messageSocket = path.join(path.dirname(require.resolve('expo/package.json')), 'src/async-require/messageSocket.native.ts');
config.resolver.resolveRequest = (context, name, platform) => {
  const result = resolve ? resolve(context, name, platform) : context.resolveRequest(context, name, platform);
  if (process.env.ST3_FABRIC_OFFLINE_DEBUG === '1' && platform === 'ios' && result.type === 'sourceFile' && result.filePath === messageSocket) return { type: 'empty' };
  return result;
};
module.exports = config;
