import * as Effect from 'effect/Effect'
import * as Queue from 'effect/Queue'
import * as Result from 'effect/Result'
import * as Schema from 'effect/Schema'
import type * as Scope from 'effect/Scope'
import * as SubscriptionRef from 'effect/SubscriptionRef'

/** Binary raw-terminal WebSocket protocol. */
export const PTY_SUBPROTOCOL = 'st3.client.pty.v0'
const SizeInt = Schema.Int.check(Schema.isBetween({ minimum: 0, maximum: 65535 }))
const sizes = { rows: SizeInt, cols: SizeInt, cellWidth: Schema.optionalKey(SizeInt), cellHeight: Schema.optionalKey(SizeInt) }
/** Frames of the PTY transport: tags 0-7, 10 and 11 are typed; 8/9 (ownership and lifecycle-CAS extensions, refused on this path) and unknown tags pass through opaque as `Unknown`. */
export const PtyFrame = Schema.Union([
  Schema.TaggedStruct('Data', { bytes: Schema.Uint8Array }),
  Schema.TaggedStruct('Attach', sizes),
  Schema.TaggedStruct('Detach', {}),
  Schema.TaggedStruct('Resize', sizes),
  Schema.TaggedStruct('Exit', { code: Schema.Int }),
  Schema.TaggedStruct('Screen', { bytes: Schema.Uint8Array }),
  Schema.TaggedStruct('Peek', { plain: Schema.Boolean, full: Schema.Boolean }),
  Schema.TaggedStruct('Status', { payload: Schema.Uint8Array }),
  Schema.TaggedStruct('Geometry', { rows: SizeInt, cols: SizeInt }),
  Schema.TaggedStruct('ResetInputModes', {}),
  Schema.TaggedStruct('Unknown', { tag: SizeInt, payload: Schema.Uint8Array }),
])
/** A decoded PTY frame. */
export type PtyFrame = typeof PtyFrame.Type
/** A malformed or oversized binary frame. */
export class PtyProtocolError extends Schema.TaggedError<PtyProtocolError>()('PtyProtocolError', { message: Schema.String }) {}
const maxFrameSize = 16 * 1024 * 1024
/** Largest DATA payload per WebSocket message; st3 caps inbound messages at 64 KiB. */
export const ptyDataChunkBytes = 16 * 1024

/** Encode one complete frame with its network-order header. */
export const encodePtyFrame = (frame: PtyFrame): Uint8Array => {
  let tag: number
  let payload: Uint8Array
  switch (frame._tag) {
    case 'Data':
      tag = 0
      payload = frame.bytes
      break
    case 'Screen':
      tag = 5
      payload = frame.bytes
      break
    case 'Status':
      tag = 7
      payload = frame.payload
      break
    case 'Unknown':
      tag = frame.tag
      payload = frame.payload
      break
    case 'Attach':
    case 'Resize':
    case 'Geometry': {
      tag = frame._tag === 'Attach' ? 1 : frame._tag === 'Resize' ? 3 : 10
      const cells = frame._tag !== 'Geometry' && (frame.cellWidth !== undefined || frame.cellHeight !== undefined)
      payload = new Uint8Array(cells ? 8 : 4)
      const view = new DataView(payload.buffer)
      view.setUint16(0, frame.rows)
      view.setUint16(2, frame.cols)
      if (cells) {
        view.setUint16(4, frame.cellWidth ?? 0)
        view.setUint16(6, frame.cellHeight ?? 0)
      }
      break
    }
    case 'Exit':
      tag = 4
      payload = new Uint8Array(4)
      new DataView(payload.buffer).setInt32(0, frame.code)
      break
    case 'Peek':
      tag = 6
      payload = Uint8Array.of(Number(frame.plain) | (Number(frame.full) << 1))
      break
    case 'Detach':
      tag = 2
      payload = new Uint8Array(0)
      break
    case 'ResetInputModes':
      tag = 11
      payload = new Uint8Array(0)
      break
  }
  const bytes = new Uint8Array(5 + payload.length)
  bytes[0] = tag
  new DataView(bytes.buffer).setUint32(1, payload.length)
  bytes.set(payload, 5)
  return bytes
}

