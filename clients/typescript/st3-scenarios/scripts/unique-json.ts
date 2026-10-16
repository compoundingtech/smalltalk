/** Reject duplicate decoded member names before JSON.parse can discard earlier values. */
export const assertUniqueJsonMembers = (source: string): void => {
  let offset = 0
  const invalid = (): never => { throw new Error('invalid JSON') }
  const whitespace = (): void => { while (/[\t\n\r ]/u.test(source[offset] ?? '') && offset < source.length) offset++ }
  const string = (): string => {
    const start = offset
    if (source[offset++] !== '"') return invalid()
    while (offset < source.length) {
      const character = source[offset++]
      if (character === '"') return source.slice(start, offset)
      if (character === '\\') offset++
    }
    return invalid()
  }
  const value = (): void => {
    whitespace()
    const first = source[offset]
    if (first === '"') { string(); return }
    if (first === '{') {
      offset++
      whitespace()
      if (source[offset] === '}') { offset++; return }
      const members = new Set<string>()
      while (true) {
        whitespace()
        const member: unknown = JSON.parse(string())
        if (typeof member !== 'string') return invalid()
        if (members.has(member)) throw new Error('duplicate JSON member')
        members.add(member)
        whitespace()
        if (source[offset++] !== ':') return invalid()
        value()
        whitespace()
        const delimiter = source[offset++]
        if (delimiter === '}') return
        if (delimiter !== ',') return invalid()
      }
    }
    if (first === '[') {
      offset++
      whitespace()
      if (source[offset] === ']') { offset++; return }
      while (true) {
        value()
        whitespace()
        const delimiter = source[offset++]
        if (delimiter === ']') return
        if (delimiter !== ',') return invalid()
      }
    }
    const start = offset
    while (offset < source.length && !/[\t\n\r ,}\]]/u.test(source[offset]!)) offset++
    if (!/^(?:null|true|false|-?(?:0|[1-9]\d*)(?:\.\d+)?(?:[eE][+-]?\d+)?)$/u.test(source.slice(start, offset))) return invalid()
  }
  value()
  whitespace()
  if (offset !== source.length) invalid()
}
