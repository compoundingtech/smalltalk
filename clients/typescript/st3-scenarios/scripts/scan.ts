/**
 * Privacy gate (spec "Gates" 4): the token, credential, host, person, address and home-path rules
 * over every published file of the package and the committed scenarios; every subject reference
 * inside the identity namespace; no absolute date or clock time in vocabulary banks.
 * `scripts/check-public-repo` runs as well, and any failure to run it fails the gate.
 */
import { spawnSync } from 'node:child_process'
import { readdirSync, readFileSync, statSync } from 'node:fs'
import { join, relative } from 'node:path'
import { fileURLToPath } from 'node:url'

const REPO_ROOT = fileURLToPath(new URL('../../../../', import.meta.url))
const PACKAGE_ROOT = fileURLToPath(new URL('../', import.meta.url))

/** Hosts reserved for documentation (RFC 2606, RFC 6761) and the replay's own origin. */
const allowedHost = (host: string): boolean => {
  const name = host.toLowerCase().replace(/\.$/u, '')
  return name === 'localhost' || name === 'scenario.invalid' || /(?:^|\.)example\.(?:com|org|net)$/u.test(name) || allowedIpv4(name)
}

/** Email domains reserved for documentation. */
const allowedEmailDomain = (domain: string): boolean => {
  const name = domain.toLowerCase()
  return /(?:^|\.)example\.(?:com|org|net)$/u.test(name) || /\.(?:invalid|example|test|localhost)$/u.test(name)
}

const ipv4 = /(?<![\d.])(\d{1,3})\.(\d{1,3})\.(\d{1,3})\.(\d{1,3})(?!\d|\.\d)/gu

/** Documentation ranges (RFC 5737) and loopback. */
const allowedIpv4 = (address: string): boolean => {
  const octets = address.split('.').map(Number)
  if (octets.length !== 4 || octets.some((octet) => !Number.isInteger(octet) || octet > 255)) return false
  const [a, b, c] = octets as [number, number, number, number]
  return a === 127 || (a === 192 && b === 0 && c === 2) || (a === 198 && b === 51 && c === 100) || (a === 203 && b === 0 && c === 113)
}

interface ContentRule {
  readonly name: string
  readonly test: (line: string) => boolean
}

const pattern = (name: string, regex: RegExp): ContentRule => ({ name, test: (line) => regex.test(line) })