const decodeFrame = (tag: number, payload: Uint8Array): Result.Result<PtyFrame, PtyProtocolError> => {
  const bad = () => Result.fail(new PtyProtocolError({ message: `Malformed PTY tag ${tag} payload (${payload.length} bytes)` }))
  const view = new DataView(payload.buffer, payload.byteOffset, payload.byteLength)
  switch (tag) {
    case 0: return Result.succeed({ _tag: 'Data', bytes: payload })
    case 5: return Result.succeed({ _tag: 'Screen', bytes: payload })
    case 7: return Result.succeed({ _tag: 'Status', payload })
    case 1:
    case 3:
      if (payload.length !== 4 && (payload.length < 8 || (tag === 3 && payload.length !== 8))) return bad()
      return Result.succeed({ _tag: tag === 1 ? 'Attach' : 'Resize', rows: view.getUint16(0), cols: view.getUint16(2), ...(payload.length >= 8 ? { cellWidth: view.getUint16(4), cellHeight: view.getUint16(6) } : {}) })
    case 10:
      return payload.length < 4 ? bad() : Result.succeed({ _tag: 'Geometry', rows: view.getUint16(0), cols: view.getUint16(2) })
    case 4: return payload.length !== 4 ? bad() : Result.succeed({ _tag: 'Exit', code: view.getInt32(0) })
    case 6: return payload.length !== 1 ? bad() : Result.succeed({ _tag: 'Peek', plain: (payload[0]! & 1) !== 0, full: (payload[0]! & 2) !== 0 })
    case 2: return payload.length !== 0 ? bad() : Result.succeed({ _tag: 'Detach' })
    case 11: return payload.length !== 0 ? bad() : Result.succeed({ _tag: 'ResetInputModes' })
    default: return Result.succeed({ _tag: 'Unknown', tag, payload })
  }
}

/** Read frames incrementally, independent of WebSocket message boundaries. */
export const makePtyFrameReader = (): { readonly feed: (chunk: Uint8Array) => Result.Result<ReadonlyArray<PtyFrame>, PtyProtocolError> } => {
  const header = new Uint8Array(5)
  let headerUsed = 0
  let payload: Uint8Array | undefined
  let used = 0
  let failure: PtyProtocolError | undefined
  return { feed: chunk => {
    if (failure !== undefined) return Result.fail(failure)
    const frames: PtyFrame[] = []
    let offset = 0
    while (offset < chunk.length || (payload !== undefined && used === payload.length)) {
      if (payload === undefined) {
        const count = Math.min(5 - headerUsed, chunk.length - offset)
        header.set(chunk.subarray(offset, offset + count), headerUsed)
        offset += count
        headerUsed += count
        if (headerUsed !== 5) break
        const length = new DataView(header.buffer).getUint32(1)
        if (length > maxFrameSize) {
          failure = new PtyProtocolError({ message: `PTY frame exceeds ${maxFrameSize} bytes` })
          return Result.fail(failure)
        }
        payload = new Uint8Array(length)
        used = 0
      }
      const count = Math.min(payload.length - used, chunk.length - offset)
      payload.set(chunk.subarray(offset, offset + count), used)
      used += count
      offset += count
      if (used !== payload.length) break
      const frame = decodeFrame(header[0]!, payload)
      if (Result.isFailure(frame)) {
        failure = frame.failure
        return Result.fail(failure)
      }
      frames.push(frame.success)
      payload = undefined
      headerUsed = 0
    }
    return Result.succeed(frames)
  } }
}

