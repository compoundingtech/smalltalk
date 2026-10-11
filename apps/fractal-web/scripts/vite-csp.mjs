import { readFileSync } from 'node:fs'
import { extname, resolve } from 'node:path'
import { createContentSecurityPolicy } from '../server/csp.mts'

/** Enforce the same policy even without the optional paired gateway middleware. */
export const fractalContentSecurityPolicy = () => {
  let connectOrigins = []
  return {
    name: 'fractal-content-security-policy',
    configResolved(config) {
      const collector = config.env.VITE_OTLP_TRACES_URL?.trim()
      connectOrigins = collector && !collector.startsWith('/') ? [new URL(collector).origin] : []
      // Validate explicit origins at startup, not on the first request.
      createContentSecurityPolicy('', connectOrigins)
      const hmr = config.server.hmr
      if (typeof hmr === 'object' && (hmr.host || hmr.port || hmr.clientPort || hmr.protocol)) {
        throw new TypeError('CSP requires Vite HMR to use the page authority; cross-origin HMR is not supported')
      }
    },
    configureServer(server) {
      const basePolicy = createContentSecurityPolicy('', connectOrigins, { development: true })
      // Vite emits fully transformed HTML through res.end. Hash those exact bytes,
      // including the React refresh preamble, rather than granting unsafe-inline scripts.
      // Keep a bounded cache for HTML HEAD/304 responses; an uncached validator is
      // ignored so a full response establishes both the HTML and its matching policy.
      const pages = new Map()
      server.middlewares.use((req, res, next) => {
        const pathname = (req.originalUrl ?? req.url ?? '/').split('?')[0]
        const htmlRequest = extname(pathname) === '' || extname(pathname) === '.html'
        if (htmlRequest && !pages.has(pathname)) delete req.headers['if-none-match']
        res.setHeader('content-security-policy', (pages.get(pathname) ?? basePolicy)(req.headers.host))
        const end = res.end
        res.end = function (content, ...args) {
          if (!res.headersSent) {
            let policy = pages.get(pathname) ?? basePolicy
            if (String(res.getHeader('content-type')).startsWith('text/html') &&
                (typeof content === 'string' || Buffer.isBuffer(content))) {
              policy = createContentSecurityPolicy(content.toString(), connectOrigins, { development: true })
              pages.delete(pathname)
              pages.set(pathname, policy)
              if (pages.size > 64) pages.delete(pages.keys().next().value)
            }
            // The paired gateway middleware may have replaced the initial header.
            // Restore the exact HTML policy on HEAD and 304 responses as well.
            res.setHeader('content-security-policy', policy(req.headers.host))
          }
          return end.call(this, content, ...args)
        }
        next()
      })
    },
    configurePreviewServer(server) {
      // The app build has one HTML entry; preview streams this trusted compiled file.
      const html = readFileSync(resolve(server.config.root, server.config.build.outDir, 'index.html'), 'utf8')
      const policy = createContentSecurityPolicy(html, connectOrigins)
      server.middlewares.use((req, res, next) => {
        res.setHeader('content-security-policy', policy(req.headers.host))
        next()
      })
    },
  }
}
