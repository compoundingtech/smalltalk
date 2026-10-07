import type { ActionOf, ConversationSearchHit, TimelineMessageBody } from './Models.generated.ts';

const valid: ActionOf<'launch.approve'> = {
    api_version: 'st3.client.v0',
    id: 'action/test',
    type: 'launch.approve',
    idempotency_key: 'test-idempotency-key',
    fence: { snapshot_id: 'snapshot/test', subject_revisions: {}, preview_token: `lpv0:${'a'.repeat(64)}` },
    parameters: { launch_id: 'launch/test', variant_id: 'variant/test' },
};
void valid;

const missingFence: ActionOf<'launch.approve'> = {
    api_version: 'st3.client.v0', id: 'action/test', type: 'launch.approve',
    idempotency_key: 'test-idempotency-key',
    // @ts-expect-error approval requires its preview token fence
    fence: { snapshot_id: 'snapshot/test', subject_revisions: {} },
    parameters: { launch_id: 'launch/test', variant_id: 'variant/test' },
};
void missingFence;

// Every resource keeps its fields: a resource kind missing from ResourceHeader's kinds would make
// its type `never`, and these would not compile.
import type { Glass, Machine } from './Models.generated.ts';
const glass = null as unknown as Glass;
const glassName: string | undefined = glass.body?.name;
const machine = null as unknown as Machine;
const machineHost: string = machine.host_id;
void glassName; void machineHost;

import type { GlassBody, GlassLayout } from './Models.generated.ts';
const groupedBody: GlassBody = { name: 'Main', layout: { split: 'right', ratio: 0.3, children: [
    { tabs: [{ title: 'Work', pane: 'opaque:key' }] }, { tabs: [] },
] } };
function tabPanes(layout: GlassLayout): string[] {
    return 'tabs' in layout ? layout.tabs.map(tab => tab.pane) : layout.children.flatMap(tabPanes);
}
void tabPanes(groupedBody.layout);
const splitRatio: number | undefined = 'split' in groupedBody.layout ? groupedBody.layout.ratio : undefined;
void splitRatio;
// @ts-expect-error a split ratio must be numeric
const badRatio: GlassLayout = { split: 'right', ratio: '0.3', children: [{ tabs: [] }, { tabs: [] }] };
void badRatio;
// @ts-expect-error the pre-deployment body with tabs at the root is no longer valid
const legacyBody: GlassBody = { name: 'Old', tabs: [] };
// @ts-expect-error panes belong to tabs, not layout leaves
const legacyLayout: GlassLayout = { pane: 'opaque:key' };
void legacyBody; void legacyLayout;

const createAgent: ActionOf<'agent.create'> = {
    api_version: 'st3.client.v0', id: 'action/create', type: 'agent.create',
    idempotency_key: 'creation-test-key', fence: { snapshot_id: 'snapshot/test', subject_revisions: {} },
    parameters: { name: 'worker', harness: 'codex', message: '--literal text', workspace: '/tmp' },
};
const createTerminal: ActionOf<'terminal.create'> = {
    ...createAgent, type: 'terminal.create', parameters: { name: 'Shell', cwd: '/tmp' },
};
void createAgent.parameters.message;
void createTerminal.parameters.cwd;

const attachTerminal: ActionOf<'terminal.attach'> = {
    api_version: 'st3.client.v0', id: 'action/attach', type: 'terminal.attach',
    idempotency_key: 'terminal-attach-key',
    fence: { snapshot_id: 'snapshot/test', subject_revisions: {}, runtime_incarnation: 'runtime:one' },
    parameters: { target_id: 'terminal/agent/example' },
};
void attachTerminal;

const dictatedBody: TimelineMessageBody = { message_id: 'message/voice', tags: ['dictated'] };
const dictatedTags: string[] | undefined = dictatedBody.tags;
void dictatedTags;

const searchHit: ConversationSearchHit = { conversation_id: 'session/scribe', entry_id: 'timeline-entry/note', agent_id: 'agent/scribe', timestamp: '2026-10-02T00:00:00Z', entry_type: 'content', excerpt: 'orchid' };
void searchHit;

// Rust None and TypeScript omission are both supported wire shapes.
import type { CollectionCommand, CollectionFrame } from './Models.generated.ts';
const nullFilters: CollectionCommand = { kind: 'subscribe', id: 'agents', collection: 'agents', person: null, actor: null, status: null, filter: null };
const currentTerminal: CollectionCommand = { kind: 'subscribe', id: 'term', collection: 'terminal', terminal: 'terminal/example', capability: 'proof' };
const nullIncarnation: CollectionCommand = { ...currentTerminal, incarnation: null };
const collectionResync: CollectionFrame = { kind: 'resync', id: 'agents', code: 'internal', message: 'Retry', retryable: true };
const conversationResync: CollectionFrame = { kind: 'resync', id: 'chat', collection: 'conversation', retryable: true };
const terminalError: CollectionFrame = { kind: 'error', id: 'term', collection: 'terminal', message: 'Restarted', retryable: false };
void [nullFilters, currentTerminal, nullIncarnation, collectionResync, conversationResync, terminalError];
// @ts-expect-error Terminal capability remains required.
const missingTerminalCapability: CollectionCommand = { kind: 'subscribe', id: 'term', collection: 'terminal', terminal: 'terminal/example' };
// @ts-expect-error Failure metadata is explicitly declared; arbitrary properties remain closed.
const extraResyncProperty: CollectionFrame = { kind: 'resync', id: 'agents', undeclared: true };
void [missingTerminalCapability, extraResyncProperty];

import type { ListOptions, TerminalListOptions, CollectionFilters } from './Client.generated.ts';
const textFilteredPage: ListOptions = { filter: 'release review', limit: 1, cursor: 'next/&' };
const textFilteredTerminals: TerminalListOptions = { filter: 'shell', owner: 'person/ada', state: 'running' };
const textFilteredWindow: CollectionFilters = { filter: 'release review', actor: 'agent/worker' };
void [textFilteredPage, textFilteredTerminals, textFilteredWindow];
