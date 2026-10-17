import assert from 'node:assert/strict';
import { createRequire } from 'node:module';
import { test } from 'node:test';

const require = createRequire(import.meta.url);
const configure = require('./app.config.js');
const baseline = require('./app.json').expo;
const withoutPush = require('./plugins/withoutPush.js');
const withNotifications = require('expo-notifications/app.plugin.js').default;

test('only ST_IOS_PUSH=1 selects notifications without changing either installed identity', () => {
  const saved = process.env.ST_IOS_PUSH;
  const savedVariant = process.env.APP_VARIANT;
  try {
    for (const variant of ['dev', 'daily']) {
      process.env.APP_VARIANT = variant;
      delete process.env.ST_IOS_PUSH;
      const expected = configure();
      for (const flag of [undefined, '0', 'true', '1']) {
        if (flag === undefined) delete process.env.ST_IOS_PUSH; else process.env.ST_IOS_PUSH = flag;
        const configured = configure();
        assert.deepEqual(configured.ios, expected.ios);
        assert.equal(configured.icon, expected.icon);
        assert.deepEqual(configured.plugins.slice(0, -1), baseline.plugins);
        assert.equal(configured.extra?.stIosPush, flag === '1' ? true : undefined);
        assert.deepEqual(configured.plugins.at(-1), flag === '1' ? ['expo-notifications', { mode: 'development' }] : './plugins/withoutPush');
      }
    }
  } finally {
    if (saved === undefined) delete process.env.ST_IOS_PUSH; else process.env.ST_IOS_PUSH = saved;
    if (savedVariant === undefined) delete process.env.APP_VARIANT; else process.env.APP_VARIANT = savedVariant;
  }
});

test('default no-op suppresses Expo automatic notifications plugin without creating native mods', () => {
  const config = withoutPush(structuredClone(baseline));
  assert.deepEqual(config.ios, baseline.ios);
  assert.equal(config.mods, undefined);
  const automatic = withNotifications(config, {});
  assert.equal(automatic, config);
  assert.equal(automatic.mods, undefined);
  assert.equal(automatic.ios.entitlements?.['aps-environment'], undefined);
});

test('flagged notification plugin installs the iOS entitlement mod', () => {
  const config = withNotifications(structuredClone(baseline), { mode: 'development' });
  assert.equal(typeof config.mods?.ios?.entitlements, 'function');
});
