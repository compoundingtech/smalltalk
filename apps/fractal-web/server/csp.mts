import { createHash } from 'node:crypto'

// Fixed style.textContent bytes in react-aria@3.52.1:
// usePress (pressable touch-action) and usePreventScroll (iOS modal overscroll).
// Recheck these hashes when changing React Aria; the browser proof covers usePress.
const reactAriaStyleSources = [
  "'sha256-38RhXrc7EdReTKsOm23ZPOCUgniTUUcjky8QOOrQx6o='",
  "'sha256-gYiS/BvZvRcK27JIXTuwhZ3hs2+VJ1X+2gUlE+farlg='",
]

/** Host-controlled configuration accepts exact HTTP origins, not CSP source expressions. */
const connectionOrigin = (value: string): string => {
  const url = new URL(value)
  if (!['http:', 'https:'].includes(url.protocol) || url.origin !== value || /[;'"*]/.test(url.hostname)) {
    throw new TypeError('CSP connections require exact HTTP(S) origins without credentials, paths or wildcards')
  }
  return url.origin
}
const socketSources = (authority?: string): readonly string[] => {
  if (authority === undefined) return []
  const url = new URL(`http://${authority}`)
  if (/[/?#@;'"\s\\]/.test(authority) || url.username || url.password || url.pathname !== '/' || url.search || url.hash || url.hostname.includes('*')) {
    throw new TypeError('CSP WebSocket sources require a valid request authority')
  }
  // Both schemes on this exact authority support HTTP development and TLS termination.
  // Do not trust forwarded headers or admit arbitrary ws:/wss: origins.
  return [`ws://${url.host}`, `wss://${url.host}`]
}

/** Hash trusted application HTML once, never agent text or request-supplied markup. */
export const createContentSecurityPolicy = (
  html = '', connectOrigins: readonly string[] = [], { development = false }: { readonly development?: boolean } = {},
): ((authority?: string) => string) => {
  const hashes = new Set<string>()
  const styleHashes = new Set<string>()
  // HTML parsing normalizes CR/CRLF before the browser checks a script or style hash.
  const hash = (source: string) => `'sha256-${createHash('sha256').update(source.replace(/\r\n?/g, '\n')).digest('base64')}'`
  for (const match of html.matchAll(/<script\b([^>]*)>([\s\S]*?)<\/script\s*>/gi)) {
    if (/\bsrc\s*=/i.test(match[1]!) || match[2] === '') continue
    hashes.add(hash(match[2]!))
  }
  // The themed first-paint shell is an inline stylesheet; it must apply before any bundle CSS loads.
  for (const match of html.matchAll(/<style\b[^>]*>([\s\S]*?)<\/style\s*>/gi)) {
    if (match[1] !== '') styleHashes.add(hash(match[1]!))
  }
  const origins = [...new Set(connectOrigins.map(connectionOrigin))]
  const beforeConnections = [
    "default-src 'self'",
    // The terminal key encoder compiles WebAssembly; 'wasm-unsafe-eval' allows that without allowing eval().
    ["script-src 'self' 'wasm-unsafe-eval'", ...hashes].join(' '),
    // Runtime StyleX variables and React Aria geometry need style attributes.
    // Fixed React Aria stylesheet elements and the document's own inline styles use hashes; only Vite dev is unbounded.
    ["style-src 'self'", ...reactAriaStyleSources, ...styleHashes].join(' '),
    "style-src-attr 'unsafe-inline'",
    ...(development ? ["style-src-elem 'self' 'unsafe-inline'"] : []),
    "img-src 'self' data: blob:",
    "font-src 'self'",
  ]
  const afterConnections = ["object-src 'none'", "base-uri 'self'", "frame-ancestors 'none'", "form-action 'self'"]
  return (authority) => [...beforeConnections,
    ["connect-src 'self'", ...socketSources(authority), ...origins].join(' '),
    ...(development ? ["worker-src 'self' blob:"] : []),
    ...afterConnections,
  ].join('; ')
}
