/** Independent synthetic data. No capture is read, transformed, hashed or renamed. */
export const generateWorld = (seed = 138) => {
  if (!Number.isInteger(seed) || seed < 0 || seed > 0xffffffff)
    throw new Error('Seed must be an unsigned 32-bit integer')
  let state = seed
  const random = () => {
    state = (Math.imul(state, 1664525) + 1013904223) >>> 0
    return state
  }
  const now = Date.UTC(2032, 0, 1) + (random() % 365) * 86400000
  const at = (seconds = 0) => new Date(now - seconds * 1000).toISOString()
  const ref = (kind, n) => `${kind}/synthetic-${seed}/${n}`
  const header = (kind, n) => ({ kind, id: ref(kind, n), revision: `rev/synthetic-${seed}-${n}`, updated_at: at() })
  // The repository's reviewed invented operator; agents and every other identity carry the seed.
  const person = 'person/operator'
  const host = ref('host', 1)
  const agents = Array.from({ length: 5 }, (_, i) => {
    const n = i + 1
    const live = i < 3
    return {
      ...header('agent', n), name: `Worker ${n}`,
      state: ['running', 'waiting', 'running', 'suspended', 'failed'][i],
      reachability: live ? 'reachable' : 'unreachable',
      runtime_ids: live ? [ref('runtime', n)] : [], owner_run_id: live ? ref('mission-run', n) : null,
      driver: live ? 'omp' : null, harness_state: live ? (i === 1 ? 'waiting' : 'working') : null,
      observation: live ? (i === 2 ? 'stale' : 'current') : 'missing',
      since: live ? at(120) : null, blocked_on: i === 1 ? 'human' : null,
      host_id: i === 3 ? ref('host', 2) : host, fault: i === 4 ? 'Synthetic process failure' : null,
      current_session_id: live ? ref('session', n) : null,
      last_activity_at: live ? at(i === 2 ? 180 : 10) : null, silent_since: live ? null : at(600),
      delivery: live ? { state: 'live', reason: null, polled_seconds_ago: 2 } : null,
      handoff: live ? { phase: 'running' } : null,
      current_work: live ? [{ id: ref('step-run', n), mission_id: ref('mission', n),
        mission_run_id: ref('mission-run', n), path: 'assemble', title: `Assemble sample ${n}`,
        state: i === 1 ? 'waiting' : 'claimed', since: at(120) }] : [],
      suspension: i === 3 ? { action: 'suspend', phase: 'suspended', suspended_at: at(300) } : null,
    }
  })
  const runtimes = agents.slice(0, 3).map((agent, i) => ({
    ...header('runtime', i + 1), owner_id: agent.id, owner_host_id: host,
    runtime_id: `synthetic-process-${seed}-${i + 1}`, terminal_id: ref('terminal', i + 1),
    incarnation_id: ref('incarnation', i + 1), state: 'running',
  }))
  const missions = agents.slice(0, 3).map((agent, i) => ({
    ...header('mission', i + 1), title: `Sample assembly ${i + 1}`, state: 'running',
    runs: [ref('mission-run', i + 1)], run_details: [{ id: ref('mission-run', i + 1), status: i === 1 ? 'waiting' : 'running' }],
  }))
  const attention = [
    { ...header('attention', 1), state: 'open', attention_kind: 'agent-request', requester_id: agents[1].id,
      source_id: agents[1].id, targets: [person], mission_run_id: missions[1].runs[0], mission_id: missions[1].id },
    { ...header('attention', 2), state: 'pending', attention_kind: 'unread-message', requester_id: null,
      source_id: person, targets: [agents[0].id], mission_run_id: null, mission_id: null },
  ]
  const collections = Object.fromEntries(Object.entries({ agents, missions, runtimes, attention })
    .map(([key, items], i) => [key, { storeIndex: 100 + (random() % 50) + i, items }]))
  const roster = { recordVersion: 1, endpoint: 'https://gateway.example.invalid', hostId: host,
    projectionVersion: 'client-projection.v0', capturedAt: now, collections }
  const nativeAgents = agents.map(({ observation, ...agent }, i) => ({
    ...agent, observation, incarnation_id: agent.runtime_ids.length ? ref('incarnation', i + 1) : null,
    ask: agent.blocked_on === 'human' ? 'question' : null, reason: null,
    delivery: agent.delivery ? { ...agent.delivery, state: 'current' } : null,
    handoff: agent.handoff ? { ...agent.handoff, desired_token: ref('claim', i + 1),
      destination: host, pending_sources: [], sources: [host] } : null,
    suspension: agent.suspension ? { ...agent.suspension, blocking: [], code: null,
      harness: 'omp', incarnation_id: ref('incarnation', i + 1), native_session_id: ref('session', i + 1),
      operation_id: ref('operation', i + 1), reason: null, updated_at: at() } : null,
    current_work_ids: agent.current_work.map((work) => work.id), active_work_count: agent.current_work.length,
    next_work_id: null, upcoming_work_ids: [], queued_work_count: 0, next_work: null, upcoming_work: [], under: [],
  }))
  // Rich native detail is projected separately: persistence must never retain these fields.
  const nativeRuntimes = runtimes.map((runtime) => ({ ...runtime, runtime_kind: 'agent',
    desired_revision: runtime.revision, owner_run_id: agents.find((agent) => agent.id === runtime.owner_id).owner_run_id,
    terminal_sequence: 0, terminal_access: { read: 'granted', input: 'ungranted', resize: 'granted' } }))
  const nativeMissions = missions.map((mission, i) => {
    const run = mission.run_details[0]
    const work = agents[i].current_work[0]
    return { ...mission, mission_revision: mission.revision, must_act: i === 1 ? 'you' : 'agent',
      active_runs: 1, total_runs: 1, runs_truncated: false, run_counts: { [run.status]: 1 },
      run_generations: { [run.id]: ref('run-generation', i + 1) }, visualization: null, usage: null,
      run_details: [{ ...run, generation_id: ref('run-generation', i + 1), requester: person,
        phase: 'execution', progress: { done: i, total: 4 },
        current_steps: [{ id: work.id, assignee: agents[i].id, claimant: agents[i].id,
          since: work.since, state: work.state, title: work.title }],
        must_act: i === 1 ? 'you' : 'agent', state_since: at(120), outcome: null,
        last_progress: null, blocker: null, after: null, deadline: null,
        steps: [{ id: work.id, path: work.path, title: work.title, state: work.state,
          since: work.since, attempt: 1, assignee: agents[i].id, claimant: agents[i].id }],
      }],
    }
  })
  const nativeAttention = attention.map((item, i) => {
    const { requester_id, mission_run_id, mission_id, ...base } = item
    return { ...base, state: 'open', ...(requester_id ? { requester_id } : {}),
      ...(mission_id ? { mission_id, mission_run_id } : {}), person_id: person,
      actions: i === 0 ? ['work.done'] : ['message.read'], priority: 'normal',
      title: i === 0 ? 'Choose a sample size' : 'Sample ready',
      detail: i === 0 ? 'Select the small or large synthetic sample.' : 'The synthetic sample is ready to inspect.',
      requested_at: at(30), preview: null, preview_token: null,
    }
  })
  const conversations = Object.fromEntries(agents.slice(0, 3).map((agent, i) => {
    const call = `sample-call-${seed}-${i + 1}`
    const entry = (sequence, type, role, body, final = true) => ({
      id: ref('timeline-entry', `${i + 1}-${sequence}`), sequence, revision: 1, final,
      timestamp: at(60 - sequence), role, type, body,
    })
    const items = [
      entry(1, 'message', 'user', { message_id: ref('message', i + 1), from: person, to: agent.id, title: 'Build a sample', reply_to: null }),
      entry(2, 'content', 'user', { media_type: 'text/plain', text: 'Prepare a synthetic sample with four rows.' }),
      entry(3, 'tool_call', 'assistant', { call_id: call, name: 'read', arguments: { path: '/srv/work/sample/input.json' } }),
      ...(i === 1 ? [] : [entry(4, 'tool_result', 'tool', { call_id: call, status: i === 2 ? 'error' : 'success',
        media_type: 'application/json', content: i === 2 ? { error: 'Synthetic input unavailable' } : { rows: 4 } })]),
      entry(5, 'content', 'assistant', { media_type: 'text/plain', text: i === 1 ? 'Waiting for the synthetic read.' : 'The sample inspection is complete.' }, i !== 1),
    ]
    return [agent.id, { kind: 'conversation', session_id: agent.current_session_id, items,
      page: { limit: 100, has_more: false } }]
  }))
  const terminalFrames = nativeRuntimes.map((runtime, i) => Array.from({ length: 3 }, (_, frame) => {
    const texts = [`Sample worker ${i + 1}`, `$ inspect /srv/work/sample/input.json`, `Progress ${frame + 1}/3`, 'Width probe: 界 e\u0301 ░']
    return { kind: 'terminal-screen', terminal_id: runtime.terminal_id, runtime_incarnation: runtime.incarnation_id,
      revision: `screen/synthetic-${seed}-${i + 1}-${frame}`, next_sequence: frame,
      title: `Sample terminal ${i + 1}`, columns: 80, rows: 24, truncated: false,
      cursor: { column: 0, row: 4, visible: true, blinking: false, style: 'bar' },
      modes: { alternate_screen: false, application_cursor: false, application_keypad: false,
        bracketed_paste: true, focus_events: false, mouse_encoding: 'sgr', mouse_tracking: 'none' },
      lines: texts.map((text, row) => ({ row, text, redacted: false, truncated: false, wrapped: false,
        runs: [{ text, ...(row === 0 ? { bold: true } : {}) }] })),
    }
  })).flat()
  const executionRuns = nativeMissions.map((mission, i) => ({
    subject: mission.runs[0], id: mission.runs[0], mission: mission.id, revision: mission.revision,
    status: mission.run_details[0].status, phase: 'execution', requester: person,
    workspace: '/srv/work/sample', created_at_unix_ms: now - 120000, updated_at_unix_ms: now,
    steps: [{ subject: agents[i].current_work[0].id, step: 'assemble', title: `Assemble sample ${i + 1}`,
      status: agents[i].current_work[0].state, assigned_to: agents[i].id, claimant: agents[i].id,
      agentless: false, attempt: 1, execution_elapsed_ms: 1000 + random() % 1000,
      created_at_unix_ms: now - 120000, updated_at_unix_ms: now, blocked_reason: null,
      goals: ['Produce four synthetic rows'], constraints: ['Use only generated data'] }],
  }))
  const queues = agents.slice(0, 3).map((agent) => ({ kind: 'agent-queue', agent_id: agent.id,
    current_work_ids: agent.current_work.map((work) => work.id), next_work_id: null,
    runs: [{ mission_run_id: agent.owner_run_id, position: 0, state: 'active',
      ready_work_ids: [], waiting_work_ids: [], claimed_work_ids: agent.current_work.map((work) => work.id),
      waiting_for_run_id: null }], moves: [], move_count: 0 }))
  const provenance = { generated: true, seed, clock: at(), source: 'Independent seeded synthetic world; no observed input' }
  const graphs = Object.fromEntries(nativeMissions.map((mission) => [mission.runs[0], {
    revision: mission.revision, steps: { assemble: { id: 'assemble', path: 'assemble',
      dependencies: [], goals: ['Produce four synthetic rows'] } }, display_order: ['assemble'],
  }]))
  const requests = { missionId: missions[0].id, subscriptionId: ref('subscription', 1),
    verifiedSubject: { state: 'present', reason: null }, requests: [] }
  return {
    'roster-snapshot-v1.json': roster,
    'agents.json': nativeAgents, 'runtimes.json': nativeRuntimes, 'attention.json': nativeAttention,
    'gateway.capture.json': { capturedAt: at(), snapshot: ref('snapshot', 1), missions: nativeMissions, runs: executionRuns, queues },
    'gateway.conversations.json': conversations, 'terminal-frames.json': terminalFrames,
    'gateway.graphs.json': graphs, 'gateway.requests.json': requests,
    'gateway.provenance.json': provenance,
  }
}
