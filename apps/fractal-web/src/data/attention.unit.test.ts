import { expect, it } from 'vitest'
import { attentionAnswerFixtures } from './attention.fixtures.ts'
import { isWaitingForAnswer } from './attention.ts'
import { attentionRefs, createProjections, subjectList } from './projections.ts'

it.each(attentionAnswerFixtures)('$name: waiting=$waiting', ({ item, waiting }) => {
  expect(isWaitingForAnswer(item)).toBe(waiting)
  expect(attentionRefs([item].filter(isWaitingForAnswer)).has(item.source_id)).toBe(waiting)
  expect(subjectList({ attention: [item] })[0]?.attention === true).toBe(waiting)
  expect(createProjections().subjectList({ attention: [item] })[0]?.attention === true).toBe(waiting)
})
