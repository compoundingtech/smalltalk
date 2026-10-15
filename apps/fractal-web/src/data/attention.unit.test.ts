import { expect, it } from 'vitest'
import { dailyDriver } from '../shell/fixtures/layouts.ts'
import { matchesRow } from '../shell/sidebar/filter.ts'
import { defaultFilters } from '../shell/sidebar/state.ts'
import { attentionAnswerFixtures } from './attention.fixtures.ts'
import { isWaitingForAnswer } from './attention.ts'
import { attentionRefs, createProjections, subjectList } from './projections.ts'

it.each(attentionAnswerFixtures)('$name: waiting=$waiting', ({ item, waiting }) => {
  expect(isWaitingForAnswer(item)).toBe(waiting)
  expect(attentionRefs([item].filter(isWaitingForAnswer)).has(item.source_id)).toBe(waiting)
  expect(subjectList({ attention: [item] })[0]?.attention === true).toBe(waiting)
  expect(createProjections().subjectList({ attention: [item] })[0]?.attention === true).toBe(waiting)
})

it.each(attentionAnswerFixtures)('Needs me filter uses answerability for $name, not harness status', ({ item, waiting }) => {
  const needsMe = attentionRefs([item].filter(isWaitingForAnswer)).has(item.source_id)
  const row = {
    workspace: { id: item.source_id, title: 'Synthetic', host: 'synthetic', layout: dailyDriver, unread: [], notifications: [] },
    agent: undefined, status: 'waiting' as const, needsMe,
  }
  expect(matchesRow({ row, filters: { ...defaultFilters, needsMe: true }, context: '' })).toBe(waiting)
  expect(matchesRow({ row, filters: defaultFilters, context: '' })).toBe(true)
})
