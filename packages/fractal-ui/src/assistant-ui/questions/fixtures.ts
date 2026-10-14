import type { Answers, QuestionRequestView, QuestionState, QuestionView } from './model'

/** Synthetic story fixtures: a sample-app setup conversation with no real people, projects or hosts. */
export const askedAt = '2026-10-01T12:00:00.000Z'
export const expiresAt = '2026-10-01T12:30:00.000Z'
export const submittedAt = '2026-10-01T12:05:00.000Z'
/** The host clock the stories render with: three minutes after the ask. */
export const viewedAt = Date.parse('2026-10-01T12:03:00.000Z')

const storage: QuestionView = {
  id: 'storage', header: 'Storage', prompt: 'Which storage should the sample use?', kind: 'single', allowOther: true, allowNote: true,
  options: [
    { id: 'embedded', label: 'Embedded database', description: 'No service to operate', recommended: true, preview: { _tag: 'markdown', text: '### Embedded storage\nKeep the sample self-contained in one file.\n\n```sql\nCREATE TABLE notes (id INTEGER PRIMARY KEY, body TEXT);\n```' } },
    { id: 'managed', label: 'Managed database', description: 'Shared access across workers', preview: { _tag: 'markdown', text: '### Managed storage\nUse a hosted relational service.\n\n- Connection string from the environment\n- Migrations run on deploy' } },
    { id: 'files', label: 'Plain files', description: 'One JSON file per note' },
  ],
}
const features: QuestionView = {
  id: 'features', header: 'Features', prompt: 'Which features ship in the first release?', kind: 'multi', allowOther: false, allowNote: false,
  options: [{ id: 'sign-in', label: 'Sign in' }, { id: 'search', label: 'Search' }, { id: 'export', label: 'Export' }, { id: 'sharing', label: 'Sharing' }],
}
/** Free text with `allowOther`, as the app projects an ask that has no options. */
const deploy: QuestionView = {
  id: 'deploy', header: 'Deploy', prompt: 'Where should the sample deploy?', kind: 'text', allowOther: true, allowNote: false, placeholder: 'Describe the host',
  options: [],
}
const review: QuestionView = {
  id: 'review', header: 'Review', prompt: 'How should the pull request open?', kind: 'confirm', allowOther: false, allowNote: true,
  options: [{ id: 'draft', label: 'As a draft', recommended: true }, { id: 'ready', label: 'Ready for review' }],
}
const checks: QuestionView = {
  id: 'checks', header: 'Checks', prompt: 'Which checks should run before merge?', kind: 'multi', allowOther: true, allowNote: true,
  options: [
    { id: 'typecheck', label: 'Typecheck', recommended: true },
    { id: 'unit', label: 'Unit tests', description: 'Fast, no browser' },
    { id: 'storybook', label: 'Storybook build' },
    { id: 'lint', label: 'Lint' },
  ],
}

const request = (id: string, questions: readonly QuestionView[], title?: string): QuestionRequestView => ({
  id, source: 'ask', agentLabel: 'Sample agent', askedAt, questions, state: { _tag: 'pending', expiresAt }, answerable: true, allowChatRedirect: false,
  ...(title === undefined ? {} : { title }),
})

export type QuestionScenario = 'one' | 'tabbed' | 'multi'
export const questionScenarios: Readonly<Record<QuestionScenario, QuestionRequestView>> = {
  one: request('ask-one', [{ ...storage, allowNote: false }]),
  tabbed: request('ask-tabbed', [storage, features, deploy, review], 'Sample app setup'),
  multi: request('ask-multi', [checks]),
}

/** Answers a reviewer would give; used for the submitted state. */
export const submittedAnswers: Readonly<Record<QuestionScenario, Answers>> = {
  one: { storage: { selected: ['embedded'] } },
  tabbed: { storage: { selected: ['embedded'], note: 'Prefer a zero-operations sample' }, features: { selected: ['sign-in', 'export'] }, deploy: { selected: [], other: 'Edge workers' }, review: { selected: ['draft'] } },
  multi: { checks: { selected: ['typecheck', 'unit'], other: 'Bundle size budget', note: 'Keep the run under ten minutes' } },
}
/** Automatic answers: the recommended option where one exists, nothing otherwise. */
export const automaticAnswers: Readonly<Record<QuestionScenario, Answers>> = {
  one: { storage: { selected: ['embedded'] } },
  tabbed: { storage: { selected: ['embedded'] }, review: { selected: ['draft'] } },
  multi: { checks: { selected: ['typecheck'] } },
}

export type QuestionPhase = 'pending' | 'submitted' | 'expired' | 'failed'
export function questionStateFor(scenario: QuestionScenario, phase: QuestionPhase): QuestionState {
  switch (phase) {
    case 'pending': return { _tag: 'pending', expiresAt }
    case 'submitted': return { _tag: 'answered', answers: submittedAnswers[scenario], by: 'You', at: submittedAt }
    case 'expired': return { _tag: 'expired', at: expiresAt, autoAnswer: automaticAnswers[scenario] }
    case 'failed': return { _tag: 'failed', error: 'the agent stopped listening before the answer arrived' }
  }
}
