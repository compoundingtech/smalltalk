import assert from 'node:assert/strict'
import { fileURLToPath } from 'node:url'
import { test } from 'node:test'
import { build } from 'vite'
import { assertExtensionBundle, extensionBuildPlugin } from '../../scripts/extension-build.mjs'

const entry = fileURLToPath(new URL('./build.ts', import.meta.url))
const fixture = fileURLToPath(new URL('./fixtures/synthetic-extension.ts', import.meta.url))

test('real bundles exclude unselected extension code and include selected local code', async () => {
  for (const injected of [false, true]) {
    const output = await build({
      configFile: false,
      logLevel: 'silent',
      plugins: injected ? [extensionBuildPlugin({ entry, composition: fixture })] : [],
      build: {
        write: false,
        minify: true,
        sourcemap: true,
        lib: { entry: fileURLToPath(new URL('./fixtures/entry.ts', import.meta.url)), formats: ['es'] },
      },
    })
    const files = (Array.isArray(output) ? output : [output]).flatMap((item) => item.output)
    const bundle = Object.fromEntries(files.map((item) => [item.fileName, item]))
    assertExtensionBundle({
      bundle,
      forbiddenModules: injected ? [] : [/synthetic-extension\.ts$/],
      forbiddenContent: injected ? [] : ['compiled-extension-proof'],
      requiredModules: injected ? [/synthetic-extension\.ts$/] : [/public\.ts$/],
      requiredContent: injected ? ['compiled-extension-proof/pane', 'compiled-extension-proof/host'] : [],
    })
    assert.ok(files.some((item) => item.type === 'chunk'), 'actual code was emitted')
  }
})

test('the content check includes lazy chunks and source-map assets', () => {
  for (const artifact of [
    { type: 'chunk', modules: {}, code: 'excluded-extension-proof' },
    { type: 'asset', source: 'excluded-extension-proof' },
    { type: 'asset', source: new TextEncoder().encode('excluded-extension-proof') },
  ]) assert.throws(() => assertExtensionBundle({
    bundle: { 'secondary-output': artifact }, forbiddenModules: [], forbiddenContent: ['excluded-extension-proof'],
  }), /Excluded extension content/)
})

test('a missing injected module fails even when its marker appears elsewhere', () => {
  assert.throws(() => assertExtensionBundle({
    bundle: { 'main.js': { type: 'chunk', modules: {}, code: 'compiled-extension-proof' } },
    forbiddenModules: [], forbiddenContent: [], requiredModules: [/synthetic-extension\.ts$/],
  }), /Required extension module/)
})
