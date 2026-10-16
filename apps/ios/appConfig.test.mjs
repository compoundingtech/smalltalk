import assert from 'node:assert/strict';
import { test } from 'node:test';
import { createRequire } from 'node:module';
const configure = createRequire(import.meta.url)('./app.config.js');

const withVariant = (value, run) => {
  const saved = process.env.APP_VARIANT;
  try {
    if (value === undefined) delete process.env.APP_VARIANT; else process.env.APP_VARIANT = value;
    return run();
  } finally {
    if (saved === undefined) delete process.env.APP_VARIANT; else process.env.APP_VARIANT = saved;
  }
};

test('daily installs as Smalltalk with the ordinary icon', () => {
  const daily = withVariant('daily', configure);
  assert.equal(daily.name, 'smalltalk');
  assert.equal(daily.ios.infoPlist.CFBundleDisplayName, 'Smalltalk');
  assert.equal(daily.ios.infoPlist.NSMicrophoneUsageDescription.length > 0, true);
  assert.equal(daily.icon, './assets/icon.png');
  assert.equal(daily.ios.bundleIdentifier, 'com.compoundingtech.smalltalk');
  assert.equal(daily.ios.supportsTablet, true);
  assert.equal(daily.updates, undefined);
});

test('default and dev keep the starter identity with the DEV-badged icon', () => {
  const dev = withVariant(undefined, configure);
  assert.equal(dev.name, 'smalltalk');
  assert.equal(dev.ios.infoPlist.CFBundleDisplayName, 'Smalltalk Dev');
  assert.equal(dev.icon, './assets/icon-dev.png');
  assert.equal(dev.ios.bundleIdentifier, 'com.compoundingtech.smalltalk.starter');
  assert.deepEqual(withVariant('dev', configure), dev);
});

test('an unknown variant fails closed', () => {
  assert.throws(() => withVariant('typo', configure), /APP_VARIANT/);
});
