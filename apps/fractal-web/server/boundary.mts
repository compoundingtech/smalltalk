import type { ServerResponse } from 'node:http'
import { Effect, Schema } from 'effect'
export class BoundaryError extends Schema.TaggedError<BoundaryError>()('BoundaryError', {
  status: Schema.Int,
  code: Schema.String,
  message: Schema.String,
  cause: Schema.optionalKey(Schema.Defect()),
}) {}
export class ServerConfigError extends Schema.TaggedError<ServerConfigError>()('ServerConfigError', {
  message: Schema.String,
  cause: Schema.optionalKey(Schema.Defect()),
}) {}
export const jsonText = Schema.encodeSync(Schema.fromJsonString(Schema.Unknown))
export const reject = (res: ServerResponse, status: number, text: string): void => {
  res.writeHead(status, { 'content-type': 'text/plain; charset=utf-8', 'cache-control': 'no-store' })
  res.end(text)
}
export const completedResponse = (res: ServerResponse) => Effect.callback<void>((resume) => {
  const cleanup = () => { res.off('finish', finished); res.off('close', closed); res.off('error', closed) }
  const finished = () => { cleanup(); resume(Effect.void) }
  const closed = () => { cleanup(); resume(res.writableFinished ? Effect.void : Effect.interrupt) }
  if (res.writableFinished) { finished(); return }
  if (res.destroyed) { closed(); return }
  res.once('finish', finished); res.once('close', closed); res.once('error', closed)
  return Effect.sync(cleanup)
})
