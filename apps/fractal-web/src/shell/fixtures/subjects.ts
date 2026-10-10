// Fixture-only resource subjects; agents, missions and attention come from shared data projections.

import { ciRuns, pullRequests } from '../../fixtures/world.ts'
import { extensionFixtures } from '../../extensions/build.ts'
import type { SubjectSummary } from '../context.tsx'

/** Every subject quick-open and tab titles know about. */
export const fixtureSubjects: readonly SubjectSummary[] = [
  ...pullRequests.map(
    (p): SubjectSummary => ({
      ref: p.ref,
      title: `${p.repo}#${p.number}`,
      detail: `pull request · ${p.state}`,
      icon: 'resources',
    }),
  ),
  ...ciRuns.map(
    (c): SubjectSummary => ({
      ref: c.ref,
      title: `CI run ${c.id}`,
      detail: `${c.repo} · ${c.workflow}`,
      icon: 'resources',
    }),
  ),
  ...extensionFixtures.subjects,
]
