import assert from 'node:assert/strict';
import { test } from 'node:test';
import { createRequire } from 'node:module';
const configure = createRequire(import.meta.url)('./app.config.js');
import { fileURLToPath } from 'node:url';
const fixtureCertificate = fileURLToPath(new URL('./certs/test-fixture.pem', import.meta.url));

test('daily keeps embedded recovery, signing and inert Authorization placeholder', () => {
  const saved = process.env.APP_VARIANT;
  const savedCertificate = process.env.ST_IOS_UPDATES_CERT;
  try {
    process.env.APP_VARIANT = 'daily';
    process.env.ST_IOS_UPDATES_CERT = fixtureCertificate;
    const daily = configure();
    assert.equal(daily.name, 'Smalltalk');
    assert.equal(daily.ios.bundleIdentifier, 'com.compoundingtech.smalltalk');
    assert.equal(daily.ios.buildNumber, '2');
    assert.deepEqual(daily.runtimeVersion, { policy: 'nativeVersion' });
    assert.equal(daily.updates.checkAutomatically, 'NEVER');
    assert.equal(daily.updates.useEmbeddedUpdate, true);
    assert.equal(daily.updates.disableAntiBrickingMeasures, false);
    assert.deepEqual(daily.updates.requestHeaders, { Authorization: 'Bearer unavailable' });
    assert.equal(daily.updates.codeSigningCertificate, 'certs/test-fixture.pem');
    assert.deepEqual(daily.updates.codeSigningMetadata, { keyid: 'main', alg: 'rsa-v1_5-sha256' });
    assert.equal(new URL(daily.updates.url).hostname, 'app-updates.invalid');
  } finally {
    if (saved === undefined) delete process.env.APP_VARIANT; else process.env.APP_VARIANT = saved;
    if (savedCertificate === undefined) delete process.env.ST_IOS_UPDATES_CERT; else process.env.ST_IOS_UPDATES_CERT = savedCertificate;
  }
});

test('default/dev retain starter bundle and Metro without OTA; invalid variant fails closed', () => {
  const saved = process.env.APP_VARIANT;
  try {
    delete process.env.APP_VARIANT;
    const dev = configure();
    assert.equal(dev.name, 'Smalltalk Dev');
    assert.equal(dev.ios.bundleIdentifier, 'com.compoundingtech.smalltalk.starter');
    assert.equal(dev.icon, './assets/icon-dev.png');
    assert.deepEqual(dev.updates, { enabled: false });
    assert.equal(dev.runtimeVersion, undefined);
    process.env.APP_VARIANT = 'dev';
    assert.deepEqual(configure(), dev);
    process.env.APP_VARIANT = 'typo';
    assert.throws(configure, /APP_VARIANT/);
  } finally {
    if (saved === undefined) delete process.env.APP_VARIANT; else process.env.APP_VARIANT = saved;
  }
});

test('daily fails closed without an operator-owned certificate', () => {
  const variant = process.env.APP_VARIANT, certificate = process.env.ST_IOS_UPDATES_CERT;
  try {
    process.env.APP_VARIANT = 'daily';
    delete process.env.ST_IOS_UPDATES_CERT;
    assert.throws(configure, /require ST_IOS_UPDATES_CERT/);
    process.env.ST_IOS_UPDATES_CERT = './certs/missing.pem';
    assert.throws(configure, /existing public code-signing certificate/);
  } finally {
    if (variant === undefined) delete process.env.APP_VARIANT; else process.env.APP_VARIANT = variant;
    if (certificate === undefined) delete process.env.ST_IOS_UPDATES_CERT; else process.env.ST_IOS_UPDATES_CERT = certificate;
  }
});
