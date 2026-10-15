import type { ResourceData as TasteResource, ResourceFile as TasteFile } from '../resources/resource-model'

/** Exact dt10 public resource fixture facts consumed by Details. */
export const files: readonly TasteFile[] = [
  { path: 'src/rows.ts', added: { _tag: 'Known', value: 1 }, removed: { _tag: 'Known', value: 1 }, lines: { _tag: 'Known', value: [' export function selectRow(id: string) {', '-  return rows.find(row => row.index === id)', '+  return rows.find(row => row.id === id)', ' }', ' ', ' export const rowHeight = 24'] } },
  { path: 'src/rows.test.ts', added: { _tag: 'Known', value: 7 }, removed: { _tag: 'Known', value: 0 }, lines: { _tag: 'Known', value: [' describe("selection", () => {', '+  it("keeps the selected row after sorting", () => {', '+    const selected = selectRow("row-a")', '+    expect(selected?.id).toBe("row-a")', '+  })', ' })'] } },
  { path: 'docs/keyboard.md', added: { _tag: 'Known', value: 2 }, removed: { _tag: 'Known', value: 1 }, lines: { _tag: 'Known', value: [' # Keyboard shortcuts', '-Select a row with Enter.', '+Select the focused row with Enter.', '+Use Escape to return to the list.', ' ', ' ## Navigation'] } },
]
export const resources: readonly TasteResource[] = [
  { kind: 'diff', label: '1 changed file', detail: 'Selection stays attached to stable row identities.', fileCount: { _tag: 'Known', value: 1 }, files: [files[0]!] },
  { kind: 'pr', label: 'PR #42 · Keep selection stable', detail: 'Open · sample-app · 2 checks passed · 1 review pending', fileCount: { _tag: 'Known', value: 3 }, files },
  { kind: 'worktree', label: 'sample-app · fix/selection', detail: 'Worktree ready · 3 modified files · based on main', fileCount: { _tag: 'Known', value: 3 }, files },
]