// Literals are assembled from parts so this source never carries a denied value itself.
const contentRules: readonly ContentRule[] = [
  pattern('internal host name', new RegExp(String.raw`\b(?:` + 'de' + String.raw`v[345]|` + 'mb' + String.raw`p[a-z0-9_-]*)\b`, 'i')),
  pattern('real user name', new RegExp('schick' + 'ling', 'i')),
  pattern('tailnet name', /\b(?:[a-z0-9-]+\.)*tail[a-z0-9-]*\.ts\.net\b/i),
  // Case-sensitive: `src/users/` is an ordinary source path, `/Users/<name>` is a macOS home.
  pattern('home path', new RegExp(String.raw`(?:^|[\s"'(=:])\/(?:home|Users|root)\/|\/` + 'run' + String.raw`\/user\b`)),
  pattern(
    'token shape',
    new RegExp(
      String.raw`\b(?:sk-[a-z0-9_-]{16,}|gh[pousr]_[a-z0-9]{20,}|` +
        'github' +
        '_pat_' +
        String.raw`[a-z0-9_]{20,}|AKIA[A-Z0-9]{16}|ASIA[A-Z0-9]{16}|` +
        'xox' +
        String.raw`[baprs]-[a-z0-9-]{12,}|npm_[a-z0-9]{20,})\b`,
      'i',
    ),
  ),
  pattern('JWT shape', /\beyJ[a-z0-9_-]{8,}\.[a-z0-9_-]{8,}\.[a-z0-9_-]{8,}\b/i),
  pattern('private key', new RegExp('-----BEGIN (?:[A-Z]+ )?PRIV' + 'ATE KEY-----')),
  pattern('authorization header', /\b(?:authorization\s*[:=]\s*["']?\s*(?:bearer|basic)|bearer\s+[a-z0-9._~+/-]{12,})/i),
  pattern('credential assignment', /["']?(?:api[_-]?key|access[_-]?token|auth[_-]?token|password|secret|credential|preview_token)["']?\s*[:=]\s*["'][^"'\s]{8,}["']/i),
  pattern('URL credentials', /https?:\/\/[^\s/@:]+:[^\s/@]+@/i),
  pattern('private registry', /\bcachix\.org\b|\bnpm\.pkg\.github\.com\b|\b(?:registry|npm)\.(?:internal|private)\b/i),
  {
    name: 'email address outside reserved domains',
    test: (line) => [...line.matchAll(/[A-Za-z0-9._%+-]+@([A-Za-z0-9-]+(?:\.[A-Za-z0-9-]+)*\.[A-Za-z]{2,})\b/gu)].some((match) => !allowedEmailDomain(match[1]!)),
  },
  { name: 'IPv4 address outside documentation ranges', test: (line) => [...line.matchAll(ipv4)].some((match) => !allowedIpv4(match[0])) },
  {
    name: 'internal URL',
    test: (line) => [...line.matchAll(/\b[a-z][a-z0-9+.-]*:\/\/(?:[^\s/@'"`]*@)?(\[[^\]\s]*\]|[^\s/:?#'"`)\]>]+)/giu)].some((match) => !allowedHost(match[1]!)),
  },
]

/** Invented people from `scripts/check-public-repo` that scenarios use. */
const PEOPLE = new Set(['ada', 'robin', 'avery', 'blair', 'alex', 'pat', 'operator'])

/** Families whose references must follow the identity namespace (spec "Identity namespace"). */
const FAMILIES =
  /(?<![A-Za-z0-9_./-])(agent|person|host|machine|mission|mission-run|step-run|run-generation|work|runtime|session|terminal|timeline-entry|attention|message|fleet|event-cursor)\/[^\s"'`),\]}]+/g

const identityOk = (family: string, rest: string): boolean => {
  switch (family) {
    case 'agent':
      return /^example\/[a-z0-9-]+\/[a-z0-9-]+$/.test(rest)
    case 'person':
      return PEOPLE.has(rest.split('/')[0]!)
    case 'host':
    case 'machine':
      return /^[a-z]+$/.test(rest)
    case 'mission':
    case 'mission-run':
    case 'step-run':
    case 'run-generation':
    case 'work':
      return /^example\/[a-z0-9-]+\//.test(rest)
    default:
      return /^(?:example|scenario)-[^\s/]+(?:\/[^\s]*)?$/.test(rest)
  }
}

/** Date and clock patterns that must not appear in authored vocabulary. */
const absoluteTime = /\b\d{4}-\d{2}-\d{2}\b|\b(?:[01]?\d|2[0-3]):[0-5]\d(?::[0-5]\d)?\b|\b(?:19|20)\d{2}\b/

export interface Finding {
  readonly path: string
  readonly line: number
  readonly rule: string
}

export const scanContent = (path: string, text: string): Finding[] =>
  text.split('\n').flatMap((line, index) => contentRules.filter((rule) => rule.test(line)).map((rule) => ({ path, line: index + 1, rule: rule.name })))

export const scanIdentities = (path: string, text: string): Finding[] =>
  text.split('\n').flatMap((line, index) =>
    [...line.matchAll(FAMILIES)]
      .filter((match) => !identityOk(match[1]!, match[0].slice(match[1]!.length + 1)))
      .map((match) => ({ path, line: index + 1, rule: `identity outside the namespace: ${match[0]}` })),
  )

export const scanVocabulary = (path: string, text: string): Finding[] =>
  text.split('\n').flatMap((line, index) => {
    const code = line.replace(/\/\/.*$/, '').replace(/^\s*\*.*$/, '')
    return absoluteTime.test(code) ? [{ path, line: index + 1, rule: 'absolute date or clock time in vocabulary' }] : []
  })

const filesUnder = (root: string): string[] => {
  const out: string[] = []
  const visit = (dir: string) => {
    for (const name of readdirSync(dir)) {
      const path = join(dir, name)
      if (name === 'node_modules' || name === '.git') continue
      if (statSync(path).isDirectory()) visit(path)
      else out.push(path)
    }
  }
  if (statSync(root).isDirectory()) visit(root)
  else out.push(root)
  return out.sort()
}

/** Every file under `root` that git would publish: walks the tree, skips `node_modules` and gitignored output. */
export const publishedFiles = (root: string): string[] => {
  const files = filesUnder(root)
  if (files.length === 0) return files
  const result = spawnSync('git', ['check-ignore', '-z', '--stdin'], { cwd: root, encoding: 'utf8', input: files.map((path) => `${relative(root, path)}\0`).join('') })
  // check-ignore exits 0 when some path is ignored and 1 when none is; anything else is a failure.
  if (result.error !== undefined || result.signal !== null || (result.status !== 0 && result.status !== 1)) {
    throw new Error(`scan: cannot list published files under ${root}: git check-ignore ${result.error?.message ?? result.signal ?? `exited ${result.status}`} ${result.stderr ?? ''}`.trim())
  }
  const ignored = new Set(result.stdout.split('\0').filter((path) => path !== ''))
  return files.filter((path) => !ignored.has(relative(root, path)))
}

export interface ScanRoots {
  /** Committed scenario fixtures: content and identity rules. */
  readonly fixturesDir: string
  /** The package: content rules over every published file. */
  readonly packageRoot: string
  /** Vocabulary banks: the absolute-time rule. */
  readonly vocabularyDir: string
}

/** The gate: fixtures get every rule; every published package file gets the content rules; banks get the time rule. */
export const scanTree = ({ fixturesDir, packageRoot, vocabularyDir }: ScanRoots): Finding[] => {
  const read = (path: string) => readFileSync(path, 'utf8')
  const rel = (path: string) => relative(REPO_ROOT, path)
  return [
    ...filesUnder(fixturesDir).flatMap((path) => [...scanContent(rel(path), read(path)), ...scanIdentities(rel(path), read(path))]),
    ...publishedFiles(packageRoot).flatMap((path) => scanContent(rel(path), read(path))),
    ...filesUnder(vocabularyDir).flatMap((path) => scanVocabulary(rel(path), read(path))),
  ]
}

const SCOPES = ['fixtures/scenarios/', 'clients/typescript/st3-scenarios/']

/**
 * `scripts/check-public-repo` findings for this package and the committed scenarios. Fails closed:
 * a spawn error, a signal, or a non-zero exit without a scoped finding is itself a finding.
 */
export const checkPublicRepo = (command: readonly [string, ...string[]] = ['python3', 'scripts/check-public-repo']): string[] => {
  const [file, ...args] = command
  const result = spawnSync(file, args, { cwd: REPO_ROOT, encoding: 'utf8' })
  if (result.error !== undefined) return [`could not run ${command.join(' ')}: ${result.error.message}`]
  if (result.signal !== null) return [`${command.join(' ')} was killed by ${result.signal}`]
  const scoped = `${result.stdout}\n${result.stderr}`.split('\n').filter((line) => SCOPES.some((scope) => line.includes(scope)))
  if (result.status !== 0 && scoped.length === 0) return [`${command.join(' ')} exited ${result.status} without a finding in ${SCOPES.join(' or ')}`]
  return scoped
}

if (import.meta.main) {
  const findings = scanTree({ fixturesDir: join(REPO_ROOT, 'fixtures/scenarios'), packageRoot: PACKAGE_ROOT, vocabularyDir: join(PACKAGE_ROOT, 'src/kit/vocabulary') })
  for (const finding of findings) console.error(`${finding.path}:${finding.line}: ${finding.rule}`)
  const publicRepo = checkPublicRepo()
  for (const line of publicRepo) console.error(`check-public-repo: ${line}`)
  const total = findings.length + publicRepo.length
  console.log(`scan: ${total === 0 ? 'ok' : `${total} finding(s)`}`)
  process.exitCode = total === 0 ? 0 : 1
}
