import assert from 'node:assert/strict';
import { execFile } from 'node:child_process';
import { mkdir, mkdtemp, readFile, rm, writeFile } from 'node:fs/promises';
import path from 'node:path';
import { fileURLToPath } from 'node:url';
import { promisify } from 'node:util';
import { test } from 'node:test';

const run = promisify(execFile);
const root = path.dirname(fileURLToPath(import.meta.url));

test('native transport keeps SDK 57 cached update selectable across bearer clearing/rotation and cold launch', { skip: process.platform !== 'darwin' }, async () => {
  // Compile pure Swift unit assertions against the installed, unmodified SDK selection policies.
  // Data-only fixture types replace Expo's iOS-only models; no selection or transport code is mocked.
  const scratchRoot = path.resolve(root, '../../tmp');
  await mkdir(scratchRoot, { recursive: true });
  const scratch = await mkdtemp(path.join(scratchRoot, 'ota-launch-filter-'));
  try {
    const source = await readFile(path.join(root, 'plugins/native/StPairedGateway.swift'), 'utf8');
    const boundary = source.indexOf('// Compiled inside source-built EXUpdates.');
    assert.ok(boundary > 0);
    const transport = path.join(scratch, 'transport.swift');
    const main = path.join(scratch, 'main.swift');
    const executable = path.join(scratch, 'regression');
    await writeFile(transport, source.slice(0, boundary));
    await writeFile(main, await readFile(path.join(root, 'plugins/native/launch-filter-main.swift')));
    const policies = path.join(root, 'node_modules/expo-updates/ios/EXUpdates/SelectionPolicy');
    await run('xcrun', ['swiftc', '-o', executable,
      path.join(root, 'plugins/native/launch-filter-fixture.swift'), transport,
      path.join(policies, 'SelectionPolicies.swift'), path.join(policies, 'LauncherSelectionPolicyFilterAware.swift'), main], { timeout: 120000 });
    const result = await run(executable, [], { timeout: 10000 });
    assert.match(result.stdout, /unmodified SDK 57 launch-filter regression passed/);
  } finally {
    await rm(scratch, { recursive: true, force: true });
  }
});
