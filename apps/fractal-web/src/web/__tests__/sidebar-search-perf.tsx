import * as React from 'react'
import { createRoot } from 'react-dom/client'
import { Agent, decodeUnknownSync, type AgentEncoded } from '@smalltalk/st3-client/schema'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import * as Atom from 'effect/reactivity/Atom'
import { DataSourceProvider } from '../../data/react.tsx'
import { fixtureSource } from '../../data/fixtureSource.ts'
import { LiveAgentWorkspace } from '../LiveAgentWorkspace.tsx'
import { setTheme } from '../../ui-compat/foundations.ts'
import { fixtureFolders } from '../../folders/fixture.ts'

// A stable 78-agent roster; every H004 query prefix retains rows and exercises highlights.
const now = Date.parse('2026-10-12T12:00:00Z')
const agents = Array.from({ length: 78 }, (_, index) => decodeUnknownSync(Agent, 'strict')({
  kind: 'agent', id: `agent/benchmark-${index}`, name: `Vista Agent ${String(index).padStart(2, '0')}`, host_id: 'host/benchmark', runtime_ids: [],
  reachability: 'reachable', state: 'running', harness_state: index % 3 === 0 ? 'working' : index % 3 === 1 ? 'waiting' : 'idle', blocked_on: null, fault: null,
  revision: String(index + 1), updated_at: new Date(now).toISOString(), last_activity_at: new Date(now - index * 1000).toISOString(),
} satisfies AgentEncoded))
const clock = Atom.make(now).pipe(Atom.keepAlive)
const registry = AtomRegistry.make()
const source = { ...fixtureSource({ world: { now, agents, missions: [], attention: [], events: [], conversations: {}, terminals: {}, envelopes: {}, usage: { _tag: 'undeclared' } } }), now: clock }
await registry.get(fixtureFolders).edit([
  { op: 'folder.create', id: '00000000-0000-7000-8000-000000000078', parent: null, name: 'Vista archive', key: 'a0' },
  { op: 'subject.place', subject: agents[76]!.id, folder: '00000000-0000-7000-8000-000000000078', key: 'a0' },
])
setTheme('dark')
const root = document.getElementById('root')
if (root === null) throw new Error('Missing benchmark root')
createRoot(root).render(<DataSourceProvider source={source} registry={registry}><button type="button" style={{ position: 'fixed', right: 8, bottom: 8, zIndex: 100 }} onClick={() => registry.set(clock, registry.get(clock) + 1000)}>Advance fixture clock</button><React.Profiler id="sidebar-benchmark" onRender={() => {}}><LiveAgentWorkspace /></React.Profiler></DataSourceProvider>)
