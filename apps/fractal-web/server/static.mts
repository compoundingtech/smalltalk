import { createHash } from 'node:crypto'
import { createReadStream, readFileSync, realpathSync } from 'node:fs'
import { stat } from 'node:fs/promises'
import type { Stats } from 'node:fs'
import type { IncomingMessage, ServerResponse } from 'node:http'
import { extname, join, resolve, sep } from 'node:path'
import { Context, Effect, Layer, Schema } from 'effect'
import { BoundaryError, jsonText, reject, ServerConfigError } from './boundary.mts'
import { spanOptions } from './tracing.mts'

const types: Readonly<Record<string, string>> = { '.html': 'text/html; charset=utf-8', '.js': 'text/javascript; charset=utf-8', '.css': 'text/css; charset=utf-8', '.json': 'application/json', '.svg': 'image/svg+xml', '.png': 'image/png', '.ico': 'image/x-icon', '.woff2': 'font/woff2', '.ttf': 'font/ttf', '.txt': 'text/plain; charset=utf-8' }
const precompressedExtensions = new Set(['.js', '.css', '.svg', '.json', '.txt'])
type Coding = 'br' | 'gzip' | 'identity'
type EncodingQualities = Readonly<Record<Coding, number>>

/** Explicit codings override the wildcard; identity is allowed unless explicitly excluded. */
const encodingQualities = (header: string | undefined): EncodingQualities => {
  const qualities = new Map<string, number>()
  if (header === undefined || header.trim() === '') return { br: 0, gzip: 0, identity: 1 }
  for (const entry of header.split(',')) {
    const [coding, ...parameters] = entry.trim().toLowerCase().split(';')
    const name = (coding ?? '').trim()
    if (name === '') continue
    let quality = 1
    if (parameters.length !== 0) {
      const match = parameters.length === 1 && /^\s*q\s*=\s*(0(?:\.\d{0,3})?|1(?:\.0{0,3})?)\s*$/.exec(parameters[0]!)
      quality = match ? Number(match[1]) : 0
    }
    qualities.set(name, Math.min(qualities.get(name) ?? 1, quality))
  }
  const wildcard = qualities.get('*')
  return {
    br: qualities.get('br') ?? wildcard ?? 0,
    gzip: qualities.get('gzip') ?? wildcard ?? 0,
    identity: qualities.get('identity') ?? (wildcard === 0 ? 0 : 1),
  }
}

/** GET and HEAD use weak comparison, including weak client tags and comma-separated lists. */
const matchesEtag = (header: string | undefined, etag: string): boolean => {
  if (header === undefined) return false
  if (header.trim() === '*') return true
  const opaque = etag.startsWith('W/') ? etag.slice(2) : etag
  for (const match of header.matchAll(/(?:^|,)\s*(?:W\/)?("[\x21\x23-\x7e\x80-\xff]*")\s*(?=,|$)/g)) {
    if (match[1] === opaque) return true
  }
  return false
}
const FileError = Schema.Struct({ code: Schema.optionalKey(Schema.String) })
const notFound = (cause?: unknown) => new BoundaryError({ status: 404, code: 'not-found', message: 'Not found\n', cause })
const fileStat = (file: string) => Effect.tryPromise({ try: () => stat(file), catch: (cause) => notFound(cause) })
const optionalFileStat = (file: string) => fileStat(file).pipe(
  Effect.catchTag('BoundaryError', (error) => Effect.gen(function* () {
    const cause = yield* Schema.decodeUnknownEffect(FileError)(error.cause).pipe(Effect.option)
    if (cause._tag === 'Some' && ['ENOENT', 'ENOTDIR'].includes(cause.value.code ?? '')) return undefined
    return yield* error
  })),
)

