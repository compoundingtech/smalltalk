import type { TimelineEntry } from '@smalltalk/st3-client'
import { ANCHOR, ANCHOR_MS, loadWorld, type World } from '../src/index.ts'
import { recordingIdentities, type JsonObject, type JsonValue, type RecordingRow, type RecordingScript, type RecordingTurn } from './recordings.ts'

export const NARRATIVE_RECORDINGS = ['fleet-mid-refactor', 'ci-failing-fix', 'long-debug-rabbit-hole', 'first-run-onboarding', 'merge-conflict-standoff', 'offline-reconnect-storm', 'release-candidate-polish'] as const

const json = (value: unknown): JsonValue => {
  if (value === null || typeof value === 'string' || typeof value === 'boolean') return value
  if (typeof value === 'number' && Number.isFinite(value)) return value
  if (Array.isArray(value)) return value.map((item: unknown) => json(item))
  if (typeof value === 'object' && value !== null) return Object.fromEntries(Object.entries(value).map(([key, item]) => [key, json(item)]))
  throw new Error('Expected JSON narrative content')
}
const body = (value: unknown): JsonObject => {
  const decoded = json(value)
  if (decoded !== null && typeof decoded === 'object' && !Array.isArray(decoded)) return decoded
  throw new Error('Expected narrative entry body')
}
const text = (value: JsonValue | undefined): string => typeof value === 'string' ? value : ''
const quote = (value: string): string => `'${value.replaceAll("'", "'\\''")}'`
/** Execute the observed story output in a real PTY, without requiring the fictional workspace. */
const outputCommand = (command: string, output: string): string => `printf '%s\\n' ${quote(`$ ${command}`)} ${quote(output)}`

const storyTurns = (world: World, owner: string): RecordingTurn[] => {
  const result: RecordingTurn[] = []
  for (const member of world.cast.agents.filter((a) => a.host.id === world.cast.agents.find((a) => a.id === owner)?.host.id)) {
    const entries: TimelineEntry[] = world.slices.conversation.state.threads.filter((t) => t.agent === member.id).flatMap((t) => t.items)
    for (const event of world.slices.conversation.timeline) {
      if ((event._tag === 'entries' || event._tag === 'replace') && event.agent === member.id) entries.push(...event.items)
    }
    const seen = new Set<string>()
    const previousContent: Record<string, string> = {}
    let prompt = world.cast.missions[0]?.title ?? world.title
    let timestamp: string | undefined
    for (const item of entries) {
      const revision = `${item.id}/${item.revision}`
      if (seen.has(revision)) continue
      seen.add(revision)
      const value = body(item.body)
      if (item.role === 'user') { prompt = text(value.text) || prompt; timestamp = item.timestamp; continue }
      if (item.role !== 'assistant') continue
      const answer = text(value.text)
      if (item.type === 'tool_call') {
        const call = text(value.call_id)
        const nativeResult = entries.find((entry) => entry.type === 'tool_result' && text(body(entry.body).call_id) === call)
        if (!nativeResult) throw new Error(`${world.id}: missing result for ${call}`)
        const nativeBody = body(nativeResult.body)
        const toolResult = nativeBody.content ?? null
        const input = value.arguments ?? null
        const command = input !== null && typeof input === 'object' && !Array.isArray(input) ? text(input.command) : ''
        result.push({ agent: member.id, prompt, answer: `Observed ${text(value.name)} result for ${prompt}.`, timestamp: timestamp ?? item.timestamp,
          tool: { id: call, name: text(value.name), input, result: toolResult },
          ...(member.id === owner && command ? { command: outputCommand(command, `${nativeBody.status === 'error' ? '\u001b[31m' : '\u001b[32m'}${typeof toolResult === 'string' ? toolResult : JSON.stringify(toolResult)}\u001b[0m`) } : {}) })
      } else if (answer) {
        const previous = previousContent[item.id]
        result.push({ agent: member.id, prompt, answer, timestamp: timestamp ?? item.timestamp,
          ...(previous !== undefined && previous !== answer ? { revisions: [previous] } : {}) })
        previousContent[item.id] = answer
      }
      timestamp = undefined
    }
  }
  return result.sort((a, b) => Date.parse(a.timestamp) - Date.parse(b.timestamp))
}

