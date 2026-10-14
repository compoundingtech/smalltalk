// Usage: bun scripts/replay-transcript.ts <saved-page-directory> [<directory> ...]
// Replay the actual SDK -> fold -> transcript path; inspect previews and tool details are not prose.
// stdout is intentionally JSON for comparison across saved wire captures.
import { readdirSync, readFileSync } from 'node:fs'
import { join, resolve } from 'node:path'
import { Effect, Exit, Schema } from 'effect'
import { decodeConversationChunk } from '../packages/st3-sdk/src/effect/mod.ts'
import { LiveTimeline } from '../src/conversation/fromTimeline.ts'
import { prepareTranscriptTurns, type PreparedTranscriptTurn } from '../src/web/conversationTranscript.ts'

class ReplayError extends Schema.TaggedError<ReplayError>()('ReplayError', {
  message: Schema.String,
  cause: Schema.optional(Schema.Defect()),
}) {}

const SavedPage = Schema.Struct({
  value: Schema.Struct({
    items: Schema.Array(Schema.Unknown),
    session_id: Schema.optional(Schema.String),
    header: Schema.optional(Schema.Unknown),
    page: Schema.optional(Schema.Struct({ has_more: Schema.optional(Schema.Boolean) })),
  }),
})

type DrawnItem = PreparedTranscriptTurn['items'][number]
type Field = { readonly field: string; readonly text: string }
const drawnFields = (item: DrawnItem): readonly Field[] => {
  switch (item._tag) {
    case 'Text': return [{ field: 'text', text: item.text }]
    case 'Notice': return [{ field: 'text', text: item.text }, ...(item.detail === undefined ? [] : [{ field: 'detail', text: item.detail }])]
    case 'Message': return item.title === undefined ? [] : [{ field: 'title', text: item.title }]
    case 'Event':
      // HarnessNotice payload/facts are explicit disclosure details, not reading-flow text.
      return [{ field: 'title', text: item.title }, { field: 'text', text: item.text }]
    case 'Unsupported':
    case 'UnknownEvent':
    case 'Reasoning':
    case 'ToolCall':
    case 'Status':
    case 'Usage': return []
    default: {
      const exhaustive: never = item
      return exhaustive
    }
  }
}

const jsonValue = Schema.decodeUnknownExit(Schema.fromJsonString(Schema.Unknown))
const rawSignature = (text: string): string | undefined => {
  const trimmed = text.trim()
  if ((trimmed.startsWith('{') || trimmed.startsWith('[')) && Exit.isSuccess(jsonValue(trimmed))) return 'json-blob'
  if (text.includes('[omp ') || text.includes('[unrecognized omp') || text.includes('{\"raw\":')
    || /<\/?(?:system-notice|system-reminder|task-result|irc)(?:\s|>)/iu.test(text)) return 'harness-envelope'
  return undefined
}

const readPages = Effect.fn('replay-transcript.read-pages')(function* (directory: string) {
  const files = yield* Effect.try({
    try: () => readdirSync(directory).filter(file => file.endsWith('.json')).sort(),
    catch: cause => new ReplayError({ message: `Cannot list saved-page directory ${directory}`, cause }),
  })
  if (files.length === 0) return yield* new ReplayError({ message: `No saved JSON pages in ${directory}` })
  return files.map(file => join(directory, file))
})

const replayPage = Effect.fn('replay-transcript.page')(function* (path: string) {
  const text = yield* Effect.try({ try: () => readFileSync(path, 'utf8'), catch: cause => new ReplayError({ message: `Cannot read saved page ${path}`, cause }) })
  const { value } = yield* Schema.decodeUnknownEffect(Schema.fromJsonString(SavedPage))(text).pipe(
    Effect.mapError(cause => new ReplayError({ message: `Cannot decode saved {value:{items}} page ${path}`, cause })),
  )
  const timeline = new LiveTimeline()
  timeline.apply(decodeConversationChunk({
    kind: 'conversation', collection: 'conversation', id: `replay/${path}`,
    // This identity is only a local frame for old captures without a session id.
    session_id: value.session_id ?? 'session/replay',
    replace: true, has_more: value.page?.has_more ?? false,
    items: value.items, ...(value.header === undefined ? {} : { header: value.header }),
  }))
  const turns = prepareTranscriptTurns(timeline.project().items, { showSystemEvents: false, firstTurnComplete: true })
  const tags: Record<string, number> = {}
  const unsupported = new Map<string, { harness: string; recordKind: string; reason: string; count: number }>()
  const raw: { id: string; tag: string; field: string; signature: string }[] = []
  for (const turn of turns) {
    for (const item of [...(turn.prompt === undefined ? [] : [turn.prompt]), ...turn.items]) {
      tags[item._tag] = (tags[item._tag] ?? 0) + 1
      if (item._tag === 'Unsupported') {
        const key = `${item.harness}\u0000${item.recordKind}\u0000${item.reason}`
        const group = unsupported.get(key)
        if (group === undefined) unsupported.set(key, { harness: item.harness, recordKind: item.recordKind, reason: item.reason, count: 1 })
        else group.count += 1
      }
      for (const { field, text } of drawnFields(item)) {
        const signature = rawSignature(text)
        if (signature !== undefined) raw.push({ id: item.id, tag: item._tag, field, signature })
      }
    }
  }
  return { path, thread: value.session_id ?? path, entries: value.items.length, turns: turns.length, tags, unsupported: [...unsupported.values()], rawCount: raw.length, raw }
})

const main = Effect.gen(function* () {
  const directories = process.argv.slice(2)
  if (directories.length === 0) return yield* new ReplayError({ message: 'Usage: bun scripts/replay-transcript.ts <saved-page-directory> [<directory> ...]' })
  const perThread = []
  for (const directory of directories) {
    const pages = yield* readPages(resolve(directory))
    for (const path of pages) perThread.push(yield* replayPage(path))
  }
  return { perThread, rawCount: perThread.reduce((count, thread) => count + thread.rawCount, 0) }
})

Effect.runPromise(main).then(report => {
  console.log(Schema.encodeSync(Schema.fromJsonString(Schema.Unknown))(report))
}).catch((error: unknown) => {
  console.error(`FAIL replay-transcript: ${String(error)}`)
  process.exitCode = 1
})
