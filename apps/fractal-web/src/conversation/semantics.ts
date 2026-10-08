import type { ConversationItem, Role, Sender, TextItem } from './model.ts'

/** The media type used by st's wire-compatible omp custom-message projection. */
export const ompEventMediaType = 'application/vnd.omp.event+json'

/** Read object fields without assuming a producer-specific shape. */
export const fieldsOf = (value: unknown): Record<string, unknown> =>
  typeof value === 'object' && value !== null && !Array.isArray(value)
    ? (value as Record<string, unknown>)
    : {}
const stringField = ({
  value,
  key,
}: {
  readonly value: unknown
  readonly key: string
}): string | undefined => {
  const field = fieldsOf(value)[key]
  return typeof field === 'string' ? field : undefined
}

/** Default sender labels never turn missing provenance into an invented agent identity. */
export const roleSender = ({
  role,
  agentName = 'Agent',
}: {
  readonly role: Role
  readonly agentName?: string
}): Sender =>
  role === 'assistant'
    ? { kind: 'agent', label: agentName }
    : role === 'user'
      ? { kind: 'human', label: 'You', via: 'harness' }
      : { kind: 'system', label: 'System' }

/** Small Talk subjects carry provenance in their explicit subject prefix. */
export const deliverySender = (from: string | undefined): Sender =>
  from?.startsWith('agent/') === true
    ? { kind: 'st-agent', label: from.slice('agent/'.length), via: 'st' }
    : from?.startsWith('human/') === true ||
        from?.startsWith('user/') === true ||
        from?.startsWith('person/') === true
      ? { kind: 'human', label: from.slice(from.indexOf('/') + 1), via: 'wf / st' }
      : { kind: 'system', label: from ?? 'Small Talk', via: 'st' }

/** st's native injection is an explicit envelope, not a human message merely because role=user. */
export const proseItem = (item: TextItem): TextItem => {
  if (item.role !== 'user' && item.role !== 'system') return item
  const envelope = /^<smalltalk-message\b([^>]*)>\s*([\s\S]*?)\s*<\/smalltalk-message>/.exec(
    item.text,
  )
  const from = envelope !== null ? /\bfrom="([^"]+)"/.exec(envelope[1]!)?.[1] : undefined
  if (envelope === null || from === undefined) return item
  const sender = deliverySender(from)
  return { ...item, text: envelope[2]!, sender, role: sender.kind === 'human' ? 'user' : 'system' }
}

/**
 * Mirrors st3-views' shown/delivered semantics for envelope, channel and PING delivery links.
 * That package is not a dependency here. Only explicit graph links suppress mail, never prose.
 * Keep unrelated text in a native turn; the two driver delivery notes are not user prose.
 */
export const withoutShownDeliveries = (raw: string, shown: ReadonlySet<string>): string => {
  let delivered = false
  let text = raw.replace(/\r\n/g, '\n')
  text = text.replace(/<channel\b([^>]*)>[\s\S]*?<\/channel>/g, (block: string, head: string) => {
    const id = /\bmessageId="([^"]+)"/.exec(head)?.[1] ??
      /^\s*\[PING from st3\] (message\/\S+) from /m.exec(block)?.[1] ??
      /<smalltalk-message\b[^>]*\bgraph="([^"]+)"/.exec(block)?.[1]
    if (id === undefined || !shown.has(id)) return block
    delivered = true
    return ''
  })
  text = text.replace(/<smalltalk-message\b([^>]*)>[\s\S]*?<\/smalltalk-message>/g, (block: string, head: string) => {
    const id = /\bgraph="([^"]+)"/.exec(head)?.[1]
    if (id === undefined || !shown.has(id)) return block
    delivered = true
    return ''
  })
  text = text.split('\n').filter((line) => {
    const id = /^\s*\[PING from st3\] (message\/\S+) from /.exec(line)?.[1]
    if (id === undefined || !shown.has(id)) return true
    delivered = true
    return false
  }).join('\n')
  if (!delivered) return raw
  return text.split('\n').filter((line) =>
    line.trim() !== "The person reads replies in st, not in the agent's session." &&
    line.trim() !== '(dictated by voice; it may contain transcription mistakes)',
  ).join('\n').trim()
}

