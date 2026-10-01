import type { ActionOf } from './Models.generated';

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
import type { Glass, Machine } from './Models.generated';
const glass = null as unknown as Glass;
const glassName: string | undefined = glass.body?.name;
const machine = null as unknown as Machine;
const machineHost: string = machine.host_id;
void glassName; void machineHost;

import type { GlassBody, GlassLayout } from './Models.generated';
const groupedBody: GlassBody = { name: 'Main', layout: { split: 'right', children: [
    { tabs: [{ title: 'Work', pane: 'opaque:key' }] }, { tabs: [] },
] } };
function tabPanes(layout: GlassLayout): string[] {
    return 'tabs' in layout ? layout.tabs.map(tab => tab.pane) : layout.children.flatMap(tabPanes);
}
void tabPanes(groupedBody.layout);
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