/** Negotiate build-time siblings only; ties prefer Brotli, then gzip, then identity. */
const staticRepresentation = Effect.fn('fractal.static.representation')(function* (
  file: string, info: Stats, qualities: EncodingQualities,
) {
  const codings: Coding[] = precompressedExtensions.has(extname(file)) ? ['br', 'gzip', 'identity'] : ['identity']
  codings.sort((left, right) => qualities[right] - qualities[left])
  for (const coding of codings) {
    if (qualities[coding] === 0) continue
    if (coding === 'identity') return { file, info, coding: undefined }
    const sibling = `${file}.${coding === 'gzip' ? 'gz' : 'br'}`
    const compressed = yield* optionalFileStat(sibling)
    if (compressed?.isFile()) return { file: sibling, info: compressed, coding }
  }
  return null
})
const makeStatic = (root: string, identity: RuntimeIdentity) => Effect.gen(function* () {
  const realRoot = yield* Effect.try({ try: () => realpathSync(root), catch: notFound })
  const index = yield* Effect.try({ try: () => readFileSync(join(root, 'index.html'), 'utf8'),
    catch: (cause) => new ServerConfigError({ message: 'Unable to read compiled application', cause }) })
  const script = `<script>globalThis.__BUILD_DEPLOYMENT_ID__=${jsonText(identity.deploymentId).replace(/[<>&\u2028\u2029]/g, (character) => `\\u${character.charCodeAt(0).toString(16).padStart(4, '0')}`)};</script>`
  const html = Buffer.from(index.includes('<head>') ? index.replace('<head>', `<head>${script}`) : script + index)
  const metadata = Buffer.from(jsonText(identity.buildIdentity))
  const htmlEtag = `"${createHash('sha256').update(html).digest('base64url')}"`
  const assetNamespace = createHash('sha256').update(root).digest('base64url')
  return { handle: (req: IncomingMessage, res: ServerResponse) => Effect.gen(function* () {
    const pathname = yield* Effect.try({ try: () => decodeURIComponent((req.url ?? '/').split('?')[0]!), catch: notFound })
    if (req.method !== 'GET' && req.method !== 'HEAD') { reject(res, 405, 'Method not allowed\n'); return }
    if (pathname.includes('\0') || pathname.includes('\\') || pathname.split('/').includes('..')) return yield* notFound()
    const qualities = encodingQualities(req.headers['accept-encoding'])
    if (pathname === '/build-identity.json') {
      res.setHeader('vary', 'Accept-Encoding')
      if (qualities.identity === 0) { reject(res, 406, 'Not acceptable\n'); return }
      res.writeHead(200, { 'content-type': 'application/json', 'content-length': metadata.byteLength,
        'cache-control': 'no-cache', 'x-content-type-options': 'nosniff' })
      res.end(req.method === 'HEAD' ? undefined : metadata)
      return
    }
    let file = resolve(root, `.${pathname}`)
    if (file !== root && !file.startsWith(root + sep)) return yield* notFound()
    let info = yield* optionalFileStat(file)
    if (!info?.isFile()) {
      if (extname(pathname) !== '' && pathname !== '/') return yield* notFound()
      file = join(root, 'index.html')
      info = yield* fileStat(file)
    }
    const isIndex = file === join(root, 'index.html')
    res.setHeader('vary', 'Accept-Encoding')
    const representation = yield* staticRepresentation(file, info!, qualities)
    if (representation === null) { reject(res, 406, 'Not acceptable\n'); return }
    const realFile = yield* Effect.try({ try: () => realpathSync(representation.file), catch: notFound })
    if (!realFile.startsWith(realRoot + sep)) return yield* notFound()
    const selected = representation.info
    const etag = isIndex ? htmlEtag : `W/"${assetNamespace}-${representation.coding ?? 'identity'}-${selected.ino}-${selected.size}-${selected.mtimeMs}-${selected.ctimeMs}"`
    const notModified = matchesEtag(req.headers['if-none-match'], etag)
    res.writeHead(notModified ? 304 : 200, {
      'content-type': types[extname(file)] ?? 'application/octet-stream',
      'content-length': isIndex ? html.byteLength : selected.size,
      'cache-control': isIndex || extname(file) === '.html' ? 'no-cache' : 'public, max-age=31536000, immutable',
      'x-content-type-options': 'nosniff', etag,
      ...(representation.coding === undefined ? {} : { 'content-encoding': representation.coding }),
    })
    if (notModified || req.method === 'HEAD') { res.end(); return }
    if (isIndex) { res.end(html); return }
    yield* Effect.callback<void, BoundaryError>((resume) => {
      const stream = createReadStream(representation.file)
      const cleanup = () => { stream.off('end', end); stream.off('error', error) }
      const end = () => { cleanup(); resume(Effect.void) }
      const error = (cause: Error) => { cleanup(); res.destroy(); resume(Effect.fail(notFound(cause))) }
      stream.once('end', end); stream.once('error', error); stream.pipe(res)
      return Effect.sync(() => { stream.unpipe(res); stream.destroy(); cleanup() })
    })
  }).pipe(Effect.withSpan('fractal.static.serve', spanOptions('assets', req.method ?? '_OTHER')),
    Effect.catchTag('BoundaryError', () => Effect.sync(() => { if (!res.destroyed && !res.headersSent) reject(res, 404, 'Not found\n') }))),
  }
})
export interface StaticAssetService {
  handle(req: IncomingMessage, res: ServerResponse): Effect.Effect<void>
}
export class StaticAssets extends Context.Service<StaticAssets, StaticAssetService>()('fractal-web/StaticAssets') {
  static layer(root: string, identity: RuntimeIdentity) { return Layer.effect(this, makeStatic(root, identity)) }
}

/** Decode the published CliBuildIdentity without formatting or replacing its version fields. */
const BuildIdentity = Schema.Struct({
  baseVersion: Schema.String.check(Schema.isPattern(/\S/)),
  displayVersion: Schema.String.check(Schema.isPattern(/\S/)),
  machineVersion: Schema.String.check(Schema.isPattern(/\S/)),
  sourceKind: Schema.Literals(['local', 'nix']),
  rev: Schema.optionalKey(Schema.String),
  dirty: Schema.Boolean,
  commitTs: Schema.optionalKey(Schema.Int),
  buildTs: Schema.optionalKey(Schema.Int),
}).annotate({ identifier: 'FractalWeb.BuildIdentity' })
export const RuntimeIdentity = Schema.Struct({ buildIdentity: BuildIdentity, deploymentId: Schema.String })
export type RuntimeIdentity = typeof RuntimeIdentity.Type

