import assert from 'node:assert/strict';
import { createHash, createVerify, generateKeyPairSync } from 'node:crypto';
import { mkdirSync, mkdtempSync, readFileSync, symlinkSync, writeFileSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';
import { signAppUpdate } from './sign-app-update.mjs';

const { privateKey, publicKey } = generateKeyPairSync('rsa', { modulusLength: 2048 });
const pem = privateKey.export({ type: 'pkcs8', format: 'pem' });

// The shape `expo export --platform ios` writes in SDK 57: metadata.json names the bundle and
// each asset as `assets/<md5>` with its extension.
function exportDir() {
  const dir = mkdtempSync(join(tmpdir(), 'sign-app-update-'));
  mkdirSync(join(dir, '_expo/static/js/ios'), { recursive: true });
  mkdirSync(join(dir, 'assets'));
  const bundle = Buffer.from('bundle bytes');
  const icon = Buffer.from('png bytes');
  const iconMd5 = createHash('md5').update(icon).digest('hex');
  writeFileSync(join(dir, '_expo/static/js/ios/index-abc.hbc'), bundle);
  writeFileSync(join(dir, 'assets', iconMd5), icon);
  writeFileSync(join(dir, 'metadata.json'), JSON.stringify({
    version: 0,
    bundler: 'metro',
    fileMetadata: { ios: { bundle: '_expo/static/js/ios/index-abc.hbc', assets: [{ path: `assets/${iconMd5}`, ext: 'png' }] } },
  }));
  return { dir, bundle, icon, iconMd5 };
}

const sha256 = (bytes, encoding) => createHash('sha256').update(bytes).digest(encoding);
const options = (dir) => ({
  dir,
  app: 'com.example.app',
  channel: 'daily',
  origin: 'https://gateway.example',
  runtimeVersion: '1.0.0',
  privateKey: pem,
  now: new Date('2026-10-07T00:00:00.000Z'),
  id: '0194b2e0-1234-7000-8000-000000000001',
});

{
  const { dir, bundle, icon, iconMd5 } = exportDir();
  const result = signAppUpdate(options(dir));
  const manifestBytes = readFileSync(join(dir, 'manifest.json'));
  const manifest = JSON.parse(manifestBytes);
  assert.deepEqual(manifestBytes, result.manifestBytes);
  assert.equal(manifest.id, '0194b2e0-1234-7000-8000-000000000001');
  assert.equal(manifest.createdAt, '2026-10-07T00:00:00.000Z');
  assert.equal(manifest.runtimeVersion, '1.0.0');
  assert.deepEqual(manifest.metadata, {});
  assert.deepEqual(manifest.extra, {});

  // Expo checks base64url SHA-256 hashes; st serves each asset under its hex SHA-256.
  assert.equal(manifest.launchAsset.hash, sha256(bundle, 'base64url'));
  assert.equal(manifest.launchAsset.contentType, 'application/javascript');
  assert.equal(manifest.launchAsset.url, `https://gateway.example/v1/client/app-updates/assets/${sha256(bundle, 'hex')}?app=com.example.app&channel=daily`);
  assert.deepEqual(manifest.assets, [{
    hash: sha256(icon, 'base64url'),
    key: iconMd5,
    contentType: 'image/png',
    fileExtension: '.png',
    url: `https://gateway.example/v1/client/app-updates/assets/${sha256(icon, 'hex')}?app=com.example.app&channel=daily`,
  }]);

  // The signature covers the exact bytes written, in Expo's structured header form.
  const header = readFileSync(join(dir, 'manifest.signature'), 'utf8');
  const match = /^sig="([A-Za-z0-9+/=]+)", keyid="main"$/.exec(header);
  assert.ok(match, header);
  assert.ok(createVerify('RSA-SHA256').update(manifestBytes).verify(publicKey, match[1], 'base64'));

  const publication = JSON.parse(readFileSync(join(dir, 'publication.json'), 'utf8'));
  assert.deepEqual(publication, {
    app: 'com.example.app',
    channel: 'daily',
    runtimeVersion: '1.0.0',
    id: '0194b2e0-1234-7000-8000-000000000001',
    assets: [
      { hash: sha256(bundle, 'hex'), content_type: 'application/javascript', path: '_expo/static/js/ios/index-abc.hbc' },
      { hash: sha256(icon, 'hex'), content_type: 'image/png', path: `assets/${iconMd5}` },
    ],
  });
}

// An app config, when given, reaches the app as Constants.expoConfig.
{
  const { dir } = exportDir();
  const { manifest } = signAppUpdate({ ...options(dir), expoConfig: { name: 'Example' } });
  assert.deepEqual(manifest.extra, { expoClient: { name: 'Example' } });
}

// Paths in metadata.json stay inside the export and never follow symlinks.
{
  const { dir } = exportDir();
  writeFileSync(join(dir, 'metadata.json'), JSON.stringify({ version: 0, bundler: 'metro', fileMetadata: { ios: { bundle: '../outside.hbc', assets: [] } } }));
  assert.throws(() => signAppUpdate(options(dir)), /leaves the export directory/);
}
{
  const { dir } = exportDir();
  symlinkSync('/etc/hosts', join(dir, 'linked.hbc'));
  writeFileSync(join(dir, 'metadata.json'), JSON.stringify({ version: 0, bundler: 'metro', fileMetadata: { ios: { bundle: 'linked.hbc', assets: [] } } }));
  assert.throws(() => signAppUpdate(options(dir)), /symlink/);
}

// The origin is a bare origin, and code signing needs RSA.
{
  const { dir } = exportDir();
  assert.throws(() => signAppUpdate({ ...options(dir), origin: 'https://gateway.example/base?x=1' }), /bare origin/);
  const ec = generateKeyPairSync('ec', { namedCurve: 'P-256' }).privateKey.export({ type: 'pkcs8', format: 'pem' });
  assert.throws(() => signAppUpdate({ ...options(dir), privateKey: ec }), /RSA private key/);
}