/** The terminal and exact runtime incarnation to attach. */
export interface PtyTarget {
  readonly terminalId: string
  readonly incarnation: string
}
/** ATTACH types and votes the shared size (`terminal.control`); PEEK only watches (`terminal.read`). */
export type PtyMode = 'attach' | 'peek'
/** Requested terminal dimensions. */
export interface PtySize {
  readonly rows: number
  readonly cols: number
}
/** A grant request failure with the server's retry policy. */
export class PtyAttachFailure extends Schema.TaggedError<PtyAttachFailure>()('PtyAttachFailure', {
  message: Schema.String, status: Schema.optionalKey(Schema.Number), code: Schema.optionalKey(Schema.String), retryable: Schema.Boolean,
}) {}
/** A single-use raw-stream capability. */
export interface PtyAttachment {
  readonly terminalId: string
  readonly incarnation: string
  readonly ownerHostId: string
  readonly capability: string
}
const AttachmentEnvelope = Schema.Struct({ api_version: Schema.String, snapshot: Schema.Unknown, value: Schema.Struct({
  terminal_id: Schema.String, runtime_incarnation: Schema.String, owner_host_id: Schema.String, mode: Schema.Literals(['attach', 'peek']), stream_capability: Schema.String,
}) })
const ErrorEnvelope = Schema.Struct({ api_version: Schema.String, error_version: Schema.String, code: Schema.String, message: Schema.String, retryable: Schema.optionalKey(Schema.Boolean) })
const terminalPath = (target: PtyTarget) => `/v1/client/terminals/${encodeURIComponent(target.terminalId.replace(/^terminal\//, ''))}`
const base = (url: string) => url.replace(/\/+$/, '')
/** Request a fresh capability, decoding success and refusal envelopes at the boundary. */
export const requestPtyAttachment = Effect.fn('st3.pty.attachment')(function* (options: { readonly baseUrl: string; readonly target: PtyTarget; readonly mode: PtyMode; readonly fetch?: typeof fetch }) {
  const response = yield* Effect.tryPromise({
    try: signal => (options.fetch ?? globalThis.fetch)(`${base(options.baseUrl)}${terminalPath(options.target)}/raw-attachments`, {
      method: 'POST', headers: { 'Content-Type': 'application/json' }, body: JSON.stringify({ runtime_incarnation: options.target.incarnation, mode: options.mode }), signal,
    }),
    catch: cause => new PtyAttachFailure({ message: String(cause), retryable: true }),
  })
  const json = yield* Effect.tryPromise({ try: () => response.json() as Promise<unknown>, catch: cause => new PtyAttachFailure({ message: `Invalid PTY attachment response: ${String(cause)}`, status: response.status, retryable: false }) })
  if (!response.ok) {
    const error = yield* Schema.decodeUnknownEffect(ErrorEnvelope)(json).pipe(Effect.mapError(cause => new PtyAttachFailure({ message: String(cause), status: response.status, retryable: false })))
    return yield* new PtyAttachFailure({ message: error.message, status: response.status, code: error.code, retryable: error.retryable ?? response.status >= 500 })
  }
  const envelope = yield* Schema.decodeUnknownEffect(AttachmentEnvelope)(json).pipe(Effect.mapError(cause => new PtyAttachFailure({ message: String(cause), status: response.status, retryable: false })))
  if (envelope.value.mode !== options.mode) return yield* new PtyAttachFailure({ message: `Requested a ${options.mode} capability, received ${envelope.value.mode}`, status: response.status, retryable: false })
  return { terminalId: envelope.value.terminal_id, incarnation: envelope.value.runtime_incarnation, ownerHostId: envelope.value.owner_host_id, capability: envelope.value.stream_capability }
})
/** Build the fenced raw-stream WebSocket URL. */
export const ptyStreamUrl = (options: { readonly baseUrl: string; readonly target: PtyTarget; readonly mode: PtyMode }): string =>
  `${base(options.baseUrl).replace(/^http:/, 'ws:').replace(/^https:/, 'wss:')}${terminalPath(options.target)}/raw-stream?incarnation=${encodeURIComponent(options.target.incarnation)}&mode=${options.mode}`