/** Cast and story data stay authoritative in the existing synthetic world definitions. */
export const narrativeScript = (id: typeof NARRATIVE_RECORDINGS[number], rows: readonly RecordingRow[] = []): RecordingScript => {
  const world = loadWorld(id, { now: ANCHOR_MS })
  const terminal = world.slices.terminal.state.terminals[0] ?? world.slices.terminal.timeline.find((event) => event._tag === 'terminal-create')?.record
  const primary = terminal ? world.cast.agents.find((a) => a.id === terminal.owner) : world.cast.agents[0]
  const person = world.cast.people[0]
  if (!primary || !person) throw new Error(`${id}: missing declared cast`)
  const members = world.cast.agents.filter((a) => a.host.id === primary.host.id)
  let turns = storyTurns(world, primary.id)
  if (id === 'long-debug-rabbit-hole') {
    // Native historical insertion causes real replacement pages: recording every repeated
    // middle batch exceeds the input budget. Keep both ends of each rejected hypothesis.
    const selected = new Set<number>()
    for (const [index, turn] of turns.entries()) {
      if (turn.answer.startsWith('Wrong hypothesis ')) for (let offset = 0; offset < 3; offset++) selected.add(index + offset)
      if (turn.answer.startsWith('Rejected hypothesis ')) for (let offset = -2; offset <= 0; offset++) selected.add(index + offset)
      if (turn.answer.startsWith('Forty minutes of evidence:') || Date.parse(turn.timestamp) >= ANCHOR_MS) selected.add(index)
    }
    turns = turns.filter((_turn, index) => selected.has(index))
  }
  if (id === 'release-candidate-polish') {
    const docs = members.find((a) => a.role === 'docs')
    if (docs) turns.push({ agent: docs.id, prompt: world.cast.missions[0]?.steps[1]?.title ?? 'Write the changelog', answer: 'Release notes for 0.8.4 are complete: preserve selected tabs when reopening settings and clarify offline recovery. No API changes or remaining release gates.', timestamp: new Date(ANCHOR_MS - 180_000).toISOString() })
  }
  if (terminal && !turns.some((turn) => turn.command)) {
    const output = terminal.cast.events.map((event) => event[2]).join('').replaceAll('\r', '')
    const selected = turns.map((turn, index) => ({ turn, index })).filter(({ turn }) => turn.agent === primary.id).at(-1)
    if (selected) turns[selected.index] = { ...selected.turn, command: outputCommand(id === 'release-candidate-polish' ? 'pnpm release:verify --version 0.8.4' : world.title, output) }
  }
  if (id === 'offline-reconnect-storm') {
    const last = turns.at(-1)
    if (last) turns[turns.length - 1] = { ...last, command: outputCommand('deployment health report', last.answer) }
  }
  turns.sort((a, b) => Date.parse(a.timestamp) - Date.parse(b.timestamp))
  const historical_turns = id === 'release-candidate-polish' ? turns.length : turns.filter((turn) => Date.parse(turn.timestamp) < ANCHOR_MS).length
  const future = turns.map((turn, index) => ({ turn, index })).filter(({ index }) => index >= historical_turns)
  const reconnect_after = id === 'offline-reconnect-storm' ? [2_000, 8_000, 16_000].map((offset) => {
    const preceding = future.filter(({ turn }) => Date.parse(turn.timestamp) - ANCHOR_MS <= offset).at(-1)
    if (!preceding) throw new Error(`${id}: missing reconnect turn at ${offset}`)
    return preceding.index
  }) : []
  const id_map: Record<string, string> = {}
  for (const member of world.cast.agents) for (const identity of [member.id, member.host.id, member.session, member.terminal, member.runtime, member.terminalRuntime]) id_map[identity] = identity
  for (const member of world.cast.people) id_map[member.id] = member.id
  id_map['action/example-attach'] = 'action/example-attach'
  id_map[`terminal/${primary.id}`] = primary.terminal
  const script: RecordingScript = { format_version: 2, anchor: ANCHOR, world: id, agent: primary.id, host: primary.host.id, person: person.id, workspace: primary.workspace,
    agents: members.map((a) => ({ agent: a.id, host: a.host.id })), historical_turns, empty_baseline: id === 'first-run-onboarding', reconnect_after, id_map, turns,
    ...(id === 'long-debug-rabbit-hole' ? { walk_pages: true } : {}),
    reads: ['/v1/client/agents', ...members.map((a) => `/v1/client/sessions/${a.id}/timeline${id === 'long-debug-rabbit-hole' ? '?limit=50' : ''}`), `/v1/client/terminals/${encodeURIComponent(primary.id)}/screen`] }
  if (rows.length) {
    const subscriptions: Record<string, string> = {}
    for (const row of rows) {
      if (row.kind === 'command' && row.value.kind === 'subscribe' && row.value.collection === 'conversation') subscriptions[text(row.value.id)] = text(row.value.conversation)
      if (row.kind === 'frame' && row.value.kind === 'conversation') {
        const member = members.find((a) => a.id === subscriptions[text(row.value.id)])
        if (member && typeof row.value.session_id === 'string') id_map[row.value.session_id] = member.session
      }
    }
    let next = 0
    for (const identity of recordingIdentities(script, rows)) {
      if (id_map[identity] !== undefined) continue
      id_map[identity] = `${identity.includes('/') ? identity.split('/')[0] : 'message'}/scenario-${id}/captured-${++next}`
    }
  }
  return script
}
