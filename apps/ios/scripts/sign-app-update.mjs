#!/usr/bin/env node
// Turn an `expo export --platform ios` directory into a signed Expo Updates v1 publication.
//
// Writes three files into the export directory:
//   manifest.json      the exact manifest bytes that were signed and that st serves
//   manifest.signature the `expo-signature` structured header for those bytes
//   publication.json   the index `st app-updates publish` reads: each asset's SHA-256, content
//                      type, and its file inside the export directory
//
// The private key is read only here; st never sees it. Keep it outside the repository.
import { createHash, createPrivateKey, createSign, randomUUID } from 'node:crypto';
import { lstatSync, readFileSync, realpathSync, writeFileSync } from 'node:fs';
import { isAbsolute, join, normalize, sep } from 'node:path';
import { pathToFileURL } from 'node:url';
import { parseArgs } from 'node:util';

const USAGE = `usage: sign-app-update.mjs --dir <expo export dir> --app <id> --channel <channel> \\
    --origin <https://gateway> --runtime-version <runtime> --private-key <pem> [--expo-config <json>]`;

// The st publish bounds; the helper fails here rather than at publish time.
const MAX_ASSET_BYTES = 32 * 1024 * 1024;
const MAX_TOTAL_BYTES = 128 * 1024 * 1024;
const MAX_ASSETS = 512;
const MAX_MANIFEST_BYTES = 1024 * 1024;
const NAME = /^[A-Za-z0-9][A-Za-z0-9._-]{0,127}$/;

const CONTENT_TYPES = {
  bmp: 'image/bmp',
  gif: 'image/gif',
  heic: 'image/heic',
  jpeg: 'image/jpeg',
  jpg: 'image/jpeg',
  png: 'image/png',
  svg: 'image/svg+xml',
  webp: 'image/webp',
  otf: 'font/otf',
  ttf: 'font/ttf',
  woff: 'font/woff',
  woff2: 'font/woff2',
  json: 'application/json',
  mp3: 'audio/mpeg',
  m4a: 'audio/mp4',
  wav: 'audio/wav',
  mp4: 'video/mp4',
  html: 'text/html',
  js: 'application/javascript',
};

export class SignAppUpdateError extends Error {}

const fail = (message) => {
  throw new SignAppUpdateError(message);
};

/** Read one regular file named by the export, refusing traversal and symlinks. */
function readExportFile(dir, relative) {
  if (typeof relative !== 'string' || relative === '' || isAbsolute(relative) || relative.includes('\\')) {
    fail(`export path ${JSON.stringify(relative)} is not a relative path`);
  }
  const parts = normalize(relative).split(sep);
  if (parts.some((part) => part === '..' || part === '.' || part === '')) {
    fail(`export path ${JSON.stringify(relative)} leaves the export directory`);
  }
  let current = dir;
  for (const part of parts) {
    current = join(current, part);
    const stat = lstatSync(current, { throwIfNoEntry: false });
    if (!stat) fail(`export path ${relative} does not exist`);
    if (stat.isSymbolicLink()) fail(`export path ${relative} goes through a symlink`);
  }
  const stat = lstatSync(current);
  if (!stat.isFile()) fail(`export path ${relative} is not a regular file`);
  if (stat.size > MAX_ASSET_BYTES) fail(`export file ${relative} is ${stat.size} bytes; the limit is ${MAX_ASSET_BYTES}`);
  return readFileSync(current);
}

function parseOrigin(value) {
  let url;
  try {
    url = new URL(value);
  } catch {
    fail(`--origin ${value} is not a URL`);
  }
  if (url.protocol !== 'https:' && url.protocol !== 'http:') fail('--origin must be an http(s) URL');
  if (url.username || url.password || url.search || url.hash || (url.pathname !== '/' && url.pathname !== '')) {
    fail('--origin must be a bare origin such as https://gateway.example');
  }
  return url.origin;
}

const extensionOf = (path) => {
  const name = path.split('/').pop();
  const dot = name.lastIndexOf('.');
  return dot > 0 ? name.slice(dot + 1).toLowerCase() : undefined;
};

/**
 * Build, sign, and write the publication for one export. Returns what was written.
 * `now` and `id` are injectable so tests can pin the manifest bytes.
 */