/** Connection state, including terminal refusal and incarnation fencing. */
export type PtyState =
  | { _tag: 'Connecting'; attempt: number }
  | { _tag: 'Live'; rows: number; cols: number }
  | { _tag: 'Reconnecting'; attempt: number; delayMs: number; issue: string }
  | { _tag: 'Refused'; message: string; status?: number; code?: string }
  | { _tag: 'Failed'; message: string }
  | { _tag: 'Stale'; message: string }
  | { _tag: 'Exited'; code: number }
  | { _tag: 'Closed' }
/** Observations driving the pure connection reducer. */
export type PtyEvent =
  | { _tag: 'AttachFailed'; failure: PtyAttachFailure }
  | { _tag: 'SocketOpened' }
  | { _tag: 'Geometry'; rows: number; cols: number }
  | { _tag: 'SocketLost'; issue: string }
  | { _tag: 'ProcessExited'; code: number }
  | { _tag: 'RetryDue' }
  | { _tag: 'Closed' }
/** Capped exponential retry delay. */
export const ptyBackoffMillis = (attempt: number): number => Math.min(30_000, 500 * 2 ** Math.max(0, attempt - 1))
type SettledPtyState = Exclude<PtyState, { _tag: 'Connecting' | 'Live' | 'Reconnecting' }>
const terminal = (state: PtyState): state is SettledPtyState =>
  state._tag !== 'Connecting' && state._tag !== 'Live' && state._tag !== 'Reconnecting'
/** Reduce one observed transport event; geometry is the live acknowledgement. */
export const ptyTransition = ({ state, event, maxAttempts = 3 }: { readonly state: PtyState; readonly event: PtyEvent; readonly maxAttempts?: number }): PtyState => {
  if (event._tag === 'Closed') return { _tag: 'Closed' }
  if (terminal(state)) return state
  if (event._tag === 'ProcessExited') return { _tag: 'Exited', code: event.code }
  if (event._tag === 'Geometry') return { _tag: 'Live', rows: event.rows, cols: event.cols }
  if (event._tag === 'RetryDue') return state._tag === 'Reconnecting' ? { _tag: 'Connecting', attempt: state.attempt + 1 } : state
  if (event._tag === 'SocketOpened') return state
  if (event._tag === 'AttachFailed') {
    const { failure } = event
    if (failure.status === 401 || failure.status === 403 || failure.code === 'forbidden' || failure.code === 'unauthorized')
      return { _tag: 'Refused', message: failure.message, ...(failure.status === undefined ? {} : { status: failure.status }), ...(failure.code === undefined ? {} : { code: failure.code }) }
    if (failure.status === 409 || failure.code === 'stale-fence') return { _tag: 'Stale', message: failure.message }
    // The terminal is not running: settle so the projected screen stays visible read-only with the reason.
    if (failure.status === 404 || failure.code === 'not-found' || failure.code === 'not_found') return { _tag: 'Failed', message: failure.message }
    if (!failure.retryable) return { _tag: 'Failed', message: failure.message }
  }
  const attempt = state._tag === 'Live' ? 1 : state.attempt
  const issue = event._tag === 'AttachFailed' ? event.failure.message : event.issue
  return attempt >= maxAttempts ? { _tag: 'Failed', message: issue } : { _tag: 'Reconnecting', attempt, delayMs: ptyBackoffMillis(attempt), issue }
}
/** Permanent transport failures that permit the projected read-only fallback. */
export const ptyFallback = (state: PtyState): { readonly reason: 'refused' | 'failed'; readonly message: string } | undefined =>
  state._tag === 'Refused' ? { reason: 'refused', message: state.message } : state._tag === 'Failed' ? { reason: 'failed', message: state.message } : undefined
