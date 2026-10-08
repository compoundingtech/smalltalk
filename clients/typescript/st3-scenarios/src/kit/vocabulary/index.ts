/**
 * Curated text banks for invented projects. Entries describe everyday engineering; they never
 * state an absolute date or clock time (the scan gate rejects such patterns here).
 */

export const projects = ['atlas', 'quill', 'lumen', 'harrow', 'tidepool'] as const

export const hostWords = ['harbor', 'quarry', 'orchard', 'lantern', 'meadow', 'foundry'] as const

export const people = [
  { id: 'person/ada', name: 'Ada' },
  { id: 'person/robin', name: 'Robin' },
  { id: 'person/avery', name: 'Avery' },
  { id: 'person/blair', name: 'Blair' },
] as const

export const roles = {
  builder: 'Builder',
  reviewer: 'Reviewer',
  migrator: 'Migrator',
  docs: 'Docs',
  tester: 'Tester',
  release: 'Release',
} as const
export type Role = keyof typeof roles

export const harnesses = [
  { driver: 'claude', model: 'opus' },
  { driver: 'codex', model: 'gpt-codex' },
] as const

/** Source files of the invented monorepo, relative to the repository root. */
export const files = [
  'packages/api/src/users/loadUser.ts',
  'packages/api/src/users/loadUser.test.ts',
  'packages/api/src/users/index.ts',
  'packages/web/src/profile/ProfileCard.tsx',
  'packages/web/src/profile/useProfile.ts',
  'packages/web/src/settings/AccountPage.tsx',
  'packages/cli/src/commands/whoami.ts',
  'packages/worker/src/jobs/syncAvatars.ts',
  'docs/api/users.md',
  'CHANGELOG.md',
] as const

export const taskTitles = [
  'Rename fetchUser to loadUser across the monorepo',
  'Update call sites in packages/web',
  'Fix the typecheck in packages/worker after the rename',
  'Review the loadUser rename',
  'Update the users API reference',
  'Bisect the red main after the lockfile bump',
] as const

export const commitMessages = [
  'refactor(api): rename fetchUser to loadUser',
  'refactor(web): use loadUser in profile hooks',
  'fix(worker): pass the session to loadUser',
  'docs(api): describe loadUser and its errors',
  'test(api): cover loadUser with a missing profile',
] as const

export const compilerErrors = [
  {
    file: 'packages/worker/src/jobs/syncAvatars.ts',
    line: 41,
    column: 18,
    code: 'TS2554',
    message: 'Expected 2 arguments, but got 1.',
  },
  {
    file: 'packages/cli/src/commands/whoami.ts',
    line: 12,
    column: 10,
    code: 'TS2305',
    message: "Module '\"@atlas/api\"' has no exported member 'fetchUser'.",
  },
] as const

export const testNames = [
  'loadUser > returns the cached profile',
  'loadUser > rejects a missing session',
  'ProfileCard > renders the display name',
  'syncAvatars > skips deleted accounts',
] as const

export const reviewComments = [
  'The old name is still exported from packages/api/src/users/index.ts; drop it or mark it deprecated.',
  'Could loadUser take the session as its first argument, like loadTeam does?',
  'This changes the error type the CLI prints; the snapshot test needs an update.',
] as const

export const personRequests = [
  'Please rename fetchUser to loadUser everywhere and keep the tests green.',
  'Can you check why the worker typecheck fails after the rename?',
  'Update the API docs once the rename lands.',
] as const

export const assistantNotes = [
  'I found 23 call sites of fetchUser in four packages. I will rename them package by package and run the typecheck after each one.',
  'The worker passes only the user id. loadUser now needs the session too, so syncAvatars has to thread it through.',
  'The rename is done in packages/api and packages/web. The typecheck is clean there; packages/worker still fails.',
  'I am waiting for a review of the rename before I touch the CLI.',
] as const

export const shellRuns = [
  {
    command: 'pnpm -r typecheck',
    lines: [
      'packages/api typecheck: Done',
      'packages/web typecheck: Done',
      '\u001b[31mpackages/worker typecheck: src/jobs/syncAvatars.ts(41,18): error TS2554: Expected 2 arguments, but got 1.\u001b[0m',
      '\u001b[31mpackages/worker typecheck: Failed\u001b[0m',
    ],
    exit: 2,
  },
  {
    command: 'pnpm --filter @atlas/api test',
    lines: [
      ' \u001b[32m✓\u001b[0m src/users/loadUser.test.ts (6 tests)',
      ' Test Files  1 passed (1)',
      '      Tests  6 passed (6)',
    ],
    exit: 0,
  },
  {
    command: 'rg -l fetchUser packages',
    lines: ['packages/cli/src/commands/whoami.ts', 'packages/worker/src/jobs/syncAvatars.ts'],
    exit: 0,
  },
] as const

/** Unified diffs of small edits, keyed by file. */
export const edits = {
  'packages/web/src/profile/useProfile.ts': {
    before: "import { fetchUser } from '@atlas/api'\n\nexport const useProfile = (id: string) => useQuery(() => fetchUser(id))\n",
    after: "import { loadUser } from '@atlas/api'\n\nexport const useProfile = (id: string) => useQuery(() => loadUser(id))\n",
  },
  'packages/worker/src/jobs/syncAvatars.ts': {
    before: '  const user = await loadUser(account.userId)\n',
    after: '  const user = await loadUser(session, account.userId)\n',
  },
  'packages/api/src/users/index.ts': {
    before: "export { fetchUser } from './fetchUser.ts'\n",
    after: "export { loadUser } from './loadUser.ts'\n",
  },
} as const
