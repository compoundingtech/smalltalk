import { lstat, readdir, readFile } from 'node:fs/promises'
import { resolve, join } from 'node:path'
import { pathToFileURL } from 'node:url'
import { parseArgs } from 'node:util'

// Values are never returned or logged. Built-in rules are generic and public; real identities
// belong only in an owner-held policy passed with --policy, never in the repository.
const patterns = [
  new RegExp(String.raw`\b(?:dev[35]|` + 'mb' + String.raw`p[a-z0-9_-]*)\b`, 'i'),
  /\b(?:[a-z0-9-]+\.)*tail[a-z0-9-]*\.ts\.net\b/i,
  new RegExp(String.raw`\/(?:home|Users)\/|\/` + 'run' + String.raw`\/user\b`, 'i'),
  // Token shapes are assembled from parts so this source itself never carries one.
  new RegExp(String.raw`\b(?:sk-[a-z0-9_-]{16,}|gh[pousr]_[a-z0-9]{20,}|` + 'github' + '_pat_' + String.raw`[a-z0-9_]{20,}|AKIA[A-Z0-9]{16}|ASIA[A-Z0-9]{16}|` + 'xox' + String.raw`[baprs]-[a-z0-9-]{12,}|npm_[a-z0-9]{20,})\b`, 'i'),
  /\beyJ[a-z0-9_-]{8,}\.[a-z0-9_-]{8,}\.[a-z0-9_-]{8,}\b/i,
  new RegExp('-----BEGIN (?:[A-Z]+ )?PRIV' + 'ATE KEY-----'),
  /\b(?:authorization\s*[:=]\s*["']?\s*(?:bearer|basic)|bearer\s+[a-z0-9._~+/-]{12,})/i,
  /["']?(?:api[_-]?key|access[_-]?token|auth[_-]?token|password|secret|credential|preview_token)["']?\s*[:=]\s*["'][^"'\s]{8,}["']/i,
  /https?:\/\/[^\s/@:]+:[^\s/@]+@/i,
  /\bcachix\.org\b|\bnpm\.pkg\.github\.com\b|\b(?:registry|npm)\.(?:internal|private)\b/i,
  // Tarball dependencies fetched from arbitrary hosts bypass the locked public registry.
  /https?:\/\/[^\s"']+\.tgz\b/i,
]
const normalize = (line) => {
  const unescaped = line.replace(/\\u([0-9a-f]{4})/gi, (_, hex) => String.fromCharCode(parseInt(hex, 16)))
    .replace(/\\x([0-9a-f]{2})/gi, (_, hex) => String.fromCharCode(parseInt(hex, 16))).replace(/\\\//g, '/')
  try { return decodeURIComponent(unescaped) } catch { return unescaped }
}
const escapeRe = (text) => text.replace(/[.*+?^${}()|[\]\\]/g, '\\$&')
/** Substring literals deny full refs and multiword identities; `{ "token": … }` denies a bare
 *  delimited identity token without rejecting longer public identifiers such as app paths.
 *  `{ "allow": … }` names an exact, case-sensitive public string (e.g. a protocol identifier)
 *  that identity rules ignore; generic rules still see the whole line. */
const compileRules = (policy) => {
  if (policy === undefined) return { rules: [], allows: [] }
  if (!Array.isArray(policy) || policy.length === 0)
    throw new Error('A supplied identity policy must be a nonempty array')
  const isAllow = (entry) => entry !== null && typeof entry === 'object' && 'allow' in entry
  const allows = policy.filter(isAllow).map((entry) =>
    typeof entry.allow === 'string' && entry.allow.trim().length >= 3 && Object.keys(entry).length === 1 ? entry.allow : null)
  const rules = policy.filter((entry) => !isAllow(entry)).map((entry) => {
    if (typeof entry === 'string')
      return entry.trim().length >= 3 ? (text) => text.toLowerCase().includes(entry.toLowerCase()) : null
    if (entry !== null && typeof entry === 'object' && typeof entry.token === 'string' && entry.token.trim().length >= 3) {
      const token = new RegExp(`(?<![A-Za-z0-9_-])${escapeRe(entry.token)}(?![A-Za-z0-9_-])`, 'i')
      return (text) => token.test(text)
    }
    return null
  })
  if (rules.some((rule) => rule === null) || allows.some((allow) => allow === null))
    throw new Error('Invalid private identity denylist entry')
  return { rules, allows }
}
// A space keeps the text on either side of an allowed string from joining into a new match.
const identityText = (text, allows) => allows.reduce((rest, allow) => rest.split(allow).join(' '), text)
const forbiddenText = (text, { rules, allows }) => {
  if (patterns.some((pattern) => pattern.test(text))) return true
  const identity = identityText(text, allows)
  return rules.some((rule) => rule(identity))
}
export const forbiddenLine = (line, policy) => forbiddenText(normalize(line), compileRules(policy))
export const scanFiles = async (roots, policy) => {
  const compiled = compileRules(policy)
  const findings = []
  const visit = async (path) => {
    const stat = await lstat(path)
    if (stat.isDirectory()) {
      // Installed dependencies and repository state are not published app content.
      const name = path.split('/').at(-1)
      if (name === 'node_modules' || name === '.git') return
      for (const entry of (await readdir(path)).sort()) await visit(join(path, entry))
      return
    }
    if (!stat.isFile()) { findings.push({ file: path, line: 1 }); return }
    const bytes = await readFile(path)
    let text
    try { text = new TextDecoder('utf-8', { fatal: true }).decode(bytes) }
    catch { findings.push({ file: path, line: 1 }); return }
    // Raster captures/build blobs cannot be certified by a text scan; reject rather than skip.
    if (text.includes('\0')) { findings.push({ file: path, line: 1 }); return }
    text.split(/\r?\n/).forEach((line, i) => {
      if (forbiddenText(normalize(line), compiled)) findings.push({ file: path, line: i + 1 })
    })
  }
  for (const root of roots) await visit(resolve(root))
  return findings
}
if (process.argv[1] && import.meta.url === pathToFileURL(resolve(process.argv[1])).href) {
  try {
    const { values, positionals } = parseArgs({ allowPositionals: true, options: { policy: { type: 'string' } } })
    if (positionals.length === 0) throw new Error('Missing scan inputs')
    const policy = values.policy === undefined ? undefined : JSON.parse(await readFile(values.policy, 'utf8'))
    const findings = await scanFiles(positionals, policy)
    for (const finding of findings) console.error(`${JSON.stringify(finding.file)}:${finding.line}`)
    console.log(`Scanned ${positionals.length} roots; ${findings.length} forbidden file-lines`)
    process.exitCode = findings.length ? 1 : 0
  } catch {
    // Do not print parser, filesystem or decoder errors: they can include input candidates.
    console.error('Scan failed: supply readable roots and optionally --policy IDENTITY_JSON_ARRAY')
    process.exitCode = 2
  }
}
