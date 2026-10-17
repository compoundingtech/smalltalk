import { ChangelogError } from '../src/changelog/contract.ts'
import type { ChangelogDeployment, ChangelogSourceRegistration, ChangelogVisibility } from '../src/changelog/contract.ts'
import { changelogEntryModules } from './registry.generated.ts'

/**
 * Public registry, derived from this directory: every other file here is an entry and is listed
 * exactly once, newest first. `registry.generated.ts` is written by scripts/changelog-registry.mjs
 * and is never committed, so adding an entry is one new file and touches no shared line. An entry
 * is public unless it exports `visibility = 'private'`, which a public build rejects.
 */
interface ChangelogEntryModule {
  readonly default: unknown
  readonly visibility?: ChangelogVisibility
}

export const registerChangelogEntry = (file: string, module: ChangelogEntryModule): ChangelogSourceRegistration => {
  const extra = Object.keys(module).filter((name) => name !== 'default' && name !== 'visibility')
  if (extra.length > 0) throw new ChangelogError('invalid-source', `${file}: unexpected exports ${extra.join(', ')}`)
  const visibility: unknown = module.visibility ?? 'public'
  if (visibility !== 'public' && visibility !== 'private') throw new ChangelogError('invalid-source', `${file}: visibility must be 'public' or 'private'`)
  return { file, source: module.default, visibility }
}

export const changelogSources: readonly ChangelogSourceRegistration[] = Object.freeze(
  (changelogEntryModules as readonly (readonly [string, ChangelogEntryModule])[]).map(([file, module]) => registerChangelogEntry(file, module)))
export const changelogDeployment: ChangelogDeployment = 'main'
