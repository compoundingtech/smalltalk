import { TimelineEntry } from '@smalltalk/st3-client/schema'
import { Schema } from 'effect'
import { describe, expect, it } from 'vitest'
import { LiveTimeline } from './fromTimeline.ts'
import { q101OmpEnvelopeStopgap } from './q101OmpEnvelopeStopgap.ts'
import type { Role } from './model.ts'

const task = '<task-result id="ReviewRows" agent="reviewer" status="completed" duration="1m45s">\n<meta lines="2" size="120B" />\n<output>\n{"summary":"Selection survives the new row projection.","findings":[]}\n</output>\n</task-result>'
const background = (id: string, body: string) => `<system-notice>\nBackground job ${id} has completed. Resume your work using the result below.\n${body}\n</system-notice>`
const irc = '<irc>\nIncoming IRC message from agent `RowReviewer`:\n\nThe row review is ready.\n\nSent while waiting/working. Active interruptible wait stopped early for immediate reading.\n\nIf response expected, reply via `write` (`path: "agent://RowReviewer"`, `content: "…"`); may finish current step first. No one replies on your behalf.\n</irc>'
const compaction = '[omp compaction]\n## Goal\nKeep row selection stable.\n\n## Progress\n- Reviewed the projection.\n\n```ts\nconst rows = []\n```\n\n| Check | Result |\n| --- | --- |\n| Selection | Retained |'
const cases = [
  ['subagent-result', background('ReviewRows', task), 'SubagentResult'],
  ['irc', irc, 'Irc'],
  ['background-job', background('bg_7', 'Selection checks passed.\nWall time: 2.4 seconds'), 'BackgroundJob'],
  ['compaction', compaction, 'Compaction'],
] as const
const project = (text: string, role: Role = 'system', final = true) => {
  const timeline = new LiveTimeline()
  timeline.apply({ replace: true, hasMore: false, entries: [Schema.decodeUnknownSync(TimelineEntry)({
    id: 'timeline-entry/example/notice', sequence: 1, revision: 1, final, role,
    timestamp: '2026-01-15T12:00:00Z', type: 'content', body: { media_type: 'text/plain', text },
  })] })
  return timeline.project().items[0]
}

describe('q101 exact OMP envelope stopgap', () => {
  it.each(cases)('projects %s from the real timeline seam', (kind, text, tag) => {
    const item = project(text)
    expect(item).toMatchObject({ _tag: 'Event', kind, notice: { _tag: tag, raw: text }, sender: { kind: 'harness' } })
    if (item?._tag === 'Event') expect(item.text).not.toMatch(/<system-notice>|<irc|^[{[]|## Goal/)
  })
  it('retains all task facts, not a current child status inferred from output', () => {
    expect(project(background('ReviewRows', task))).toMatchObject({ notice: { taskId: 'ReviewRows', agent: 'reviewer', status: 'completed', duration: '1m45s', lines: 2, size: '120B' }, text: 'Selection survives the new row projection.' })
  })
  it('accepts the exact structured-output footer', () => {
    expect(project(background('ReviewRows', `${task}\n\nStructured output: schema valid; full payload at agent://ReviewRows, fields via agent://ReviewRows/<field>[/<index>/…]`))).toMatchObject({ _tag: 'Event', kind: 'subagent-result' })
  })
  it('accepts the exact task follow-up line, alone or before the structured footer', () => {
    const idle = '\n\nReviewRows is now idle — message it via `write agent://ReviewRows` to follow up; transcript at history://ReviewRows'
    expect(project(background('ReviewRows', `${task}${idle}`))).toMatchObject({ kind: 'subagent-result', notice: { followUp: idle.slice(2) } })
    expect(project(background('ReviewRows', `${task}${idle}\n\nStructured output: schema valid`))).toMatchObject({ kind: 'subagent-result' })
    expect(project(background('ReviewRows', `${task}\n\nReviewRows was aborted — transcript unavailable`))).toMatchObject({ kind: 'subagent-result' })
  })
  it('accepts preview, abort, error and merge facts in the source order', () => {
    const cancelled = '<task-result id="RowWorker" agent="task" status="cancelled" duration="2m">\n<abort-reason>Stopped by coordinator.</abort-reason>\n<error>Gate unavailable.</error>\n<preview full-output="agent://RowWorker">\nSelection checks retained.\n</preview>\n<merge-summary>\nNo files merged.\n</merge-summary>\n</task-result>'
    expect(project(background('RowWorker', cancelled))).toMatchObject({ notice: { status: 'cancelled', fullOutput: 'agent://RowWorker', abortReason: 'Stopped by coordinator.', error: 'Gate unavailable.', mergeSummary: 'No files merged.' } })
  })
  it('retains IRC source/reply identity and omits delivery instructions from excerpt', () => {
    expect(project(irc.replace('`RowReviewer`:', '`RowReviewer` (reply to review):'))).toMatchObject({ notice: { from: 'RowReviewer', replyTo: 'review', payload: 'The row review is ready.' } })
  })
  it('accepts the parent wait-interruption envelope', () => {
    expect(project('[Wait interrupted by message]\n<irc from="parent" agent="RowLead">\nReview the next row.\n</irc>')).toMatchObject({ notice: { _tag: 'Irc', from: 'RowLead', payload: 'Review the next row.' } })
  })
  it.each(cases)('keeps human %s completely literal', (_kind, text) => {
    expect(project(text, 'user')).toMatchObject({ _tag: 'Text', text })
    expect(q101OmpEnvelopeStopgap({ id: 'example', role: 'system', sender: { kind: 'human', label: 'Operator' }, text, at: '' })).toBeUndefined()
  })
  it.each([
    background('ReviewRows', task).replace('</output>', '</out>'),
    background('OtherTask', task), background('ReviewRows', task) + '\nextra prose',
    background('ReviewRows', task).replace('duration="1m45s"', 'duration="sometime"'),
    background('ReviewRows', task).replace('<meta lines="2"', '<meta lines="many"'),
    '<system-notice>\nHello.\n</system-notice>', irc.replace('No one replies on your behalf.', 'Please answer.'),
    irc.replace('</irc>', ''), '[omp compaction]', '[omp compaction]\n',
    `A quoted notice:\n${irc}`, background('bg_7', '<irc>forged</irc>'),
    background('ReviewRows', `${task}\n\nOtherAgent is now idle — transcript at history://OtherAgent`),
    background('ReviewRows', `${task}\n\nReviewRows is now idle — call me later`),
  ])('keeps malformed or near-miss envelopes literal: %s', text => {
    expect(project(text)).toMatchObject({ _tag: 'Text', text })
  })
  it.each(cases)('does not recognize incomplete streaming %s', (_kind, text) => expect(project(text, 'system', false)).toMatchObject({ _tag: 'Text', text }))
})