export function signAppUpdate({ dir, app, channel, origin, runtimeVersion, privateKey, expoConfig, now = new Date(), id = randomUUID() }) {
  if (!NAME.test(app ?? '')) fail('--app must be 1-128 characters of letters, digits, ".", "_" or "-"');
  if (!NAME.test(channel ?? '')) fail('--channel must be 1-128 characters of letters, digits, ".", "_" or "-"');
  if (!runtimeVersion || runtimeVersion.length > 256 || /[\u0000-\u001f\u007f]/.test(runtimeVersion)) {
    fail('--runtime-version must be 1-256 printable characters');
  }
  const base = parseOrigin(origin);
  const root = realpathSync(dir);
  let metadata;
  try {
    metadata = JSON.parse(readExportFile(root, 'metadata.json').toString('utf8'));
  } catch (error) {
    if (error instanceof SignAppUpdateError) throw error;
    fail(`metadata.json is not JSON: ${error.message}`);
  }
  if (metadata?.version !== 0 || metadata.bundler !== 'metro') fail('metadata.json is not an Expo export (version 0, metro)');
  const ios = metadata.fileMetadata?.ios;
  if (!ios || typeof ios.bundle !== 'string' || !Array.isArray(ios.assets)) {
    fail('metadata.json has no iOS bundle; export with `expo export --platform ios`');
  }

  const published = new Map();
  let total = 0;
  const describe = (path, contentType, fileExtension) => {
    const bytes = readExportFile(root, path);
    const digest = createHash('sha256').update(bytes).digest();
    const hex = digest.toString('hex');
    if (!published.has(hex)) {
      total += bytes.length;
      if (total > MAX_TOTAL_BYTES) fail(`the export holds more than ${MAX_TOTAL_BYTES} asset bytes`);
      published.set(hex, { hash: hex, content_type: contentType, path });
    }
    const query = `app=${encodeURIComponent(app)}&channel=${encodeURIComponent(channel)}`;
    return {
      hash: digest.toString('base64url'),
      // Expo's asset key is the bundler's MD5 file hash; the client caches by it.
      key: createHash('md5').update(bytes).digest('hex'),
      contentType,
      ...(fileExtension ? { fileExtension } : {}),
      url: `${base}/v1/client/app-updates/assets/${hex}?${query}`,
    };
  };

  const launchAsset = describe(ios.bundle, 'application/javascript');
  const assets = ios.assets.map((asset) => {
    if (!asset || typeof asset.path !== 'string') fail('metadata.json lists an asset without a path');
    const ext = typeof asset.ext === 'string' && asset.ext ? asset.ext.toLowerCase() : extensionOf(asset.path);
    if (ext !== undefined && !/^[a-z0-9]{1,16}$/.test(ext)) fail(`asset ${asset.path} has an invalid extension`);
    return describe(asset.path, (ext && CONTENT_TYPES[ext]) || 'application/octet-stream', ext ? `.${ext}` : undefined);
  });
  if (published.size > MAX_ASSETS) fail(`the export holds ${published.size} assets; the limit is ${MAX_ASSETS}`);

  const manifest = {
    id,
    createdAt: now.toISOString(),
    runtimeVersion,
    launchAsset,
    assets,
    metadata: {},
    extra: expoConfig === undefined ? {} : { expoClient: expoConfig },
  };
  const manifestBytes = Buffer.from(JSON.stringify(manifest), 'utf8');
  if (manifestBytes.length > MAX_MANIFEST_BYTES) fail(`manifest.json is ${manifestBytes.length} bytes; the limit is ${MAX_MANIFEST_BYTES}`);

  let key;
  try {
    key = createPrivateKey(privateKey);
  } catch (error) {
    fail(`the private key is not a PEM key: ${error.message}`);
  }
  if (key.asymmetricKeyType !== 'rsa') fail('Expo code signing needs an RSA private key');
  const signature = createSign('RSA-SHA256').update(manifestBytes).sign(key, 'base64');
  // Expo's structured `expo-signature` header; keyid names the certificate the app embeds.
  const header = `sig="${signature}", keyid="main"`;

  writeFileSync(join(root, 'manifest.json'), manifestBytes);
  writeFileSync(join(root, 'manifest.signature'), header);
  const publication = { app, channel, runtimeVersion, id, assets: [...published.values()] };
  writeFileSync(join(root, 'publication.json'), `${JSON.stringify(publication, null, 2)}\n`);
  return { manifest, manifestBytes, signature: header, publication };
}

function main(argv) {
  const { values } = parseArgs({
    args: argv,
    options: {
      dir: { type: 'string' },
      app: { type: 'string' },
      channel: { type: 'string' },
      origin: { type: 'string' },
      'runtime-version': { type: 'string' },
      'private-key': { type: 'string' },
      'expo-config': { type: 'string' },
      help: { type: 'boolean' },
    },
  });
  if (values.help) {
    console.log(USAGE);
    return;
  }
  for (const name of ['dir', 'app', 'channel', 'origin', 'runtime-version', 'private-key']) {
    if (!values[name]) fail(`--${name} is required\n${USAGE}`);
  }
  const expoConfig = values['expo-config'] === undefined ? undefined : JSON.parse(readFileSync(values['expo-config'], 'utf8'));
  const { manifest, publication } = signAppUpdate({
    dir: values.dir,
    app: values.app,
    channel: values.channel,
    origin: values.origin,
    runtimeVersion: values['runtime-version'],
    privateKey: readFileSync(values['private-key']),
    expoConfig,
  });
  console.log(`signed update ${manifest.id} for ${publication.app}/${publication.channel} (runtime ${manifest.runtimeVersion}, ${publication.assets.length} assets)`);
  console.log(`next: st app-updates publish --app ${publication.app} --channel ${publication.channel} --dir ${values.dir}`);
}

if (process.argv[1] && import.meta.url === pathToFileURL(realpathSync(process.argv[1])).href) {
  try {
    main(process.argv.slice(2));
  } catch (error) {
    console.error(`sign-app-update: ${error.message}`);
    process.exit(1);
  }
}
