const fs = require('node:fs/promises');
const path = require('node:path');
const { withDangerousMod, withExpoPlist, withPodfile } = require('expo/config-plugins');

// SDK 57's stock URL override disables embedded recovery. Compile our narrowly scoped
// transport extension inside EXUpdates instead, with no preview/anti-bricking override.
module.exports = config => {
  config = withExpoPlist(config, result => {
    if (result.ios?.bundleIdentifier === 'com.compoundingtech.smalltalk') {
      result.modResults.EXUpdatesScopeKey = 'com.compoundingtech.smalltalk.daily';
    }
    return result;
  });
  config = withPodfile(config, result => {
    const declaration = "$ExpoUseSources = ($ExpoUseSources || []) | ['expo-updates']";
    if (!result.modResults.contents.includes(declaration)) {
      result.modResults.contents = `${declaration}\n${result.modResults.contents}`;
    }
    return result;
  });
  return withDangerousMod(config, ['ios', async result => {
    const root = result.modRequest.projectRoot;
    const packagePath = require.resolve('expo-updates/package.json', { paths: [root] });
    const metadata = JSON.parse(await fs.readFile(packagePath, 'utf8'));
    if (!/^57\.0\./.test(metadata.version)) {
      throw new Error('The paired gateway native integration must be reviewed before changing expo-updates SDK 57');
    }
    await fs.copyFile(
      path.join(__dirname, 'native/StPairedGateway.swift'),
      path.join(path.dirname(packagePath), 'ios/EXUpdates/StPairedGateway.swift'),
    );
    // Xcode's bundle and EXUpdates resource phases both source this file. Keep their
    // config variant identical to prebuild even when Xcode launches without shell env.
    const envPath = path.join(result.modRequest.platformProjectRoot, '.xcode.env');
    const existing = await fs.readFile(envPath, 'utf8').catch(error => {
      if (error.code === 'ENOENT') return 'export NODE_BINARY=$(command -v node)\n';
      throw error;
    });
    const variant = result.ios?.bundleIdentifier === 'com.compoundingtech.smalltalk' ? 'daily' : 'dev';
    const lines = existing.split('\n').filter(line => !line.startsWith('export APP_VARIANT=') && !line.startsWith('export ST_IOS_UPDATES_CERT='));
    const certificate = variant === 'daily' ? result.updates?.codeSigningCertificate : undefined;
    const certificateEnvironment = certificate ? `export ST_IOS_UPDATES_CERT='${certificate.replaceAll("'", "'\\''")}'\n` : '';
    await fs.writeFile(envPath, `${lines.join('\n').trimEnd()}\nexport APP_VARIANT=${variant}\n${certificateEnvironment}`);
    return result;
  }]);
};
