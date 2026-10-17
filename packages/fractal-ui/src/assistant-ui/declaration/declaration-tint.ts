/** Read-only tinting for declaration source. Display only: it never validates or rewrites the text. */
export type DeclarationLanguage = 'kdl' | 'json'
export type DeclarationTokenKind = 'node' | 'property' | 'string' | 'number' | 'keyword' | 'comment' | 'punctuation' | 'redacted' | 'text'
export interface DeclarationToken { readonly kind: DeclarationTokenKind; readonly text: string }
export type DeclarationLine = readonly DeclarationToken[]

/** The server's placeholder for an environment value it withheld. */
const REDACTED_LITERAL = '"<redacted>"'
const KDL_KEYWORDS: Record<string, true> = { true: true, false: true, null: true }
const KDL_DELIMITERS: Record<string, true> = { ' ': true, '\t': true, '\r': true, '\n': true, '{': true, '}': true, '(': true, ')': true, ';': true, '=': true, '"': true, '\\': true }
const NUMBER = /^[+-]?(?:\d|\.\d)/

class Lines {
  readonly lines: DeclarationToken[][] = [[]]
  push(kind: DeclarationTokenKind, text: string) {
    const parts = text.split('\n')
    parts.forEach((part, index) => {
      if (index > 0) this.lines.push([])
      if (part !== '') this.lines[this.lines.length - 1]!.push({ kind, text: part })
    })
  }
}

/** End index of a quoted string starting at `start`, honouring backslash escapes. */
const quotedEnd = (source: string, start: number): number => {
  let index = start + 1
  while (index < source.length) {
    const char = source[index]
    if (char === '\\') index += 2
    else if (char === '"') return index + 1
    else index += 1
  }
  return source.length
}

const nextNonSpace = (source: string, from: number): string | undefined => {
  let index = from
  while (index < source.length && (source[index] === ' ' || source[index] === '\t')) index += 1
  return source[index]
}

const tokenizeKdl = (source: string): DeclarationLine[] => {
  const out = new Lines()
  let index = 0
  // A node name opens each statement: at the start, after a newline, `{`, `}` or `;`.
  let statementStart = true
  const value = (text: string, end: number) => {
    const kind: DeclarationTokenKind = text === REDACTED_LITERAL ? 'redacted'
      : source[end] === '=' ? 'property'
      : statementStart ? 'node'
      : 'string'
    out.push(kind, text)
    statementStart = false
    index = end
  }
  while (index < source.length) {
    const char = source[index]!
    const rest = source.slice(index, index + 3)
    if (char === '\n') { out.push('text', char); index += 1; statementStart = true; continue }
    if (char === ' ' || char === '\t' || char === '\r' || char === '\\') {
      let end = index + 1
      while (end < source.length && (source[end] === ' ' || source[end] === '\t' || source[end] === '\r')) end += 1
      out.push('text', source.slice(index, end)); index = end; continue
    }
    if (rest.startsWith('//')) {
      const newline = source.indexOf('\n', index)
      const end = newline === -1 ? source.length : newline
      out.push('comment', source.slice(index, end)); index = end; continue
    }
    if (rest.startsWith('/*')) {
      let depth = 0
      let end = index
      while (end < source.length) {
        if (source.startsWith('/*', end)) { depth += 1; end += 2 }
        else if (source.startsWith('*/', end)) { depth -= 1; end += 2; if (depth === 0) break }
        else end += 1
      }
      out.push('comment', source.slice(index, end)); index = end; continue
    }
    if (rest.startsWith('/-')) { out.push('comment', '/-'); index += 2; continue }
    if (char === '{' || char === '}' || char === ';') { out.push('punctuation', char); index += 1; statementStart = true; continue }
    if (char === '=') { out.push('punctuation', char); index += 1; continue }
    if (char === '(') {
      const close = source.indexOf(')', index)
      const end = close === -1 ? source.length : close + 1
      out.push('punctuation', source.slice(index, end)); index = end; continue
    }
    if (rest === '"""') {
      const close = source.indexOf('"""', index + 3)
      const end = close === -1 ? source.length : close + 3
      value(source.slice(index, end), end); continue
    }
    if (char === '"') { const end = quotedEnd(source, index); value(source.slice(index, end), end); continue }
    const raw = /^(?:r?)(#+)"/.exec(source.slice(index, index + 64))
    if (raw !== null && (char === '#' || char === 'r')) {
      const terminator = `"${raw[1]}`
      const close = source.indexOf(terminator, index + raw[0].length)
      const end = close === -1 ? source.length : close + terminator.length
      value(source.slice(index, end), end); continue
    }
    let end = index + 1
    while (end < source.length && !Object.hasOwn(KDL_DELIMITERS, source[end]!) && !(source[end] === '/' && (source[end + 1] === '/' || source[end + 1] === '*'))) end += 1
    const word = source.slice(index, end)
    if (source[end] === '=') out.push('property', word)
    else if (statementStart) out.push('node', word)
    else if (word.startsWith('#') || Object.hasOwn(KDL_KEYWORDS, word)) out.push('keyword', word)
    else if (NUMBER.test(word)) out.push('number', word)
    else out.push('string', word)
    statementStart = false
    index = end
  }
  return out.lines
}

const JSON_NUMBER = /^-?\d+(?:\.\d+)?(?:[eE][+-]?\d+)?/
const JSON_KEYWORD = /^(?:true|false|null)\b/

const tokenizeJson = (source: string): DeclarationLine[] => {
  const out = new Lines()
  let index = 0
  while (index < source.length) {
    const char = source[index]!
    if (char === '"') {
      const end = quotedEnd(source, index)
      const text = source.slice(index, end)
      out.push(text === REDACTED_LITERAL ? 'redacted' : nextNonSpace(source, end) === ':' ? 'property' : 'string', text)
      index = end; continue
    }
    const number = JSON_NUMBER.exec(source.slice(index, index + 64))
    if (number !== null) { out.push('number', number[0]); index += number[0].length; continue }
    const keyword = JSON_KEYWORD.exec(source.slice(index, index + 6))
    if (keyword !== null) { out.push('keyword', keyword[0]); index += keyword[0].length; continue }
    if ('{}[],:'.includes(char)) { out.push('punctuation', char); index += 1; continue }
    // Only whitespace runs together; any other stray character stays a one-character text token.
    let end = index + 1
    if (/\s/.test(char)) while (end < source.length && /\s/.test(source[end]!)) end += 1
    out.push('text', source.slice(index, end)); index = end
  }
  return out.lines
}

/** Split declaration source into display lines of tinted tokens; concatenated text equals the input. */
export const tokenizeDeclaration = (source: string, language: DeclarationLanguage): readonly DeclarationLine[] =>
  language === 'kdl' ? tokenizeKdl(source) : tokenizeJson(source)
