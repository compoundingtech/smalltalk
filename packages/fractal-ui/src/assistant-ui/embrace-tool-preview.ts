export type DiffLine = {
  readonly kind: 'context' | 'added' | 'removed' | 'hunk'
  readonly text: string
  readonly oldNumber?: number
  readonly newNumber?: number
}

export type ToolDiff = {
  readonly path: string
  readonly lines: readonly DiffLine[]
  readonly added: number
  readonly removed: number
  readonly excerpt: boolean
}

export const toolFields = (value: unknown): Record<string, unknown> =>
  typeof value === 'object' && value !== null && !Array.isArray(value)
    ? value as Record<string, unknown>
    : {}

export const toolString = (value: unknown, keys: readonly string[]): string => {
  const fields = toolFields(value)
  for (const key of keys) {
    const field = fields[key]
    if (typeof field === 'string') return field
  }
  return ''
}

/** Observed human intent takes precedence over command and result diagnostics. */
export const toolHumanSummary = (input: unknown, name: string): string =>
  toolString(input, ['i', 'description', 'summary']) || name

/** Extract supported textual output, never substitute a JSON dump for a preview. */
export const toolOutput = (value: unknown): string => {
  if (typeof value === 'string') return value
  if (Array.isArray(value)) return value.map(toolOutput).filter(Boolean).join('\n')
  const output = toolString(value, ['text', 'output', 'stdout', 'content', 'message', 'error'])
  const stderr = toolString(value, ['stderr'])
  return [output, stderr].filter(Boolean).join('\n')
}

const counted = (path: string, lines: DiffLine[], excerpt: boolean): ToolDiff => {
  let added = 0
  let removed = 0
  for (const line of lines) {
    if (line.kind === 'added') added++
    if (line.kind === 'removed') removed++
  }
  return { path, lines, added, removed, excerpt }
}

/** Unified patches retain file boundaries, hunk offsets and side-specific line numbers. */
export const parseUnifiedDiff = (patch: string): readonly ToolDiff[] => {
  const files: ToolDiff[] = []
  let path = ''
  let lines: DiffLine[] = []
  let oldNumber = 0
  let newNumber = 0
  let inHunk = false
  const flush = () => {
    if (path !== '' && lines.length > 0) files.push(counted(path, lines, false))
    lines = []
    inHunk = false
  }
  for (const line of patch.split('\n')) {
    if (line.startsWith('diff --git ')) {
      flush()
      path = ''
    } else if (line.startsWith('--- ')) {
      flush()
      path = line.slice(4).split('\t')[0]!.replace(/^a\//, '')
    } else if (line.startsWith('+++ ')) {
      const nextPath = line.slice(4).split('\t')[0]!.replace(/^b\//, '')
      if (nextPath !== '/dev/null') path = nextPath
    } else {
      const hunk = /^@@ -(\d+)(?:,\d+)? \+(\d+)(?:,\d+)? @@/.exec(line)
      if (hunk !== null) {
        oldNumber = Number(hunk[1])
        newNumber = Number(hunk[2])
        inHunk = true
        lines.push({ kind: 'hunk', text: line })
      } else if (inHunk && line.startsWith('+')) {
        lines.push({ kind: 'added', text: line.slice(1), newNumber: newNumber++ })
      } else if (inHunk && line.startsWith('-')) {
        lines.push({ kind: 'removed', text: line.slice(1), oldNumber: oldNumber++ })
      } else if (inHunk && line.startsWith(' ')) {
        lines.push({ kind: 'context', text: line.slice(1), oldNumber: oldNumber++, newNumber: newNumber++ })
      } else if (inHunk && line.startsWith('\\ No newline')) {
        lines.push({ kind: 'hunk', text: line })
      }
    }
  }
  flush()
  return files
}

/** Codex patch excerpts have no absolute offsets; do not invent full-file line numbers. */
export const parseToolPatch = (patch: string): readonly ToolDiff[] => {
  const files: ToolDiff[] = []
  let path = ''
  let lines: DiffLine[] = []
  const flush = () => {
    if (path !== '' && lines.length > 0) files.push(counted(path, lines, true))
    lines = []
  }
  for (const line of patch.split('\n')) {
    const header = /^\*\*\* (?:Update|Add|Delete) File: (.+)$/.exec(line)
    if (header !== null) {
      flush()
      path = header[1]!
    } else if (path !== '' && line.startsWith('@@')) {
      lines.push({ kind: 'hunk', text: line })
    } else if (path !== '' && /^[+\- ]/.test(line)) {
      lines.push({ kind: line[0] === '+' ? 'added' : line[0] === '-' ? 'removed' : 'context', text: line.slice(1) })
    }
  }
  flush()
  return files
}

/** LCS snippet comparison ported from the source conversation/tools.ts helper. */
export const diffToolSnippet = (path: string, beforeText: string, afterText: string): ToolDiff => {
  const before = beforeText === '' ? [] : beforeText.split('\n')
  const after = afterText === '' ? [] : afterText.split('\n')
  const columns = after.length + 1
  const lcs = new Uint32Array((before.length + 1) * columns)
  for (let i = before.length - 1; i >= 0; i--) {
    for (let j = after.length - 1; j >= 0; j--) {
      lcs[i * columns + j] = before[i] === after[j]
        ? lcs[(i + 1) * columns + j + 1]! + 1
        : Math.max(lcs[(i + 1) * columns + j]!, lcs[i * columns + j + 1]!)
    }
  }
  const lines: DiffLine[] = []
  let i = 0
  let j = 0
  while (i < before.length || j < after.length) {
    if (i < before.length && j < after.length && before[i] === after[j]) {
      lines.push({ kind: 'context', text: before[i]! })
      i++
      j++
    } else if (i < before.length && (j >= after.length || lcs[(i + 1) * columns + j]! >= lcs[i * columns + j + 1]!)) {
      lines.push({ kind: 'removed', text: before[i++]! })
    } else {
      lines.push({ kind: 'added', text: after[j++]! })
    }
  }
  return counted(path, lines, true)
}

export const toolDiffs = (input: unknown, output: unknown): readonly ToolDiff[] => {
  const outputPatch = parseUnifiedDiff(toolOutput(output))
  if (outputPatch.length > 0) return outputPatch
  const patch = toolString(input, ['patch', 'input'])
  const unified = parseUnifiedDiff(patch)
  if (unified.length > 0) return unified
  const custom = parseToolPatch(patch)
  if (custom.length > 0) return custom
  const fields = toolFields(input)
  const before = fields.oldString ?? fields.old_string ?? fields.before
  const after = fields.newString ?? fields.new_string ?? fields.after
  if (typeof before === 'string' && typeof after === 'string') {
    return [diffToolSnippet(toolString(input, ['path', 'file_path']), before, after)]
  }
  return []
}