/** Minimal browser socket boundary for injectable transports. */
export interface PtySocket {
  binaryType: string
  send(data: ArrayBuffer): void
  close(code?: number, reason?: string): void
  onopen: ((event: unknown) => void) | null
  onmessage: ((event: { readonly data: unknown }) => void) | null
  onclose: ((event: { readonly code: number; readonly reason: string }) => void) | null
  onerror: ((event: unknown) => void) | null
}
/** Construct a binary socket with the required capability subprotocols. */
export type PtySocketFactory = (url: string, protocols: ReadonlyArray<string>) => PtySocket
/** The browser WebSocket behind the minimal socket boundary. */
const browserSocket: PtySocketFactory = (url, protocols) => {
  const socket = new WebSocket(url, [...protocols])
  socket.binaryType = 'arraybuffer'
  const boundary: PtySocket = {
    binaryType: 'arraybuffer',
    send: data => socket.send(data),
    close: (code, reason) => socket.close(code, reason),
    onopen: null,
    onmessage: null,
    onclose: null,
    onerror: null,
  }
  socket.onopen = event => boundary.onopen?.(event)
  socket.onmessage = event => boundary.onmessage?.(event)
  socket.onclose = event => boundary.onclose?.(event)
  socket.onerror = event => boundary.onerror?.(event)
  return boundary
}
/** Terminal bytes and authoritative geometry delivered to the emulator. */
export type PtyOutput = { _tag: 'Screen'; bytes: Uint8Array } | { _tag: 'Data'; bytes: Uint8Array } | { _tag: 'Geometry'; rows: number; cols: number } | { _tag: 'Exit'; code: number }
/** Scoped raw-terminal connection with unbuffered synchronous input. */
export interface PtySession {
  readonly mode: PtyMode
  readonly state: SubscriptionRef.SubscriptionRef<PtyState>
  /** Send DATA only while a live ATTACH; input is never retained for a retry. A PEEK sends nothing. */
  readonly send: (bytes: Uint8Array) => boolean
  /** Remember the next attach size and resize a live ATTACH immediately. A PEEK has no size vote. */
  readonly resize: (size: PtySize) => void
  /** Clear server-side input modes only while a live ATTACH. */
  readonly resetInputModes: () => boolean
}

