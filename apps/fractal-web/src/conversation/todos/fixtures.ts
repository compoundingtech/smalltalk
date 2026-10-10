import { Agent, decodeUnknownSync, type AgentEncoded } from '@smalltalk/st3-client/schema'

/** Synthetic agent identity shared by the native todo projection tests. */
export const todoAgentRef = 'agent/example/example'
/** Accepted native todo claim illustrating completed, active, blocked and pending phased tasks. */
export const todoFixture = {
  claim_id: 'claim/todo-fixture',
  accepted_at: '2026-10-04T12:00:01.000Z',
  stale: false,
  snapshot: {
    harness: 'omp',
    session_id: 'native-fixture-session',
    incarnation_id: 'fixture-incarnation',
    observed_at: '2026-10-04T12:00:00.000Z',
    source_op: 'todo_write',
    truncated: false,
    totals: { completed: 2, pending: 1, in_progress: 1, blocked: 1 },
    phases: [
      {
        name: 'Understand',
        tasks: [
          { content: 'Trace the accepted todo claim and session binding', status: 'completed' },
          { content: 'Confirm gateway projection semantics', status: 'completed' },
        ],
      },
      {
        name: 'Implement the progress strip while preserving composer width and wrapping long task titles',
        tasks: [
          { content: 'Pin phased progress above the composer', status: 'in_progress' },
          {
            content: 'Verify the real upstream producer on a current branch',
            status: 'blocked',
            blocker: 'Waiting for a native todo observation',
          },
        ],
      },
      {
        name: 'Verify',
        tasks: [
          { content: 'Exercise expanded tasks and inspect the detail tab', status: 'pending' },
        ],
      },
    ],
  },
} satisfies NonNullable<AgentEncoded['todo']>

/** Decoded running agent carrying an accepted synthetic todo claim. */
export const todoAgent = decodeUnknownSync(
  Agent,
  'strict',
)({
  kind: 'agent',
  id: todoAgentRef,
  name: 'example',
  state: 'running',
  driver: 'omp',
  runtime_ids: [],
  reachability: 'reachable',
  revision: '1',
  updated_at: todoFixture.accepted_at,
  incarnation_id: 'fixture-incarnation',
  todo: todoFixture,
})

