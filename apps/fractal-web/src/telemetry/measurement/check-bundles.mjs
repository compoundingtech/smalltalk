import assert from 'node:assert/strict'
import { mkdtemp, rm, readFile, readdir } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'

// The CI production build passes its actual output directory; scan all emitted JS/CSS,
// including lazy chunks. These are stable literals from devbar and meters, not minified names.
const diagnosticMarkers = [
  '@overeng/devbar', '@overeng/meters', 'Developer tools',
  'useMeters requires a MetersProvider', 'Browser performance clock is unavailable',
  'Browser animation frames are unavailable', 'wf.longFrames', 'JS heap (approx.)',
]
if (process.argv[2] !== undefined) {
  const assets = join(process.argv[2], 'assets')
  const files = (await readdir(assets)).filter((name) => /\.(js|css)$/.test(name))
  assert.ok(files.length > 0, 'production assets must exist')
  for (const name of files) {
    assert.equal(/devbar|meters/i.test(name), false, `${name}: diagnostic chunk`)
    const text = await readFile(join(assets, name), 'utf8')
    for (const marker of diagnosticMarkers) {
      assert.equal(text.includes(marker), false, `${name}: diagnostic marker ${marker}`)
    }
  }
  console.log(JSON.stringify({ diagnosticBundleExclusion: { files: files.length, markers: diagnosticMarkers, hits: 0 } }))
  process.exit(0)
}
// Run with Bun. Its compile-time defines exercise the same boolean boundary as Vite.
const directory = await mkdtemp(join(tmpdir(), 'fractal-measurement-'))
const entry = fileURLToPath(new URL('./index.ts', import.meta.url))
const developerCode = /HUD|Renders\.|requestAnimationFrame|createElement|setInterval|beginMeasure|recordCommit|mountMeasurementHud/
try {
  const builds = []
  for (const [mode, dev, perf] of [['production', false, false], ['development', true, false], ['perf', false, true]]) {
    const outfile = join(directory, `${mode}.js`)
    const result = await Bun.build({
      entrypoints: [entry], target: 'browser', minify: true,
      outdir: directory, naming: `${mode}.js`,
      define: { 'import.meta.env.DEV': String(dev), 'import.meta.env.PERF': String(perf) },
    })
    assert.equal(result.success, true, result.logs.join('\n'))
    const text = await readFile(outfile, 'utf8')
    assert.equal(developerCode.test(text), dev || perf, `${mode}: development code boundary`)
    const bundle = await import(pathToFileURL(outfile).href)
    bundle.incrDebug('Wf.frames', 24)
    bundle.setDebug('Wf.decodeMs', 2.5)
    assert.equal(bundle.getDebug('Wf.frames'), 24)
    assert.equal(bundle.getDebug('Wf.decodeMs'), 2.5)
    if (mode === 'production') assert.equal(bundle.developmentMeasurements, undefined)
    else {
      bundle.developmentMeasurements.recordCommit('sidebar')
      assert.equal(bundle.getDebug('Renders.sidebar'), 1)
    }
    builds.push({ mode, bytes: Buffer.byteLength(text), developerCode: developerCode.test(text) })
  }
  console.log(JSON.stringify({ bundleExclusion: builds }))
} finally {
  await rm(directory, { recursive: true, force: true })
}
