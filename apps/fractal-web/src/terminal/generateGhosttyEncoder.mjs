import { execFileSync } from 'node:child_process'
// Regenerates the committed browser bootstrap asset from Smalltalk's native Ghostty pin.
// Usage: node src/terminal/generateGhosttyEncoder.mjs <ghostty-source> <zig-0.15.2> <zig-system-deps> <bulk-scratch>
import { createHash } from 'node:crypto'
import { readFile, writeFile, chmod } from 'node:fs/promises'
import { resolve } from 'node:path'

const revision = 'a887df42c56f6de86c0fe6da9c4eeca37931e083'
const [source, zig, deps, scratch] = process.argv.slice(2)
if (!source || !zig || !deps || !scratch)
  throw new Error('Expected source, zig, system dependencies and bulk scratch paths')
if (execFileSync(zig, ['version'], { encoding: 'utf8' }).trim() !== '0.15.2')
  throw new Error('Ghostty requires Zig 0.15.2')
const sha256 = (bytes) => createHash('sha256').update(bytes).digest('hex')
const header = await readFile(resolve(source, 'include/ghostty/vt/key/event.h'), 'utf8')
// This fingerprint binds the enum to the native Smalltalk pin, not whichever source is nearby.
if (sha256(header) !== '0eb11bfab67b887f4b409557b396a5da6ea17e9e62d92b97a38e723b46db635d')
  throw new Error('Source is not the pinned Ghostty key API')
execFileSync(
  zig,
  [
    'build',
    '--system',
    resolve(deps),
    '-Demit-lib-vt',
    '-Dtarget=wasm32-freestanding',
    '-Doptimize=ReleaseSmall',
    '--prefix',
    resolve(scratch, 'output'),
    '--cache-dir',
    resolve(scratch, 'cache'),
    '--global-cache-dir',
    resolve(scratch, 'global'),
    '-j2',
  ],
  { cwd: source, stdio: 'inherit' },
)
const asset = await readFile(resolve(scratch, 'output/bin/ghostty-vt.wasm'))
// Bind the ENTIRE native encoder output, not merely its C key enumeration.
if (sha256(asset) !== 'e7ddb75d97d435605ea32907544b34d18ebd421b5e885079f87c9c656e9c73c0')
  throw new Error('Native WASM differs from the reviewed pinned Ghostty encoder')
const keyEnum = header.slice(
  header.indexOf('    GHOSTTY_KEY_UNIDENTIFIED'),
  header.indexOf('    GHOSTTY_KEY_MAX_VALUE'),
)
const entries = [...keyEnum.matchAll(/^\s+GHOSTTY_KEY_([A-Z0-9_]+)(?: = 0)?,/gm)]
const keyCodes = Object.fromEntries(
  entries.map(([, name], index) => [
    name.length === 1
      ? `Key${name}`
      : name
          .split('_')
          .map((part) => part[0] + part.slice(1).toLowerCase())
          .join(''),
    index,
  ]),
)
const outputs = [
  ['assets/ghostty-key-encoder.generated.wasm', asset],
  [
    'ghosttyKeyCodes.generated.ts',
    `// GENERATED; do not edit. Ghostty ${revision} include/ghostty/vt/key/event.h.\n// Regenerate with generateGhosttyEncoder.mjs (see asset provenance).\n/** Native Ghostty key enumeration indexed by browser physical-key code. */\nexport const ghosttyKeyCodes: Readonly<Record<string, number>> = ${JSON.stringify(keyCodes, undefined, 2)}\n`,
  ],
  [
    'assets/ghostty-key-encoder.generated.json',
    JSON.stringify(
      {
        generated: true,
        reason:
          'Browser runtime bootstrap asset; the native encoder is used without a renderer or JS semantic port',
        revision,
        source: `https://github.com/ghostty-org/ghostty/tree/${revision}`,
        zig: '0.15.2',
        target: 'wasm32-freestanding',
        optimize: 'ReleaseSmall',
        wasmSha256: sha256(asset),
        keyHeaderSha256: sha256(header),
        regenerate:
          'node src/terminal/generateGhosttyEncoder.mjs <pinned-ghostty-source> <zig-0.15.2> <zig-system-deps> <bulk-scratch>',
        options:
          'Mirrored cursor/keypad/kitty flags. Native defaults for modes not published in TerminalModes. Explicit alt escape prefix for browser Alt shortcuts. Browser supplies committed IME text.',
      },
      undefined,
      2,
    ) + '\n',
  ],
  ['assets/Ghostty-LICENSE', await readFile(resolve(source, 'LICENSE'))],
]
for (const [name, bytes] of outputs) {
  const path = new URL(name, import.meta.url)
  const old = await readFile(path).catch(() => undefined)
  if (old && Buffer.compare(old, Buffer.from(bytes)) === 0) continue
  await chmod(path, 0o644).catch(() => {})
  await writeFile(path, bytes)
  await chmod(path, 0o444)
}
