import * as React from 'react'
import { createRoot } from 'react-dom/client'
import * as Atom from 'effect/reactivity/Atom'
import * as AtomRegistry from 'effect/reactivity/AtomRegistry'
import { DataSourceProvider } from '../src/data/react.tsx'
import { fixtureSource } from '../src/data/fixtureSource.ts'
import { fixtureProjections } from '../src/fixtures/projections.ts'
import type { ConversationItem } from '../src/conversation/model.ts'
import type { AttachmentPort, ConversationPage, Feed } from '../src/data/source.ts'
import { LiveAgentWorkspace } from '../src/web/LiveAgentWorkspace.tsx'
import 'virtual:overeng-stylex.css'

const count = Number(new URLSearchParams(location.search).get('turns') ?? 100)
const registry = AtomRegistry.make()
const refs = ['agent/switch-alpha', 'agent/switch-beta', 'agent/switch-gamma']
const at = '2026-10-01T00:00:00.000Z'
const items = (ref: string): readonly ConversationItem[] => Array.from({ length: count }, (_, i) => [
  { _tag: 'Text' as const, id: `${ref}/prompt/${i}`, role: 'user' as const, text: `Explain iteration ${i} and its result.`, attachments: [], streaming: false, at },
  { _tag: 'Text' as const, id: `${ref}/answer/${i}`, role: 'assistant' as const, text: `## Iteration ${i}

A **bounded** collection preserves _identity_ and [documentation](https://example.com).

- First result
- Second result

\`\`\`typescript
const rows = [1, 2, 3]
export const total = rows.reduce((sum, row) => sum + row, 0)
\`\`\`

The result is **6**.`, attachments: [], streaming: false, at },
]).flat()
const pages = Object.fromEntries(refs.map(ref => [ref, { items: items(ref), hasOlder: false, observation: { empty: false } } satisfies ConversationPage]))
const feeds = Object.fromEntries(refs.map(ref => [ref, Atom.make<Feed<ConversationPage>>({ _tag: 'Waiting' }).pipe(Atom.keepAlive)]))
const base = fixtureSource({ world: { ...fixtureProjections, agents: refs.map((id, i) => ({ ...fixtureProjections.agents[0]!, id, name: ['Switch Alpha', 'Switch Beta', 'Switch Gamma'][i]! })), missions: [], attention: [], events: [] } })
const pending = new Set<string>()
const interests = Object.fromEntries(refs.map(ref => [ref, Atom.make(() => {
  if (registry.get(feeds[ref]!)._tag !== 'Waiting' || pending.has(ref)) return
  pending.add(ref)
  performance.mark(`switch-request:${ref}`)
  void fetch('/switch-page?ref=' + encodeURIComponent(ref)).then(async response => {
    await response.text()
    performance.mark(`switch-frame:${ref}`)
    registry.set(feeds[ref]!, { _tag: 'Observed', freshness: 'live', value: pages[ref]! })
  })
})]))
// The synthetic send port records attempted sends, then refuses them; no gateway is touched.
const refusal = { _tag: 'Refused', reason: 'invalid', detail: 'Synthetic fixture has no delivery backend.' } as const
const attachments: AttachmentPort = {
  capabilities: async () => refusal,
  upload: async () => refusal,
  chunk: async () => refusal,
  send: async request => { performance.mark('synthetic-send:' + request.parameters.to); return refusal },
}
const source = { ...base, grants: Atom.make({ actions: 'granted', messageSend: 'granted', terminalInput: 'ungranted' } as const), attachments,
  conversation: (ref: string) => feeds[ref]!, conversationInterest: (ref: string) => interests[ref]! }
window.history.replaceState(null, '', '/w/' + refs[0] + location.search)
createRoot(document.getElementById('root')!).render(<DataSourceProvider source={source} registry={registry}><LiveAgentWorkspace /></DataSourceProvider>)
