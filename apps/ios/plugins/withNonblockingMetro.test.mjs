import assert from 'node:assert/strict';
import test from 'node:test';
import withNonblockingMetro from './withNonblockingMetro.cjs';

const debugCall = 'return RCTBundleURLProvider.sharedSettings().jsBundleURL(forBundleRoot: ".expo/.virtual-metro-entry")';
const releaseCall = 'return Bundle.main.url(forResource: "main", withExtension: "jsbundle")';
const entry = `override func bundleURL() -> URL? {
#if DEBUG
    ${debugCall}
#else
    ${releaseCall}
#endif
  }`;

async function apply(contents, language = 'swift') {
  const config = withNonblockingMetro({ name: 'test', slug: 'test' });
  const result = await config.mods.ios.appDelegate({
    ...config,
    modRequest: {},
    modResults: { language, contents },
  });
  return result.modResults.contents;
}

test('prebuild bypasses synchronous discovery but keeps RN URL options and Release bundle', async () => {
  const result = await apply(entry);
  assert.ok(!result.includes(debugCall));
  assert.ok(result.includes(releaseCall));
  assert.match(result, /settings\.jsLocation/);
  assert.match(result, /Bundle\.main\.url\(forResource: "ip", withExtension: "txt"\)/);
  assert.match(result, /\?\? "localhost"/);
  assert.match(result, /RCTBundleURLProvider\.jsBundleURL\(/);
  for (const option of ['packagerScheme', 'enableDev', 'enableMinification', 'inlineSourceMap']) {
    assert.ok(result.includes(`${option}: settings.${option}`));
  }
  assert.ok(!result.includes('isPackagerRunning'));
  assert.ok(!result.includes('packagerServerHost'));
});

test('repeated prebuild does not duplicate the fix', async () => {
  const result = await apply(entry);
  assert.equal(await apply(result), result);
});

test('template drift and unsupported languages fail instead of losing the fix', async () => {
  await assert.rejects(apply('override func bundleURL() -> URL? { return nil }'), /expected exactly one/);
  await assert.rejects(apply(`${entry}\n${entry}`), /expected exactly one/);
  await assert.rejects(apply(entry, 'objc'), /requires a Swift/);
});
