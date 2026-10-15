import type { ChangelogDeployment, ChangelogSourceRegistration } from '../src/changelog/contract.ts'
import agentControls from './2026-10-14-agent-controls.ts'
import liveTranscriptPolish from './2026-10-14-live-transcript-polish.ts'
import scopedChangelog from './2026-10-13-scoped-changelog.ts'
import changelogPlacement from './2026-10-12-changelog-placement.ts'
import changelogGlass from './2026-10-12-changelog-glass.ts'
import changelogDismissFocus from './2026-10-11-changelog-dismiss-focus.ts'
import missionsScreen from './2026-10-14-missions-screen.ts'
import fasterFirstLoad from './2026-10-15-faster-first-load.ts'
import threadChromePolish from './2026-10-15-thread-chrome-polish.ts'

/** Public registry: every other file in this directory must be listed here exactly once. */
export const changelogSources: readonly ChangelogSourceRegistration[] = Object.freeze([
  { file: '2026-10-14-agent-controls.ts', source: agentControls, visibility: 'public' },
  { file: '2026-10-14-live-transcript-polish.ts', source: liveTranscriptPolish, visibility: 'public' },
  { file: '2026-10-13-scoped-changelog.ts', source: scopedChangelog, visibility: 'public' },
  { file: '2026-10-12-changelog-placement.ts', source: changelogPlacement, visibility: 'public' },
  { file: '2026-10-12-changelog-glass.ts', source: changelogGlass, visibility: 'public' },
  { file: '2026-10-11-changelog-dismiss-focus.ts', source: changelogDismissFocus, visibility: 'public' },
  { file: '2026-10-14-missions-screen.ts', source: missionsScreen, visibility: 'public' },
  { file: '2026-10-15-faster-first-load.ts', source: fasterFirstLoad, visibility: 'public' },
  { file: '2026-10-15-thread-chrome-polish.ts', source: threadChromePolish, visibility: 'public' },
])
export const changelogDeployment: ChangelogDeployment = 'main'
