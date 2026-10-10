import { decodeSteps, encodeSteps, type Script } from './script.ts'
export { worldNow, WORLD_NOW_ISO, minutesAgo } from './clock.ts'
export { decodeSteps, encodeSteps, stepTimestamp, sessionAgentRef } from './script.ts'
export type { Script, Step } from './script.ts'

/** Stable names are shared with the app book; labels may change without renaming sessions. */
export const sessionNames = ['short-success', 'flaky-test', 'multi-file-refactor', 'interrupted-draft', 'offline-retry', 'first-result', 'long-debug', 'waiting-queued'] as const
export type SessionName = typeof sessionNames[number]
export interface Session extends Script {
  readonly id: SessionName
  readonly title: string
  readonly draft?: string
  /** Number of authored steps visible before a host-controlled transition. */
  readonly initialStepCount?: number
}

/** Hand-authored public fiction: Cedar's tiny inventory project, no captures or templates. */
export const sessions: readonly Session[] = [
  {
    id: 'short-success', title: 'Short success', startedMinutesAgo: 3,
    steps: [
      { kind: 'status', t: 0, status: 'running' },
      { kind: 'user', t: 0, text: 'Correct the spelling of the inventory heading.' },
      { kind: 'tool', t: 2, end: 4, id: 'spelling', name: 'edit', input: { path: 'src/inventory.tsx', old: 'Inventroy', new: 'Inventory' }, output: 'Updated src/inventory.tsx (+1 −1)' },
      { kind: 'say', t: 7, text: 'Inventory heading corrected. No behavior changed.' },
      { kind: 'status', t: 7, status: 'completed' },
    ],
  },
  {
    id: 'flaky-test', title: 'Flaky test', startedMinutesAgo: 18,
    steps: [
      { kind: 'status', t: 0, status: 'running' },
      { kind: 'user', t: 0, text: 'The reservation expiry check sometimes fails. Find the cause rather than relaxing the assertion.' },
      { kind: 'think', t: 9, ms: 8000, text: 'My first hypothesis is an ordering race between reservation callbacks.' },
      { kind: 'tool', t: 21, end: 48, id: 'wrong-hypothesis', name: 'bash', input: { command: 'pnpm test reservation --runInBand' }, output: 'FAIL reservation expiry\nExpected: expired\nReceived: active\nSerial execution still fails; callback ordering is not the cause.', isError: true },
      { kind: 'say', t: 60, text: 'Serial execution still failed. The wrong hypothesis was callback ordering; the test instead mixes a fake scheduler with the wall clock.' },
      { kind: 'tool', t: 88, end: 110, id: 'repair-clock', name: 'edit', input: { path: 'src/reservation.test.ts', old: 'Date.now()', new: 'clock.now()' }, output: 'Expiry expectation now uses the injected test clock.' },
      { kind: 'tool', t: 125, end: 162, id: 'repeat-expiry', name: 'bash', input: { command: 'pnpm test reservation --repeat 20' }, output: '20 runs passed\n60 assertions passed\nNo retries or widened expiry tolerance.' },
      { kind: 'say', t: 174, text: 'Expiry rerun passed all 20 runs after the clock repair. The original assertion is unchanged.' },
      { kind: 'status', t: 175, status: 'completed' },
    ],
  },
  {
    id: 'multi-file-refactor', title: 'Multi-file refactor', startedMinutesAgo: 44,
    steps: [
      { kind: 'user', t: 0, text: 'Separate inventory parsing from presentation, retain the same totals, then hand off the diff for review.' },
      { kind: 'status', t: 0, status: 'running' },
      { kind: 'tool', t: 25, end: 38, id: 'read-inventory', name: 'read', input: { path: 'src/inventory.tsx' }, output: 'export const Inventory = ({ rows }) => {\n  const total = rows.reduce((sum, row) => sum + Number(row.quantity), 0)\n  return <output>{total}</output>\n}' },
      { kind: 'tool', t: 94, end: 125, id: 'extract-parser', name: 'edit', input: { paths: ['src/inventory.tsx', 'src/quantity.ts', 'src/quantity.test.ts'] }, output: 'diff --git a/src/inventory.tsx b/src/inventory.tsx\n-  const total = rows.reduce((sum, row) => sum + Number(row.quantity), 0)\n+  const total = totalQuantity(rows)\ndiff --git a/src/quantity.ts b/src/quantity.ts\n+export const totalQuantity = rows => rows.reduce((sum, row) => sum + Number(row.quantity), 0)\ndiff --git a/src/quantity.test.ts b/src/quantity.test.ts\n+expect(totalQuantity([{ quantity: "3" }])).toBe(3)' },
      { kind: 'tool', t: 169, end: 203, id: 'quantity-checks', name: 'bash', input: { command: 'pnpm test quantity inventory' }, output: 'quantity: 6 passed\ninventory: 4 passed\nTotals and empty inventory behavior retained.' },
      { kind: 'mail', t: 248, id: 'message/example-refactor/review', from: 'person/robin', title: 'Review handoff: inspect parser extraction and unchanged inventory totals.' },
      { kind: 'say', t: 270, text: 'Review handoff ready: three files changed, parsing isolated, totals retained. Robin owns the review; no merge was requested.' },
      { kind: 'status', t: 271, status: 'waiting', detail: 'Waiting for review of the parser extraction.' },
    ],
  },
  {
    id: 'interrupted-draft', title: 'Interrupted with preserved draft', startedMinutesAgo: 12,
    draft: 'Resume with the saved inventory migration plan; do not rerun the interrupted write.',
    steps: [
      { kind: 'user', t: 0, text: 'Draft a migration plan before changing the inventory format.' },
      { kind: 'status', t: 0, status: 'running' },
      { kind: 'tool', t: 11, end: 13, id: 'read-format', name: 'read', input: { path: 'docs/inventory-format.md' }, output: 'Version 1: quantity is a string. Version 2 will store an integer.' },
      { kind: 'say', t: 34, text: 'Migration draft: validate quantities, preserve the original file, then write the version marker.' },
      { kind: 'tool', t: 45, id: 'interrupted-write', name: 'write', input: { path: 'docs/migration-plan.md' } },
      { kind: 'status', t: 52, status: 'cancelled', detail: 'Interrupted by the operator. Draft and prior output are retained.' },
    ],
  },
  {
    id: 'offline-retry', title: 'Offline, stale, retry', startedMinutesAgo: 9, initialStepCount: 4,
    draft: 'Check the cached stock totals once the connection returns.',
    steps: [
      { kind: 'user', t: 0, text: 'Read the latest stock totals without discarding the cached result.' },
      { kind: 'tool', t: 8, end: 12, id: 'cached-stock', name: 'read', input: { path: 'fixtures/stock.json' }, output: 'Cached stock: 18 units, revision 4.' },
      { kind: 'say', t: 16, text: 'Cached stock is retained while the connection is offline. This result may be stale.' },
      { kind: 'status', t: 17, status: 'waiting', detail: 'Offline; retry does not discard the saved draft.' },
      { kind: 'tool', t: 83, end: 91, id: 'refresh-stock', name: 'read', input: { path: 'fixtures/stock.json' }, output: 'Current stock: 21 units, revision 5.' },
      { kind: 'say', t: 94, text: 'Stock refreshed after retry: 21 units at revision 5. The cached result remains in the history.' },
      { kind: 'status', t: 95, status: 'completed' },
    ],
  },
  {
    id: 'first-result', title: 'First empty to first result', startedMinutesAgo: 1, initialStepCount: 0,
    steps: [
      { kind: 'user', t: 0, text: 'What is in this new inventory project?' },
      { kind: 'status', t: 1, status: 'running' },
      { kind: 'tool', t: 3, end: 6, id: 'first-list', name: 'read', input: { path: 'README.md' }, output: 'Cedar inventory: a small fictional stock tracker.' },
      { kind: 'say', t: 11, text: 'First result: this project tracks fictional stock quantities. There was no earlier conversation.' },
      { kind: 'status', t: 12, status: 'completed' },
    ],
  },
  {
    id: 'long-debug', title: 'Long debug', startedMinutesAgo: 93,
    steps: [
      { kind: 'user', t: 0, text: 'Investigate why importing the second inventory page duplicates a row. Keep the raw observations and explain the fix.' },
      { kind: 'status', t: 1, status: 'running' },
      { kind: 'tool', t: 30, end: 61, id: 'trace-pages', name: 'bash', input: { command: 'pnpm debug import-pages --trace' }, output: 'page=1 cursor=start row=cedar-01\npage=1 cursor=start row=cedar-02\npage=1 cursor=start row=cedar-03\npage=1 cursor=start row=cedar-04\npage=2 cursor=cedar-04 row=cedar-04\npage=2 cursor=cedar-04 row=cedar-05\npage=2 cursor=cedar-04 row=cedar-06\npage=2 cursor=cedar-04 row=cedar-07\npage=3 cursor=cedar-07 row=cedar-07\npage=3 cursor=cedar-07 row=cedar-08\npage=3 cursor=cedar-07 row=cedar-09\nrows=12 unique=10' },
      { kind: 'think', t: 117, ms: 43000, text: 'The repeated boundary row is present in the source page, before rendering. Inspect cursor inclusion rather than changing list keys.' },
      { kind: 'say', t: 220, text: '## Boundary observations\n\n| Page | Returned rows | Repeated boundary |\n| --- | --- | --- |\n| 1 | 4 | none |\n| 2 | 4 | cedar-04 |\n| 3 | 4 | cedar-07 |\n\nThe trace has **12 rows but 10 distinct identifiers**. The bug is cursor inclusion, not list identity.' },
      { kind: 'tool', t: 380, end: 405, id: 'no-list-key-findings', name: 'bash', input: { command: 'pnpm check list-keys --quiet' }, output: '' },
      { kind: 'tool', t: 492, end: 530, id: 'read-cursor', name: 'read', input: { path: 'src/import-page.ts' }, output: 'export const afterCursor = (rows, cursor) =>\n  rows.filter(row => row.id >= cursor)\n' },
      { kind: 'tool', t: 803, end: 816, id: 'exclusive-cursor', name: 'edit', input: { path: 'src/import-page.ts', old: 'row.id >= cursor', new: 'row.id > cursor' }, output: 'Cursor boundary is now exclusive; the first page still has no cursor filter.' },
      { kind: 'say', t: 960, text: '## Repair\n\n```ts\nexport const afterCursor = (rows, cursor) =>\n  rows.filter(row => row.id > cursor)\n```\n\nThe cursor denotes the last row already consumed, so the next page must exclude it. Empty pages keep the previous cursor.' },
      { kind: 'tool', t: 1120, end: 1196, id: 'page-regressions', name: 'bash', input: { command: 'pnpm test import-page --cases boundary,empty,last' }, output: 'PASS first page: 4 unique rows\nPASS second page: boundary excluded\nPASS empty page: cursor retained\nPASS final page: no duplicate\nPASS ordering unchanged\nPASS retry idempotent\n6 tests passed' },
      { kind: 'tool', t: 1450, end: 1451, id: 'quiet-diff-check', name: 'bash', input: { command: 'git diff --check' }, output: '' },
      { kind: 'say', t: 1515, text: 'Debug conclusion: the inclusive cursor duplicated boundary rows. Six regressions pass with the exclusive comparison; both quiet checks produced zero output.' },
      { kind: 'status', t: 1516, status: 'completed' },
    ],
  },
  {
    id: 'waiting-queued', title: 'Waiting for human and queued', startedMinutesAgo: 6,
    steps: [
      { kind: 'user', t: 0, text: 'Prepare a stock correction, but wait for my choice of rounding policy.' },
      { kind: 'status', t: 0, status: 'queued', detail: 'Queued behind the current inventory audit.' },
      { kind: 'status', t: 95, status: 'running', detail: 'Inventory audit finished; preparation started.' },
      { kind: 'tool', t: 114, end: 120, id: 'inspect-fractions', name: 'read', input: { path: 'fixtures/fractional-stock.json' }, output: 'Three rows have fractional quantities: 1.5, 2.25, 0.5.' },
      { kind: 'say', t: 133, text: 'Waiting for your rounding policy: retain fractions or round down? No stock correction has been applied.' },
      { kind: 'mail', t: 134, id: 'message/example-rounding/decision', from: 'person/operator', title: 'Decision requested: choose the rounding policy before applying corrections.' },
      { kind: 'status', t: 135, status: 'waiting', detail: 'Waiting for human approval; the correction remains queued.' },
    ],
  },
]

export const sessionByName = (name: SessionName): Session => {
  const session = sessions.find(candidate => candidate.id === name)
  if (session === undefined) throw new Error(`Session corpus has no session named ${name}`)
  return session
}

/** Counts come from emitted items, never copied counters in the authored sessions. */
export const sessionCounts = (session: Script): { readonly entries: number; readonly toolCalls: number; readonly userMessages: number; readonly durationMs: number } => {
  const entries = encodeSteps(session).flat()
  const times = entries.map(entry => Date.parse(entry.timestamp))
  return {
    entries: entries.length,
    toolCalls: entries.filter(entry => entry.type === 'tool_call').length,
    userMessages: entries.filter(entry => entry.type === 'content' && entry.role === 'user').length,
    durationMs: times.length === 0 ? 0 : Math.max(...times) - Math.min(...times),
  }
}

/** App adapters consume these decoded production entries with their existing projection. */
export const sessionEntries = (session: Script) => decodeSteps(session).flat()
