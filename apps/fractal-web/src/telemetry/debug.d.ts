export type { DebugBag } from './meters.tsx'

declare module './meters.tsx' {
  interface DebugBag {
    'Wf.httpInFlight': number
    'Wf.httpErrors': number
    'Wf.httpP50Ms': number
    'Wf.httpP95Ms': number
    'Wf.socketErrors': number
    'Wf.socketLive': number
    'Wf.activeFollows': number
    'Wf.followCap': number
    'Wf.frames': number
    'Wf.resyncs': number
    'Wf.retries': number
    'Wf.decodeMs': number
    'Wf.terminalUpdates': number
    'Wf.conversationEntries': number
    'Renders.sidebar': number
    'Renders.conversation-content': number
    'Renders.conversation-list': number
    'Renders.terminal': number
    'Fn.conversationRow': number
    'Fn.terminal': number
    'Renders.inbox': number
  }
}
