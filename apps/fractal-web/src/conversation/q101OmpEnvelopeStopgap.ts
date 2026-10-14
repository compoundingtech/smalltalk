// q101 STOPGAP: remove this module and its fromTimeline hook when st adapters preserve OMP
// customType/details as typed TimelineBlocks (including Unsupported), and the four curated
// envelope projection tests pass against those blocks. external_sessions.rs currently drops them.
// This is a bounded, whole-body producer-grammar recognizer, NOT an XML/prose classifier.
import type { EventItem, HarnessNotice, Role, Sender } from './model.ts'

const identity = '[^"<>\\n]+'
const taskPattern = new RegExp(`^<task-result id="(${identity})" agent="(${identity})" status="(completed|cancelled|merge failed|failed \\(exit -?\\d+\\))" duration="([0-9][0-9a-z. ]*)">\\n(?:<meta lines="(\\d+)" size="([0-9.]+ ?(?:B|KB|MB|GB))" />\\n)?(?:<abort-reason>([^]*?)</abort-reason>\\n)?(?:<error>([^]*?)</error>\\n)?(?:<output>\\n([^]*?)\\n</output>|<preview full-output="agent://(${identity})">\\n([^]*?)\\n</preview>)(?:\\n<merge-summary>\\n([^]*?)\\n</merge-summary>)?\\n</task-result>$`)
const harnessTag = /<\/?(?:system-notice|task-result|irc|meta|output|preview|abort-reason|error|merge-summary)(?:[\s>])/i

