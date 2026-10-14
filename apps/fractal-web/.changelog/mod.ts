import type { ChangelogDeployment, ChangelogSourceRegistration } from '../src/changelog/contract.ts'
import liveSidebarSearch from './2026-10-14-live-sidebar-search.ts'
import scopedChangelog from './2026-10-13-scoped-changelog.ts'
import changelogPlacement from './2026-10-12-changelog-placement.ts'
import changelogGlass from './2026-10-12-changelog-glass.ts'
import changelogDismissFocus from './2026-10-11-changelog-dismiss-focus.ts'

/** Public registry: every other file in this directory must be listed here exactly once. */
export const changelogSources: readonly ChangelogSourceRegistration[] = Object.freeze([
  { file: '2026-10-14-live-sidebar-search.ts', source: liveSidebarSearch, visibility: 'public' },
  { file: '2026-10-13-scoped-changelog.ts', source: scopedChangelog, visibility: 'public' },
  { file: '2026-10-12-changelog-placement.ts', source: changelogPlacement, visibility: 'public' },
  { file: '2026-10-12-changelog-glass.ts', source: changelogGlass, visibility: 'public' },
  { file: '2026-10-11-changelog-dismiss-focus.ts', source: changelogDismissFocus, visibility: 'public' },
])
export const changelogDeployment: ChangelogDeployment = 'main'