/** Run fresh-capability reconnects for the lifetime of the caller's scope. */
export const openPtySession = Effect.fn('st3.pty.session')(function* (options: {
  readonly baseUrl: string
  readonly target: PtyTarget
  readonly mode: PtyMode
  /** The ATTACH size; a PEEK only watches the shared geometry. */
  readonly size: PtySize
  readonly onOutput: (output: PtyOutput) => void
  readonly fetch?: typeof fetch
  readonly socket?: PtySocketFactory
  readonly maxAttempts?: number
}) {
  const scope: Scope.Scope = yield* Effect.scope
  const state = yield* SubscriptionRef.make<PtyState>({ _tag: 'Connecting', attempt: 1 })
  let current: PtyState = yield* SubscriptionRef.get(state)
  let size = options.size
  // A resize refused before the first GEOMETRY ack is flushed once the stream is live.
  let resizePending = false
  let liveSocket: PtySocket | undefined
  let writable = false
  let disposed = false
  const transition = (event: PtyEvent) => {
    current = ptyTransition({ state: current, event, ...(options.maxAttempts === undefined ? {} : { maxAttempts: options.maxAttempts }) })
    return SubscriptionRef.set(state, current)
  }
  const attach = options.mode === 'attach'
  // The server closes a PEEK socket on any frame but DETACH and STATUS.
  const write = (frame: PtyFrame) => {
    if ((!attach && frame._tag !== 'Detach') || !writable || current._tag !== 'Live' || liveSocket === undefined) return false
    try {
      liveSocket.send(encodePtyFrame(frame).buffer as ArrayBuffer)
      return true
    } catch {
      liveSocket.onerror?.({})
      return false
    }
  }
  const attempt = Effect.fn('st3.pty.connect')(function* () {
    const grant = yield* Effect.result(requestPtyAttachment(options))
    if (Result.isFailure(grant)) {
      yield* transition({ _tag: 'AttachFailed', failure: grant.failure })
      return
    }
    if (disposed) return
    const events = yield* Queue.unbounded<PtyEvent | PtyOutput>()
    const created = yield* Effect.try({
      try: () => (options.socket ?? browserSocket)(ptyStreamUrl(options), [PTY_SUBPROTOCOL, `st3.cap.${grant.success.capability}`]),
      catch: cause => String(cause),
    }).pipe(Effect.result)
    if (Result.isFailure(created)) {
      yield* transition({ _tag: 'SocketLost', issue: created.failure })
      return
    }
    const socket = created.success
    liveSocket = socket
    socket.binaryType = 'arraybuffer'
    const reader = makePtyFrameReader()
    let ended = false
    const lose = (issue: string) => {
      if (ended || disposed) return
      ended = true
      writable = false
      Queue.offerUnsafe(events, { _tag: 'SocketLost', issue })
      socket.close()
    }
    socket.onopen = () => {
      if (ended || disposed) return
      try {
        socket.send(encodePtyFrame(attach ? { _tag: 'Attach', ...size } : { _tag: 'Peek', plain: false, full: false }).buffer as ArrayBuffer)
        resizePending = false
        Queue.offerUnsafe(events, { _tag: 'SocketOpened' })
      } catch (cause) {
        lose(String(cause))
      }
    }
    socket.onclose = event => lose(`PTY socket closed (${event.code}): ${event.reason}`)
    socket.onerror = () => lose('PTY socket error')
    socket.onmessage = event => {
      if (ended || disposed || !(event.data instanceof ArrayBuffer)) return
      const decoded = reader.feed(new Uint8Array(event.data))
      if (Result.isFailure(decoded)) {
        lose(decoded.failure.message)
        return
      }
      for (const frame of decoded.success) {
        if (frame._tag === 'Data' || frame._tag === 'Screen' || frame._tag === 'Geometry' || frame._tag === 'Exit') Queue.offerUnsafe(events, frame)
        if (frame._tag === 'Exit') {
          ended = true
          writable = false
          socket.close()
          break
        }
      }
    }
    yield* Effect.gen(function* () {
      while (!disposed) {
        const event = yield* Queue.take(events)
        if (event._tag === 'Screen' || event._tag === 'Data') options.onOutput(event)
        else if (event._tag === 'Exit') {
          options.onOutput(event)
          yield* transition({ _tag: 'ProcessExited', code: event.code })
          break
        } else {
          yield* transition(event)
          if (event._tag === 'Geometry') {
            writable = !ended
            options.onOutput(event)
            if (resizePending && write({ _tag: 'Resize', ...size })) resizePending = false
          }
          if (event._tag === 'SocketLost') break
        }
      }
    }).pipe(Effect.ensuring(Effect.sync(() => {
      socket.onopen = null
      socket.onmessage = null
      socket.onclose = null
      socket.onerror = null
      writable = false
      socket.close()
      if (liveSocket === socket) liveSocket = undefined
    })))
  })
  yield* Effect.forkIn(Effect.gen(function* () {
    while (!disposed && !terminal(current)) {
      yield* attempt()
      if (current._tag === 'Reconnecting') {
        yield* Effect.sleep(current.delayMs)
        if (!disposed) yield* transition({ _tag: 'RetryDue' })
      }
    }
  }), scope)
  yield* Effect.addFinalizer(() => Effect.gen(function* () {
    disposed = true
    if (current._tag === 'Live') write({ _tag: 'Detach' })
    writable = false
    liveSocket?.close(1000)
    yield* transition({ _tag: 'Closed' })
  }))
  return {
    mode: options.mode,
    state,
    send: (bytes: Uint8Array) => {
      for (let start = 0; start < bytes.length; start += ptyDataChunkBytes) {
        if (!write({ _tag: 'Data', bytes: bytes.subarray(start, start + ptyDataChunkBytes) })) return false
      }
      return true
    },
    resize: (next: PtySize) => {
      if (!attach) return
      size = next
      resizePending = !write({ _tag: 'Resize', ...next })
    },
    resetInputModes: () => write({ _tag: 'ResetInputModes' }),
  } satisfies PtySession
})
