import assert from 'node:assert/strict'
import { mkdtemp, writeFile, rm, symlink } from 'node:fs/promises'
import { tmpdir } from 'node:os'
import { join } from 'node:path'
import { spawnSync } from 'node:child_process'
import { test } from 'node:test'
import { generateWorld } from './world.mjs'
import { forbiddenLine, scanFiles } from './scan.mjs'

const policy = ['Actual Operator', 'agent/example-company/real-worker']
test('the seed controls the world and rejects lossy numeric inputs', () => {
  assert.deepEqual(generateWorld(138), generateWorld(138))
  assert.notDeepEqual(generateWorld(138), generateWorld(139))
  for (const seed of [-1, 1.5, 2 ** 32, NaN, '138']) assert.throws(() => generateWorld(seed))
})
test('each deny category fails, including escaped and URL-encoded identities', () => {
  const forbidden = [
    'dev' + '3', 'dev' + '5', 'mb' + 'p-laptop', 'node.tail' + '123.ts' + '.net',
    '/ho' + 'me/operator/file', '/Us' + 'ers/operator/file', '/run/' + 'user/1000',
    'Actual Operator', 'agent/example-company/real-worker',
    'sk-' + 'aB7'.repeat(12), 'ghp_' + 'aB7'.repeat(12), 'github' + '_pat_' + 'aB7'.repeat(12),
    'AKIA' + 'ABCDEF0123456789', 'xox' + 'b-' + 'a1b2c3d4e5f6g7',
    'eyJ' + 'a'.repeat(12) + '.' + 'b'.repeat(12) + '.' + 'c'.repeat(12),
    '-----BEGIN PRIV' + 'ATE KEY-----', 'pass' + 'word="' + 'aB7'.repeat(12) + '"',
    'Auth' + 'orization: Bearer ' + 'aB7'.repeat(12), 'https://name:' + 'password@example.invalid',
    'https://cache.' + 'ca' + 'chix.org/serve/artifact', 'https://npm.pkg.' + 'github.com/package',
    '\\u002fho' + 'me/operator', '%2Fho' + 'me%2Foperator', 'https://artifacts.example.invalid/pkg' + '.tgz',
  ]
  forbidden.forEach((value, i) => assert.equal(forbiddenLine(value, policy), true, `Deny category ${i}`))
  for (const value of ['https://gateway.example.invalid', '/srv/work/sample', 'person/operator', 'preview_token: null'])
    assert.equal(forbiddenLine(value, policy), false)
  assert.equal(forbiddenLine('Actual Operator', undefined), false, 'identities need an owner-held policy')
  assert.equal(forbiddenLine('/ho' + 'me/operator', undefined), true, 'generic rules apply without a policy')
})
test('CLI fails with file and line only, and rejects binary or symlink bypasses', async () => {
  const dir = await mkdtemp(join(tmpdir(), 'synthetic-privacy-'))
  try {
    const file = join(dir, 'input.txt')
    const policyPath = join(dir, 'policy.json')
    const candidate = 'sk-' + 'aB7'.repeat(12)
    await writeFile(file, `safe\n${candidate}\nsafe\n`)
    await writeFile(policyPath, JSON.stringify(policy))
    assert.deepEqual(await scanFiles([file], policy), [{ file, line: 2 }])
    const run = spawnSync(process.execPath, [new URL('./scan.mjs', import.meta.url).pathname, '--policy', policyPath, file], { encoding: 'utf8' })
    assert.equal(run.status, 1)
    assert.equal(run.stderr.trim(), `${JSON.stringify(file)}:2`)
    assert.equal((run.stderr + run.stdout).includes(candidate), false)
    await writeFile(file, Buffer.from([0xff, 0, 0x13]))
    assert.deepEqual(await scanFiles([file], policy), [{ file, line: 1 }])
    const link = join(dir, 'linked.txt')
    await symlink(file, link)
    assert.deepEqual(await scanFiles([link], policy), [{ file: link, line: 1 }])
    await assert.rejects(scanFiles([file], []))
  } finally { await rm(dir, { recursive: true, force: true }) }
})
