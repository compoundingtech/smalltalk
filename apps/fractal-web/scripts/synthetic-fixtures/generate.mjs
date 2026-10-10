import { mkdir, writeFile } from 'node:fs/promises'
import { resolve } from 'node:path'
import { parseArgs } from 'node:util'
import { generateWorld } from './world.mjs'

const { values } = parseArgs({ options: { seed: { type: 'string', default: '138' }, out: { type: 'string' } } })
if (!values.out || !/^\d+$/.test(values.seed)) throw new Error('Usage: node generate.mjs --seed UINT32 --out DIRECTORY')
const fixtures = generateWorld(Number(values.seed))
await mkdir(values.out, { recursive: true })
for (const [name, value] of Object.entries(fixtures))
  await writeFile(resolve(values.out, name), `${JSON.stringify(value, null, 2)}\n`, { mode: 0o600 })
console.log(`Generated ${Object.keys(fixtures).length} synthetic fixture files`)
