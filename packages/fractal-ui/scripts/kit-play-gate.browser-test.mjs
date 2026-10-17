import { test } from 'node:test'
import assert from 'node:assert/strict'
import { mkdtemp, mkdir, writeFile, readFile, rm } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { spawn } from 'node:child_process'
import { fileURLToPath } from 'node:url'

// A one-story fixture build. Like Storybook's addon-a11y, it reports an a11y
// result unless the a11y.manual global is set; `report` overrides that report.
const iframe = report => `<!doctype html><meta charset="utf-8"><title>Gate fixture</title>
<script>
  const listeners = new Map()
  const manual = new URLSearchParams(location.search).get('globals') === 'a11y.manual:!true'
  window.__STORYBOOK_ADDONS_CHANNEL__ = {
    on(name, listener) {
      listeners.set(name, listener)
      if (name === 'storyErrored') queueMicrotask(() => listeners.get('storyFinished')({
        storyId: new URLSearchParams(location.search).get('id'), status: 'success',
        reporters: ${report === undefined ? "manual ? [] : [{ type: 'a11y', status: 'passed' }]" : JSON.stringify(report)},
      }))
    },
  }
</script>`

const runGate = async ({ env = {}, report } = {}) => {
  const directory = await mkdtemp(join(tmpdir(), 'kit-play-gate-test-'))
  try {
    const build = join(directory, 'build'), output = join(directory, 'receipt')
    await mkdir(build)
    await writeFile(join(build, 'index.json'), JSON.stringify({ entries: {
      critical: { type: 'story', id: 'kit--critical', importPath: './Kit.stories.tsx', tags: ['dev', 'critical'] },
      other: { type: 'story', id: 'kit--other', importPath: './Kit.stories.tsx', tags: ['dev'] },
    } }))
    await writeFile(join(build, 'iframe.html'), iframe(report))
    // The job's own KIT_PLAY_* options (CI sets them per event) must not reach the fixture runs.
    const inherited = Object.fromEntries(Object.entries(process.env).filter(([name]) => !name.startsWith('KIT_PLAY_')))
    const child = spawn(process.execPath, [fileURLToPath(new URL('./kit-play-gate.mjs', import.meta.url)), build, output], {
      env: { ...inherited, ...env },
      stdio: 'ignore',
    })
    const code = await new Promise((resolve, reject) => {
      child.once('error', reject)
      child.once('close', resolve)
    })
    return { code, receipt: JSON.parse(await readFile(join(output, 'receipt.json'), 'utf8')) }
  } finally {
    await rm(directory, { recursive: true, force: true })
  }
}

test('full set with a11y requires one passed a11y report per case', { timeout: 60000 }, async () => {
  // Empty values are unset, as a workflow expression with no value yields them.
  const { code, receipt } = await runGate({ env: { KIT_PLAY_SET: '', KIT_PLAY_A11Y: '', KIT_PLAY_MAX_SECONDS: '' } })
  assert.equal(code, 0)
  assert.equal(receipt.pass, true)
  assert.deepEqual([receipt.set, receipt.a11y, receipt.maxSeconds, receipt.stories, receipt.cases], ['all', 'on', null, 2, 8])
  assert.ok(receipt.cpu.count > 0 && receipt.cpu.busyCoreMinutes >= 0 && receipt.cpu.loadavgEnd.length === 3)
})

test('a11y on rejects a case whose axe report is missing', { timeout: 60000 }, async () => {
  const { code, receipt } = await runGate({ report: [] })
  assert.equal(code, 1)
  assert.equal(receipt.pass, false)
  assert.ok(receipt.rows.every(row => /expected one passed a11y report/.test(row.error)))
})

test('critical set without a11y selects tagged stories and asks addon-a11y not to run', { timeout: 60000 }, async () => {
  const { code, receipt } = await runGate({ env: { KIT_PLAY_SET: 'critical', KIT_PLAY_A11Y: 'off' } })
  assert.equal(code, 0)
  assert.deepEqual([receipt.set, receipt.a11y, receipt.stories, receipt.cases], ['critical', 'off', 1, 4])
  assert.ok(receipt.rows.every(row => row.id === 'kit--critical' && row.strict.every(record => record.url.includes('globals=a11y.manual'))))
})

test('the per-commit set refuses a build whose critical stories are not the ones it names', { timeout: 60000 }, async () => {
  const { code, receipt } = await runGate({ env: { KIT_PLAY_SET: 'commit', KIT_PLAY_A11Y: 'off' } })
  assert.equal(code, 1)
  assert.equal(receipt.pass, false)
  assert.equal(receipt.cases, 0)
  assert.match(receipt.fatalError, /names fractal-ui-.*, which is not a critical story/)
})

test('a11y off rejects a case where axe still ran', { timeout: 60000 }, async () => {
  const { code, receipt } = await runGate({ env: { KIT_PLAY_A11Y: 'off' }, report: [{ type: 'a11y', status: 'passed' }] })
  assert.equal(code, 1)
  assert.ok(receipt.rows.every(row => /a11y off: axe still ran/.test(row.error)))
})

test('a run over its wall-time budget fails after running every case', { timeout: 60000 }, async () => {
  const { code, receipt } = await runGate({ env: { KIT_PLAY_SET: 'critical', KIT_PLAY_MAX_SECONDS: '0.001' } })
  assert.equal(code, 1)
  assert.equal(receipt.pass, false)
  assert.equal(receipt.overBudget, true)
  assert.equal(receipt.maxSeconds, 0.001)
  assert.equal(receipt.cases, 4)
  assert.ok(receipt.rows.every(row => row.pass))
})

test('invalid options fail before any case runs', { timeout: 60000 }, async () => {
  for (const env of [{ KIT_PLAY_SET: 'some' }, { KIT_PLAY_A11Y: 'no' }, { KIT_PLAY_MAX_SECONDS: '-1' }, { KIT_PLAY_MAX_SECONDS: 'soon' }]) {
    await assert.rejects(() => runGate({ env }), /ENOENT/)
  }
})
