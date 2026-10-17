import { createHash } from 'node:crypto'
import { readFile, readdir } from 'node:fs/promises'
import { join } from 'node:path'
import { pathToFileURL } from 'node:url'

export function shardNumber(id, count) {
  if (!Number.isSafeInteger(count) || count < 1) throw new Error('Shard count must be a positive integer')
  return Number(BigInt(`0x${createHash('sha256').update(id).digest('hex')}`) % BigInt(count))
}

export function shardStories(ids, index, count) {
  shardNumber('', count)
  if (!Number.isSafeInteger(index) || index < 0 || index >= count) throw new Error('Invalid shard index')
  if (!ids.length || new Set(ids).size !== ids.length) throw new Error('Invalid full story index')
  const selected = ids.filter(id => shardNumber(id, count) === index)
  if (!selected.length && count <= ids.length) throw new Error(`Unexpected empty shard ${index}/${count}`)
  return selected
}

export function shardArtifactName(receipt) {
  return `kit-play-${receipt.runId}-${receipt.headSha}-shard-${receipt.shardIndex}-attempt-${receipt.runAttempt}`
}

export function aggregateShards(candidates, count, provenance) {
  const { runId, headSha, runAttempt } = provenance
  if (!/^\d+$/.test(runId) || !/^[0-9a-f]{40}$/.test(headSha) || !Number.isSafeInteger(runAttempt) || runAttempt < 1) throw new Error('Invalid expected workflow provenance')
  const newest = new Map()
  const attempts = new Set()
  for (const receipt of candidates) {
    if (receipt.runId !== runId || receipt.headSha !== headSha || !Number.isSafeInteger(receipt.runAttempt) || receipt.runAttempt < 1 || receipt.runAttempt > runAttempt) throw new Error('Shard workflow provenance mismatch')
    if (!Number.isSafeInteger(receipt.shardIndex) || receipt.shardIndex < 0 || receipt.shardIndex >= count) throw new Error('Invalid shard identity')
    const key = `${receipt.shardIndex}/${receipt.runAttempt}`
    if (attempts.has(key)) throw new Error('Duplicate shard attempt')
    attempts.add(key)
    if (!newest.has(receipt.shardIndex) || newest.get(receipt.shardIndex).runAttempt < receipt.runAttempt) newest.set(receipt.shardIndex, receipt)
  }
  // Rerun-failed-jobs retains successful jobs from older attempts of this exact run/head.
  // Never fall back to an older success when a newer receipt exists for that shard.
  const receipts = [...newest.values()].sort((a, b) => a.shardIndex - b.shardIndex)
  if (receipts.length !== count) throw new Error(`Expected ${count} shard receipts; got ${receipts.length}`)
  const full = receipts[0].fullIds
  if (!Array.isArray(full)) throw new Error('Missing full index')
  const seen = new Set()
  const allCases = new Set()
  for (const receipt of receipts) {
    const { shardIndex, shardCount, rows } = receipt
    if (!receipt.pass || shardCount !== count || seen.has(shardIndex)) throw new Error('Failed, duplicate or mismatched shard')
    seen.add(shardIndex)
    if (JSON.stringify(receipt.fullIds) !== JSON.stringify(full)) throw new Error('Shards validated different full indexes')
    const expected = shardStories(full, shardIndex, count)
    if (!Array.isArray(rows) || rows.length !== expected.length * 4) throw new Error('Incomplete shard cases')
    const assigned = new Set(expected)
    for (const row of rows) {
      const key = JSON.stringify([row.id, row.scheme, row.width])
      if (!row.pass || !assigned.has(row.id) || !['dark', 'light'].includes(row.scheme) || ![440, 1440].includes(row.width) || allCases.has(key)) throw new Error('Missing, duplicate or invalid play case')
      allCases.add(key)
    }
  }
  if (allCases.size !== full.length * 4) throw new Error('Shard union does not cover the full index')
  return { stories: full.length, cases: allCases.size, shards: count, pass: true }
}

if (process.argv[1] && import.meta.url === pathToFileURL(process.argv[1]).href) {
  const [directory, count] = process.argv.slice(2)
  const names = await readdir(directory)
  const receipts = await Promise.all(names.map(async name => {
    const receipt = JSON.parse(await readFile(join(directory, name, 'receipt.json'), 'utf8'))
    if (name !== shardArtifactName(receipt)) throw new Error('Artifact name does not match receipt provenance')
    return receipt
  }))
  console.log(JSON.stringify(aggregateShards(receipts, Number(count), {
    runId: process.env.GITHUB_RUN_ID, headSha: process.env.GITHUB_SHA,
    runAttempt: Number(process.env.GITHUB_RUN_ATTEMPT),
  })))
}
