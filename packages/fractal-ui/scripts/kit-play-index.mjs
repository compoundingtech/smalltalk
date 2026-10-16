const isObject = value => value !== null && typeof value === 'object' && !Array.isArray(value)
const isNonemptyString = value => typeof value === 'string' && value.trim().length > 0

// With a tag (the per-commit `critical` set), only stories carrying it are
// selected; the whole index is still validated, and an empty selection fails.
export function storyIdsFromIndex(index, { tag } = {}) {
  if (!isObject(index) || !isObject(index.entries)) {
    throw new Error('Storybook index must contain an entries object')
  }
  const ids = []
  for (const [key, entry] of Object.entries(index.entries)) {
    if (!isObject(entry) || !['story', 'docs'].includes(entry.type) || !isNonemptyString(entry.id) || !isNonemptyString(entry.importPath)) {
      throw new Error(`Invalid Storybook index entry ${JSON.stringify(key)}: expected type story/docs and nonempty id/importPath`)
    }
    if (entry.tags !== undefined && (!Array.isArray(entry.tags) || !entry.tags.every(isNonemptyString))) {
      throw new Error(`Invalid Storybook index entry ${JSON.stringify(key)}: tags must be an array of nonempty strings`)
    }
    if (entry.type === 'story' && (!tag || entry.tags?.includes(tag))) ids.push(entry.id)
  }
  if (tag && !ids.length) throw new Error(`Storybook index has no stories tagged ${JSON.stringify(tag)}`)
  if (!ids.length || new Set(ids).size !== ids.length) {
    throw new Error('Storybook index must contain unique, nonempty story IDs')
  }
  return ids.sort()
}
