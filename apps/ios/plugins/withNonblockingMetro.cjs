const { withAppDelegate } = require('expo/config-plugins');

const original = 'return RCTBundleURLProvider.sharedSettings().jsBundleURL(forBundleRoot: ".expo/.virtual-metro-entry")';
const replacement = `// Resolve the URL locally: the instance API synchronously probes /status,
    // which can exhaust iOS's scene-create watchdog when Metro is unreachable.
    let settings = RCTBundleURLProvider.sharedSettings()
    let configuredHost = settings.jsLocation?.trimmingCharacters(in: .whitespacesAndNewlines)
    let ipFile = Bundle.main.url(forResource: "ip", withExtension: "txt")
    let buildHost = ipFile.flatMap { try? String(contentsOf: $0, encoding: .utf8) }?
      .trimmingCharacters(in: .whitespacesAndNewlines)
    let host = configuredHost.flatMap { $0.isEmpty ? nil : $0 }
      ?? buildHost.flatMap { $0.isEmpty ? nil : $0 }
      ?? "localhost"
    // React Native loads this URL asynchronously and owns its normal error UI.
    return RCTBundleURLProvider.jsBundleURL(
      forBundleRoot: ".expo/.virtual-metro-entry",
      packagerHost: host,
      packagerScheme: settings.packagerScheme,
      enableDev: settings.enableDev,
      enableMinification: settings.enableMinification,
      inlineSourceMap: settings.inlineSourceMap,
      modulesOnly: false,
      runModule: true
    )`;

module.exports = function withNonblockingMetro(config) {
  return withAppDelegate(config, (config) => {
    if (config.modResults.language !== 'swift') {
      throw new Error('withNonblockingMetro requires a Swift AppDelegate');
    }
    const source = config.modResults.contents;
    if (source.includes(replacement)) return config;
    if (source.split(original).length !== 2) {
      throw new Error('withNonblockingMetro: expected exactly one Expo Debug bundle URL call; review the updated template');
    }
    config.modResults.contents = source.replace(original, replacement);
    return config;
  });
};
