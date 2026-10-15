import { RegistryContext, useAtomRefresh, useAtomValue } from '@effect/atom-react'
import { Schema } from 'effect'
import * as Atom from 'effect/reactivity/Atom'
import type { AtomRegistry } from 'effect/reactivity/AtomRegistry'
import * as React from 'react'
import { buildIdentity } from 'virtual:build-identity'
import { persistence, persistedAtom } from '../state/persistence.ts'
import { decodeChangelogManifest } from './contract.ts'
import { ChangelogPopover, ChangelogView, type ChangelogLoad } from './ChangelogView.tsx'
import { changelogDeployment, changelogEntryIds } from './metadata.ts'
import { acknowledgeEntryIds, initialSeen, seenKey, type ChangelogSeen } from './seen.ts'

const seen = persistedAtom({ key: seenKey(changelogDeployment), schema: Schema.Struct({ visited: Schema.Boolean, entryIds: Schema.Array(Schema.String) }), defaultValue: initialSeen })
const baseline = Atom.make<ChangelogSeen>(initialSeen)
const manifest = Atom.make<ChangelogLoad>((get) => {
  let active = true
  get.addFinalizer(() => { active = false })
  void import('./build.ts').then((module) => {
    const decoded = decodeChangelogManifest(module.changelogManifest, { machineVersion: buildIdentity.machineVersion })
    if (active) get.setSelf({ _tag: 'Ready', manifest: decoded })
  }).catch(() => { if (active) get.setSelf({ _tag: 'Failed' }) })
  return { _tag: 'Loading' }
}).pipe(Atom.keepAlive)

/** Opening acknowledges IDs without waiting for prose to load. */
const acknowledgeChangelog = (registry: AtomRegistry): void => {
  const previous = registry.get(seen)
  registry.set(baseline, previous)
  registry.set(seen, acknowledgeEntryIds(changelogEntryIds, previous))
  persistence.flush()
}
const ChangelogContent = () => {
  const state = useAtomValue(manifest)
  const previous = useAtomValue(baseline)
  const retry = useAtomRefresh(manifest)
  return <ChangelogView state={state} baseline={previous} onRetry={retry} buildRevision={buildIdentity.rev} />
}
/** Unseen while this build lists an entry the device has not acknowledged; drives every dot. */
export const useChangelogUnseen = (): boolean => {
  const cursor = useAtomValue(seen)
  const ids = new Set(cursor.entryIds)
  return changelogEntryIds.some((id) => !ids.has(id))
}
/** The status-bar disclosure loads prose only after activation. */
export const Changelog = ({ dark }: { readonly dark: boolean }) => {
  const registry = React.useContext(RegistryContext)
  const hasUnseen = useChangelogUnseen()
  const [open, setOpen] = React.useState(false)
  const onOpenChange = (next: boolean) => {
    if (next) acknowledgeChangelog(registry)
    setOpen(next)
  }
  return <ChangelogPopover dark={dark} isOpen={open} onOpenChange={onOpenChange} hasUnseen={hasUnseen}>{open ? <ChangelogContent /> : null}</ChangelogPopover>
}