/** Deliberately decode an event envelope; arbitrary prose and JSON remain prose. */
export const structuredEvent = ({
  id,
  data,
  eventType,
  at,
}: {
  readonly id: string
  readonly data: unknown
  readonly eventType: string
  readonly at?: string
}): ConversationItem => {
  const event = fieldsOf(data)
  const kind = stringField({ value: data, key: 'kind' }) ?? eventType
  const details = fieldsOf(event['details'])
  const content = typeof event['content'] === 'string' ? event['content'] : ''
  const base = { id, ...(at !== undefined ? { at } : {}) }
  if (kind === 'irc:incoming') {
    const from = stringField({ value: details, key: 'from' })
    return {
      _tag: 'Event',
      ...base,
      kind: 'harness-message',
      title: 'Harness message',
      sender: { kind: 'harness', label: from ?? 'IRC', via: 'IRC' },
      text: stringField({ value: details, key: 'message' }) ?? content,
      data,
    }
  }
  if (kind === 'async-result') {
    const jobs = Array.isArray(details['jobs']) ? details['jobs'] : []
    const task =
      jobs.length === 1 && stringField({ value: jobs[0], key: 'type' }) === 'task'
        ? jobs[0]
        : undefined
    const output = /<(?:output|preview)>([\s\S]*?)<\/(?:output|preview)>/.exec(content)?.[1]?.trim()
    let outputSummary = output
    if (output !== undefined && /^[{[]/.test(output)) {
      try {
        outputSummary = stringField({ value: JSON.parse(output), key: 'summary' })
      } catch {
        outputSummary = undefined
      }
    }
    const schemaData = fieldsOf(fieldsOf(fieldsOf(task)['schema'])['data'])
    const summary = stringField({ value: schemaData, key: 'summary' })
    return {
      _tag: 'Event',
      ...base,
      kind: task !== undefined ? 'subagent-result' : 'harness-message',
      title: task !== undefined ? 'Subagent result' : 'Background result',
      sender:
        task !== undefined
          ? {
              kind: 'subagent',
              label:
                stringField({ value: task, key: 'label' }) ??
                stringField({ value: task, key: 'jobId' }) ??
                'Subagent',
              via: 'native',
            }
          : { kind: 'system', label: 'Harness', via: 'background' },
      text:
        task !== undefined
          ? (summary ??
            outputSummary ??
            'Result received. Open event details to inspect the native payload.')
          : jobs.length > 0
            ? `${jobs.length} background ${jobs.length === 1 ? 'job' : 'jobs'} completed.`
            : 'Background result received.',
      data,
    }
  }
  if (kind === 'resource-observed' || kind === 'resource.observed') {
    const resource = fieldsOf(
      event['data'] ?? event['details'] ?? fieldsOf(event['body'])['fields'] ?? data,
    )
    const ref =
      stringField({ value: event, key: 'subject' }) ??
      stringField({ value: resource, key: 'ref' }) ??
      stringField({ value: resource, key: 'resource_ref' }) ??
      stringField({ value: resource, key: 'subject' })
    return {
      _tag: 'Event',
      ...base,
      kind: 'resource-observed',
      title: 'Resource observed',
      sender: { kind: 'system', label: 'Small Talk', via: 'daemon' },
      text: ref ?? 'Resource observation received',
      data,
    }
  }
  // User-attributed extension messages (for example skills) are actual human input.
  if (
    event['_tag'] === 'OmpEvent' &&
    event['version'] === 1 &&
    event['attribution'] === 'user' &&
    content !== ''
  ) {
    return {
      _tag: 'Text',
      id,
      at: at ?? '',
      role: 'user',
      text: content,
      attachments: [],
      streaming: false,
      sender: { kind: 'human', label: 'You', via: 'harness' },
    }
  }
  return { _tag: 'UnknownEvent', ...base, eventType: kind, data }
}

/** Explicit structured media types authorize decoding; ordinary prose never gets guessed at. */
export const contentEvent = ({
  id,
  text,
  mediaType,
  at,
}: {
  readonly id: string
  readonly text: string
  readonly mediaType: string
  readonly at: string
}): ConversationItem | undefined => {
  if (mediaType !== ompEventMediaType && mediaType !== 'application/json') return undefined
  try {
    const data: unknown = JSON.parse(text)
    const event = fieldsOf(data)
    if (mediaType === 'application/json')
      return structuredEvent({
        id,
        data,
        eventType: stringField({ value: data, key: 'type' }) ?? 'structured content',
        at,
      })
    if (event['_tag'] !== 'OmpEvent' || event['version'] !== 1 || typeof event['kind'] !== 'string')
      return { _tag: 'UnknownEvent', id, at, eventType: 'invalid omp event', data }
    return structuredEvent({ id, data, eventType: event['kind'], at })
  } catch {
    return { _tag: 'UnknownEvent', id, at, eventType: 'invalid omp event', data: { content: text } }
  }
}

/** Attribution for every renderer; authoritative item provenance takes precedence. */
export const senderOf = ({
  item,
  agentName,
}: {
  readonly item: ConversationItem
  readonly agentName: string
}): Sender => {
  if ('sender' in item && item.sender !== undefined) return item.sender
  switch (item._tag) {
    case 'Text':
      return roleSender({ role: item.role, agentName: agentName })
    case 'Message':
      return deliverySender(item.from)
    case 'Reasoning':
    case 'ToolCall':
      return { kind: 'agent', label: agentName }
    case 'Usage':
      return { kind: 'system', label: 'Usage' }
    case 'Status':
      return { kind: 'system', label: 'Run' }
    case 'Notice':
    case 'UnknownEvent':
      return { kind: 'system', label: 'System' }
    case 'Event':
      return item.sender
  }
}
