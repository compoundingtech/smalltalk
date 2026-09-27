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
