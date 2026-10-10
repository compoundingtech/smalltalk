/**
 * Privacy gate (spec "Gates" 4): the token, credential, host and home-path rules of the earlier
 * synthetic-fixtures scan; every subject reference inside the identity namespace; no absolute date
 * or clock time in vocabulary banks. `scripts/check-public-repo` runs as well.
 */
import { spawnSync } from 'node:child_process'
import { readdirSync, readFileSync, statSync } from 'node:fs'
import { join, relative } from 'node:path'
import { fileURLToPath } from 'node:url'

const REPO_ROOT = fileURLToPath(new URL('../../../../', import.meta.url))
const PACKAGE_ROOT = fileURLToPath(new URL('../', import.meta.url))

// Literals are assembled from parts so this source never carries a denied value itself.
const contentRules: readonly { readonly name: string; readonly pattern: RegExp }[] = [
  { name: 'internal host name', pattern: new RegExp(String.raw`\b(?:dev[35]|` + 'mb' + String.raw`p[a-z0-9_-]*)\b`, 'i') },
  { name: 'tailnet name', pattern: /\b(?:[a-z0-9-]+\.)*tail[a-z0-9-]*\.ts\.net\b/i },
  // Case-sensitive: `src/users/` is an ordinary source path, `/Users/<name>` is a macOS home.
  { name: 'home path', pattern: new RegExp(String.raw`(?:^|[\s"'(=:])\/(?:home|Users|root)\/|\/` + 'run' + String.raw`\/user\b`) },
  {
    name: 'token shape',
    pattern: new RegExp(
      String.raw`\b(?:sk-[a-z0-9_-]{16,}|gh[pousr]_[a-z0-9]{20,}|` +
        'github' +
        '_pat_' +
        String.raw`[a-z0-9_]{20,}|AKIA[A-Z0-9]{16}|ASIA[A-Z0-9]{16}|` +
        'xox' +
        String.raw`[baprs]-[a-z0-9-]{12,}|npm_[a-z0-9]{20,})\b`,
      'i',
    ),
  },
  { name: 'JWT shape', pattern: /\beyJ[a-z0-9_-]{8,}\.[a-z0-9_-]{8,}\.[a-z0-9_-]{8,}\b/i },
  { name: 'private key', pattern: new RegExp('-----BEGIN (?:[A-Z]+ )?PRIV' + 'ATE KEY-----') },
  { name: 'authorization header', pattern: /\b(?:authorization\s*[:=]\s*["']?\s*(?:bearer|basic)|bearer\s+[a-z0-9._~+/-]{12,})/i },
  {
    name: 'credential assignment',
    pattern: /["']?(?:api[_-]?key|access[_-]?token|auth[_-]?token|password|secret|credential|preview_token)["']?\s*[:=]\s*["'][^"'\s]{8,}["']/i,
  },
  { name: 'URL credentials', pattern: /https?:\/\/[^\s/@:]+:[^\s/@]+@/i },
  { name: 'private registry', pattern: /\bcachix\.org\b|\bnpm\.pkg\.github\.com\b|\b(?:registry|npm)\.(?:internal|private)\b/i },
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
  text.split('\n').flatMap((line, index) => contentRules.filter((rule) => rule.pattern.test(line)).map((rule) => ({ path, line: index + 1, rule: rule.name })))

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
      if (name === 'node_modules') continue
      if (statSync(path).isDirectory()) visit(path)
      else out.push(path)
    }
  }
  if (statSync(root).isDirectory()) visit(root)
  else out.push(root)
  return out.sort()
}

/** The gate: fixtures get every rule; kit sources get the content rules; banks get the time rule. */
export const scanTree = (fixturesDir: string, vocabularyDir: string, sourceDirs: readonly string[]): Finding[] => {
  const read = (path: string) => readFileSync(path, 'utf8')
  const rel = (path: string) => relative(REPO_ROOT, path)
  return [
    ...filesUnder(fixturesDir).flatMap((path) => [...scanContent(rel(path), read(path)), ...scanIdentities(rel(path), read(path))]),
    ...sourceDirs.flatMap((dir) => filesUnder(dir).flatMap((path) => scanContent(rel(path), read(path)))),
    ...filesUnder(vocabularyDir).flatMap((path) => scanVocabulary(rel(path), read(path))),
  ]
}

/** `scripts/check-public-repo` findings for this package and the committed scenarios. */
export const checkPublicRepo = (): string[] => {
  const result = spawnSync('python3', ['scripts/check-public-repo'], { cwd: REPO_ROOT, encoding: 'utf8' })
  return `${result.stdout}\n${result.stderr}`
    .split('\n')
    .filter((line) => line.includes('fixtures/scenarios/') || line.includes('clients/typescript/st3-scenarios/'))
}

if (import.meta.main) {
  const findings = scanTree(join(REPO_ROOT, 'fixtures/scenarios'), join(PACKAGE_ROOT, 'src/kit/vocabulary'), [
    join(PACKAGE_ROOT, 'src'),
    join(PACKAGE_ROOT, 'scripts'),
    join(PACKAGE_ROOT, 'test'),
    join(PACKAGE_ROOT, 'README.md'),
    join(PACKAGE_ROOT, 'spec.md'),
  ])
  for (const finding of findings) console.error(`${finding.path}:${finding.line}: ${finding.rule}`)
  const publicRepo = checkPublicRepo()
  for (const line of publicRepo) console.error(`check-public-repo: ${line}`)
  const total = findings.length + publicRepo.length
  console.log(`scan: ${total === 0 ? 'ok' : `${total} finding(s)`}`)
  process.exitCode = total === 0 ? 0 : 1
}
