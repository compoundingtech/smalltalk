import assert from 'node:assert/strict'
import { mkdtemp, rm, readFile } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { fileURLToPath, pathToFileURL } from 'node:url'

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
