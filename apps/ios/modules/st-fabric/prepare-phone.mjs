// Opt-in, local Debug packaging. Only Expo's ignored native project is changed.
import { existsSync, readFileSync, writeFileSync } from 'node:fs';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

if (process.argv[2] !== '--offline-debug') throw new Error('Usage: node prepare-phone.mjs --offline-debug (after Expo prebuild)');
const native = new URL('../../ios/', import.meta.url);
// Expo sources the preserved local override after setting Debug's SKIP_BUNDLING.
// Its updates override is recreated/deleted by pod install, so do not use that file.
const updates = new URL('.xcode.env.local', native);
const previous = existsSync(updates) ? readFileSync(updates, 'utf8') : '';
if (!previous.includes('# ST3 offline Debug proof')) writeFileSync(updates, previous + '\n# ST3 offline Debug proof\nif [[ "${FORCE_BUNDLING:-}" == "1" ]]; then unset SKIP_BUNDLING; export ST3_FABRIC_OFFLINE_DEBUG=1; fi\n');
const delegate = new URL('smalltalk/AppDelegate.swift', native);
let source = readFileSync(delegate, 'utf8');
const original = '#if DEBUG\n    return RCTBundleURLProvider.sharedSettings().jsBundleURL(forBundleRoot: ".expo/.virtual-metro-entry")';
const replacement = '#if DEBUG\n    if let embedded = Bundle.main.url(forResource: "main", withExtension: "jsbundle") { return embedded }\n    return RCTBundleURLProvider.sharedSettings().jsBundleURL(forBundleRoot: ".expo/.virtual-metro-entry")';
if (!source.includes(replacement)) {
  if (!source.includes(original)) throw new Error('Generated AppDelegate changed; inspect its Debug bundle selection before packaging.');
  source = source.replace(original, replacement);
  writeFileSync(delegate, source);
}

const identifier = 'com.compoundingtech.smalltalk.fabricproof';
const project = new URL('smalltalk.xcodeproj/project.pbxproj', native);
source = readFileSync(project, 'utf8');
if (!source.includes('PRODUCT_BUNDLE_IDENTIFIER = com.compoundingtech.smalltalk.starter;') && !source.includes(`PRODUCT_BUNDLE_IDENTIFIER = ${identifier};`)) throw new Error('Unexpected generated app identifier.');
writeFileSync(project, source.replaceAll('PRODUCT_BUNDLE_IDENTIFIER = com.compoundingtech.smalltalk.starter;', `PRODUCT_BUNDLE_IDENTIFIER = ${identifier};`));

const plist = fileURLToPath(new URL('smalltalk/Info.plist', native));
const read = spawnSync('/usr/bin/plutil', ['-convert', 'json', '-o', '-', plist], { encoding: 'utf8' });
if (read.status !== 0) throw new Error('Cannot read generated app Info.plist');
const info = JSON.parse(read.stdout);
info.CFBundleDisplayName = 'Small Talk Fabric Proof';
info.CFBundleURLTypes = [{ CFBundleURLName: identifier, CFBundleURLSchemes: [identifier] }];
const write = spawnSync('/usr/bin/plutil', ['-convert', 'xml1', '-o', plist, '--', '-'], { input: JSON.stringify(info), encoding: 'utf8' });
if (write.status !== 0) throw new Error('Cannot write generated app Info.plist');
console.log('Prepared a separate offline Debug proof app. Build with FORCE_BUNDLING=1; leave SKIP_BUNDLING unset.');