/** A transport-free excerpt: authored prose or a useful JSON string, never braces/XML. */
export const ompNoticeExcerpt = (payload: string): string => {
  let prose = payload
  if (/^[\s]*[\[{]/.test(payload)) {
    try {
      const value: unknown = JSON.parse(payload)
      if (value !== null && typeof value === 'object' && !Array.isArray(value)) {
        const fields = Object.entries(value)
        const preferred = fields.find(([key, text]) => /^(summary|explanation|message|result|rebase|gate)$/.test(key) && typeof text === 'string')
        const text = preferred?.[1] ?? fields.find(([, text]) => typeof text === 'string' && text.length > 30 && /\s/.test(text))?.[1]
        prose = typeof text === 'string' ? text : ''
      } else prose = typeof value === 'string' ? value : ''
    } catch { prose = '' }
  }
  const line = prose.split('\n').map(line => line.trim()).find(line => line !== '' && !/^[#<>{}\[\]"`]/.test(line))
  return (line ?? 'Full result available in details.').replace(/^#+\s+|^[-*]\s+/g, '').replaceAll('**', '').slice(0, 200)
}

const taskNotice = (body: string, raw: string): HarnessNotice | undefined => {
  const match = taskPattern.exec(body)
  if (match === null) return undefined
  const [, taskId, agent, status, duration, lines, size, abortReason, error, output, fullOutput, preview, mergeSummary] = match
  if (duration === undefined || !/^(?:\d+ms|\d+(?:\.\d)?s|\d+m(?:\d+s)?|\d+h(?:\d+m)?|\d+d(?:\d+h)?)$/.test(duration)) return undefined
  const payload = output ?? preview ?? ''
  if (fullOutput !== undefined && fullOutput !== taskId) return undefined
  if ([payload, abortReason, error, mergeSummary].some(text => text !== undefined && harnessTag.test(text))) return undefined
  return { _tag: 'SubagentResult', taskId: taskId!, agent: agent!, status: status!, duration: duration!, payload, raw,
    ...(lines === undefined ? {} : { lines: Number(lines), size }),
    ...(fullOutput === undefined ? {} : { fullOutput: `agent://${fullOutput}` }),
    ...(abortReason === undefined ? {} : { abortReason }), ...(error === undefined ? {} : { error }),
    ...(mergeSummary === undefined ? {} : { mergeSummary }) }
}

/** Human-role text is never reinterpreted, even if it contains a perfectly valid envelope. */
export const q101OmpEnvelopeStopgap = ({ id, role, sender, text, at }: {
  readonly id: string; readonly role: Role; readonly sender?: Sender; readonly text: string; readonly at: string
}): EventItem | undefined => {
  if (role !== 'system' || sender?.kind === 'human' || text.length > 65536) return undefined
  let notice: HarnessNotice | undefined
  if (text.startsWith('[omp compaction]\n') && text.length > 17) {
    notice = { _tag: 'Compaction', payload: text.slice(17), raw: text }
  } else {
    const background = /^<system-notice>\nBackground job ([^\s<>]+) has completed\. Resume your work using the result below\.\n([^]*)\n<\/system-notice>$/.exec(text)
    if (background !== null) {
      const jobId = background[1]!, body = background[2]!
      // After the task envelope the producer may append the task follow-up line, then the
      // async structured-output footer; both are exact templates keyed by the same id.
      const taskEnd = body.lastIndexOf('</task-result>') + '</task-result>'.length
      const task = taskEnd >= '</task-result>'.length ? taskNotice(body.slice(0, taskEnd), text) : undefined
      const id = jobId.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
      const transcript = `(?:transcript at history://${id}|transcript unavailable)`
      const followUp = new RegExp(`^\\n\\n(${id} (?:is now idle — (?:message it via \`write agent://${id}\` to follow up; )?transcript at history://${id}|ran isolated and cannot be resumed or messaged — transcript at history://${id}|was stopped but is still resumable — (?:message it via \`write agent://${id}\` to resume; )?${transcript}|was aborted — ${transcript}))`).exec(body.slice(taskEnd))
      const suffix = body.slice(taskEnd + (followUp?.[0].length ?? 0))
      const structured = /^\n\nStructured output: schema (?:valid|invalid|unavailable)(?:: [^\n]+)?(?:; full payload at agent:\/\/[^\s,<>]+, fields via agent:\/\/[^\s<>]+\/<field>\[\/<index>\/…\])?(?:; preview:\n```json\n[^]*\n```)?$/.test(suffix)
      if (task?._tag === 'SubagentResult' && task.taskId === jobId && (suffix === '' || structured))
        notice = followUp === null ? task : { ...task, followUp: followUp[1]! }
      else if (!harnessTag.test(body)) {
        const duration = /(?:^|\n)Wall time: ([0-9.]+ seconds)$/.exec(body)?.[1]
        notice = { _tag: 'BackgroundJob', jobId, state: 'completed', payload: body, raw: text, ...(duration === undefined ? {} : { duration }) }
      }
    }
    const parent = /^\[Wait interrupted by message\]\n<irc from="parent" agent="([^"<>\n]+)">\n([^]*)\n<\/irc>$/.exec(text)
    if (parent !== null && !harnessTag.test(parent[2]!)) notice = { _tag: 'Irc', from: parent[1]!, payload: parent[2]!, raw: text }
    const incoming = /^<irc>\nIncoming IRC message from agent `([^`<>\n]+)`(?: \(reply to ([^<>\n]+)\))?:\n\n([^]*)\n<\/irc>$/.exec(text)
    if (incoming !== null) {
      const from = incoming[1]!, body = incoming[3]!
      const direct = `If response expected, reply via \`write\` (\`path: "agent://${from}"\`, \`content: "…"\`); may finish current step first. No one replies on your behalf.`
      const relay = `If response expected, reply via \`write\` (\`path: "agent://${from}"\`, \`content: "…"\`), when available; otherwise what you \`yield\` or say last this turn is delivered to \`${from}\` when you stop.`
      const tail = body.endsWith(direct) ? direct : body.endsWith(relay) ? relay : undefined
      if (tail !== undefined) {
        const payload = body.slice(0, -tail.length).replace(/\n\nSent while waiting\/working\. Active interruptible wait stopped early for immediate reading\.\n\n$/, '').trimEnd()
        if (payload !== '' && !harnessTag.test(payload)) notice = { _tag: 'Irc', from, payload, raw: text, ...(incoming[2] === undefined ? {} : { replyTo: incoming[2] }) }
      }
    }
  }
  if (notice === undefined) return undefined
  const kind = notice._tag === 'SubagentResult' ? 'subagent-result' : notice._tag === 'Irc' ? 'irc' : notice._tag === 'Compaction' ? 'compaction' : 'background-job'
  const title = notice._tag === 'SubagentResult' ? `${notice.taskId} · ${notice.status} · ${notice.duration}`
    : notice._tag === 'Irc' ? `Message from ${notice.from}` : notice._tag === 'Compaction' ? 'Context compacted' : `Background job ${notice.jobId} completed${notice.duration === undefined ? '' : ` · ${notice.duration}`}`
  return { _tag: 'Event', id, kind, title, text: ompNoticeExcerpt(notice.payload), notice,
    sender: { kind: 'harness', label: 'OMP' }, data: notice, at }
}
