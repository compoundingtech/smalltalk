import assert from 'node:assert/strict'
import { readFile } from 'node:fs/promises'
import { resolve } from 'node:path'
import { pathToFileURL } from 'node:url'
import { parseArgs } from 'node:util'

// The public gate decodes generated fixtures with this repository's own canonical client schema.
// The roster-slice and gateway-review consumers join when the application import lands; pass
// --roster/--review then to extend the same checks to their exact decoders.
const { values } = parseArgs({ options: {
  fixtures: { type: 'string' }, native: { type: 'string' },
  roster: { type: 'string' }, review: { type: 'string' },
} })
if (!values.fixtures || !values.native)
  throw new Error('Usage: node verify.mjs --fixtures DIRECTORY --native SDK_SCHEMA_TS [--roster ROSTER_SCHEMA_TS] [--review REVIEW_TS]')
const Native = await import(pathToFileURL(resolve(values.native)).href)
const load = async (file) => JSON.parse(await readFile(resolve(values.fixtures, file), 'utf8'))
const conversations = await load('gateway.conversations.json')
for (const [file, schema] of [['agents.json', Native.Agent], ['runtimes.json', Native.Runtime], ['attention.json', Native.Attention]])
  for (const item of await load(file)) Native.decodeUnknownSync(schema)(item)
const capture = await load('gateway.capture.json')
for (const mission of capture.missions) Native.decodeUnknownSync(Native.Mission)(mission)
for (const conversation of Object.values(conversations)) {
  Native.decodeUnknownSync(Native.Id)(conversation.session_id)
  for (const entry of conversation.items) Native.decodeUnknownSync(Native.TimelineEntry)(entry)
}
for (const screen of await load('terminal-frames.json')) Native.decodeUnknownSync(Native.TerminalScreen)(screen)
if (values.roster || values.review) {
  if (!values.roster || !values.review) throw new Error('--roster and --review are required together')
  const { decodeRosterSnapshot, sanitizeRosterSnapshot, snapshotAgents } = await import(pathToFileURL(resolve(values.roster)).href)
  const roster = await load('roster-snapshot-v1.json')
  assert.deepEqual(decodeRosterSnapshot(roster), roster)
  assert.deepEqual(sanitizeRosterSnapshot(roster), roster)
  snapshotAgents(roster)
  const { gatewayReviewData } = await import(pathToFileURL(resolve(values.review)).href)
  for (const [agent, page] of Object.entries(gatewayReviewData.conversations)) {
    assert.equal(page._tag, 'Captured', `Synthetic conversation contract for ${agent}`)
    assert.equal(page.page.session_id, conversations[agent].session_id)
  }
}
// Every generated identity is namespaced to the synthetic world: no observed ref can appear.
const families = /^(?:agent|person|host|mission|mission-run|runtime|session|terminal|incarnation|operation|step-run|run-generation|snapshot|subscription|claim|timeline-entry|message|attention|rev|screen)\/synthetic-\d+(?:[/-]|$)/
const refs = []
// Any slash-separated reference string (one or more segments), including object keys such as
// run-generation maps and per-agent conversation pages. Media types are not identities.
const collect = (text) => {
  if (/^[a-z][a-z-]*\/\S+$/.test(text) && !/^(?:text|application|image|audio|video|multipart|font)\//.test(text)) refs.push(text)
}
const walk = (value) => {
  if (typeof value === 'string') collect(value)
  else if (Array.isArray(value)) value.forEach(walk)
  else if (value !== null && typeof value === 'object')
    for (const [key, child] of Object.entries(value)) { collect(key); walk(child) }
}
for (const file of ['roster-snapshot-v1.json', 'agents.json', 'runtimes.json', 'attention.json', 'gateway.capture.json', 'gateway.conversations.json', 'terminal-frames.json', 'gateway.graphs.json', 'gateway.requests.json'])
  walk(await load(file))
assert(refs.some((ref) => ref.startsWith('agent/')), 'Namespace check found no agent references')
for (const ref of refs) if (ref !== 'person/operator') assert.match(ref, families, `Generated identity escaped the synthetic namespace`)
// Consumer-visible cross-file invariants, independent of the generator's implementation.
const roster = await load('roster-snapshot-v1.json')
const { agents, missions, runtimes, attention } = roster.collections
for (const agent of agents.items) {
  for (const id of agent.runtime_ids) assert.equal(runtimes.items.find((runtime) => runtime.id === id)?.owner_id, agent.id)
  for (const work of agent.current_work) assert(missions.items.find((mission) => mission.id === work.mission_id)?.runs.includes(work.mission_run_id))
  if (agent.current_session_id) assert.equal(conversations[agent.id]?.session_id, agent.current_session_id)
}
for (const item of attention.items) {
  if (item.mission_id) assert(missions.items.some((mission) => mission.id === item.mission_id && mission.runs.includes(item.mission_run_id)))
  for (const target of item.targets) assert(target.startsWith('person/') || agents.items.some((agent) => agent.id === target))
}
for (const screen of await load('terminal-frames.json'))
  assert(runtimes.items.some((runtime) => runtime.terminal_id === screen.terminal_id && runtime.incarnation_id === screen.runtime_incarnation))
console.log('PASS: canonical native decoders, synthetic namespace, linked references')
